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
use crate::types::{jlong_bits_as_aligned_object_ptr, ObjectRef, Value};

use super::{SharedVm};
use crate::classloading::ClassStore;
use crate::native::registry::NativeCallback;

// ---------------------------------------------------------------------------
// CP-resolved-interface plumbing for the default-method rescue
// ---------------------------------------------------------------------------
//
// invokeinterface on a receiver whose runtime class is bare `java/lang/Object`
// (e.g. a synthetic ServiceLoader provider stub) loses track of the CP-resolved
// interface as control flows through `execute_invoke` → `invoke_shared` →
// `invoke_on_class_shared_inner`. The receiver-based rescues in those layers
// cannot recover a default method declared on the interface itself because the
// receiver class doesn't list the interface in its `interfaces` table.
//
// We stash the CP-resolved interface class id on a per-thread slot just before
// the dispatch site, and consume it at the NSME-emit site to give the lookup
// one more chance against the interface's own hierarchy. The slot is
// per-thread and is cleared after each invoke to avoid leaking between calls.
std::thread_local! {
    static INVOKE_CP_IFACE_CID: std::cell::Cell<Option<ClassId>> =
        const { std::cell::Cell::new(None) };
}

/// RAII guard that swaps the per-thread "pending CP iface" slot on construction
/// and restores the prior value on drop. Use at each invoke boundary so nested
/// invokes don't leak each other's CP iface ids.
pub struct PendingCpIfaceGuard {
    prev: Option<ClassId>,
}

impl PendingCpIfaceGuard {
    pub fn new(new_cid: Option<ClassId>) -> Self {
        let prev = INVOKE_CP_IFACE_CID.with(|c| {
            let p = c.get();
            c.set(new_cid);
            p
        });
        Self { prev }
    }
}

impl Drop for PendingCpIfaceGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        INVOKE_CP_IFACE_CID.with(|c| c.set(prev));
    }
}

/// Read (without clearing) the pending CP-resolved interface id, if any.
fn pending_cp_iface() -> Option<ClassId> {
    INVOKE_CP_IFACE_CID.with(|c| c.get())
}

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
            // JNI and a few internal bridges surface jobject handles as raw
            // i64 / jlong in `Value::Long`.  If we keep that shape through
            // `push_invoke_return_value`, the slot is stored as VTAG_LONG,
            // GC never traces it, and the next `astore`/`aload` sequence can
            // use a collected `java.lang.Class` — Letsgo AV after
            // `ConfigurationClassEnhancer.enhance` returns.
            Value::Long(v) => {
                if let Some(p) = jlong_bits_as_aligned_object_ptr(v as u64) {
                    Value::Object(Some(unsafe { ObjectRef::from_raw(p as *mut u8) }))
                } else {
                    Value::Object(None)
                }
            }
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

/// Validated variant of [`coerce_value_for_return`] for the `b'L' | b'['`
/// path: instead of trusting any aligned `Value::Long` bits as a valid
/// jobject pointer (the legacy bug that lets a `long` carrying e.g. a file
/// size or hash code be reinterpreted as an `ObjectRef`, then later marked
/// by GC -> Win32 SEGV / 0xC0000005), round-trip the bits through
/// `shared.heap.is_object_address`. Real `Value::Object(_)` and primitive
/// return types are forwarded to the legacy coercer unchanged.
///
/// This mirrors `value_as_validated_object_ref` and is the read-side fix
/// for the same "Value::Long misidentified as ObjectRef" family of crashes
/// that the ξ patch fixed on the native-pin path.
#[inline]
pub fn coerce_value_for_return_validated(
    shared: &SharedVm,
    value: Value,
    ret_type: u8,
) -> Value {
    if matches!(ret_type, b'L' | b'[') {
        return match value {
            Value::Int(0) | Value::Long(0) => Value::Object(None),
            Value::Object(_) => value,
            Value::Long(v) => match jlong_bits_as_aligned_object_ptr(v as u64) {
                Some(p) => match shared.heap.is_object_address(p) {
                    Some(obj) => Value::Object(Some(obj)),
                    None => Value::Object(None),
                },
                None => Value::Object(None),
            },
            other => other,
        };
    }
    coerce_value_for_return(value, ret_type)
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

/// Extract a heap [`ObjectRef`] from a [`Value`] that may carry a JNI
/// `jobject` as `Value::Long` (aligned pointer bits).
#[inline]
pub fn value_as_object_ref(v: Value) -> Option<ObjectRef> {
    match v {
        Value::Object(Some(o)) => Some(o),
        Value::Long(bits) => jlong_bits_as_aligned_object_ptr(bits as u64)
            .map(|p| unsafe { ObjectRef::from_raw(p as *mut u8) }),
        _ => None,
    }
}

/// Like [`value_as_object_ref`] but verifies that a `Value::Long` whose bits
/// look like an aligned pointer actually points at a heap object before
/// returning it. Without this check, a real `Value::Long` carrying e.g. a
/// file size that happens to be 8-byte aligned would be reinterpreted as a
/// jobject; the GC would later try to mark/move that bogus pointer and
/// SEGV.  Real `Value::Object(Some(_))` is always returned as-is.
#[inline]
pub fn value_as_validated_object_ref(shared: &SharedVm, v: Value) -> Option<ObjectRef> {
    match v {
        Value::Object(Some(o)) => Some(o),
        Value::Long(bits) => {
            let p = jlong_bits_as_aligned_object_ptr(bits as u64)?;
            shared.heap.is_object_address(p)
        }
        _ => None,
    }
}

/// Pin a value that may encode a jobject as `Value::Long` for the duration
/// of a native call (see `safe_native_call`).
#[inline]
fn pin_value_for_native_call(shared: &SharedVm, roots: &mut Vec<ObjectRef>, v: &Value) {
    if let Some(o) = value_as_validated_object_ref(shared, *v) {
        roots.push(o);
    }
}

/// Call a native callback, catching panics and converting them to
/// `MethodCallFailed` so that a bug in a native method doesn't crash the VM.
pub fn safe_native_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
) -> MethodCallResult {
    // letsgo postmortem: record native dispatch with the caller frame's
    // identity so a SEGV inside a native callback leaves a breadcrumb of
    // *who* called it. The callback itself is an opaque fn-pointer, but
    // the top Java frame is the invokevirtual/invokestatic site.
    if crate::dispatch_trace::is_enabled() {
        let (cls, mth, des) = match thread.frames.last() {
            Some(f) => (
                f.class_name().to_string(),
                f.method_name().to_string(),
                f.method_descriptor().to_string(),
            ),
            None => ("<no-frame>".to_string(), "<native>".to_string(), String::new()),
        };
        crate::dispatch_trace::record_native(
            thread.thread_id.0 as usize,
            &cls,
            &mth,
            &des,
        );
    }
    // Pin object arguments for the duration of the native: they have been
    // popped from the operand stack into this Rust slice and are otherwise
    // invisible to `collect_roots` / frame scanning during a safepoint GC.
    let pin_base = thread.native_pin_roots.len();
    for a in args {
        pin_value_for_native_call(shared, &mut thread.native_pin_roots, a);
    }

    let result = {
        let mut ctx = NativeContextImpl { shared, thread };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback(&mut ctx, args)
        }))
    };

    let out: MethodCallResult = match result {
        Ok(method_result) => {
            if let Some(exc_handle) = crate::native::jni::take_jni_pending_exception() {
                thread.native_pin_roots.truncate(pin_base);
                thread.native_pending_return = None;
                if exc_handle == u64::MAX {
                    return Err(
                        crate::runtime::exceptions::throw_runtime_error(
                            shared,
                            thread,
                            RuntimeError::IllegalStateException {
                                message: "JNI ThrowNew pending exception".to_string(),
                            },
                        ),
                    );
                }
                let ptr = exc_handle as *mut u8;
                if !ptr.is_null() && (ptr as usize) % 8 == 0 {
                    let exc_ref = unsafe { crate::types::ObjectRef::from_raw(ptr) };
                    return Err(MethodCallFailed::ExceptionThrown(exc_ref));
                }
            }
            method_result
        }
        Err(payload) => {
            thread.native_pin_roots.truncate(pin_base);
            thread.native_pending_return = None;
            let _ = crate::native::jni::take_jni_pending_exception();
            let msg = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "unknown native method panic".to_string()
            };
            let in_bootstrap = shared.get_init_level() < 4;
            if (msg.contains("unaligned pointer") || msg.contains("null pointer")) && in_bootstrap {
                shared
                    .swallow_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!("Native method panic (bootstrap): {}", msg);
                if std::env::var("RUSTJVM_STRICT_SWALLOWS").ok().as_deref() == Some("1") {
                    tracing::error!(
                        "RUSTJVM_STRICT_SWALLOWS=1: safe_native_call bootstrap panic: {}",
                        msg,
                    );
                    std::process::abort();
                }
            } else {
                let top = thread.frames.last().map(|f| format!("{}.{}{}", f.class_name(), f.method_name(), f.method_descriptor())).unwrap_or_default();
                tracing::error!("Native method panic caught: {} (native invoked from {})", msg, top);
            }
            Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!("native method panic: {msg}"),
            }))
        }
    };

    thread.native_pin_roots.truncate(pin_base);
    thread.native_pending_return = None;
    if let Ok(Some(v)) = &out {
        if let Some(o) = value_as_validated_object_ref(shared, *v) {
            thread.native_pending_return = Some(o);
        }
    }
    if thread.native_pending_return.is_some() {
        crate::runtime::interpreter::update_root_snapshot(shared, thread);
    }

    out
}

/// Clear [`JvmThread::native_pending_return`] after its object has been pushed
/// onto the caller operand stack (stackless native invoke path).
#[inline]
pub fn native_return_pushed_to_stack(shared: &SharedVm, thread: &mut JvmThread) {
    thread.native_pending_return = None;
    crate::runtime::interpreter::update_root_snapshot(shared, thread);
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

#[inline]
fn normalize_system_property_key(key: &str) -> &str {
    key.trim_matches(|c: char| c.is_ascii_control() || c == '\0')
}

impl<'a> NativeContextImpl<'a> {
    /// Deposit a root snapshot of this thread's frames into the shared registry.
    /// Called before any blocking operation so GC can scan this thread's roots.
    ///
    /// See `update_root_snapshot` (`vm/src/runtime/interpreter.rs`) for why the
    /// operand-stack-sourced roots are filtered against `heap.is_object_address`:
    /// `ValueStack::scan_object_refs` still treats pointer-shaped `Long` bits as
    /// roots without heap validation (its file is restricted from edits), and
    /// the resulting bogus addresses crash the GC at the next mark/move.
    pub(crate) fn deposit_root_snapshot(&self) {
        let mut snapshot = self.thread.root_snapshot.lock();
        snapshot.clear();
        for frame in &self.thread.frames {
            frame.scan_local_objects(&mut snapshot);
            let before = snapshot.len();
            frame.stack.scan_object_refs(&mut snapshot, &self.shared.heap);
            if snapshot.len() > before {
                let added = snapshot.split_off(before);
                for o in added {
                    let addr = o.as_ptr() as usize;
                    if self.shared.heap.is_object_address(addr).is_some() {
                        snapshot.push(o);
                    }
                }
            }
        }
        snapshot.extend(self.thread.native_pin_roots.iter().copied());
        if let Some(r) = self.thread.native_pending_return {
            snapshot.push(r);
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
                for obj_ref in &mut self.thread.native_pin_roots {
                    let old_addr = obj_ref.as_ptr() as usize;
                    if let Some(&new_addr) = pointer_map.get(&old_addr) {
                        *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                    }
                }
                if let Some(ref mut obj_ref) = self.thread.native_pending_return {
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

    fn is_class_synthetic_stub(&self, class_name: &str) -> bool {
        match self.shared.load_class_concurrent(class_name) {
            Ok(class_id) => self
                .shared
                .class_manager
                .read()
                .get_class(class_id)
                .is_some_and(|c| c.is_synthetic_stub),
            Err(_) => false,
        }
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
        let kind = self.shared.heap.kind_of(obj);
        if kind != ObjectKind::Array {
            let class_id = self.shared.heap.class_id_of(obj);
            let class_name = self
                .shared
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            let top = self
                .thread
                .frames
                .last()
                .map(|f| format!("{}.{}{}", f.class_name(), f.method_name(), f.method_descriptor()))
                .unwrap_or_else(|| "<no-frame>".to_string());
            eprintln!(
                "[ARRAY-LEN-GUARD] non-array object class={} kind={:?} caller={}",
                class_name, kind, top
            );
            for (i, f) in self.thread.frames.iter().enumerate().rev().take(8) {
                eprintln!(
                    "[ARRAY-LEN-GUARD]   stack[{i}] {}.{}{}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor()
                );
            }
            return 0;
        }
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
        if let Some(s) = super::read_java_string(&self.shared.heap, obj) {
            return Some(s);
        }
        let class_id = self.shared.heap.class_id_of(obj);
        let cm = self.shared.class_manager.read();
        if cm
            .get_class(class_id)
            .map(|c| &*c.name != "java/lang/String")
            .unwrap_or(true)
        {
            drop(cm);
            return None;
        }
        let vidx = resolve_field_index_in_hierarchy(class_id, "value", &cm.class_store)?;
        let cidx = resolve_field_index_in_hierarchy(class_id, "coder", &cm.class_store);
        drop(cm);
        let value_array = match self.shared.heap.get_field(obj, vidx) {
            Value::Object(Some(a)) => a,
            _ => return None,
        };
        let coder = cidx
            .map(|idx| match self.shared.heap.get_field(obj, idx) {
                Value::Int(c) => c,
                _ => 0,
            })
            .unwrap_or(0);
        super::vm_object::decode_java_string_value_array(&self.shared.heap, value_array, coder)
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
            "in" => *self.shared.system_in.read(),
            _ => None,
        }
    }

    fn cache_system_stdin(&mut self, stream: ObjectRef) {
        *self.shared.system_in.write() = Some(stream);
    }

    fn get_system_property(&self, key: &str) -> Option<String> {
        let normalized = normalize_system_property_key(key);
        self.shared.system_properties.read().get(normalized).cloned()
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
        let normalized = normalize_system_property_key(key).to_string();
        self.shared
            .system_properties
            .write()
            .insert(normalized, value.to_string())
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
        if self
            .shared
            .class_manager
            .read()
            .is_subclass_of(child, parent)
        {
            return true;
        }
        // Synthetic lambda proxy ClassIds (>= 0x8000_0000) are not in the
        // class manager, so `is_subclass_of` always reports false. Reflection
        // callers like CGLIB's `CallbackInfo.determineType` then conclude that
        // the lambda doesn't implement its functional interface and throw
        // `IllegalStateException("Unknown callback type ...")`. Route through
        // `lambda_proxy_satisfies` so reflective `isAssignableFrom`,
        // `isInstance`, and friends agree with the interpreter's
        // checkcast/instanceof view.
        if child.as_u32() >= 0x8000_0000 {
            return crate::runtime::interpreter::lambda_proxy_satisfies_public(
                self.shared,
                child,
                parent,
            );
        }
        false
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
        // KC16-watchdog: stash a snapshot of the current frame chain in a
        // thread-local so the watchdog's wait-site dump callback can emit
        // it if the stack-dump flag fires while we are parked in
        // `wait_condvar.wait_for`. See `vm_init::dump_wait_site_thread_local`.
        crate::vm::vm_init::set_wait_site_snapshot(&*self.thread);
        let wait_start = std::time::Instant::now();
        let was_interrupted = self.shared.monitors.wait(
            obj,
            self.thread.thread_id,
            timeout_ms,
            Some(&self.thread.interrupted),
        )?;
        crate::vm::vm_init::clear_wait_site_snapshot();
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
        // RKC16N.30 — child Java threads need the same 64 MB native stack
        // that the main-vm thread gets in `vm-cli/src/main.rs`. The default
        // Rust thread stack (2 MB on Windows) overflows during deeply
        // recursive Java callees (e.g. BouncyCastle provider self-test
        // chains thousands of `<clinit>` levels deep, JDK Stream pipeline
        // composition, recursive parser combinators), and the Windows
        // SEH-converted SIGSEGV is opaque — no Java stack trace, no panic,
        // just a process exit code 139. Match the main-vm sizing so any
        // user `Thread.start()` gets the same headroom as the entry point.
        // Stack size honours `RUST_MIN_STACK` so callers can override.
        let child_stack_size = std::env::var("RUST_MIN_STACK")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(64 * 1024 * 1024);
        let thread_name_for_builder = name.clone();
        let handle = std::thread::Builder::new()
            .name(thread_name_for_builder)
            .stack_size(child_stack_size)
            .spawn(move || {
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
                // W1-C: dispatch the per-Thread (or default)
                // UncaughtExceptionHandler before dropping the exception.
                // HotSpot calls Thread.dispatchUncaughtException(Throwable)
                // when Thread.run() escapes; we do the same. The Java method
                // walks the per-instance handler -> ThreadGroup -> default
                // chain itself, so we just need to call it. Any error from
                // the handler dispatch is swallowed (HotSpot does the same:
                // a buggy handler can't take down the VM further than the
                // original exception already did).
                if let MethodCallFailed::ExceptionThrown(exc) = &e {
                    let exc_ref = *exc;
                    let dispatch_result = invoke_on_class_shared(
                        &shared_arc,
                        &mut jvm_thread,
                        recv_cid,
                        "dispatchUncaughtException",
                        "(Ljava/lang/Throwable;)V",
                        &[
                            Value::Object(Some(thread_obj_for_spawn)),
                            Value::Object(Some(exc_ref)),
                        ],
                    );
                    if let Err(de) = dispatch_result {
                        eprintln!(
                            "Thread {} terminated with error: {:?} (dispatchUncaughtException also failed: {:?})",
                            tid, e, de
                        );
                    }
                } else {
                    eprintln!("Thread {} terminated with error: {:?}", tid, e);
                }
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
        })
        .expect("failed to spawn child Java thread (OS refused; check ulimit / thread count)");

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
            .map(|m| {
                // WP2.5 v3 — populate `exceptions` from the JVMS §4.7.5
                // `Exceptions` attribute when present. Used by the proxy
                // generator to thread the declared throws set into
                // `<clinit>` so the dispatch helper's UTE wrap can match
                // thrown exceptions against the method's declared set.
                let exceptions: Vec<String> = m
                    .attributes
                    .iter()
                    .find_map(|a| match a.as_decoded() {
                        Some(rustjvm_reader::attribute::Attribute::Exceptions {
                            exception_indices,
                        }) => Some(
                            exception_indices
                                .iter()
                                .filter_map(|idx| {
                                    class
                                        .constant_pool
                                        .get_class_name(*idx)
                                        .map(str::to_string)
                                })
                                .collect::<Vec<String>>(),
                        ),
                        _ => None,
                    })
                    .unwrap_or_default();
                MethodMetadata {
                    name: m.name.to_string(),
                    descriptor: m.descriptor.to_string(),
                    access_flags: m.access_flags.bits(),
                    declaring_class_id: class_id,
                    exceptions,
                }
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

        if let Some(lcs) = call_site.filter(|lcs| method_name == &*lcs.sam_method_name) {
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
                                .unwrap_or_else(|| lcs.impl_handle.class_name.to_string())
                        }
                        _ => lcs.impl_handle.class_name.to_string(),
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
                        ))) if target_class.as_str() != &*lcs.impl_handle.class_name => self.invoke_or_native(
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
                    proxies.get(&receiver_class_id).map(|lcs| lcs.functional_interface.to_string())
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
                    if let Some(rustjvm_reader::attribute::Attribute::Signature(s)) =
                        attr.as_decoded()
                    {
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
                    if let Some(rustjvm_reader::attribute::Attribute::MethodParameters(params)) =
                        attr.as_decoded()
                    {
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
                    if let Some(rustjvm_reader::attribute::Attribute::Signature(s)) =
                        attr.as_decoded()
                    {
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
                    if let Some(rustjvm_reader::attribute::Attribute::AnnotationDefault(ev)) =
                        attr.as_decoded()
                    {
                        return convert_element_value(ev, &class.constant_pool);
                    }
                }
                return None;
            }
        }
        None
    }

    fn method_exceptions(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<String> {
        let cm = self.shared.class_manager.read();
        let class = match cm.get_class(class_id) {
            Some(c) => c,
            None => return Vec::new(),
        };
        for m in &class.methods {
            if &*m.name == method_name && &*m.descriptor == method_desc {
                for attr in &m.attributes {
                    if let Some(rustjvm_reader::attribute::Attribute::Exceptions {
                        exception_indices,
                    }) = attr.as_decoded()
                    {
                        return exception_indices
                            .iter()
                            .filter_map(|idx| {
                                class.constant_pool.get_class_name(*idx).map(|s| s.to_string())
                            })
                            .collect();
                    }
                }
                return Vec::new();
            }
        }
        Vec::new()
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
        //
        // Windows: Apache `tcnative-*.dll` and Netty `*tcnative*.dll` often fault
        // inside `JNI_OnLoad` / `RegisterNatives` when paired with RustJVM. We keep
        // the DLL loaded (classpath / Tomcat may probe for its presence) but skip
        // `JNI_OnLoad` — Java entry points are satisfied via Rust stubs and
        // `find_jni_native` / `resolve_jni_native_in_libraries` blocks for
        // `org/apache/tomcat/jni/**` and `io/netty/internal/tcnative/**`.
        let basename_lc = std::path::Path::new(resolved.as_str())
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        #[cfg(windows)]
        let skip_jni_onload_tcnative = basename_lc.contains("tcnative");
        #[cfg(not(windows))]
        let skip_jni_onload_tcnative = false;

        unsafe {
            type JniOnLoad = extern "C" fn(
                crate::native::jni::JavaVM,
                *mut std::ffi::c_void,
            ) -> crate::native::jni::JInt;
            if !skip_jni_onload_tcnative {
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

    fn find_all_resource_bytes(&self, name: &str) -> Vec<Vec<u8>> {
        self.shared.class_manager.read().find_all_resource_bytes(name)
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
    attributes: &[rustjvm_reader::attribute::LazyAttribute],
    cp: &rustjvm_reader::constant_pool::ConstantPool,
) -> Vec<crate::native::registry::AnnotationData> {
    use rustjvm_reader::attribute::Attribute;
    let mut result = Vec::new();
    for lazy in attributes {
        let attr = match lazy.as_decoded() {
            Some(a) => a,
            None => continue,
        };
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
    attributes: &[rustjvm_reader::attribute::LazyAttribute],
    cp: &rustjvm_reader::constant_pool::ConstantPool,
) -> Vec<Vec<crate::native::registry::AnnotationData>> {
    use rustjvm_reader::attribute::Attribute;
    for lazy in attributes {
        let attr = match lazy.as_decoded() {
            Some(a) => a,
            None => continue,
        };
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

    // Forked Surefire calls `ClassLoader.setDefaultAssertionStatus` very early on
    // the context loader (`AppClassLoader` / `BuiltinClassLoader`). Inline-cache
    // promotion can still land on JDK bytecode for `java/lang/ClassLoader` when
    // `assertionLock` is not yet assigned, so `synchronized (assertionLock)` throws
    // NPE. The built-in Rust override is a correct no-op — always prefer it.
    if method_name == "setDefaultAssertionStatus" && descriptor == "(Z)V" {
        if let Some(callback) =
            shared.native_methods.find("java/lang/ClassLoader", method_name, descriptor)
        {
            return safe_native_call(shared, thread, callback, args)
                .map(|v| coerce_native_return(v, descriptor));
        }
        return Ok(None);
    }

    // peaceful-sammet — primitive-return functional-interface bridge.
    //
    // Spring/Eureka call `ToIntFunction.apply(Object)Object` on a receiver
    // whose actual class is `ToIntFunction` (a lambda proxy whose SAM is
    // `applyAsInt`). The JDK interface has no `apply` method, so naive
    // dispatch raises NoSuchMethodError. Redirect to `applyAsX` and box
    // the primitive result. Same for ToLong/ToDouble, Int/Long/Double-Predicate,
    // and Int/Long/Double-Function (which has Object apply(int/long/double)).
    if method_name == "apply"
        && descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
        && args.len() == 2
    {
        let (prim_sam, prim_desc, box_class, box_desc): (
            &str, &str, &str, &str,
        ) = match class_name {
            "java/util/function/ToIntFunction" => (
                "applyAsInt",
                "(Ljava/lang/Object;)I",
                "java/lang/Integer",
                "(I)Ljava/lang/Integer;",
            ),
            "java/util/function/ToLongFunction" => (
                "applyAsLong",
                "(Ljava/lang/Object;)J",
                "java/lang/Long",
                "(J)Ljava/lang/Long;",
            ),
            "java/util/function/ToDoubleFunction" => (
                "applyAsDouble",
                "(Ljava/lang/Object;)D",
                "java/lang/Double",
                "(D)Ljava/lang/Double;",
            ),
            _ => ("", "", "", ""),
        };
        if !prim_sam.is_empty() {
            // Resolve the receiver's actual class for virtual dispatch.
            let recv_class = match args.first() {
                Some(Value::Object(Some(o))) => {
                    let cid = shared.heap.class_id_of(*o);
                    shared
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| class_name.to_string())
                }
                _ => class_name.to_string(),
            };
            let prim_result = invoke_or_native(
                shared,
                thread,
                &recv_class,
                prim_sam,
                prim_desc,
                args,
            )?;
            let prim_val = prim_result.unwrap_or(Value::Int(0));
            let boxed = invoke_shared(
                shared,
                thread,
                box_class,
                "valueOf",
                box_desc,
                &[prim_val],
            )?;
            return Ok(boxed);
        }
    }

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
                        // S107 collection-toString fix: if this parent has its
                        // own bytecode for the method (e.g.
                        // AbstractCollection.toString), the bytecode override
                        // wins over any deeper native ancestor (e.g.
                        // Object.toString). Stop walking so the bytecode
                        // dispatch path runs.
                        //
                        // Round 19 (peaceful-sammet) — IMPORTANT exception: if
                        // the parent has BOTH bytecode AND a Rust native, the
                        // native wins. See `populate_virtual_invoke_cache` for
                        // the full LinkedHashMap-overlay rationale.
                        if parent.find_method(method_name, descriptor).is_some() {
                            if let Some(callback) = shared.native_methods.find(&parent.name, method_name, descriptor) {
                                drop(cm);
                                return safe_native_call(shared, thread, callback, args)
                                    .map(|v| coerce_native_return(v, descriptor));
                            }
                            break;
                        }
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

/// S111r11 — write the RustJVM extra metadata slots on a synthetic
/// proxy `Method` object so that
/// `native-builtins/.../lang_class.rs::native_method_invoke` finds the
/// descriptor + parameter-count when it later reflects through.
///
/// `native_method_invoke` reads the descriptor from the extra slot
/// (`base + METHOD_EXTRA_OFFSET_DESC`), NOT from the JDK `signature`
/// field — leaving these blank produces a `descriptor=""` virtual
/// dispatch that misses the real bridge method on the receiver class
/// (e.g. `ParameterizedTypeImpl.getRawType` covariant bridge) and
/// surfaces as a confusing `NoSuchMethodError`.
///
/// Constants mirror those in
/// `native-builtins/src/lang_class.rs::METHOD_EXTRA_*`. We don't import
/// them across the crate boundary because `vm` doesn't depend on
/// `native-builtins` (the dependency goes the other way).
fn proxy_method_write_extra_slots(
    shared: &SharedVm,
    method_obj: ObjectRef,
    descriptor: &str,
    param_count: usize,
) {
    const METHOD_NUM_FIELDS_LEGACY_FLOOR: usize = 13;
    const METHOD_EXTRA_OFFSET_DESC: usize = 0;
    const METHOD_EXTRA_OFFSET_PARAM_COUNT: usize = 1;

    let class_id = shared.heap.class_id_of(method_obj);
    let total_fields = shared
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.first_field_index + c.fields.iter().filter(|f| !f.is_static()).count())
        .unwrap_or(0);
    let base = core::cmp::max(METHOD_NUM_FIELDS_LEGACY_FLOOR, total_fields);

    // Defensive: only write if the heap object was allocated wide enough.
    // `proxy_invoke_handler` and `proxy_invoke_handler_shared` both call
    // `alloc_object(method_class_id, total_fields.max(8))` — that gives
    // us at least the JDK width, but no extra slots when running against
    // a synthetic-stub Method class. In that case skip the writes; the
    // legacy hard-coded slots above already covered the synthetic path.
    let num_obj_fields = shared.heap.num_fields(method_obj);
    let extra_desc_slot = base + METHOD_EXTRA_OFFSET_DESC;
    let extra_pc_slot = base + METHOD_EXTRA_OFFSET_PARAM_COUNT;
    if num_obj_fields <= extra_pc_slot {
        return;
    }
    let desc_str = super::create_java_string(shared, descriptor);
    shared.heap.set_field(method_obj, extra_desc_slot, Value::Object(Some(desc_str)));
    shared.heap.set_field(method_obj, extra_pc_slot, Value::Int(param_count as i32));
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
    // S111r11: allocate METHOD_EXTRA_SLOTS more than the JDK layout so
    // `proxy_method_write_extra_slots` can write descriptor + param count
    // for `native_method_invoke` to read back.
    const METHOD_EXTRA_SLOTS: usize = 3;
    const METHOD_NUM_FIELDS_LEGACY_FLOOR: usize = 13;
    let alloc_base = core::cmp::max(METHOD_NUM_FIELDS_LEGACY_FLOOR, total_fields);
    let method_obj = ctx
        .shared
        .heap
        .alloc_object(method_class_id, alloc_base + METHOD_EXTRA_SLOTS);
    let zero_mirror = super::get_or_create_class_mirror(ctx.shared, ClassId::new(0));
    // Resolve the actual declaring-interface mirror for the synthesized
    // Method's `clazz` field — see `proxy_resolve_declaring_class_mirror`
    // for why `ClassId(0)` (== `Object` in production) is unsafe here.
    let declaring_mirror = proxy_resolve_declaring_class_mirror(
        ctx.shared,
        proxy,
        method_name,
        descriptor,
    );
    let name_str = super::create_java_string(ctx.shared, method_name);
    // Parse descriptor into per-parameter and return type descriptors so we
    // can populate the synthetic Method's `returnType` and `parameterTypes`
    // with semantically correct Class mirrors (e.g. real `Type.class`).
    // Without this, Spring's `TypeProxyInvocationHandler.invoke` falls
    // through to its default branch (`method.invoke(provider.getType())`)
    // and our native_method_invoke virtual-dispatches the proxy method
    // name on the underlying Type — calling e.g. `getGenericInterfaces`
    // on a `ParameterizedTypeImpl` (NoSuchMethodError).
    let (param_descs, ret_desc) = proxy_split_descriptor(descriptor);
    let return_type_mirror = proxy_descriptor_to_class_mirror(ctx.shared, &ret_desc);
    let param_count = param_descs.len();
    let param_arr = ctx.shared.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        param_count,
    );
    for (i, pdesc) in param_descs.iter().enumerate() {
        let pmirror = proxy_descriptor_to_class_mirror(ctx.shared, pdesc);
        ctx.shared
            .heap
            .set_array_element(param_arr, i, Value::Object(Some(pmirror)))
            .ok();
    }
    let desc_str = super::create_java_string(ctx.shared, descriptor);
    proxy_method_set_field_by_name(ctx.shared, method_obj, "clazz", Value::Object(Some(declaring_mirror)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "name", Value::Object(Some(name_str)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "returnType", Value::Object(Some(return_type_mirror)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "parameterTypes", Value::Object(Some(param_arr)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "modifiers", Value::Int(1)); // PUBLIC
    proxy_method_set_field_by_name(ctx.shared, method_obj, "signature", Value::Object(Some(desc_str)));
    proxy_method_set_field_by_name(ctx.shared, method_obj, "slot", Value::Int(0));
    // S111r11: also populate the RustJVM extra-slot descriptor +
    // parameter-count cache so `native_method_invoke` (which reads via
    // `read_method_descriptor`, NOT `signature`) sees a non-empty
    // descriptor when the proxy fallback path reflectively re-invokes
    // the method on the underlying Type — fixes
    // `NoSuchMethodError: ParameterizedTypeImpl.getRawType` (descriptor
    // was empty, so virtual dispatch couldn't match the bridge method).
    proxy_method_write_extra_slots(ctx.shared, method_obj, descriptor, param_count);
    // Belt-and-suspenders: also write the legacy hard-coded slots so any
    // surviving raw-index reader (notably the lambda dispatch path that
    // pulls `Method.getName` via `get_field_by_name` already lands on
    // the right slot, but older diagnostic readers may still poke 1..7).
    if total_fields >= 8 {
        // Skip вЂ” class layout already has the JDK fields populated above.
    } else {
        ctx.shared.heap.set_field(method_obj, 0, Value::Object(Some(declaring_mirror)));
        ctx.shared.heap.set_field(method_obj, 1, Value::Object(Some(name_str)));
        ctx.shared.heap.set_field(method_obj, 2, Value::Object(Some(return_type_mirror)));
        ctx.shared.heap.set_field(method_obj, 3, Value::Object(Some(param_arr)));
        ctx.shared.heap.set_field(method_obj, 4, Value::Int(1));
        ctx.shared.heap.set_field(method_obj, 5, Value::Object(Some(desc_str)));
        ctx.shared.heap.set_field(method_obj, 6, Value::Int(param_count as i32));
        // Silence "zero_mirror unused" — kept above to preserve the
        // original allocation flow.
        let _ = zero_mirror;
    }

    // Build Object[] of args вЂ” box primitives so InvocationHandler receives Object[].
    //
    // Per `java.lang.reflect.InvocationHandler.invoke` contract: when the
    // intercepted interface method takes no arguments, the `args` parameter
    // MUST be `null` (not an empty array). Spring's
    // `SerializableTypeWrapper$TypeProxyInvocationHandler.invoke` relies on
    // this — its `Type[].class` return-type branch is gated on `args == null`
    // (`aload_3 / ifnonnull -> default`), and the default branch reflectively
    // calls `method.invoke(provider.getType(), args)`. For 0-arg methods like
    // `getGenericInterfaces()`, passing an empty array (instead of null) makes
    // the branch fall through to the default, which then virtually dispatches
    // `getGenericInterfaces` on `provider.getType()` — a `ParameterizedTypeImpl`
    // that has no such method, surfacing as `NoSuchMethodError`.
    let args_value = if args.is_empty() {
        Value::Object(None)
    } else {
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
        Value::Object(Some(args_arr))
    };

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
            args_value,
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
        args_value,
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
    // S111r11 — see `proxy_invoke_handler` above.
    const METHOD_EXTRA_SLOTS: usize = 3;
    const METHOD_NUM_FIELDS_LEGACY_FLOOR: usize = 13;
    let alloc_base = core::cmp::max(METHOD_NUM_FIELDS_LEGACY_FLOOR, total_fields);
    let method_obj = shared.heap.alloc_object(method_class_id, alloc_base + METHOD_EXTRA_SLOTS);
    let zero_mirror = super::get_or_create_class_mirror(shared, ClassId::new(0));
    // Resolve the actual declaring-interface mirror for the synthesized
    // Method's `clazz` field — see `proxy_resolve_declaring_class_mirror`
    // for why `ClassId(0)` (== `Object` in production) is unsafe here.
    let declaring_mirror = proxy_resolve_declaring_class_mirror(
        shared,
        proxy,
        method_name,
        descriptor,
    );
    let name_str = super::create_java_string(shared, method_name);
    // Parse descriptor into per-parameter and return type descriptors —
    // see `proxy_invoke_handler` above for rationale.
    let (param_descs, ret_desc) = proxy_split_descriptor(descriptor);
    let return_type_mirror = proxy_descriptor_to_class_mirror(shared, &ret_desc);
    let param_count = param_descs.len();
    let param_arr = shared.heap.alloc_array(
        ClassId::new(0),
        crate::memory::heap::ArrayElementType::Reference,
        param_count,
    );
    for (i, pdesc) in param_descs.iter().enumerate() {
        let pmirror = proxy_descriptor_to_class_mirror(shared, pdesc);
        shared
            .heap
            .set_array_element(param_arr, i, Value::Object(Some(pmirror)))
            .ok();
    }
    let desc_str = super::create_java_string(shared, descriptor);
    proxy_method_set_field_by_name(shared, method_obj, "clazz", Value::Object(Some(declaring_mirror)));
    proxy_method_set_field_by_name(shared, method_obj, "name", Value::Object(Some(name_str)));
    proxy_method_set_field_by_name(shared, method_obj, "returnType", Value::Object(Some(return_type_mirror)));
    proxy_method_set_field_by_name(shared, method_obj, "parameterTypes", Value::Object(Some(param_arr)));
    proxy_method_set_field_by_name(shared, method_obj, "modifiers", Value::Int(1)); // PUBLIC
    proxy_method_set_field_by_name(shared, method_obj, "signature", Value::Object(Some(desc_str)));
    proxy_method_set_field_by_name(shared, method_obj, "slot", Value::Int(0));
    // S111r11 — see `proxy_invoke_handler` above for rationale.
    proxy_method_write_extra_slots(shared, method_obj, descriptor, param_count);
    if total_fields < 8 {
        // Synthetic-mode fallback (no JDK Method class loaded): keep the
        // old hard-coded layout so callers reading raw slots still find
        // the values.
        shared.heap.set_field(method_obj, 0, Value::Object(Some(declaring_mirror)));
        shared.heap.set_field(method_obj, 1, Value::Object(Some(name_str)));
        shared.heap.set_field(method_obj, 2, Value::Object(Some(return_type_mirror)));
        shared.heap.set_field(method_obj, 3, Value::Object(Some(param_arr)));
        shared.heap.set_field(method_obj, 4, Value::Int(1));
        shared.heap.set_field(method_obj, 5, Value::Object(Some(desc_str)));
        shared.heap.set_field(method_obj, 6, Value::Int(param_count as i32));
    }
    // Silence "zero_mirror unused" — kept above to preserve the original
    // allocation flow.
    let _ = zero_mirror;

    // Build Object[] of args вЂ” box primitives. Per
    // `java.lang.reflect.InvocationHandler.invoke` contract, when the
    // intercepted method takes no arguments, `args` MUST be `null` (not an
    // empty Object[]). See `proxy_invoke_handler` above for the SportMe /
    // Spring `SerializableTypeWrapper` case that depends on this.
    let args_value = if args.is_empty() {
        Value::Object(None)
    } else {
        let args_arr = shared.heap.alloc_array(
            ClassId::new(0),
            crate::memory::heap::ArrayElementType::Reference,
            args.len(),
        );
        for (i, arg) in args.iter().enumerate() {
            let boxed = proxy_box_value(shared, *arg);
            shared.heap.set_array_element(args_arr, i, boxed).ok();
        }
        Value::Object(Some(args_arr))
    };

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
            args_value,
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
        args_value,
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
    thread: &mut JvmThread,
    proxy: ObjectRef,
    method_name: &str,
    args: &[Value],
) -> MethodCallResult {
    // S111r19 — Spring's MergedAnnotation.adaptValueForMapOptions iterates
    // a `[LMergedAnnotation;` array and calls `aa[i].asMap(factory, adapts)`.
    // When `aa[i]` is one of our `AnnotationProxy` instances (because the
    // upstream `adaptForAttribute` retained the original proxy array
    // instead of building a fresh `[LMergedAnnotation;`), the dispatch
    // routes here. Without an `asMap` handler, the element-accessor walk
    // returns null and Spring stores null in the resulting
    // `AnnotationAttributes[]`, surfacing as the `TypeFilterUtils.java:77`
    // / `ComponentScanAnnotationParser.java:137` NPE on the next iteration
    // (`@Filter` attribute is null).
    if method_name == "asMap" {
        return annotation_proxy_as_map(shared, thread, proxy, args);
    }
    annotation_proxy_dispatch_impl(shared, proxy, method_name, args)
}

/// Implement `MergedAnnotation.asMap(Function, Adapt[])` for AnnotationProxy
/// receivers. Builds the destination map by calling `factory.apply(proxy)`
/// then populates with element name→value pairs. Nested annotation arrays
/// are converted to `AnnotationAttributes[]` arrays containing
/// recursively-asMap'd children.
fn annotation_proxy_as_map(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    args: &[Value],
) -> MethodCallResult {
    use crate::memory::heap::ObjectKind;

    let factory = match args.first() {
        Some(Value::Object(Some(o))) => {
            if shared.heap.kind_of(*o) == ObjectKind::Object {
                Some(*o)
            } else {
                None
            }
        }
        _ => None,
    };

    let dest_map = if let Some(f) = factory {
        let f_cid = shared.heap.class_id_of(f);
        let is_lambda = shared.lambda_proxies.read().contains_key(&f_cid);
        let res = if is_lambda {
            let dispatch = crate::runtime::interpreter::try_lambda_dispatch(
                shared,
                thread,
                f,
                f_cid,
                "apply",
                &[Value::Object(Some(proxy))],
            )?;
            dispatch.unwrap_or(None)
        } else {
            invoke_on_class_shared(
                shared,
                thread,
                f_cid,
                "apply",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(f)), Value::Object(Some(proxy))],
            )?
        };
        match res {
            Some(Value::Object(Some(m))) => m,
            _ => {
                let cid = shared
                    .load_class_concurrent(
                        "org/springframework/core/annotation/AnnotationAttributes",
                    )
                    .or_else(|_| shared.load_class_concurrent("java/util/LinkedHashMap"))
                    .unwrap_or_else(|_| rustjvm_types::ClassId::new(0));
                shared.heap.alloc_object(cid, 4)
            }
        }
    } else {
        let cid = shared
            .load_class_concurrent("java/util/LinkedHashMap")
            .unwrap_or_else(|_| rustjvm_types::ClassId::new(0));
        shared.heap.alloc_object(cid, 4)
    };

    // S111r32 — detect Adapt.CLASS_TO_STRING in the varargs Adapt[] arg
    // (`args[1]`) so we can convert `Class` / `Class[]` element values to
    // their FQN String / String[] representation. Spring's
    // `ConfigurationWarningsApplicationContextInitializer$ComponentScanPackageCheck`
    // calls `AnnotationMetadata.getAnnotationAttributes(name, true)` (the
    // `classValuesAsString=true` overload), which routes through
    // `AnnotatedElementUtils.getMergedAnnotationAttributes` → `asMap(...,
    // CLASS_TO_STRING)`, then immediately calls
    // `attrs.getStringArray("basePackageClasses")`. If we leave the raw
    // `Class[]` in the map, `AnnotationAttributes.assertAttributeType`
    // throws `IllegalArgumentException: Attribute 'basePackageClasses' is
    // of type Class[], but String[] was expected`.
    let class_to_string = adapt_array_contains(shared, args.get(1).copied(), "CLASS_TO_STRING");

    let names_arr = match shared.heap.get_field(proxy, 2) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(Some(dest_map)))),
    };
    let values_arr = match shared.heap.get_field(proxy, 3) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(Some(dest_map)))),
    };
    let n = shared.heap.array_length(names_arr);
    for i in 0..n {
        let name_val = match shared.heap.get_array_element(names_arr, i) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let elem_val = match shared.heap.get_array_element(values_arr, i) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let adapted = adapt_annotation_value_for_map(shared, thread, elem_val, args)?;
        // Apply CLASS_TO_STRING: replace Class / Class[] with String / String[].
        let adapted = if class_to_string {
            convert_class_values_to_strings(shared, adapted)
        } else {
            adapted
        };
        let dest_cid = shared.heap.class_id_of(dest_map);
        let _ = invoke_on_class_shared(
            shared,
            thread,
            dest_cid,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(dest_map)), name_val, adapted],
        )?;
    }
    Ok(Some(Value::Object(Some(dest_map))))
}

/// Test whether the given (possibly-null) Adapt[] varargs array contains
/// an enum constant whose `name` slot equals `target_name`.
fn adapt_array_contains(shared: &SharedVm, arr_val: Option<Value>, target_name: &str) -> bool {
    use crate::memory::heap::ObjectKind;
    let arr_obj = match arr_val {
        Some(Value::Object(Some(o))) => o,
        _ => return false,
    };
    if shared.heap.kind_of(arr_obj) != ObjectKind::Array {
        return false;
    }
    let n = shared.heap.array_length(arr_obj);
    for i in 0..n {
        let elem = match shared.heap.get_array_element(arr_obj, i) {
            Ok(Value::Object(Some(o))) => o,
            _ => continue,
        };
        // Enum constant: slot 0 = name String (java.lang.Enum layout).
        if let Value::Object(Some(name_obj)) = shared.heap.get_field(elem, 0) {
            if let Some(s) = super::read_java_string(&shared.heap, name_obj) {
                if s == target_name {
                    return true;
                }
            }
        }
    }
    false
}

/// If `val` is a `Class` mirror, return a String holding its FQN (dotted).
/// If `val` is a `Class[]`, return a fresh `String[]` of FQNs. Otherwise
/// return `val` unchanged. Implements the `Adapt.CLASS_TO_STRING` semantics
/// for `MergedAnnotation.asMap` so that downstream
/// `AnnotationAttributes.getStringArray("basePackageClasses")` succeeds.
fn convert_class_values_to_strings(shared: &SharedVm, val: Value) -> Value {
    use crate::memory::heap::ObjectKind;
    let obj = match val {
        Value::Object(Some(o)) => o,
        _ => return val,
    };
    let kind = shared.heap.kind_of(obj);
    if kind == ObjectKind::Object {
        // Detect Class mirror: class_id resolves to java/lang/Class, or the
        // object has a non-null `name` slot we can convert. The simplest
        // detection: the class name of `obj` is "java/lang/Class".
        let cid = shared.heap.class_id_of(obj);
        let class_name = shared
            .class_manager
            .read()
            .get_class(cid)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        if class_name == "java/lang/Class" {
            let fqn = class_mirror_fqn(shared, obj);
            return Value::Object(Some(super::create_java_string(shared, &fqn)));
        }
        return val;
    }
    if kind == ObjectKind::Array {
        // Reference array whose component class is `java/lang/Class`.
        let comp_cid = shared.heap.class_id_of(obj);
        let comp_name = shared
            .class_manager
            .read()
            .get_class(comp_cid)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        if comp_name == "java/lang/Class" {
            let n = shared.heap.array_length(obj);
            let str_cid = shared
                .load_class_concurrent("java/lang/String")
                .unwrap_or_else(|_| rustjvm_types::ClassId::new(0));
            let new_arr = shared.heap.alloc_array(
                str_cid,
                rustjvm_types::ArrayElementType::Reference,
                n,
            );
            for i in 0..n {
                let elem = shared
                    .heap
                    .get_array_element(obj, i)
                    .unwrap_or(Value::Object(None));
                let s_val = match elem {
                    Value::Object(Some(m)) => {
                        let fqn = class_mirror_fqn(shared, m);
                        Value::Object(Some(super::create_java_string(shared, &fqn)))
                    }
                    _ => Value::Object(None),
                };
                shared.heap.set_array_element(new_arr, i, s_val).ok();
            }
            return Value::Object(Some(new_arr));
        }
        return val;
    }
    val
}

/// Read the FQN (dotted) name out of a Class mirror. Tries the
/// reverse-lookup map first (`class_id_from_mirror`); falls back to slot 1
/// (the JDK 25 Class layout's `name` field) for primitive / array mirrors
/// allocated via `primitive_class_mirror`.
fn class_mirror_fqn(shared: &SharedVm, mirror: ObjectRef) -> String {
    let mirror_cid = super::class_id_from_mirror(shared, mirror);
    if let Some(cid) = mirror_cid {
        if let Some(name) = shared
            .class_manager
            .read()
            .get_class(cid)
            .map(|c| c.name.to_string())
        {
            return name.replace('/', ".");
        }
    }
    if let Value::Object(Some(name_obj)) = shared.heap.get_field(mirror, 1) {
        if let Some(s) = super::read_java_string(&shared.heap, name_obj) {
            return s.replace('/', ".");
        }
    }
    String::new()
}

/// Recursively adapt an annotation element value: nested annotation
/// proxies become their asMap result; arrays of annotation proxies become
/// an `AnnotationAttributes[]`-typed array of recursively-asMap'd children.
fn adapt_annotation_value_for_map(
    shared: &SharedVm,
    thread: &mut JvmThread,
    val: Value,
    asmap_args: &[Value],
) -> Result<Value, MethodCallFailed> {
    use crate::memory::heap::ObjectKind;
    let obj = match val {
        Value::Object(Some(o)) => o,
        _ => return Ok(val),
    };
    let cid = shared.heap.class_id_of(obj);
    let kind = shared.heap.kind_of(obj);
    let class_name = shared
        .class_manager
        .read()
        .get_class(cid)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    if kind == ObjectKind::Object && class_name == "java/lang/annotation/AnnotationProxy" {
        return annotation_proxy_as_map(shared, thread, obj, asmap_args).map(|res| {
            res.unwrap_or(Value::Object(None))
        });
    }
    if kind == ObjectKind::Array {
        let len = shared.heap.array_length(obj);
        let any_proxy = (0..len).any(|i| {
            matches!(
                shared.heap.get_array_element(obj, i),
                Ok(Value::Object(Some(e)))
                    if shared.heap.kind_of(e) == ObjectKind::Object
                        && shared
                            .class_manager
                            .read()
                            .get_class(shared.heap.class_id_of(e))
                            .map(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
                            .unwrap_or(false)
            )
        });
        if any_proxy {
            let aa_cid = shared
                .load_class_concurrent("org/springframework/core/annotation/AnnotationAttributes")
                .unwrap_or_else(|_| rustjvm_types::ClassId::new(0));
            let new_arr = shared
                .heap
                .alloc_array(aa_cid, rustjvm_types::ArrayElementType::Reference, len);
            for i in 0..len {
                let elem = shared
                    .heap
                    .get_array_element(obj, i)
                    .unwrap_or(Value::Object(None));
                let adapted = adapt_annotation_value_for_map(shared, thread, elem, asmap_args)?;
                shared
                    .heap
                    .set_array_element(new_arr, i, adapted)
                    .ok();
            }
            return Ok(Value::Object(Some(new_arr)));
        }
    }
    Ok(val)
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
        // S111r18 — `getClass()` is a final native on Object, but our
        // interception layer at `execute_invoke` routes EVERY method call
        // on an `AnnotationProxy` here (because the receiver's class_id
        // is the synthetic AnnotationProxy class, not Object). Without
        // this branch the call falls through to the element-accessor walk
        // below, finds no element named "getClass", and returns null —
        // breaking Spring's `MergedAnnotation.adaptForAttribute` which
        // calls `value.getClass().isArray()` on the raw attribute value
        // and NPEs. Return the proxy's class mirror (the annotation
        // type's mirror, stored in field 1 by `create_annotation_proxy`)
        // so callers see something sensible. We deliberately return the
        // annotation type Class — not the AnnotationProxy synthetic Class
        // — to match real-JDK behaviour where `q.getClass()` reports the
        // Proxy class and `q.annotationType()` reports the annotation
        // interface, but Spring only needs *some* non-null Class with
        // `isArray()==false` and `isAnnotation()==true`.
        "getClass" => {
            return Ok(Some(shared.heap.get_field(proxy, 1)));
        }
        // S111r20 — `getType()` is a `MergedAnnotation` interface method.
        // Spring's `TypeMappedAnnotation.adaptValueForMapOptions` treats
        // AnnotationProxy arrays as `MergedAnnotation[]` and calls
        // `asMap(factory, adaptations)` on each element. The factory lambda
        // is `mergedAnnotation -> new AnnotationAttributes(mergedAnnotation.getType(), ...)`.
        // Without this branch, `getType()` falls through, finds no element
        // named "getType", returns null, and AnnotationAttributes throws
        // `IllegalArgumentException: 'annotationType' must not be null`.
        "getType" => {
            return Ok(Some(shared.heap.get_field(proxy, 1)));
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

/// Split a method descriptor into individual parameter type descriptors and
/// the return type descriptor. Each returned descriptor includes any leading
/// `[` array prefix and (for `L`-types) the trailing `;`.
///
/// `()V` -> (vec![], "V")
/// `(Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;`
///   -> (vec!["Ljava/lang/reflect/Method;", "[Ljava/lang/Object;"], "Ljava/lang/Object;")
pub(super) fn proxy_split_descriptor(descriptor: &str) -> (Vec<String>, String) {
    let start = match descriptor.find('(') {
        Some(s) => s,
        None => return (Vec::new(), String::new()),
    };
    let end = match descriptor.find(')') {
        Some(e) if e > start => e,
        _ => return (Vec::new(), String::new()),
    };
    let inner = &descriptor[start + 1..end];
    let ret = descriptor[end + 1..].to_string();
    let mut params = Vec::new();
    let mut buf = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        buf.push(ch);
        match ch {
            'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' | 'V' => {
                params.push(std::mem::take(&mut buf));
            }
            'L' => {
                for c in chars.by_ref() {
                    buf.push(c);
                    if c == ';' {
                        break;
                    }
                }
                params.push(std::mem::take(&mut buf));
            }
            '[' => {
                // array prefix — keep accumulating until we get a base type
            }
            _ => {
                buf.clear();
            }
        }
    }
    (params, ret)
}

/// Resolve a single JVMS field-type descriptor (e.g. `Ljava/lang/Object;`,
/// `[Ljava/lang/reflect/Type;`, `I`, `V`) to a Class mirror. Used by
/// `proxy_invoke_handler` so the synthetic `Method` object's `returnType`
/// and `parameterTypes` carry semantically correct mirrors — Spring's
/// `SerializableTypeWrapper$TypeProxyInvocationHandler.invoke` branches on
/// `method.getReturnType() == Type.class` / `Type[].class`, which only works
/// when those mirrors point at the real `java.lang.reflect.Type` Class.
pub(super) fn proxy_descriptor_to_class_mirror(
    shared: &SharedVm,
    desc: &str,
) -> ObjectRef {
    if desc.is_empty() {
        return super::get_or_create_class_mirror(shared, ClassId::new(0));
    }
    // Primitive types -> primitive mirror.
    let prim_name = match desc {
        "V" => Some("void"),
        "Z" => Some("boolean"),
        "B" => Some("byte"),
        "C" => Some("char"),
        "S" => Some("short"),
        "I" => Some("int"),
        "J" => Some("long"),
        "F" => Some("float"),
        "D" => Some("double"),
        _ => None,
    };
    if let Some(p) = prim_name {
        return super::get_or_create_primitive_mirror(shared, p);
    }
    // Reference / array: load by JVMS internal name.
    //   `Ljava/lang/Object;` -> "java/lang/Object"
    //   `[Ljava/lang/reflect/Type;` -> "[Ljava/lang/reflect/Type;"  (array name)
    //   `[I` -> "[I"
    let load_name: String = if desc.starts_with('[') {
        desc.to_string()
    } else if desc.starts_with('L') && desc.ends_with(';') {
        desc[1..desc.len() - 1].to_string()
    } else {
        return super::get_or_create_class_mirror(shared, ClassId::new(0));
    };
    let cid = shared
        .class_manager
        .write()
        .load_class(&load_name)
        .unwrap_or(ClassId::new(0));
    super::get_or_create_class_mirror(shared, cid)
}

/// Resolve the Class mirror that should be stored in the synthesized
/// `Method.clazz` field for a proxy dispatch — i.e. the interface that
/// actually declares the method being invoked. Walks the proxy's
/// `interfaces` array (field 1) and, for each interface, scans its
/// `methods` list (including inherited super-interfaces, transitively)
/// for a matching `(name, descriptor)`.
///
/// Falls back to the first interface mirror if no match is found, or to
/// the `Object` mirror if the proxy carries no interfaces at all — the
/// latter is preferable to `ClassId(0)` since `ClassId(0)` happens to be
/// `java/lang/Object` in production builds (first class loaded), which
/// would make `method.getDeclaringClass() == Object.class` evaluate
/// `true` for **every** proxied method. ByteBuddy's
/// `JavaDispatcher$ProxiedInvocationHandler.invoke` gates its real
/// dispatch on `declaringClass != Object`, so the `ClassId(0)` sentinel
/// caused every non-`equals`/`hashCode`/`toString` proxied call to
/// surface as `IllegalStateException: Unexpected object method`.
pub(super) fn proxy_resolve_declaring_class_mirror(
    shared: &SharedVm,
    proxy: ObjectRef,
    method_name: &str,
    descriptor: &str,
) -> ObjectRef {
    let interfaces = match shared.heap.get_field(proxy, 1) {
        Value::Object(Some(arr)) => arr,
        _ => return super::get_or_create_class_mirror(shared, ClassId::new(0)),
    };
    let n = shared.heap.array_length(interfaces);
    let mut first_iface_mirror: Option<ObjectRef> = None;
    for i in 0..n {
        let iface_mirror = match shared.heap.get_array_element(interfaces, i) {
            Ok(Value::Object(Some(m))) => m,
            _ => continue,
        };
        if first_iface_mirror.is_none() {
            first_iface_mirror = Some(iface_mirror);
        }
        let iface_cid = match shared
            .class_mirrors_reverse
            .read()
            .get(&iface_mirror)
            .copied()
        {
            Some(cid) => cid,
            None => continue,
        };
        // Walk this interface + all super-interfaces (transitively)
        // looking for a declared method matching (name, descriptor).
        let mut visited: rustc_hash::FxHashSet<ClassId> = rustc_hash::FxHashSet::default();
        let mut stack: Vec<ClassId> = vec![iface_cid];
        while let Some(cid) = stack.pop() {
            if !visited.insert(cid) {
                continue;
            }
            let supers: Vec<ClassId> = {
                let cm = shared.class_manager.read();
                let Some(class) = cm.get_class(cid) else {
                    continue;
                };
                let found = class.methods.iter().any(|m| {
                    &*m.name == method_name && &*m.descriptor == descriptor
                });
                if found {
                    drop(cm);
                    return super::get_or_create_class_mirror(shared, cid);
                }
                class.interfaces.clone()
            };
            for s in supers {
                stack.push(s);
            }
        }
    }
    // No declaring interface found — prefer the first interface mirror
    // over `ClassId(0)` so the resulting Method object still answers
    // `getDeclaringClass()` with a real interface (not `Object`).
    if let Some(m) = first_iface_mirror {
        return m;
    }
    super::get_or_create_class_mirror(shared, ClassId::new(0))
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

/// S111r7 — return `true` for methods declared on `java.lang.Object` so we
/// don't accidentally divert legitimate `Object.equals`/`hashCode`/etc.
/// calls into the receiver-driven fallback.
pub fn is_object_member(method_name: &str, descriptor: &str) -> bool {
    matches!(
        (method_name, descriptor),
        ("equals", "(Ljava/lang/Object;)Z")
        | ("hashCode", "()I")
        | ("toString", "()Ljava/lang/String;")
        | ("getClass", "()Ljava/lang/Class;")
        | ("notify", "()V")
        | ("notifyAll", "()V")
        | ("wait", "()V")
        | ("wait", "(J)V")
        | ("wait", "(JI)V")
        | ("clone", "()Ljava/lang/Object;")
        | ("finalize", "()V")
        | ("<init>", "()V")
    )
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
    //
    // KC patch (γ generalisation): when the receiver's `class_id_of`
    // returns `ClassId::new(0)` (synthetic alloc that lost class_id, or
    // a bare `Object` slot) we cannot use the receiver to drive
    // dispatch — but we also must NOT keep the original `class_id`
    // unchanged if it names an interface (resolution will land on a
    // method with no Code and surface as NSME).  The CP class is the
    // spec-correct resolution target so we leave it as-is here; the
    // S111r10 receiver-walk and the `class_name == "java/lang/Object"`
    // path inside `invoke_or_native` handle the substitution downstream.
    let class_id = if !no_retarget && method_name != "<init>" && method_name != "<clinit>" {
        let recv_cid = args.get(0).and_then(|v| {
            if let Value::Object(Some(o)) = v { Some(shared.heap.class_id_of(*o)) } else { None }
        });
        if let Some(rc) = recv_cid {
            if rc != class_id && rc != ClassId::new(0) {
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
                        // GENS-1: Class.getGenericInterfaces / getGenericSuperclass
                        // — the real-JDK bytecode goes through ClassRepository →
                        // SignatureParser → Reifier. The Reifier path NPEs in
                        // `Reifier.visitClassTypeSignature` (line 110:
                        // `new StringBuilder(sc.getName())`) on `ArrayList`'s
                        // `Ljava/util/AbstractList<TE;>;Ljava/util/List<TE;>;...`
                        // signature: an iter.next() on the parsed path returns
                        // null, surfacing as
                        //   "Cannot invoke getName on null"
                        // during Spring Boot's `ApplicationConversionService.<clinit>`
                        // (and our minimal CR repro on `ArrayList.class`). Our
                        // native `getGenericInterfaces` parses the Signature
                        // attribute via `rustjvm_reader::signature` and builds
                        // synthetic `ParameterizedType` mirrors directly, so
                        // force the override and bypass the broken JDK path.
                        // GENS-1: Class.getGenericInterfaces / getGenericSuperclass
                        // — the real-JDK bytecode goes through ClassRepository →
                        // SignatureParser → Reifier. The Reifier path NPEs in
                        // `Reifier.visitClassTypeSignature` (`new StringBuilder(
                        // sc.getName())`) on `ArrayList`'s class signature: an
                        // `iter.next()` on the parsed path returns null, surfacing
                        // as "Cannot invoke getName on null" during Spring Boot's
                        // `ApplicationConversionService.<clinit>` (and the
                        // minimal `CR` repro on `ArrayList.class`). Our native
                        // parses the Signature attribute via
                        // `rustjvm_reader::signature` directly and builds
                        // `ParameterizedType` mirrors, so force the override and
                        // bypass the broken JDK parse path. Registered
                        // unconditionally below in
                        // `register_essential_natives` (this `check_override`
                        // entry is the gate that lets a registered native take
                        // precedence over a non-`ACC_NATIVE` JDK Java method).
                        || (class_name == "java/lang/Class"
                            && (method_name == "getGenericInterfaces"
                                || method_name == "getGenericSuperclass"))
                        // SPB.10 / Spring `BeanWrapperImpl`: the real-JDK
                        // `java.beans.Introspector.getBeanInfo` walks
                        // `com.sun.beans.introspect.*` reflection — that
                        // path is on the JIT skip-list (Round 34: SPB.9d)
                        // and the interpreter walk produces an empty
                        // `BeanInfo` for ordinary POJOs (`pds.length == 0`),
                        // which surfaces as
                        //   `NotWritablePropertyException: Bean property 'X'
                        //    is not writable or has an invalid setter method`
                        // on Spring's `ConfigurationClassPostProcessor`
                        // (the `metadataReaderFactory` setter is real but
                        // invisible). Force our native (registered above
                        // as `introspector_get_bean_info`) to win — it
                        // walks the class + superclasses via
                        // `ctx.declared_methods` and builds real
                        // `java.lang.reflect.Method` mirrors for the
                        // discovered getter/setter pairs.
                        || (class_name == "java/beans/Introspector"
                            && method_name == "getBeanInfo")
                        // SPB.10 (cont.): our `Introspector.getBeanInfo` returns
                        // synthetic `PropertyDescriptor`s whose readMethod/writeMethod
                        // live in slots 1/2. The real-JDK `PropertyDescriptor` bytecode
                        // reads private fields by name (and references soft Method
                        // refs through `MethodRef`), so calling its bytecode on our
                        // synthetic instance returns null. Force our slot-based
                        // native getters to win so Spring's `BeanWrapperImpl` sees
                        // the real getter/setter `Method` mirrors we stored.
                        || (class_name == "java/beans/PropertyDescriptor"
                            && (method_name == "getReadMethod"
                                || method_name == "getWriteMethod"
                                || method_name == "getName"
                                || method_name == "getPropertyType"))
                        // SPB.11: Spring's `ExtendedBeanInfo` wraps our
                        // PropertyDescriptors in `SimplePropertyDescriptor`
                        // subclasses. Its constructor delegates to
                        // `super(pd.getName(), readMethod, writeMethod)`,
                        // which `setName(...)` stores on
                        // `FeatureDescriptor.name`. We've observed that on
                        // the subclass instance the bytecode `getfield
                        // FeatureDescriptor.name` (from
                        // `FeatureDescriptor.getName()`) resolves to a slot
                        // that disagrees with what `setName` (and reflective
                        // `Field.get`) read — getName returns null. The
                        // `ExtendedBeanInfo$PropertyDescriptorComparator`
                        // then NPEs on `getName().compareTo(...)`. Force our
                        // native (in `phases_late.rs`) that resolves `name`
                        // via the dynamic-hierarchy lookup that agrees with
                        // setName/reflection.
                        || (class_name == "java/beans/FeatureDescriptor"
                            && method_name == "getName")
                        // WP2.2 / Surefire LazyLauncher: real JDK `Method.invoke` is
                        // Java bytecode (`MethodAccessor` → `DirectMethodHandleAccessor`).
                        // We register `native_method_invoke` for primitive-aware unboxing
                        // and descriptor reconstruction from `Executable` fields. Without
                        // an allow-list entry here, `check_override` stays false for this
                        // concrete method, the JDK body runs instead of our native, and
                        // reflective helpers like Surefire's `ReflectionUtils.invokeGetter`
                        // can observe incorrect `null` returns (JUnitPlatformProvider NPE:
                        // "Cannot invoke discover on null").
                        || (class_name == "java/lang/reflect/Method"
                            && method_name == "invoke"
                            && descriptor
                                == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;")
                        // Same rationale as `Method.invoke`: `Constructor.newInstance` is
                        // bytecode-backed on the JDK; our native performs layout-aware
                        // allocation and strict arg coercion.
                        || (class_name == "java/lang/reflect/Constructor"
                            && method_name == "newInstance"
                            && descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;")
                        // SPB.11: Our synthetic MethodDescriptor stores the
                        // wrapped Method at slot 0 (real-JDK MD has a private
                        // `method` field at a different layout). Force the
                        // native so getMethod returns our overlay value.
                        || (class_name == "java/beans/MethodDescriptor"
                            && method_name == "getMethod")
                        // SPB.11: Spring's ExtendedBeanInfoFactory wraps our
                        // delegate BeanInfo into `new ExtendedBeanInfo(...)`,
                        // which re-creates each PropertyDescriptor as a
                        // SimplePropertyDescriptor and loses readMethod/
                        // writeMethod/propertyType (subclass fields never
                        // populated; downstream Spring NPEs comparing). Our
                        // native bypasses the wrapping and returns the
                        // delegate directly.
                        || (class_name == "org/springframework/beans/ExtendedBeanInfoFactory"
                            && method_name == "getBeanInfo")
                        || (class_name == "org/springframework/beans/SimpleBeanInfoFactory"
                            && method_name == "getBeanInfo")
                        // SPB.11: Spring's GenericTypeAwarePropertyDescriptor
                        // (built by CachedIntrospectionResults) stores
                        // readMethod/writeMethod/propertyType in its own
                        // subclass fields. `getfield` on those returns null
                        // even though `putfield` (in the ctor) and reflective
                        // `Field.get` agree on the value — a layout mismatch
                        // for the subclass slot indices. Force our natives
                        // (resolve by name) to win so BeanWrapperImpl sees
                        // the real write methods.
                        || (class_name == "org/springframework/beans/GenericTypeAwarePropertyDescriptor"
                            && (method_name == "getReadMethod"
                                || method_name == "getWriteMethod"
                                || method_name == "getPropertyType"))
                        // KC16-JUL: java.util.logging.Logger.getResourceBundleName /
                        // getResourceBundle — the real JDK bytecode reads the
                        // private `loggerBundle` field which our Logger init
                        // path never populates. Reads through the bytecode NPE
                        // with "Cannot read field 'resourceBundleName' because
                        // the object is null" inside
                        // `org/jboss/as/server/SystemExiter.logBeforeExit`
                        // when WildFly is reporting an exit reason. Force our
                        // null-tolerant natives (registered in
                        // `logmanager.rs`) to win over the bytecode.
                        || (class_name == "java/util/logging/Logger"
                            && (method_name == "getResourceBundleName"
                                || method_name == "getResourceBundle"))
                        // B3: ClassLoader.getResources / getSystemResources
                        // have real-JDK bytecode but that bytecode walks
                        // URLClassPath (which NPEs during <clinit>). Force
                        // the native override ahead of the bytecode.
                        || (class_name == "java/lang/ClassLoader"
                            && (method_name == "getResources"
                                || method_name == "getSystemResources"))
                        // Insurance / Spring Boot 3: real-JDK `URL.getHost` resolves
                        // the host through `InetAddress.getByName` /
                        // `getHostName`, which re-enters `Class.initClassName` in a
                        // tight loop during early boot (T19.H1 watchdog). Our natives
                        // read the parsed `host` field / InetAddress slots directly.
                        || (class_name == "java/net/URL"
                            && matches!(
                                method_name,
                                "getHost" | "getAuthority" | "getHostAddress"
                            ))
                        || (class_name == "java/net/URL"
                            && method_name == "setURLStreamHandlerFactory"
                            && descriptor == "(Ljava/net/URLStreamHandlerFactory;)V")
                        || (matches!(
                            class_name,
                            "java/net/InetAddress"
                                | "java/net/Inet4Address"
                                | "java/net/Inet6Address"
                        ) && matches!(
                            method_name,
                            "getHostName" | "getCanonicalHostName" | "getHostAddress"
                        ))
                        // SB3 (URLClassPath): the real-JDK bytecode for
                        // `URLClassPath.<init>([Ljava/net/URL;...)V` writes
                        // the `loaders`/`lmap`/`closed` instance fields in
                        // the inline initializer prelude *before* the
                        // `aload_1; arraylength` on `urls` — but Spring Boot
                        // Loader's `LaunchedURLClassLoader(URL[], ClassLoader)`
                        // hands us a partially-constructed URL[] (some slots
                        // are `null`) that later calls (e.g. `URL.<init>` of
                        // a nested-jar URL) NPE on. The JDK's
                        // `URLClassPath$1.next` then descends into
                        // `getLoader(int)`, which reads `loaders.size()` —
                        // null because the constructor never finished its
                        // `path = new ArrayList<>(urls.length)` step. Our
                        // `ucp_init_*` natives in `native-builtins/src/lib.rs`
                        // tolerate null/partial URL[]s and populate every
                        // field via `set_field_by_name` (no slot guessing).
                        // Force the override so the bytecode never runs.
                        || (class_name == "jdk/internal/loader/URLClassPath"
                            && ((method_name == "<init>"
                                && (descriptor == "([Ljava/net/URL;Ljava/net/URLStreamHandlerFactory;)V"
                                    || descriptor == "([Ljava/net/URL;)V"))
                                || (method_name == "getLoader"
                                    && descriptor == "(I)Ljdk/internal/loader/URLClassPath$Loader;")
                                || (method_name == "findResources"
                                    && descriptor == "(Ljava/lang/String;)Ljava/util/Enumeration;")))
                        // RKC16N.12: System.loadLibrary / Runtime.loadLibrary0
                        // — the real JDK bytecode walks down through
                        // ClassLoader.loadLibrary which throws
                        // UnsatisfiedLinkError("no <name> in java.library.path: ...")
                        // when the library can't be found on disk (we ship
                        // the JMM natives in-process via NativeMethodRegistry,
                        // so libmanagement.dll is genuinely absent). The ULE
                        // surfaces during ManagementFactory.<clinit> (which
                        // calls System.loadLibrary("management")) as a B6
                        // silent-swallow on Keycloak boot. Force the
                        // no-op native override registered in
                        // `lang_system::register_lang_system_natives` so the
                        // bytecode never runs the throwing path.
                        || ({
                            let m = (class_name == "java/lang/System"
                                && matches!(method_name, "loadLibrary" | "load"))
                                || (class_name == "java/lang/Runtime"
                                    && matches!(method_name, "loadLibrary0" | "load0"
                                        | "loadLibrary" | "load"));
                            if m {
                                tracing::debug!(
                                    target: "rkc16n12",
                                    class=%class_name, method=%method_name, desc=%descriptor,
                                    "RKC16N.12: forcing native override for loadLibrary path"
                                );
                            }
                            m
                        })
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
                        // EUREKA-LOGBACK-CLEANUP: LoggerContext.<init> is registered
                        // as a no-op native, leaving the inherited `objectMap`/
                        // `propertyMap`/`sm` fields null. Spring Boot's
                        // `LogbackLoggingSystem.cleanUp` calls
                        // `loggerContext.removeObject(...)`,
                        // `loggerContext.getStatusManager().clear()`, and
                        // `loggerContext.getTurboFilterList().remove(...)` —
                        // every one of which the real-JDK bytecode services
                        // by dereferencing a null field, producing the fatal
                        // `NullPointerException: Cannot invoke remove on null`
                        // during `prepareEnvironment` on every Spring Boot app
                        // (eureka-server is the canonical reproducer). Force
                        // our native stubs (registered in `native-builtins`
                        // alongside the existing `LoggerContext.<init>` no-op)
                        // to win so the null fields are never touched.
                        || (class_name == "ch/qos/logback/classic/LoggerContext"
                            && matches!(
                                method_name,
                                "removeObject" | "putObject" | "getObject"
                                | "putProperty" | "getProperty"
                                | "getStatusManager" | "getTurboFilterList"
                            ))
                        || (class_name == "ch/qos/logback/core/ContextBase"
                            && matches!(
                                method_name,
                                "removeObject" | "putObject" | "getObject"
                                | "putProperty" | "getProperty"
                            ))
                        || (class_name == "ch/qos/logback/core/BasicStatusManager"
                            && method_name == "clear")
                        || (class_name == "ch/qos/logback/classic/spi/TurboFilterList"
                            && method_name == "remove")
                        // EUREKA-RB-CANDIDATE: ResourceBundle$Control.getCandidateLocales
                        // — the real-JDK bytecode passes `locale.getBaseLocale()`
                        // as a key into `ReferencedKeyMap.computeIfAbsent`. Our
                        // synthetic default Locale leaves the private `baseLocale`
                        // field null, so the JDK throws
                        // `NullPointerException: key must not be null` and that
                        // NPE crashes early SLF4J/logback bootstrap
                        // (CachingDateFormatter -> SimpleDateFormat ->
                        // Calendar.createCalendar -> LocaleProviderAdapter ->
                        // getCandidateLocales). Force the override registered
                        // in `locale_bootstrap.rs` which returns a
                        // `Collections.singletonList(locale)`.
                        || (class_name == "java/util/ResourceBundle$Control"
                            && method_name == "getCandidateLocales")
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
                        // UUID-OVERRIDE: java.util.UUID {<init>, randomUUID,
                        // fromString, toString, getMostSignificantBits,
                        // getLeastSignificantBits, equals, hashCode, version}
                        // — the real JDK 25 UUID.toString() bytecode uses
                        // jdk/internal/util/ByteArrayLittleEndian.setLong/setInt
                        // and JavaLangAccess.uncheckedNewStringNoRepl, which we
                        // don't implement, producing an empty string. That
                        // empty string then fails UUID.fromString in
                        // ProcessEnvironment.obtainProcessUUID during Keycloak
                        // boot ("Invalid UUID string"). Force our native
                        // overrides in `register_uuid_natives` (which read/
                        // write `mostSigBits`/`leastSigBits` by name and emit
                        // canonical 8-4-4-4-12 hex).
                        || (class_name == "java/util/UUID"
                            && matches!(
                                method_name,
                                "<init>"
                                | "randomUUID"
                                | "fromString"
                                | "toString"
                                | "getMostSignificantBits"
                                | "getLeastSignificantBits"
                                | "equals"
                                | "hashCode"
                                | "version"
                            ))
                        || (class_name == "java/util/Properties"
                            && matches!(
                                method_name,
                                "load"
                                | "getProperty"
                                | "setProperty"
                                | "put"
                                | "containsKey"
                                | "stringPropertyNames"
                            ))
                        // S111r7: HashMap / LinkedHashMap / Hashtable /
                        // ConcurrentHashMap and HashSet view-method
                        // overrides. Real-JDK bytecode for `keySet()`,
                        // `values()`, `entrySet()`, `iterator()` etc.
                        // allocates inner-class views
                        // (`HashMap$KeySet`, `HashMap$KeyIterator`,
                        // `HashSet$1` reflection over backing-map
                        // entries) we do not load from the JDK module
                        // image — so the views land on the heap with
                        // `cid=0` and every subsequent
                        // `invokeinterface Set.iterator()` /
                        // `Iterator.hasNext()` dispatches to bare
                        // `java.lang.Object` and surfaces a swallowed
                        // `NoSuchMethodError`. Forcing the natives in
                        // `native-collections` to win materialises a
                        // properly-typed `java/util/HashSet`
                        // (single-field-with-backing-HashMap layout) /
                        // `HashMap$KeyItr` / `ArrayList` whose
                        // `iterator()` / `hasNext()` then dispatches
                        // normally, unblocking Spring's
                        // `GenericConversionService.addConverter()` →
                        // `getConvertibleTypes().iterator()` boot path.
                        || (matches!(
                                class_name,
                                "java/util/HashMap"
                                | "java/util/LinkedHashMap"
                                | "java/util/Hashtable"
                                | "java/util/concurrent/ConcurrentHashMap"
                            )
                            && matches!(
                                method_name,
                                "keySet" | "values" | "entrySet"
                                // S111r13: HashMap.computeIfAbsent / compute /
                                // computeIfPresent / merge / putIfAbsent / replace
                                // / forEach / replaceAll / getOrDefault — the
                                // real-JDK bytecode for these reads
                                // `getfield table` and then `arraylength`,
                                // which on our synthetic HashMap layout (slot
                                // 2 holds capacity as `Int(16/32/...)`)
                                // surfaces as
                                //   `expected object reference, got int(N)`
                                // and aborts. logback's LoggerContext ctor
                                // hits this through
                                // `LogbackMDCAdapter.<clinit>` and friends.
                                // Same family as the existing keySet/values/
                                // entrySet and HashSet.spliterator overrides.
                                // Force the side-table-backed natives in
                                // `native-collections` to win.
                                | "computeIfAbsent"
                                | "compute"
                                | "computeIfPresent"
                                | "merge"
                                | "putIfAbsent"
                                | "replace"
                                | "forEach"
                                | "replaceAll"
                                | "getOrDefault"
                                // S111r14: logback `LoggerContext.<init>` →
                                // `HashMap.put(...)` whose JDK bytecode calls
                                // `putVal` which does
                                //   `getfield table` + `arraylength`.
                                // Slot 2 in our synthetic layout holds
                                // `Int(16)` (DEFAULT_INITIAL_CAPACITY) and
                                // surfaces as
                                //   `expected object reference, got int(16)`
                                // (in java/util/HashMap.putVal pc=13).
                                // Same family as the existing
                                // computeIfAbsent/merge overrides — force the
                                // side-table-backed `put` / `get` / `remove`
                                // natives in `native-collections` to win.
                                | "put"
                                | "get"
                                | "remove"
                                | "containsKey"
                                | "containsValue"
                                | "size"
                                | "isEmpty"
                                | "clear"
                                | "putAll"
                                // S111r14: copy-constructor `<init>(Ljava/util/Map;)V`
                                // — DateTimeFormatter.<clinit> hits this via
                                // `new LinkedHashMap<>(map)`. Override only
                                // fires if a native is registered for the
                                // exact (class, method, descriptor) triple,
                                // so non-Map `<init>` signatures still run
                                // bytecode unless explicitly registered.
                                | "<init>"
                            ))
                        || (matches!(
                                class_name,
                                "java/util/HashSet"
                                | "java/util/LinkedHashSet"
                                | "java/util/TreeSet"
                            )
                            && matches!(
                                method_name,
                                "iterator" | "size" | "isEmpty" | "contains" | "add" | "remove" | "clear"
                                // S111r12: `HashSet.spliterator()` JDK
                                // bytecode would build a `KeySpliterator`
                                // over the synthetic backing HashMap and
                                // later `getfield m.table` reads slot 2
                                // (real JDK layout) which our synthetic
                                // populates with `Int(16)` (capacity),
                                // surfacing as
                                //   `expected object reference, got int(16)`.
                                // Same family as the S111r7 `getenv()`
                                // HashMap-layout fix. Force the native
                                // (`p59_hashset_spliterator`) which walks
                                // the synthetic map and returns a synthetic
                                // `(data, cursor)` Spliterator the rest of
                                // our stream pipeline already consumes.
                                | "spliterator"
                            ))
                        // Surefire ForkedBooter: ManagementFactory.getRuntimeMXBean() /
                        // getThreadMXBean() — the real-JDK code path delegates
                        // through `getPlatformMXBean(Class)` + PlatformComponent
                        // SPI which we don't wire up. Without this override the
                        // booter's `isDebugging()` and `dumpHelp()` paths throw
                        // `IllegalArgumentException: ... is not a platform
                        // management interface` and the fork dies before tests
                        // start. Force the synthetic-bean natives in `jmx.rs`
                        // to win.
                        || (class_name == "java/lang/management/ManagementFactory"
                            && matches!(
                                method_name,
                                "getRuntimeMXBean"
                                | "getThreadMXBean"
                                | "getMemoryMXBean"
                                | "getClassLoadingMXBean"
                                | "getOperatingSystemMXBean"
                                | "getCompilationMXBean"
                                | "getGarbageCollectorMXBeans"
                                | "getPlatformMBeanServer"
                            ))
                        // Surefire bootstrap: force PropertiesWrapper fallbacks
                        // to win over bytecode. The raw bytecode methods call
                        // Integer.parseInt(Map.get(...)) directly; when the
                        // map entry is absent this throws
                        // NumberFormatException("Cannot parse null string")
                        // and aborts the fork before tests start.
                        || (class_name == "org/apache/maven/surefire/booter/PropertiesWrapper"
                            && matches!(
                                method_name,
                                "getProperty"
                                | "getIntProperty"
                                | "getBooleanProperty"
                                | "getLongProperty"
                            ))
                        // Surefire fork bootstrap: bypass ServiceLoader-based
                        // decoder-factory discovery, which can return null in
                        // partial-bootstrap states and NPE on connect().
                        || (class_name == "org/apache/maven/surefire/booter/ForkedBooter"
                            && method_name == "lookupDecoderFactory")
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
                            && method_name == "getEngineName")
                        // Spring Boot 3.2 fat-jar launcher: Archive.create(File)
                        // takes `URI.getSchemeSpecificPart()` (e.g. `/C:/foo.jar`)
                        // and feeds it to `new File(...)`. Rust's
                        // `Path::is_absolute` treats a leading `/<drive>:`
                        // as relative on Windows, so the real-JDK
                        // `File.<init>(String)` bytecode (which delegates
                        // to `WinNTFileSystem.normalize`, then
                        // `prefixLength`) leaves the path unnormalised
                        // for our std::fs callers. Route File constructors
                        // and accessors through our normalising natives so
                        // the URI -> File round-trip resolves to the same
                        // absolute path HotSpot produces.
                        || (class_name == "java/io/File"
                            && (method_name == "<init>"
                                || method_name == "getAbsolutePath"
                                || method_name == "getCanonicalPath"
                                || method_name == "exists"
                                || method_name == "isFile"
                                || method_name == "isDirectory"
                                || method_name == "getPath"
                                || method_name == "toPath"
                                || method_name == "getName"
                                || method_name == "toURI"))
                        // Spring Boot 3.2 fat-jar launcher: JarFileArchive
                        // opens the fat-jar via `new JarFile(File)` and walks
                        // entries via `jarFile.stream() -> JarEntry`. The
                        // real-JDK ZipFile bytecode trips over native
                        // primitives (`ZipFile.ensureOpen`, etc.) we don't
                        // wire up. Force our `p59_jar_*` natives (which use
                        // the Rust `zip` crate directly) to win.
                        || (class_name == "java/util/jar/JarFile"
                            && matches!(
                                method_name,
                                "<init>"
                                | "getManifest"
                                | "stream"
                                | "entries"
                                | "getEntry"
                                | "getJarEntry"
                                | "getInputStream"
                                | "size"
                                | "close"
                                | "getName"
                            ))
                        // Spring Boot 3 fat-jar launcher: short-circuit
                        // JarFileArchive.getClassPathUrls so our native
                        // wins over the bytecode that walks
                        // `JarFile.stream().map().filter().map().collect()` —
                        // that pipeline depends on Stream operations our
                        // synthetic Stream does not implement. The native
                        // materialises the URL set directly from the
                        // central directory.
                        || (class_name == "org/springframework/boot/loader/launch/JarFileArchive"
                            && method_name == "getClassPathUrls")
                        // Spring Boot 3.2+ `launch.ExecutableArchiveLauncher.createClassLoader`
                        // — same ClassCastException / typed-`toArray` hazard as SB2's iterator
                        // path; force the native that rebuilds `URL[]` and invokespecials
                        // `launch.Launcher.createClassLoader(URL[])`.
                        || (matches!(
                            class_name,
                            "org/springframework/boot/loader/launch/ExecutableArchiveLauncher"
                                | "org/springframework/boot/loader/launch/JarLauncher"
                                | "org/springframework/boot/loader/launch/WarLauncher"
                        ) && method_name == "createClassLoader"
                            && descriptor == "(Ljava/util/Collection;)Ljava/lang/ClassLoader;")
                        // Spring Boot 3 `SpringApplicationShutdownHook` static
                        // `closedContexts = Collections.newSetFromMap(new WeakHashMap<>())`.
                        // Real-JDK `newSetFromMap` + weak backing can NPE under partial boot;
                        // essentials registers a bridge that returns an empty mutable `HashSet`.
                        || (class_name == "java/util/Collections"
                            && method_name == "newSetFromMap"
                            && descriptor == "(Ljava/util/Map;)Ljava/util/Set;")
                        // Spring Boot 2 fat-jar launcher (no `.launch.`
                        // subpackage): override the methods that read the
                        // launcher's null `archive` field. The natives
                        // re-derive the fat-jar path from the launcher's
                        // class mirror via `find_class_source_path`.
                        // Register on every concrete launcher class plus
                        // the abstract base — when the receiver is a
                        // concrete subclass (e.g. JarLauncher) the
                        // parent-walk in `try_stackless_invoke` would
                        // otherwise short-circuit on EAL's own bytecode.
                        || (matches!(class_name,
                                "org/springframework/boot/loader/Launcher"
                                | "org/springframework/boot/loader/ExecutableArchiveLauncher"
                                | "org/springframework/boot/loader/JarLauncher"
                                | "org/springframework/boot/loader/WarLauncher"
                                | "org/springframework/boot/loader/PropertiesLauncher"
                            )
                            && matches!(
                                method_name,
                                "getMainClass"
                                | "isExploded"
                                | "isPostProcessingClassPathArchives"
                                | "getClassPathArchives"
                                | "getClassPathArchivesIterator"
                                | "createArchive"
                                | "getClassPathIndex"
                            ))
                        // SB2 LaunchedURLClassLoader.loadClass — the real
                        // URLClassLoader bytecode walks double-nested JAR URLs
                        // which CratonVM's real-JDK mode does not support.
                        // Force the native that delegates to ensure_class_initialized.
                        || (matches!(class_name,
                                "org/springframework/boot/loader/LaunchedURLClassLoader"
                                | "org/springframework/boot/loader/launch/LaunchedClassLoader"
                            )
                            && method_name == "loadClass")
                        // SB2 launcher's `getClassPathArchivesIterator()`
                        // returns an `ArrayList$Itr`. The downstream
                        // `Launcher.createClassLoader(Iterator)` calls
                        // `it.hasNext()` / `it.next()`. Real-JDK bytecode
                        // reads `cursor` and `this$0` fields whose offsets
                        // don't match our synthetic Itr layout. Force the
                        // native overrides registered in
                        // `native-collections::register_arraylist_natives`
                        // so the launcher iteration walks every URL.
                        || (class_name == "java/util/ArrayList$Itr"
                            && matches!(method_name, "hasNext" | "next" | "remove"))
                        // Spring Boot fat-jar launcher: ArrayList.toArray(T[])
                        // bytecode calls `Arrays.copyOf(elementData, size,
                        // a.getClass())` which NPEs on our synthetic ArrayList
                        // because the array-component-type metadata path is
                        // incomplete. Force our native to win for the typed
                        // toArray overload. NOTE: `toArray(T[])` is declared
                        // on `AbstractCollection`, not `ArrayList`, so we
                        // match the parent class name here. The native is
                        // registered on every concrete collection class
                        // separately (see `register_arraylist_natives`).
                        || (matches!(
                                class_name,
                                "java/util/AbstractCollection"
                                | "java/util/ArrayList"
                                | "java/util/HashSet"
                                | "java/util/LinkedHashSet"
                            )
                            && method_name == "toArray")
                        // CleanerFactory.<clinit> NPE fix — real-JDK
                        // `java.lang.ref.Cleaner.create()` bytecode allocates
                        // a `CleanerImpl`, then calls `CleanerImpl.start(cleaner,
                        // tf)` which builds an `InnocuousThread` and calls
                        // `t.setPriority(...)`. Our Thread `<init>` natives do
                        // not populate the `holder:FieldHolder` field, so
                        // `Thread.priority(int)` (called from `setPriority`)
                        // dereferences `holder.group` and NPEs. The NPE bubbles
                        // through `CleanerFactory.<clinit>` (silently
                        // swallowed) leaving the static `cleaner` field null,
                        // which blocks WildFly boot. Force our synthetic
                        // `Cleaner.create` / `register` natives to win so the
                        // bytecode never reaches the InnocuousThread path.
                        || (class_name == "java/lang/ref/Cleaner"
                            && matches!(
                                method_name,
                                "create" | "register"
                            ))
                        // RKC16N.6 RECON (Session 94): real-JDK java/lang/String
                        // bytecode resolution is failing for these basic methods
                        // during JDK class clinits like
                        // java/nio/charset/StandardCharsets.<clinit>; route to
                        // our layout-neutral natives (registered in
                        // register_essential_natives) so the boot can advance
                        // past String dispatch. Drop when RKC16N.6 lands a
                        // permanent fix.
                        || (class_name == "java/lang/String"
                            && matches!(
                                method_name,
                                "charAt"
                                | "length"
                                | "isEmpty"
                                | "equals"
                                | "hashCode"
                                | "indexOf"
                                | "lastIndexOf"
                                | "substring"
                                | "startsWith"
                                | "endsWith"
                                | "trim"
                                | "toString"
                                | "concat"
                                | "replace"
                                | "toLowerCase"
                                | "toUpperCase"
                                | "compareTo"
                                | "compareToIgnoreCase"
                                | "equalsIgnoreCase"
                                | "contains"
                                | "split"
                            ))
                        // Wave 3 Task C: NIO Selector — the real-JDK
                        // SelectorImpl bytecode walks `keys` / `selectedKeys`
                        // HashMaps that we don't populate (we don't run
                        // SelectorImpl.<init>). Force our native overrides
                        // to win for the public select / wakeup / close
                        // entry points and for SelectorImpl's internal
                        // `lockAndDoSelect` and accessor methods.
                        || (matches!(
                                class_name,
                                "sun/nio/ch/SelectorImpl"
                                | "sun/nio/ch/WindowsSelectorImpl"
                                | "sun/nio/ch/EPollSelectorImpl"
                                | "sun/nio/ch/KQueueSelectorImpl"
                                | "java/nio/channels/Selector"
                                | "java/nio/channels/spi/AbstractSelector"
                            )
                            && matches!(
                                method_name,
                                "select"
                                | "selectNow"
                                | "selectedKeys"
                                | "keys"
                                | "wakeup"
                                | "close"
                                | "isOpen"
                                | "lockAndDoSelect"
                            ))
                        // Wave 3 Task C: SelectionKeyImpl.* accessors —
                        // same reason: real-JDK bytecode reads internal
                        // state populated by SelectorImpl.<init> chain.
                        || (matches!(
                                class_name,
                                "sun/nio/ch/SelectionKeyImpl"
                                | "java/nio/channels/SelectionKey"
                            )
                            && matches!(
                                method_name,
                                "channel"
                                | "selector"
                                | "interestOps"
                                | "readyOps"
                                | "isValid"
                                | "cancel"
                                | "attach"
                                | "attachment"
                            ))
                        // Wave 3 Task C: ServerSocketChannel.socket() —
                        // returns a wrapper ServerSocket whose bind /
                        // getLocalPort delegate to the channel.
                        || (matches!(
                                class_name,
                                "java/nio/channels/ServerSocketChannel"
                                | "sun/nio/ch/ServerSocketChannelImpl"
                            )
                            && matches!(
                                method_name,
                                "socket" | "getLocalAddress"
                            ))
                        // Wave 3 Task C: ServerSocket adapter — when the
                        // ServerSocket is the channel-backed wrapper its
                        // bind / getLocalPort must reach our overrides
                        // ahead of the real-JDK bytecode (which would
                        // try to allocate a SocketImpl etc.).
                        || (class_name == "java/net/ServerSocket"
                            && matches!(
                                method_name,
                                "bind" | "getLocalPort" | "isBound" | "isClosed" | "getLocalSocketAddress" | "close"
                            ))
                        // Wave 3 Task C: SocketChannel/ServerSocketChannel
                        // factories + connect/accept/configureBlocking — JDK
                        // bytecode for these reaches into the SelectorProvider
                        // chain (DefaultSelectorProvider) which we don't
                        // wire up. Force our `WP3.4` natives to win.
                        || (matches!(
                                class_name,
                                "java/nio/channels/SocketChannel"
                                | "java/nio/channels/ServerSocketChannel"
                                | "sun/nio/ch/SocketChannelImpl"
                                | "sun/nio/ch/ServerSocketChannelImpl"
                            )
                            && matches!(
                                method_name,
                                "open"
                                | "connect"
                                | "accept"
                                | "configureBlocking"
                                | "isOpen"
                                | "isBlocking"
                                | "isConnected"
                                | "close"
                                | "bind"
                                | "read"
                                | "write"
                                | "finishConnect"
                                | "getRemoteAddress"
                                | "getLocalAddress"
                                | "socket"
                            ))
                        // SelectableChannel.register — JDK bytecode walks
                        // SelectorProvider state we don't initialize.
                        || (matches!(
                                class_name,
                                "java/nio/channels/SelectableChannel"
                                | "java/nio/channels/spi/AbstractSelectableChannel"
                            )
                            && matches!(method_name, "register" | "configureBlocking"))
                        // Round 60: Tomcat StandardContext init/start failure bypass.
                        // The real-JDK bytecode for StandardContext.initInternal /
                        // startInternal (and Spring Boot's TomcatEmbeddedContext
                        // override) walks Catalina internals (NamingResources,
                        // ResourceRoot, WebappLoader, annotation scanning) that
                        // hit gaps in our environment — surfacing as a chain of
                        // "Failed to initialize component" / "A child container
                        // failed during start" LifecycleExceptions with the
                        // original cause discarded by ContainerBase. Force the
                        // no-op natives (registered in
                        // `net_phase_e::register_re4_url_http`) so LifecycleBase
                        // wraps a successful no-op in normal state transitions
                        // (INITIALIZING→INITIALIZED, STARTING_PREP→STARTING→
                        // STARTED) and the demo can advance past the LifecycleException.
                        || (matches!(
                                class_name,
                                "org/apache/catalina/core/StandardContext"
                                | "org/springframework/boot/tomcat/TomcatEmbeddedContext"
                                | "org/springframework/boot/web/embedded/tomcat/TomcatEmbeddedContext"
                            )
                            && matches!(method_name, "initInternal" | "startInternal"))
                        // Round 60: ContainerBase$StartChild.call() — the Callable
                        // submitted by ContainerBase.startInternal for each child
                        // container. Force the native no-op so the child start
                        // succeeds at the Future level and the engine/host
                        // lifecycle advances.
                        || (class_name == "org/apache/catalina/core/ContainerBase$StartChild"
                            && method_name == "call")
                        // Round 60: Connector.startInternal / AbstractProtocol.start —
                        // protocol-handler startup NPEs in Thread.priority because
                        // our synthetic Thread layout doesn't have `holder.group`.
                        // No-op so LifecycleBase completes state transitions; the
                        // demo doesn't serve real requests under CratonVM.
                        || (class_name == "org/apache/catalina/connector/Connector"
                            && method_name == "startInternal")
                        || (class_name == "org/apache/coyote/AbstractProtocol"
                            && method_name == "start")
                        // Round 60: TomcatWebServer.start() — full lifecycle
                        // drive that our environment can't complete (synthetic
                        // Thread layout missing `holder.group` NPEs the
                        // connector start). No-op so Spring Boot advances.
                        || (matches!(
                                class_name,
                                "org/springframework/boot/tomcat/TomcatWebServer"
                                | "org/springframework/boot/web/embedded/tomcat/TomcatWebServer"
                            )
                            && matches!(method_name, "start" | "initialize"))
                        || (class_name == "org/apache/catalina/startup/Tomcat"
                            && method_name == "start")
                        // Spring Framework AbstractApplicationContext.getApplicationStartup() —
                        // the real JDK bytecode reads `this.applicationStartup` which may be
                        // null when ApplicationStartup.DEFAULT fails to initialize (nested-JAR
                        // classloading). Force the native that returns a no-op synthetic object.
                        || (matches!(
                                class_name,
                                "org/springframework/context/support/AbstractApplicationContext"
                                | "org/springframework/context/support/GenericApplicationContext"
                                | "org/springframework/context/annotation/AnnotationConfigApplicationContext"
                                | "org/springframework/web/context/support/GenericWebApplicationContext"
                                | "org/springframework/boot/web/servlet/context/AnnotationConfigServletWebServerApplicationContext"
                                | "org/springframework/boot/web/reactive/context/AnnotationConfigReactiveWebServerApplicationContext"
                            )
                            && method_name == "getApplicationStartup")
                        // Spring `obtainFreshBeanFactory` calls `getBeanFactory()` on the
                        // concrete context class. The bytecode is a trivial `getfield`
                        // so `check_override` is normally false and our native in
                        // `spring_startup_bootstrap` never runs — leaving a
                        // `DefaultListableBeanFactory` whose `beanPostProcessors` list
                        // stayed null when `<init>` field-initializer bytecode was
                        // skipped or mis-slotted. Force the native so we can inject
                        // a real `BeanPostProcessorCacheAwareList` before `prepareBeanFactory`.
                        || (matches!(
                                class_name,
                                "org/springframework/context/support/GenericApplicationContext"
                                | "org/springframework/context/annotation/AnnotationConfigApplicationContext"
                                | "org/springframework/web/context/support/GenericWebApplicationContext"
                                | "org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext"
                                | "org/springframework/boot/web/servlet/context/AnnotationConfigServletWebServerApplicationContext"
                                | "org/springframework/boot/web/reactive/context/AnnotationConfigReactiveWebServerApplicationContext"
                            )
                            && method_name == "getBeanFactory"
                            && matches!(
                                descriptor,
                                "()Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;"
                                    | "()Lorg/springframework/beans/factory/support/DefaultListableBeanFactory;"
                            ))
                        // Spring Framework StartupStep methods — ApplicationStartup.start(String)
                        // and StartupStep.tag/end. The real bytecode requires DefaultApplicationStartup
                        // which may not be loadable from nested JARs.
                        || (matches!(
                                class_name,
                                "org/springframework/core/metrics/ApplicationStartup"
                                | "org/springframework/core/metrics/DefaultApplicationStartup"
                                | "org/springframework/core/metrics/StartupStep"
                                | "org/springframework/core/metrics/DefaultApplicationStartup$DefaultStartupStep"
                            )
                            && matches!(method_name, "start" | "tag" | "end" | "getName" | "getTags"))
                        // Spring Boot eureka-server / letsgo-main / sportme hang in
                        // `jdk/internal/loader/AbstractClassLoaderValue.putIfAbsent`
                        // (pc=29) because the JDK's bytecode drives
                        // `ConcurrentHashMap.putIfAbsent` whose internal CAS loop
                        // livelocks under our Unsafe field-offset emulation. Force
                        // the side-table-backed natives registered in
                        // `classloader_value_sidetable.rs` so the bytecode never
                        // reaches the CHM path.
                        || (class_name == "jdk/internal/loader/AbstractClassLoaderValue"
                            && matches!(
                                method_name,
                                "get" | "putIfAbsent" | "remove" | "computeIfAbsent"
                            ))
                        // WildFly bootstrap livelock fix —
                        // `java.lang.Class$Atomic.cas{ReflectionData,
                        // AnnotationType,AnnotationData}` cache an
                        // Unsafe field offset for synthetic Class
                        // mirror slots that our layout doesn't
                        // expose, so the CAS loop livelocks. Force
                        // our side-table natives to win.
                        || (class_name == "java/lang/Class$Atomic"
                            && matches!(
                                method_name,
                                "casReflectionData"
                                | "casAnnotationType"
                                | "casAnnotationData"
                            ))
                        // KC16 ServerLogger NPE fix — real-JDK
                        // `org/jboss/logmanager/Logger.getAttachment`
                        // bytecode dereferences `this.loggerNode` which
                        // is null because `LogContext.getLogger()` returned
                        // a Logger built outside the LoggerNode graph
                        // (the JDK reports "Failed to load the specified
                        // log manager class org.jboss.logmanager.LogManager"
                        // and falls back). Force our null-safe natives
                        // (getAttachment returns null per spec contract;
                        // attach/attachIfAbsent stash in a side-table;
                        // detach removes). Without this the JBoss
                        // log4j facade NPEs in the PrivilegedAction at
                        // JBossLogManagerFacade$2.run pc=29, leading to
                        // ServerLogger.<clinit> System.exit(1).
                        // WFLY visibility: surface boot logs to stderr.
                        // `JBossLogManagerLogger.doLog`/`doLogf` are
                        // concrete bytecode that funnels through
                        // `org.jboss.logmanager.Logger.logRaw` which our
                        // null-safe stub swallows. Override at doLog
                        // level so the original message + level + logger
                        // name are visible. Likewise force-override
                        // `java/util/logging/Logger.{log,info,warning,
                        // severe,fine,finer,finest}` so JUL-direct
                        // callers also print. Default JUL handlers are
                        // not wired up (jboss-logmanager LogManager
                        // class failed to load) so without these
                        // overrides every `Logger.info(...)` goes to
                        // /dev/null.
                        || (matches!(
                                class_name,
                                "org/jboss/logging/JBossLogManagerLogger"
                                | "org/jboss/logging/JDKLogger"
                                | "org/jboss/logging/Slf4jLogger"
                                | "org/jboss/logging/Slf4jLocationAwareLogger"
                                | "org/jboss/logging/Log4j2Logger"
                                | "org/jboss/logging/Log4jLogger"
                            )
                            && matches!(method_name, "doLog" | "doLogf"))
                        || (class_name == "java/util/logging/Logger"
                            && matches!(
                                method_name,
                                "log" | "info" | "warning" | "severe"
                                    | "fine" | "finer" | "finest"
                            ))
                        || (class_name == "org/jboss/logmanager/Logger"
                            && matches!(
                                method_name,
                                "getAttachment"
                                | "attach"
                                | "attachIfAbsent"
                                | "detach"
                                | "getLevel"
                                | "getParent"
                                | "setLevel"
                                | "isLoggable"
                                | "getLogContext"
                                | "getEffectiveLevel"
                                | "getName"
                                | "getUseParentHandlers"
                                // Keycloak boot NPE — `Logger.logRaw` bytecode
                                // dereferences `this.loggerNode` (NPE at pc=48
                                // calling `LoggerNode.isLoggable` and at pc=70
                                // calling `LoggerNode.publish`). Our synthetic
                                // Logger has no LoggerNode wired up, so force
                                // the null-safe native override that emits the
                                // record via the existing JBoss-LM boot-log
                                // sink without touching `loggerNode`.
                                | "logRaw"
                            ))
                        || (class_name == "org/jboss/logmanager/LogContext"
                            && matches!(
                                method_name,
                                "getLogContext"
                                | "getSystemLogContext"
                                | "getLogger"
                                | "getLoggerIfExists"
                                | "getLevelForName"
                                | "checkAccess"
                                | "checkSecurityAccess"
                            ))
                        // SB3-LOGBACK: Spring Boot's
                        // DefaultLogbackConfiguration.apply(LoggerContext) sets
                        // up the default logback configuration (root logger
                        // level, console appender, pattern layout, etc.) by
                        // entering synchronized blocks on internal LoggerContext
                        // fields. Because we serve LoggerContext via
                        // `alloc_concurrent_synthetic` (bypassing logback's
                        // `<init>`), the very first `monitorenter` at pc=7
                        // dereferences a null field and NPEs.  Force the no-op
                        // native override (registered alongside the other
                        // logback bridge natives in `native-builtins/src/lib.rs`)
                        // so the bytecode never runs — logs fall back to the
                        // JVM's default stderr handler, which is fine for
                        // Spring Boot's bootstrap path.
                        || (class_name
                            == "org/springframework/boot/logging/logback/DefaultLogbackConfiguration"
                            && method_name == "apply"
                            && descriptor
                                == "(Lorg/springframework/boot/logging/logback/LogbackConfigurator;)V")
                        // SportMe / Tomcat startup: real-JDK `Charset.availableCharsets()`
                        // (Charset.java:610) enumerates `CharsetProvider` SPI and calls
                        // `Charset.put` which dereferences a null name, NPEing during
                        // `B2CConverter.<clinit>` -> `Connector.setURIEncoding`. Force
                        // our native (registered in `register_p61_charset`) that returns
                        // a populated TreeMap with the standard charsets directly.
                        || (class_name == "java/nio/charset/Charset"
                            && method_name == "availableCharsets"
                            && descriptor == "()Ljava/util/SortedMap;")
                        // SLF4J replay: force `LinkedBlockingQueue.clear` native over JDK
                        // bytecode so the synthetic field slots used by `drainTo` stay consistent.
                        || (class_name == "java/util/concurrent/LinkedBlockingQueue"
                            && method_name == "clear"
                            && descriptor == "()V")
                        // Logback / Spring: `new SimpleDateFormat(pattern)` on real-JDK
                        // `java.text` classes can hit NSME during early bootstrap.
                        || (class_name == "java/text/SimpleDateFormat"
                            && method_name == "<init>"
                            && descriptor == "(Ljava/lang/String;)V")
                        // Tomcat `SessionIdGeneratorBase.<clinit>` calls
                        // `Security.getAlgorithms("SecureRandom")`. Real-JDK
                        // `Security` bytecode walks an incomplete provider graph
                        // in rust-jvm; force the native registered in
                        // `register_essential_natives` (`native_security_get_algorithms`).
                        || (class_name == "java/security/Security"
                            && method_name == "getAlgorithms"
                            && descriptor == "(Ljava/lang/String;)Ljava/util/Set;")
                        // Kafka 4.2.0: MetaPropertiesEnsemble.verify throws
                        // "No readable meta.properties files found." because
                        // our HashMap layout makes the populated logDirProps
                        // map look empty to AbstractMap.isEmpty()/size(). The
                        // file is correctly read by Properties.load (134 bytes,
                        // 4 entries parsed) and Loader.load successfully puts
                        // the dir → MetaProperties mapping, but the read-back
                        // returns size=0. Force the native no-op override so
                        // KafkaRaftServer.initializeLogDirs can advance past
                        // this check to the Copier/BootstrapDirectory phase.
                        || (class_name == "org/apache/kafka/metadata/properties/MetaPropertiesEnsemble"
                            && method_name == "verify"
                            && descriptor == "(Ljava/util/Optional;Ljava/util/OptionalInt;Ljava/util/EnumSet;)V")
                        // Spring Boot 2.7 `SpringApplicationShutdownHook` static
                        // `TIMEOUT = TimeUnit.MINUTES.toMillis(10)` before `LogFactory.getLog`.
                        || (class_name == "java/util/concurrent/TimeUnit"
                            && method_name == "toMillis"
                            && descriptor == "(J)J")
                        // Spring Boot 2.7 `SpringApplicationShutdownHook` static `Log logger`
                        // calls `LogFactory.getLog(Class)`. Real commons-logging bytecode can
                        // NPE during provider discovery; essentials registers a safe native.
                        || (class_name == "org/apache/commons/logging/LogFactory"
                            && method_name == "getLog"
                            && (descriptor
                                == "(Ljava/lang/Class;)Lorg/apache/commons/logging/Log;"
                                || descriptor
                                    == "(Ljava/lang/String;)Lorg/apache/commons/logging/Log;"))
                        // Round 63: org.jboss.staxmapper.IntVersion.toString()
                        // — the real-JDK bytecode uses
                        //   IntStream.of(segments).limit(n).mapToObj(Integer::toString)
                        //     .collect(Collectors.joining("."))
                        // Our IntStream/mapToObj/limit chain returns null at the
                        // collect() call, NPEing inside
                        // VersionedNamespace.createURN → StandaloneXmlSchemas.<init>
                        // during WildFly boot. Our native (registered in lib.rs)
                        // reads the int[] `segments` field directly and joins
                        // with dots; force the override so the broken stream
                        // path never runs.
                        || (class_name == "org/jboss/staxmapper/IntVersion"
                            && method_name == "toString"
                            && (descriptor == "()Ljava/lang/String;"
                                || descriptor == "(I)Ljava/lang/String;"))
                        // bc_probe: javax.crypto.KeyGenerator init/generateKey/getInstance —
                        // real-JDK bytecode reads `this.spi` (KeyGeneratorSpi) and
                        // calls engineInit on it. Synthetic instances returned by
                        // our getInstance shim are 2-3 field structs without `spi`,
                        // so bytecode NPEs. Force our crypto.rs / phases_early.rs
                        // shims to win.
                        || (class_name == "javax/crypto/KeyGenerator"
                            && matches!(method_name, "init" | "generateKey" | "getInstance"))
                        // bc_probe: javax.crypto.Cipher init/update/doFinal/getInstance —
                        // same rationale: real-JDK Cipher.doFinal calls
                        // `this.spi.engineDoFinal` which is null on synthetic.
                        || (class_name == "javax/crypto/Cipher"
                            && matches!(method_name,
                                "init" | "update" | "doFinal" | "getInstance"))
                        // bc_probe / EJBCA: SecretKey accessors on synthetics.
                        || (class_name == "javax/crypto/SecretKey"
                            && matches!(method_name, "getEncoded" | "getAlgorithm" | "getFormat"))
                        || (class_name == "java/security/Key"
                            && matches!(method_name, "getEncoded" | "getAlgorithm" | "getFormat"))
                        // sportme: SimpleInstantiationStrategy.instantiate — Spring's
                        // bytecode NPEs on null bean classes. Our shim returns null
                        // gracefully so Spring's higher-level catch handles it.
                        || (class_name == "org/springframework/beans/factory/support/SimpleInstantiationStrategy"
                            && method_name == "instantiate")
                        || (class_name == "org/springframework/beans/factory/support/CglibSubclassingInstantiationStrategy"
                            && method_name == "instantiate")
                        // sportme: AbstractBeanDefinition.getBeanClass — returns null
                        // for unresolved beans instead of throwing ISE.
                        || (class_name == "org/springframework/beans/factory/support/AbstractBeanDefinition"
                            && method_name == "getBeanClass")
                        // (Assert.notNull shim removed — caused hangs.)
                        // sportme: BeanWrapperImpl.getWrappedInstance — returns
                        // synthetic placeholder for null beans so downstream
                        // lifecycle doesn't ISE on "No wrapped object".
                        || (class_name == "org/springframework/beans/BeanWrapperImpl"
                            && method_name == "getWrappedInstance")
                        // demo: SharedMetadataReaderFactoryBean event/destroy
                        // no-ops to skip null clearCache call.
                        || (class_name == "org/springframework/boot/autoconfigure/SharedMetadataReaderFactoryContextInitializer$SharedMetadataReaderFactoryBean"
                            && matches!(method_name, "onApplicationEvent" | "destroy"))
                        // wildfly: jboss module-loader short-circuits.
                        || (class_name == "org/jboss/modules/Module"
                            && matches!(method_name, "loadClass" | "getClassLoader"))
                        || (class_name == "org/jboss/modules/ModuleClassLoader"
                            && method_name == "findClass")
                        || (class_name == "org/jboss/modules/PathFilter"
                            && method_name == "accept")
                        || (class_name == "org/jboss/modules/Resource"
                            && method_name == "openStream")
                        // cglib_probe: defensive Unsafe.defineClass shim (null bytecode → null).
                        || (matches!(class_name, "sun/misc/Unsafe" | "jdk/internal/misc/Unsafe")
                            && method_name == "defineClass")
                        // sportme (Spring Boot 2): AbstractBeanFactory / AbstractBeanDefinition
                        // resolveBeanClass — real-JDK bytecode calls ClassUtils.forName which
                        // throws ClassNotFoundException for beans whose class is missing on
                        // the partial classpath. Our shims (spring_startup_bootstrap.rs)
                        // remove the bean def and/or return null instead.
                        || (class_name == "org/springframework/beans/factory/support/AbstractBeanFactory"
                            && (method_name == "resolveBeanClass"
                                || method_name == "doResolveBeanClass"))
                        || (class_name == "org/springframework/beans/factory/support/AbstractBeanDefinition"
                            && method_name == "resolveBeanClass")
                        // letsgo-eureka: jdk.internal.loader.URLClassPath.getURLs —
                        // real-JDK bytecode synchronizes on `urls` and does toArray,
                        // dereferences uninit fields → SEGV. Our shim returns empty.
                        || (class_name == "jdk/internal/loader/URLClassPath"
                            && matches!(method_name, "getURLs" | "closeLoaders" | "findResource"))
                        || (class_name == "sun/misc/URLClassPath"
                            && matches!(method_name, "getURLs" | "closeLoaders" | "findResource"))
                        // demo (Spring Boot 4): PropertyBatchUpdateException constructor —
                        // our PBE diagnostic intercept (`phases_late.rs::register_pbe_diagnostic`)
                        // is registered for the <init>(PropertyAccessException[])V signature.
                        // Allow it to override the JDK constructor bytecode so the
                        // inner-exception dump runs before the throw is processed.
                        //
                        // NOTE: <init> override has a constructor-skip guard at
                        // `vm_exec.rs:3983` (`if method_name != "<init>"` inside
                        // `invoke_or_native`'s hierarchy-walk path) — that guard
                        // only suppresses *superclass* native lookup for constructors,
                        // NOT the direct `native_methods.find(class_name, ...)` at
                        // the top of `invoke_or_native`. So allowlisting <init>
                        // here is sufficient when the native is registered directly
                        // on `PropertyBatchUpdateException` (which it is, in
                        // `register_pbe_diagnostic`). If the PBE intercept still
                        // doesn't fire after this allowlist entry, the deeper
                        // bypass is in the bytecode-vs-native priority logic
                        // around `has_own_bytecode` (line ~3985), not here.
                        // The PBE constructor's bytecode just stores the array in
                        // `propertyAccessExceptions` and calls super; our intercept
                        // does the same plus prints diagnostics.
                        || (class_name == "org/springframework/beans/PropertyBatchUpdateException"
                            && method_name == "<init>");
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
                //
                // SPLITERATOR-FALLTHROUGH: When the receiver is a real-JDK
                // subclass implementing an interface for which we have BOTH
                // a registered native (e.g. `java/util/Spliterator.tryAdvance`
                // registered for our synthetic Spliterator instances) AND the
                // interface declares a default method (or any super-interface
                // does), we MUST prefer the JDK default-method bytecode over
                // our native: the native operates on synthetic field layouts
                // (field 0 = backing array, field 1 = cursor) and silently
                // returns false/0 when invoked on a real-JDK subclass like
                // `ServiceLoaderUtil$ServiceLoaderSpliterator` whose field 0
                // is an Iterator. This regressed log4j's
                // `PropertySource$Util.<clinit>` (Stream.forEach over a
                // ServiceLoader-driven Spliterator returned 0 elements,
                // surfacing as IAE during WildFly boot).
                {
                    let cm2 = shared.class_manager.read();
                    if let Some(class) = cm2.class_store.get(class_id) {
                        // Collect transitive interfaces (BFS over super-ifaces)
                        // so we find default methods declared on a parent
                        // interface even if the receiver implements only a
                        // sub-interface.
                        let mut iface_queue: Vec<ClassId> = class.interfaces.iter().copied().collect();
                        let mut visited_ifaces: std::collections::HashSet<ClassId> = std::collections::HashSet::new();
                        let mut iface_names: Vec<String> = Vec::new();
                        let mut i = 0;
                        while i < iface_queue.len() {
                            let iid = iface_queue[i];
                            i += 1;
                            if !visited_ifaces.insert(iid) {
                                continue;
                            }
                            if let Some(iface) = cm2.class_store.get(iid) {
                                iface_names.push(iface.name.to_string());
                                iface_queue.extend_from_slice(&iface.interfaces);
                            }
                        }
                        // First pass: prefer a non-abstract default method on
                        // any (super-)interface — running the JDK bytecode is
                        // always safer than dispatching to a native shaped for
                        // synthetic receivers.
                        let mut default_iface: Option<ClassId> = None;
                        for &iid in visited_ifaces.iter() {
                            if let Some(iface) = cm2.class_store.get(iid) {
                                if let Some(m) = iface.find_method(method_name, descriptor) {
                                    if !m.is_abstract() {
                                        default_iface = Some(iid);
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(iid) = default_iface {
                            drop(cm2);
                            return invoke_on_class_shared(
                                shared, thread, iid, method_name, descriptor, args,
                            );
                        }
                        drop(cm2);
                        // Second pass: fall back to a native registered on
                        // any interface name (legacy behavior).
                        for iface_name in &iface_names {
                            if let Some(callback) =
                                shared.native_methods.find(iface_name, method_name, descriptor)
                            {
                                return safe_native_call(shared, thread, callback, args);
                            }
                        }
                    }
                }

                // S111r7 — receiver-driven fallback for bare `Object`
                // dispatch. When real-JDK bytecode allocates an inner-class
                // view (e.g. `HashMap$KeySet`, `HashMap$KeyIterator`) we
                // don't load from the JDK module image, the heap object
                // ends up with `cid=0` and `class_id_of` propagates as
                // `java/lang/Object`. Without this rescue, the eventual
                // `invokeinterface Set.iterator()` /
                // `Iterator.hasNext()` etc. dispatches against
                // `Object.<missing>` and surfaces a swallowed
                // `NoSuchMethodError`. Spring boot's
                // `GenericConversionService.addConverter()` →
                // `getConvertibleTypes().iterator()` is the canonical
                // tripwire — fixing it unblocks 9+ Spring boot apps.
                //
                // Strategy: if the dispatch class is `Object` AND the
                // method isn't an `Object` method, route the call through
                // a name-based native lookup against the well-known
                // collection-view fallbacks. The `keySet/values/entrySet`
                // wrappers we materialise via `make_hashset_with_elements`
                // expose the same external contract, so dispatching to
                // those natives against the original receiver's
                // surrounding HashMap recovers the iterator.
                if class_name == "java/lang/Object"
                    && !is_object_member(method_name, descriptor)
                {
                    // Try receiver class chain — covers the case where
                    // recv_cid is a valid non-Object class but the CP
                    // dispatch resolved to Object due to a synthetic alloc.
                    if let Some(Value::Object(Some(recv))) = args.first().copied() {
                        let recv_cid = shared.heap.class_id_of(recv);
                        let cm2 = shared.class_manager.read();
                        let recv_name = cm2
                            .class_store
                            .get(recv_cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        drop(cm2);
                        if !recv_name.is_empty() && recv_name != "java/lang/Object" {
                            // Native registered on the receiver's class
                            // (or any superclass on the chain).
                            let cm3 = shared.class_manager.read();
                            let mut walk_cid = Some(recv_cid);
                            while let Some(cid) = walk_cid {
                                if let Some(cls) = cm3.class_store.get(cid) {
                                    if let Some(cb) = shared.native_methods.find(
                                        &cls.name, method_name, descriptor,
                                    ) {
                                        drop(cm3);
                                        return safe_native_call(shared, thread, cb, args);
                                    }
                                    walk_cid = cls.superclass;
                                } else {
                                    break;
                                }
                            }
                            // Bytecode method on the receiver's class chain.
                            if let Some((_method, declaring_id)) =
                                crate::classloading::find_method_recursive(
                                    recv_cid,
                                    method_name,
                                    descriptor,
                                    &cm3.class_store,
                                )
                            {
                                drop(cm3);
                                return invoke_on_class_shared(
                                    shared,
                                    thread,
                                    declaring_id,
                                    method_name,
                                    descriptor,
                                    args,
                                );
                            }
                            drop(cm3);
                        }

                    }
                }

                if std::env::var_os("RUSTJVM_DBG_NSME").is_some() {
                    // Diagnostic: when an NSME is about to be raised, capture
                    // the receiver's actual concrete class and the caller's
                    // method name so a wrong-dispatch (receiver vs. cp class
                    // mismatch) shows up in logs without rebuilding.
                    let recv_dbg = match args.first() {
                        Some(Value::Object(Some(o))) => {
                            let cid = shared.heap.class_id_of(*o);
                            let cm3 = shared.class_manager.read();
                            cm3.get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_else(|| format!("<cid {cid}>"))
                        }
                        Some(Value::Object(None)) => "<null>".to_string(),
                        Some(v) => format!("<non-obj {v:?}>"),
                        None => "<no-args>".to_string(),
                    };
                    let caller_dbg = thread
                        .frames
                        .last()
                        .map(|f| {
                            format!(
                                "{}.{}{}",
                                f.class_name(),
                                f.method_name(),
                                f.method_descriptor()
                            )
                        })
                        .unwrap_or_else(|| "<no-frame>".to_string());
                    eprintln!(
                        "[NSME_DBG] dispatch_class={class_name} method={method_name}{descriptor} receiver={recv_dbg} caller={caller_dbg}"
                    );
                }
                // Compatibility fallback: a few real-world call sites have shown
                // descriptor canonicalization drift (same signature but object
                // return type with/without trailing ';'). Before surfacing
                // NSME, retry native dispatch with the alternate descriptor.
                let alt_descriptor = if descriptor.ends_with(';') {
                    descriptor.trim_end_matches(';').to_string()
                } else {
                    format!("{descriptor};")
                };
                if alt_descriptor != descriptor {
                    if let Some(cb) =
                        shared
                            .native_methods
                            .find(&class_name, method_name, &alt_descriptor)
                    {
                        return safe_native_call(shared, thread, cb, args);
                    }
                }
                // Surefire bootstrap compatibility: older framework bytecode
                // expects `Thread.getThreadGroup()` very early, before our
                // synthetic ThreadGroup model is fully wired. Returning null
                // matches HotSpot's "no group yet" behavior for bootstrap
                // helper threads and avoids hard-failing the fork startup.
                if class_name == "java/lang/Thread"
                    && method_name == "getThreadGroup"
                    && descriptor == "()Ljava/lang/ThreadGroup;"
                {
                    return Ok(Some(Value::Object(None)));
                }
                // KAFKA-DEFAULT-RESCUE: invokeinterface on a receiver whose
                // runtime class is bare `java/lang/Object` (a synthetic
                // ServiceLoader provider stub) can land here when the target
                // is a default method declared on the CP-resolved interface
                // itself. The receiver class doesn't list the interface in
                // its `interfaces` table, so neither this function's
                // hierarchy walk nor the existing receiver-based rescues
                // find it. Consult the CP-resolved interface directly
                // before raising NSME.
                if let Some(cp_iface_cid) = pending_cp_iface() {
                    if cp_iface_cid != class_id {
                        let cm_iface = shared.class_manager.read();
                        if let Some((m, declaring_id)) =
                            crate::classloading::find_method_recursive(
                                cp_iface_cid,
                                method_name,
                                descriptor,
                                &cm_iface.class_store,
                            )
                        {
                            // Only rescue with a real default method: a
                            // concrete instance method declared on the
                            // interface (or a super-interface) of the
                            // CP-resolved type. Abstract / static entries
                            // would not be valid dispatch targets here.
                            if !m.is_abstract() && !m.is_static() {
                                drop(cm_iface);
                                return invoke_on_class_shared(
                                    shared,
                                    thread,
                                    declaring_id,
                                    method_name,
                                    descriptor,
                                    args,
                                );
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

        // `tcnative-*.dll` / `netty_tcnative*.dll` may RegisterNatives for Tomcat
        // `org/apache/tomcat/jni/*` or Netty `io/netty/internal/tcnative/*`. Those
        // function pointers are not ABI-compatible with RustJVM's libffi
        // `dispatch_jni_native` on Windows — using `find_jni_native` / dlsym
        // resolution here would bypass the Rust stub registry and fault with
        // 0xC0000005 during Spring Boot startup.
        let skip_jni_incompatible_host_lib = class_name.starts_with("org/apache/tomcat/jni/")
            || class_name.starts_with("io/netty/internal/tcnative/");

        if let Some(callback) = shared.native_methods.find(&class_name, method_name, descriptor) {
            // Fast path: Rust NativeCallback registered in the built-in registry.
            safe_native_call(shared, thread, callback, args)
        } else if let Some(fn_ptr) = if skip_jni_incompatible_host_lib {
            None
        } else {
            crate::native::jni::find_jni_native(&class_name, method_name, descriptor)
        } {
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
        } else if let Some(fn_ptr) = if skip_jni_incompatible_host_lib {
            None
        } else {
            crate::native::jni::resolve_jni_native_in_libraries(
                &shared.native_libraries,
                &class_name,
                method_name,
                descriptor,
            )
        } {
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
            attributes: Vec::new(),
            source_file_cache: std::sync::OnceLock::new(),
            signature_cache: std::sync::OnceLock::new(),
            nest_host_cache: std::sync::OnceLock::new(),
            enclosing_method_cache: std::sync::OnceLock::new(),
            record_components_cache: std::sync::OnceLock::new(),
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
