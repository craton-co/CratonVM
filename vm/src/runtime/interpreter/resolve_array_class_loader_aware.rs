#[allow(unused_imports)]
use super::*;


// ---------------------------------------------------------------------------
// Helper: loader-faithful ARRAY class resolution (JVMS §5.3.3)
// ---------------------------------------------------------------------------

/// Resolve the array class `name` the way JVMS §5.3.3 requires, from the
/// perspective of `referencing_class_id`'s defining loader.
///
/// > If the component type is a `reference` type, the Java Virtual Machine
/// > marks C to have the defining loader of the component type as its defining
/// > loader. Otherwise, the Java Virtual Machine marks C to have the bootstrap
/// > class loader as its defining loader.
/// >
/// > — JVMS SE 21 §5.3.3, step 2
///
/// So `[Lp/X;` referenced from a class defined by loader A is a *different*
/// runtime class from `[Lp/X;` referenced from loader B whenever A and B each
/// define their own `p/X`. Collapsing the two is type confusion: array class
/// identity is what `checkcast`/`instanceof` on an array type, the verifier's
/// array-assignability rules and `Class.getComponentType()` all read.
///
/// Returns `None` — meaning "no loader-faithful answer, use the ordinary global
/// resolution" — for a non-array name, for a referencing class whose defining
/// loader is built-in, or when the class manager cannot synthesise the array.
/// It can therefore only ever return a *more* precise answer, never fail a
/// resolution that the global path would have answered.
///
/// Locking: the O(1) exact-key probe runs under the class-manager **read**
/// lock, and the write lock is taken only on the cold "this loader has never
/// resolved this array descriptor" path — at most once per
/// `(loader, array descriptor)` pair. Same discipline as
/// `SharedVm::load_class_concurrent`; each guard is bound to a `let` so it is
/// dropped at the semicolon rather than being held across the next acquisition
/// (`parking_lot::RwLock` is not reentrant — see the note in
/// `typecheck::array_is_assignable_to_impl`).
///
/// CratonVM's own by-name lookups call this; a class-resolution door calls
/// [`resolve_array_class_loader_aware_checked`], which propagates the element
/// loader's refusal under `--real-jdk`.
pub(crate) fn resolve_array_class_loader_aware(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    resolve_array_class_loader_aware_impl(shared, thread, referencing_class_id, name, false)
        .ok()
        .flatten()
}

/// [`resolve_array_class_loader_aware`] for a class-resolution door: under
/// `--real-jdk` the element class is asked of the user loader through the
/// checked door (`drive_array_component_load_checked`), so the loader's
/// throwable, or its `null`, is the array resolution's error, as on HotSpot,
/// instead of a refusal the flat store then overrules by loading the
/// application's same-named element (interpreter round i1 wave 41, lane L5;
/// probe `L5W41ArrayComponentLoaderThrow`). `--compatible` unchanged.
pub(crate) fn resolve_array_class_loader_aware_checked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Result<Option<ClassId>, MethodCallFailed> {
    resolve_array_class_loader_aware_impl(shared, thread, referencing_class_id, name, true)
}

/// The body of both: `checked` selects the element drive's door.
pub(in crate::runtime::interpreter) fn resolve_array_class_loader_aware_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    checked: bool,
) -> Result<Option<ClassId>, MethodCallFailed> {
    if !name.starts_with('[') {
        return Ok(None);
    }
    let Some(requesting_loader) = ({
        shared
            .classes
            .class_manager
            .read()
            .get_loader_id(referencing_class_id)
    }) else {
        return Ok(None);
    };
    // Only a user-defined component loader can produce a non-bootstrap array
    // class (see `ClassManager::array_defining_loader` for why the three
    // built-in loaders are deliberately collapsed onto `Bootstrap`), so for a
    // built-in referencing class this path has nothing to add and must not pay
    // for the probe.
    if !matches!(
        requesting_loader,
        cratonvm_types::ClassLoaderId::UserDefined(_)
    ) {
        // HotSpot-aligned (`--real-jdk`, or `--compatible` with the
        // `NamespacedArrayTypes` row): the ordinary global resolution finds an
        // array class BY NAME, and once a user loader has resolved `[Lp/X;` over
        // its own `p/X` that is the only array of that name the store holds -- so
        // the application's own `X[].class` came back as the child loader's array
        // (an enum's `values()` threw `[LE; cannot be cast to [LE;` in the
        // application loader after a Spring forked-loader test).
        // Ask for the bootstrap-keyed array explicitly (its component resolved
        // through the built-in chain), exactly as a user requester asks for
        // its own. Costs nothing until some user loader has defined a class.
        if shared
            .classes
            .aligned(cratonvm_types::HotSpotAlignment::NamespacedArrayTypes)
            && cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity)
        {
            let cached = {
                shared
                    .classes
                    .class_manager
                    .read()
                    .loaded_class_under_exact_key(name, cratonvm_types::ClassLoaderId::Bootstrap)
            };
            if let Some(id) = cached {
                return Ok(Some(id));
            }
            return Ok(shared
                .classes
                .class_manager_write_settling()
                .load_array_class_for_loader(name, requesting_loader)
                .ok());
        }
        return Ok(None);
    }
    // Fast path. Exact key only: a delegating probe would hand back another
    // loader's array class, which is the whole bug.
    //
    // Probing under `requesting_loader` is sound even though the array's key is
    // its *component's* loader — a hit means an array class named `name` is
    // filed under this loader, which by construction (the key is derived from
    // the component) means its component was defined by this loader.
    let cached = {
        shared
            .classes
            .class_manager
            .read()
            .loaded_class_under_exact_key(name, requesting_loader)
    };
    if let Some(id) = cached {
        return Ok(Some(id));
    }
    // §5.3.3 step 1: "the algorithm of this section is applied recursively
    // **using L** in order to load and thereby create the component type".
    // `ClassManager` cannot invoke a Java `loadClass`, so drive the component
    // through this loader here, before deriving the array's defining loader
    // from it. Without this the component would fall back to the flat global
    // store and the array would come out bootstrap-keyed again.
    if let Some(component) = array_component_class_name(name) {
        if checked && shared.classes.loader_throws_on && !is_global_resolution_namespace(component)
        {
            // `--real-jdk`, a class-resolution door (wave 41, lane L5; probe
            // `L5W41ArrayComponentLoaderThrow`). The element is what the
            // loader answers for its name, as for a plain reference
            // (`resolve_class_loader_aware`): what it defined or initiated,
            // else one request of it (JVMS §5.3.3, §5.3.2), whose refusal
            // fails the array. `find_class_by_name_for_loader` below guesses
            // instead (its own namespace, then the built-in chain), so a
            // child-first loader's `X[]` was an array of the APPLICATION's
            // `X` once that was loaded, and a refusing loader was never
            // heard. The array is then keyed by the answer's defining loader
            // (§5.3.3 step 2), which also covers an answer another user
            // loader defined.
            let answer = match lookup_loader_initiated(shared, referencing_class_id, component) {
                Some(id) => Some(id),
                None => drive_array_component_load_checked(
                    shared,
                    thread,
                    referencing_class_id,
                    component,
                    name,
                )?,
            };
            if let Some(element) = answer {
                let element_loader = shared
                    .classes
                    .class_manager
                    .read()
                    .get_loader_id(element)
                    .unwrap_or(requesting_loader);
                return Ok(shared
                    .classes
                    .class_manager_write_settling()
                    .load_array_class_for_loader(name, element_loader)
                    .ok());
            }
        } else {
            let known = {
                shared
                    .classes
                    .class_manager
                    .read()
                    .find_class_by_name_for_loader(component, requesting_loader)
            };
            if known.is_none() {
                let _ = drive_defining_loader_load(shared, thread, referencing_class_id, component);
            }
        }
    }
    Ok(shared
        .classes
        .class_manager_write_settling()
        .load_array_class_for_loader(name, requesting_loader)
        .ok())
}

/// [`resolve_array_class_loader_aware`] with the ordinary
/// [`resolve_class_loader_aware`] as its fallback — the drop-in replacement for
/// a `CONSTANT_Class` resolution site that may be handed either an array
/// descriptor or a plain class name.
pub(crate) fn resolve_class_or_array_loader_aware(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Result<ClassId, MethodCallFailed> {
    if name.starts_with('[') {
        let loader_faithful =
            resolve_array_class_loader_aware_checked(shared, thread, referencing_class_id, name)?;
        if let Some(id) = loader_faithful {
            return Ok(id);
        }
    }
    resolve_class_loader_aware(shared, thread, referencing_class_id, name)
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Pop a reference operand, deferring the NPE-context message construction
/// until the NPE actually fires — call sites with a `format!`-built message
/// used to pay the formatting on every pop, including the successful ones.
///
/// This is the ONE reference-pop helper. It had two siblings,
/// `pop_object_ref` and `pop_object_ref_ctx`, that recovered a smuggled
/// `jobject` from any 8-aligned sub-2^48 `long`/`double` bit pattern WITHOUT
/// the heap-membership check below (the C7 hole this function closes). Their
/// last caller, the legacy non-JEP-358 `*aload` arm, moved to this checked
/// form (`opcodes::array_operand_slow`), and both were removed rather than
/// left as an unchecked path for the next caller to find.
///
/// `heap` is required because Double/Long slots can carry smuggled
/// `jobject` bit patterns (the JNI "long-as-jobject" pattern used by
/// e.g. WildFly's jboss-modules bootloader). Without a heap-membership
/// check, an honest `Value::Double` whose `to_bits()` value happens to
/// satisfy the 8-aligned + <2^48 predicate would be coerced into a
/// wild `ObjectRef` and dereferenced by the next GC scan or field
/// access (C7). Mirrors the validation used by
/// `value_stack::scan_object_refs` for the same smuggle path.
pub(in crate::runtime::interpreter) fn pop_object_ref_ctx_with<F>(
    stack: &mut crate::runtime::ValueStack,
    heap: &crate::memory::vm_heap::VmHeap,
    make_context: F,
) -> Result<ObjectRef, MethodCallFailed>
where
    F: FnOnce() -> String,
{
    match stack.pop()? {
        Value::Object(Some(obj_ref)) => Ok(obj_ref),
        Value::Object(None) => Err(RuntimeError::NullPointerException {
            message: Some(make_context()),
        }
        .into()),
        Value::Uninitialized => Err(RuntimeError::NullPointerException {
            message: Some(make_context()),
        }
        .into()),
        Value::Int(0) | Value::Long(0) => Err(RuntimeError::NullPointerException {
            message: Some(make_context()),
        }
        .into()),
        Value::Double(d) => {
            let bits = d.to_bits();
            if bits == 0 {
                Err(RuntimeError::NullPointerException {
                    message: Some(make_context()),
                }
                .into())
            } else if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                // C7 fix: alignment + 48-bit-range alone are NOT sufficient
                // to prove a Double slot holds a smuggled jobject — an honest
                // f64 (e.g. `f64::from_bits(0x800)`) can satisfy them too. Use
                // `is_heap_addr` (the same loose arena+alignment check that
                // `value_stack::scan_object_refs` uses for the JNI long-as-
                // jobject smuggle pattern) to confirm the bits actually point
                // into the managed heap before fabricating an `ObjectRef`.
                // Widening: small integer index -> usize (non-negative, fits in pointer width)
                if let Some(obj_ref) = heap.is_heap_addr(bits as usize) {
                    // SAFETY: `is_heap_addr` returned `Some(ObjectRef)`,
                    // meaning the address is within one of the GC's managed
                    // arenas and 8-byte aligned. The returned `ObjectRef`
                    // was constructed by the heap layer itself (not by us);
                    // we forward it unchanged. The GC's MAX_SANE_OBJECT_SIZE
                    // sanity guard in `forward_object` handles any residual
                    // bogus root gracefully (over-retention only).
                    Ok(obj_ref)
                } else {
                    Err(VmError::Internal {
                        message: format!(
                            "expected object reference, got double({d}) with non-heap bit pattern"
                        ),
                    }
                    .into())
                }
            } else {
                Err(VmError::Internal {
                    message: format!("expected object reference, got double({d})"),
                }
                .into())
            }
        }
        Value::Long(l) => {
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            let bits = l as u64;
            if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                // C7 fix: see Double-arm comment above. The JNI long-as-
                // jobject smuggle (WildFly jboss-modules bootloader) is
                // preserved, but we now require the bits to land inside a
                // managed-heap arena before we trust them as an `ObjectRef`.
                // Widening: small integer index -> usize (non-negative, fits in pointer width)
                if let Some(obj_ref) = heap.is_heap_addr(bits as usize) {
                    // SAFETY: `is_heap_addr` returned `Some(ObjectRef)`,
                    // meaning the address is within one of the GC's managed
                    // arenas and 8-byte aligned. The returned `ObjectRef`
                    // was constructed by the heap layer itself; we forward
                    // it unchanged. The GC's MAX_SANE_OBJECT_SIZE sanity
                    // guard in `forward_object` handles any residual bogus
                    // root gracefully (over-retention only).
                    Ok(obj_ref)
                } else {
                    Err(VmError::Internal {
                        message: format!(
                            "expected object reference, got long({l}) with non-heap bit pattern"
                        ),
                    }
                    .into())
                }
            } else {
                Err(VmError::Internal {
                    message: format!("expected object reference, got long({l})"),
                }
                .into())
            }
        }
        other => {
            let ctx = make_context();
            if crate::runtime::env_cache::iae_trace_os() {
                eprintln!(
                    "[pop_object_ref] ERROR: expected object reference, got {other} ctx={ctx:?}"
                );
                let bt = std::backtrace::Backtrace::capture();
                eprintln!("[pop_object_ref] Rust backtrace:\n{bt}");
            }
            Err(VmError::Internal {
                message: format!("expected object reference, got {other} ctx={ctx:?}"),
            }
            .into())
        }
    }
}

pub(in crate::runtime::interpreter) fn branch_target(saved_pc: usize, offset: i16) -> usize {
    (saved_pc as isize + offset as isize) as usize // Cast: signed branch offset arithmetic
}

/// Test whether two reference values are the same object (identity comparison).
/// Exposed as `pub` for testing from vm.rs.
pub fn test_refs_equal(a: &Value, b: &Value) -> bool {
    refs_equal(a, b)
}

#[inline]
pub(in crate::runtime::interpreter) fn value_as_object_ptr(v: &Value) -> Option<*mut u8> {
    match v {
        Value::Object(Some(o)) => Some(o.as_ptr()),
        Value::Long(bits) => {
            // Cast: reinterpret pointer/address to typed pointer
            crate::types::jlong_bits_as_aligned_object_ptr(*bits as u64).map(|p| p as *mut u8)
        }
        _ => None,
    }
}

/// JVMS `ifnull`/`ifnonnull` (and any shared reference null test): determine
/// whether a reference operand is the null reference.
///
/// Beyond the obvious `Value::Object(None)` / `Value::Uninitialized`, this also
/// treats a reference-typed `Value::Long(0)` as null — a JNI `jobject` null
/// handle smuggled through the operand stack as raw long bits (the same
/// `Long(0)`-is-null contract already honored by `pop_object_ref*` and
/// `refs_equal`). A non-zero `Value::Long` whose bits form an aligned heap
/// pointer is a live jobject and is NOT null; any non-reference numeric value
/// cannot legally reach `ifnull`/`ifnonnull` (the verifier requires a reference
/// operand), so only the zero case is special-cased.
///
/// `Value::Int(0)` is null too (round i1 wave 5): it is a never-written
/// reference slot (an all-zero legacy slot decodes as `Int(0)`, e.g. a
/// JIT-allocated object's reference field read raw by a native) and cannot
/// spell an object. Every other reader already says so — `getfield`, `aload`
/// (`load_ref_local`), `Unsafe` (`recover_object_arg`), `refs_equal` (so
/// `if_acmpeq x, null` was true while `ifnull x` was false), and compiled
/// `ifnull`, which tests the payload word — and this test was the one that
/// took the NON-null branch and dereferenced it.
#[inline]
pub(in crate::runtime::interpreter) fn ref_operand_is_null(v: &Value) -> bool {
    match v {
        Value::Object(None) | Value::Uninitialized => true,
        // jobject-as-long: 0 is the null handle. A non-zero long is either a
        // live jobject pointer or an honest long value — neither is null.
        Value::Long(0) => true,
        // A never-written reference slot. See the doc.
        Value::Int(0) => true,
        _ => false,
    }
}

/// Pop the operand stack's top reference and answer whether it is null, as
/// the fast `ifnull` / `ifnonnull` arm tests it: an `Object`-tagged slot is
/// non-null whatever its kind mark (the `Value` decode of such bits is
/// `Object(Some)`, or a non-zero `Long` / a `Double` under a cat-2 mark --
/// never null), so the common non-null operand skips the `Value` decode;
/// every other shape takes the decode and [`ref_operand_is_null`].
///
/// One implementation for the arm and for the fused `aload_N; ifnull |
/// ifnonnull` (interpreter round i6 wave 115, lane L8,
/// `CRATONVM_INTERP_FUSED_NULL_BRANCH`), so the two cannot disagree.
#[inline(always)]
pub(in crate::runtime::interpreter) fn pop_ref_operand_is_null(stack: &mut crate::runtime::ValueStack) -> bool {
    if stack.peek_compact().as_object_ptr().is_some() {
        stack.pop_compact();
        false
    } else {
        let v = stack.pop_unchecked();
        ref_operand_is_null(&v)
    }
}


pub(in crate::runtime::interpreter) fn refs_equal(a: &Value, b: &Value) -> bool {
    match (value_as_object_ptr(a), value_as_object_ptr(b)) {
        (Some(pa), Some(pb)) => pa == pb,
        (None, None) => match (a, b) {
            (Value::Object(None), Value::Object(None)) => true,
            (Value::Int(va), Value::Int(vb)) => va == vb,
            (Value::Int(0), Value::Object(None)) | (Value::Object(None), Value::Int(0)) => true,
            (Value::Long(0), Value::Object(None)) | (Value::Object(None), Value::Long(0)) => true,
            // Both sides a JNI-smuggled null handle. `ref_operand_is_null`
            // already calls `Value::Long(0)` the null reference, and the arm
            // above says a `Long(0)` equals an `Object(None)` — so leaving this
            // pair out made `if_acmpeq(nullHandle, nullHandle)` answer FALSE
            // while both `if_acmpeq(nullHandle, null)` and `ifnull(nullHandle)`
            // answered TRUE. Equality on the same value has to be reflexive.
            (Value::Long(0), Value::Long(0)) => true,
            _ => false,
        },
        _ => false,
    }
}

/// `if_acmp<cond>` decided on the two raw operand slots (top = `b`), or
/// `None` when either slot needs the `Value` decode.
///
/// Answers only when both slots are plain references: `Object`- or
/// `Null`-tagged and not under a KIND_LONG / KIND_DOUBLE mark (the `Value`
/// path decodes a marked slot by its mark, so those keep it). Then identity is
/// payload equality, and null equals only null — what `refs_equal` answers for
/// the decoded pair, with one difference, which is a fix: `to_value` decodes an
/// `Object`-tagged payload that has no recorded `ObjectRef` provenance as a
/// `long`, and `refs_equal` called two such slots holding the SAME reference
/// unequal. Here they are equal. The JNI `long`-handle shapes are
/// `Long`-tagged and still go to `refs_equal`.
#[inline(always)]
pub(in crate::runtime::interpreter) fn acmp_on_raw_slots(stack: &crate::runtime::ValueStack) -> Option<bool> {
    let plain = |(cv, kind): (CompactValue, u8)| {
        kind != crate::runtime::ValueStack::KIND_MARK_LONG
            && kind != crate::runtime::ValueStack::KIND_MARK_DOUBLE
            && matches!(cv.tag(), CompactTag::Object | CompactTag::Null)
    };
    let b = stack.peek_with_kind_at(0);
    let a = stack.peek_with_kind_at(1);
    if !(plain(a) && plain(b)) {
        return None;
    }
    Some(match (a.0.as_object_ptr(), b.0.as_object_ptr()) {
        (Some(pa), Some(pb)) => pa == pb,
        (None, None) => true, // both null
        _ => false,
    })
}

/// The throwable a thrown `ExceptionThrown` actually unwinds with, after the
/// one repair the dispatch loop applies to every thrown reference: drain
/// `native_pending_return` (a native call's returned object, rooted until it
/// reaches an operand stack — a leftover must not outlive this throw), and
/// fall back to it only when `exc` itself is no longer a live object address
/// (a moving collection ran during the failing native call). Applied to
/// every throw on both paths: the decoded path's `ExceptionThrown` and
/// runtime-error arms, `classify_fastpath_invoke_error` (every raw-bytecode
/// arm's error), and the loop top's `pending_runtime_error` conversion (wave
/// 5; until then only `athrow` and `invokedynamic` among the raw-bytecode
/// arms applied it). See the decoded `ExceptionThrown` arm for the history.
#[inline]
pub(in crate::runtime::interpreter) fn settle_thrown_exception(shared: &SharedVm, thread: &mut JvmThread, exc: ObjectRef) -> ObjectRef {
    let pending_return = thread.native_pending_return.take();
    if shared
        .mem
        .heap
        .is_object_address(exc.as_ptr() as usize) // Cast: address probe
        .is_some()
    {
        exc
    } else {
        pending_return.unwrap_or(exc)
    }
}

/// The executing method's per-entry branch-profile arm
/// (`jit_bridge::arm_method_branch_profile`). The dispatch loop caches it in
/// `branch_profile_on` and re-reads it here at frame switches and backward
/// branches only.
#[inline(always)]
pub(in crate::runtime::interpreter) fn frame_branch_profile_armed(frame: &Frame) -> bool {
    frame.cached_method().is_some_and(|cm| {
        cm.branch_profile_armed
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

// ---------------------------------------------------------------------------
// Dispatch loop: return values kept rooted across the exit callbacks (wave 4)
// ---------------------------------------------------------------------------
//
// A value return pops the result off the callee's operand stack, and then the
// callee still has to leave: JVMTI `MethodExit`, the `ACC_SYNCHRONIZED`
// monitor release and JVMTI `FramePop` (`pop_and_recycle_frame`). An agent
// callback can allocate or run Java and so collect; a reference held only in
// a Rust local across it is not a root. A stackless return therefore pushes
// the value into the parent before any of that runs (the return arms), and
// the paths with no parent frame pin it here.

/// Push a fast-path return value onto the parent's operand stack, preserving
/// the kind the return opcode implies.
///
/// `lreturn` / `dreturn` mark the slot KIND_LONG / KIND_DOUBLE: `push_compact`
/// alone marks it KIND_UNKNOWN, and a collision-shaped long (bits aliasing the
/// NaN-tag int space, e.g. `0xFFFC_...` whose masked payload also fits 32
/// bits) is then indistinguishable from a real int the next time the parent
/// pops it (invokestatic argument marshalling, `lload`, ...), which truncates
/// it. `areturn` pushes the normalized reference `value`; `ireturn` /
/// `freturn` have no collision ambiguity, so the raw copy suffices.
#[inline(always)]
pub(in crate::runtime::interpreter) fn push_fast_return_value(
    stack: &mut crate::runtime::ValueStack,
    opcode: u8,
    cv: CompactValue,
    value: Value,
) {
    match opcode {
        0xb0 => stack.push_unchecked(value),
        0xad => stack.push_compact_long(cv),
        0xaf => stack.push_compact_double(cv),
        _ => stack.push_compact(cv),
    }
}

/// Fire JVMTI `MethodExit` for a normal return of `value` from frame
/// `frame_idx`, with a reference `value` pinned in `native_pin_roots` for the
/// callback's duration and re-read from the pin afterwards (a moving
/// collection rewrites the pin slot, not the caller's copy). For a return
/// whose value has no frame to live in: the outermost return of a dispatch
/// loop and the decoded `areturn` handler. One union-flag load when no agent
/// listens, exactly as `fire_jvmti_method_exit_normal` alone.
#[inline]
pub(in crate::runtime::interpreter) fn fire_method_exit_keeping_value(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    value: &mut Option<Value>,
) {
    if !crate::runtime::jvmti::any_method_exit_listener_active() {
        return;
    }
    let Some(Value::Object(Some(r))) = *value else {
        fire_jvmti_method_exit_normal(shared.vm_identity, thread, frame_idx, value);
        return;
    };
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(r);
    fire_jvmti_method_exit_normal(shared.vm_identity, thread, frame_idx, value);
    *value = Some(Value::Object(Some(thread.native_pin_roots[pin_base])));
    thread.native_pin_roots.truncate(pin_base);
}

/// The tail every caller of `execute_frame_from_index` that owns the root
/// frame runs: pop any orphaned inner frames an early `return Err` left, then
/// the root frame itself (`depth_before_push` is the depth before the root
/// was pushed), keeping a reference `result` — a returned object or a thrown
/// exception — pinned across the pops. Each pop can fire JVMTI `FramePop`,
/// whose callback may collect; the pin is taken only while a `FramePop`
/// listener exists anywhere, so the agent-free path is the two loops it
/// replaces.
pub(crate) fn pop_root_frames_keeping_result(
    shared: &SharedVm,
    thread: &mut JvmThread,
    depth_before_push: usize,
    result: &mut MethodCallResult,
) {
    let held = match &*result {
        Ok(Some(Value::Object(Some(r)))) => Some(*r),
        Err(MethodCallFailed::ExceptionThrown(e)) => Some(*e),
        _ => None,
    };
    let pin = held.filter(|_| crate::runtime::jvmti::any_frame_pop_listener_active());
    let pin_base = thread.native_pin_roots.len();
    if let Some(r) = pin {
        thread.native_pin_roots.push(r);
    }
    while thread.frames.len() > depth_before_push + 1 {
        pop_and_recycle_frame(shared, thread);
    }
    pop_and_recycle_frame(shared, thread);
    if pin.is_some() {
        let moved = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        match result {
            Ok(Some(Value::Object(Some(r)))) => *r = moved,
            Err(MethodCallFailed::ExceptionThrown(e)) => *e = moved,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch loop: debugger breakpoints (wave 3, lane L1)
// ---------------------------------------------------------------------------

/// Is a JDWP breakpoint or step request, or a debugger suspension, in force
/// in this VM? The dispatch loop reads it at entry, at every frame switch and
/// at every backward branch (`refresh_debugger_gate!`), never per bytecode.
///
/// `shared.debug.breakpoints_active` is kept current by the JDWP server after
/// every command (`debug::publish_debugger_gates`). A build without the JDWP
/// surface answers the constant `false`, so the loop's gate folds away. The
/// per-method half is [`debugger_gate_for_frame`] (wave 7).
///
/// Wave 10: the VM's JVMTI breakpoints (`jvmti::set_breakpoint_for_vm`) and
/// its JDWP field watches and exception requests raise it too; the per-method
/// gate decides which methods run the suspend point. The Rust-side
/// `JvmtiEnv::set_breakpoint` table is still not consulted (it belongs to no
/// VM); native agents set theirs through the C table (`jvmti::native_env`,
/// wave 12). See
/// docs/feature-designs/jvmti-delivery-threading.md.
#[cfg(feature = "experimental-debug")]
#[inline(always)]
pub(in crate::runtime::interpreter) fn breakpoints_armed_now(shared: &SharedVm) -> bool {
    shared
        .debug
        .breakpoints_active
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// [`breakpoints_armed_now`] in a build without the JDWP surface.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(in crate::runtime::interpreter) fn breakpoints_armed_now(_shared: &SharedVm) -> bool {
    false
}

/// While [`breakpoints_armed_now`] is set: do the JDWP requests in force
/// concern `frame`'s method (`debug::DebuggerGates::concerns_method`: a
/// breakpoint concerns its own method, a step request or a suspension every
/// method)? The dispatch loop's per-method debugger gate (wave 7).
///
/// `seen` / `concerned` are the loop's cache of the last answer, valid while
/// the summary's generation equals `seen` and the method is the one it was
/// computed for (the loop resets `seen` to `u64::MAX` at every frame switch).
/// A back edge in an unchanged session therefore pays one load.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(in crate::runtime::interpreter) fn debugger_gate_for_frame(
    shared: &SharedVm,
    frame: &Frame,
    seen: &mut u64,
    concerned: &mut bool,
) -> bool {
    let gates = &shared.debug.debugger_gates;
    let generation = gates.generation();
    if generation != *seen {
        *seen = generation;
        *concerned = gates.concerns_method(
            frame.class_id.as_u32(),
            frame.method_name(),
            frame.method_descriptor(),
        );
    }
    *concerned
}

/// [`debugger_gate_for_frame`] in a build without the JDWP surface (never
/// reached: the VM-wide gate is the constant `false`).
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(in crate::runtime::interpreter) fn debugger_gate_for_frame(
    _shared: &SharedVm,
    _frame: &Frame,
    _seen: &mut u64,
    _concerned: &mut bool,
) -> bool {
    false
}

/// `jit_bridge::jvmti_requires_interpreter_for` for a door that has the
/// method's identity but no `CachedBytecodeMethod`: the OSR gate
/// (`try_osr_with_backoff`) and the first-call gate in `execute` (wave 8,
/// lane L1). The JVMTI half stays VM-wide; the JDWP half asks whether the
/// requests in force concern THIS method (a breakpoint in it, or any step
/// request or suspension). Those two doors used the VM-wide answer, so one
/// breakpoint anywhere stopped every OSR transfer and every first-call
/// compile in the VM for the whole session.
#[inline]
pub(in crate::runtime::interpreter) fn jvmti_requires_interpreter_for_method(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity)
        || debugger_concerns_method(shared, class_id, method_name, method_descriptor)
        // JIT round 22 wave 1, lane review: a JNI re-`<init>` witness window
        // (`CRATONVM_JNI_REINIT_WITNESS_INTERPRETS`; one relaxed load).
        || jni_reinit_witness_window_open(shared)
}

/// The JDWP half of [`jvmti_requires_interpreter_for_method`]: the VM-wide
/// gate, then the per-method summary (`debug::DebuggerGates`), which since
/// wave 10 also answers yes for every method while a field watch or an
/// exception request is in force (`requires_interpreter`).
#[cfg(feature = "experimental-debug")]
#[inline(always)]
pub(in crate::runtime::interpreter) fn debugger_concerns_method(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    breakpoints_armed_now(shared)
        && shared.debug.debugger_gates.requires_interpreter(
            class_id.as_u32(),
            method_name,
            method_descriptor,
        )
}

/// [`debugger_concerns_method`] in a build without the JDWP surface.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(in crate::runtime::interpreter) fn debugger_concerns_method(
    _shared: &SharedVm,
    _class_id: ClassId,
    _method_name: &str,
    _method_descriptor: &str,
) -> bool {
    false
}

/// The JDWP suspend point of the dispatch loop. Called only while
/// [`breakpoints_armed_now`] is set, before the bytecode at `pc` of frame
/// `frame_idx` (the top frame) runs.
///
/// 1. **Breakpoints and single steps.** Every `Breakpoint` and (wave 5)
///    `SingleStep` request that reports at this location
///    (`EventManager::match_location_events`, which applies the `Count`,
///    `ThreadOnly`, class-pattern and step depth / size modifiers) queues an
///    event on the JDWP server's channel, exactly as
///    `debug::send_thread_event` queues thread events. Each suspend policy is
///    applied first — `SUSPEND_EVENT_THREAD` counts one suspension of this
///    thread, `SUSPEND_ALL` one of the VM — so a debugger that reacts to the
///    event already sees the thread suspended.
///    A step is never reported at the location it was requested from: the
///    request is made while the thread is parked HERE, and the check for
///    this pc has already run.
/// 2. **Suspension.** If this thread is suspended (by that policy, by
///    `VirtualMachine.Suspend`, or by `ThreadReference.Suspend`) its frames
///    and locals are published to the JDWP state (`ThreadReference.Frames`,
///    `StackFrame.GetValues` read them) and the thread parks in
///    `park_for_debugger` until resumed. Until wave 4 the event was
///    reported and the thread ran on, so `jdb` could never inspect a stop.
/// 3. **Method entries and exits** (interpreter round i1 wave 24): at
///    bytecode 0 of a frame pushed while a `MethodEntry` request is in force
///    (`debug::take_frame_entry`), and before a return bytecode, the
///    `MethodEntry` / `MethodExit(WithReturnValue)` requests report, in one
///    event set with the breakpoints and steps here.
/// 4. **`ThreadReference.Stop`** (wave 24): the answer is the throwable the
///    debugger left for this thread (`debug::take_pending_stop`), which the
///    dispatch loop throws at `pc` — before the bytecode, or after the park
///    when the stop was made while the thread was suspended here.
///
/// Never on the JDWP server thread, nor on a parked thread while it runs heap
/// work for the debugger (`debug::debugger_hooks_suppressed`, wave 8). Inside
/// a debugger invocation it runs (wave 9): a stop there is reported, and the
/// thread parks again one level deeper, above the invocation's frames
/// (`park_for_debugger` nests), as HotSpot stops a thread inside a method the
/// debugger invoked.
///
/// Returning resumes at `pc`: the bytecode the breakpoint sits on executes
/// once, after the stop. Nothing the park does touches the executing frame's
/// code or position: a safepoint pushes no frame, and a debugger invocation
/// run from the park (wave 8) pushes its frames ABOVE this one and pops them
/// before the park returns, as a native callback's would — frames have stable
/// addresses (`FrameStack`) and the bytecode is the frame's own `Arc`, so the
/// caller's already-decoded `code_ptr` stays valid. `StackFrame.SetValues`
/// (wave 9) may rewrite this frame's locals while it is parked; the loop reads
/// locals from the frame, never from a copy held across this call.
///
/// The depth a step measures this frame at is [`debugger_step_depth`]
/// (interpreter round i7 wave 138, lane L1): the lambda proxy rows the
/// listing places below it count, as the step's starting depth counts them.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn deliver_breakpoint_if_set(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    loop_base: usize,
    pc: usize,
    step_frame_control: bool,
) -> Option<DebuggerStop> {
    #[cfg(feature = "experimental-debug")]
    {
        use crate::debug::events::{EventKind, EventLocation};
        // Interpreter round i2 wave 62, lane L1: a C agent's `SingleStep`
        // callback at this pc asked for a `PopFrame` or a forced return of
        // this frame (`DebuggerGates::c_callback_frame_control`): taken now,
        // before anything else is reported at a bytecode that will not run.
        if step_frame_control {
            // Interpreter round i6 wave 134, lane L1 (i133-L1): a `PopFrame`
            // recorded while the thread was blocked entering a monitor was
            // checked with no loop known; this loop's entry frame cannot be
            // popped here (its caller runs in another loop), so it is dropped.
            if frame_idx <= loop_base && shared.config.jvmti_blocked_entry_frame_control {
                crate::debug::pop_frames::drop_jvmti_pop_at(
                    shared,
                    thread.thread_id.0,
                    thread.frames.len(),
                );
            }
            if let Some(stop) = take_callback_frame_control(shared, thread, frame_idx, pc) {
                return Some(stop);
            }
        }
        // Work the debugger runs on this parked thread (or anything on the
        // server thread): stopping — or reporting a stop — there would wait
        // on the server, which is waiting for that work.
        if crate::debug::debugger_hooks_suppressed() {
            return None;
        }
        let Some(frame) = thread.frames.get(frame_idx) else {
            return None;
        };
        let class_id = u64::from(frame.class_id.as_u32());
        // The null id for an obsolete method (wave 19, lane L3): no
        // breakpoint set on the current method stops in its replaced body.
        let method_id = crate::debug::frame_method_id(frame);
        let offset = pc as u64; // Widening: bytecode index
        let tid = thread.thread_id.0;
        // JVMTI breakpoints (wave 10, `jvmti::set_breakpoint_for_vm`): the
        // agent's `Breakpoint` callback, on this thread, before the bytecode
        // runs and before any JDWP stop here. One load when no VM has one.
        if crate::runtime::jvmti::breakpoint_at_for_vm(
            shared.vm_identity,
            class_id,
            method_id,
            offset,
        ) {
            let jvmti_method = jvmti_method_id(shared.vm_identity, frame);
            // Interpreter round i2 wave 62, lane L1: a `PopFrame` or a
            // `ForceEarlyReturn*` the callback makes of this frame
            // (`DebuggerGates::c_callback_frame_control`) is recorded at a
            // callback point and taken here, before the bytecode runs.
            let point = enter_callback_point(shared, thread, loop_base);
            // gci i11/ja: delivered under this thread's own handle.
            post_on_own_thread(thread, || {
                // Cast: a bytecode index is a JVMTI `jlocation`.
                crate::runtime::jvmti::fire_breakpoint_for_vm(
                    shared.vm_identity,
                    tid,
                    jvmti_method,
                    pc as i64,
                )
            });
            if point && leave_callback_point(shared, thread) {
                if let Some(stop) = take_callback_frame_control(shared, thread, frame_idx, pc) {
                    return Some(stop);
                }
            }
        }
        // Wave 13: re-borrowed. An agent's callback (a C agent's, since wave
        // 12) may run Java on this thread through JNI, whose frames push onto
        // this thread's frame vector: the borrow taken above may point into
        // the vector's old buffer once it grew.
        let Some(frame) = thread.frames.get(frame_idx) else {
            return None;
        };
        // What the requests ask of this point, in one short look BEFORE the
        // class manager is read (line information, `ClassOnly` answers and a
        // return value's tag need it, and `debug::populate_class_metadata`
        // nests those two locks the other way round): a step (line
        // information), `ClassOnly` filters, method entries and exits
        // (interpreter round i1 wave 24), and a `ThreadReference.Stop` left
        // for this thread (wave 24).
        let (stepping, filter_classes, (entries, exits), stop_pending, instance_ids) = {
            let ds = shared.debug.debug_state.lock();
            (
                ds.events.has_single_steps(),
                ds.events.location_filter_class_ids(),
                ds.events.method_events_requested(),
                ds.has_pending_stop(tid),
                ds.events.location_instance_filter_ids(),
            )
        };
        // `InstanceOnly` (interpreter round i4 wave 85, lane L1): the frame's
        // active `this`, read only while such a filter is in force.
        let frame_this = if instance_ids.is_empty() {
            None
        } else {
            debugger_frame_this(shared, frame)
        };
        // A stop is thrown here, at the bytecode about to run, as HotSpot
        // throws an asynchronous exception at the thread's current bytecode;
        // nothing else is reported at a bytecode that does not run.
        if stop_pending {
            if let Some(exc) = crate::debug::take_pending_stop(shared, tid) {
                return Some(DebuggerStop::Throw(exc));
            }
        }
        let (line_start, line) = if stepping {
            debugger_line_info(shared, frame, pc)
        } else {
            (None, None)
        };
        // Wave 24: bytecode 0 of a frame pushed while a `MethodEntry` request
        // is in force is its method's entry (a backward branch to 0 is not);
        // a return bytecode is a method's normal exit, reported before it
        // runs, with the value it returns on the operand stack.
        let entry = entries && pc == 0 && crate::debug::take_frame_entry(frame_idx + 1);
        let opcode = frame.code.get(pc).copied();
        let exit = exits && matches!(opcode, Some(0xac..=0xb1));
        let location_supers = if filter_classes.is_empty() {
            Vec::new()
        } else {
            let cm = shared.classes.class_manager.read();
            debugger_supertypes_among(&cm, frame.class_id, &filter_classes)
        };
        let location = EventLocation {
            class_id,
            class_name: frame.class_name(),
            method_id,
            offset,
            thread_id: tid,
            frame_depth: if stepping {
                debugger_step_depth(shared, thread)
            } else {
                thread.frames.len()
            },
            line_start,
            line,
        };
        let (events, park) = {
            let mut ds = shared.debug.debug_state.lock();
            let class_is = |id: u64| location_supers.contains(&id);
            let instance_hits = debugger_instance_hits(shared, &ds, &instance_ids, frame_this);
            let instance_is = |id: u64| frame_this.is_none() || instance_hits.contains(&id);
            let mut events = Vec::new();
            // The order HotSpot's back end reports co-located events in: a
            // method entry first (wave 24,
            // `tools/probes/interp/L1/L1W24JdiSurface.java`). A method exit
            // is NOT one of them (wave 25): HotSpot posts it when the frame
            // returns, after the events at the return bytecode's location
            // were reported and resumed, so it is its own event set — a
            // one-bytecode method (`return` at 0) reported
            // `[MethodEntry, Breakpoint, MethodExit]` as one set here, where
            // HotSpot reports `[MethodEntry, Breakpoint]` and the exit after
            // the resume ([`deliver_method_exit_events`]).
            // Interpreter round i4 wave 85 (lane L1): a step before the
            // breakpoints at its location, each kind's newest request first
            // (`EventManager::match_location_events_with_instance`) --
            // MEASURED on HotSpot 25.0.3 (`L1W85EventSetOrder`: `[MethodEntry,
            // Breakpoint bp2, Breakpoint bp1]` and `[Step, Breakpoint]`):
            // JVMTI posts the single step before the `_breakpoint` bytecode
            // posts the breakpoint, and the back end's `deferEventReport`
            // holds the entry and the step in the event bag until the
            // breakpoint reports. It was `[MethodEntry, Breakpoint, Step]`.
            let kinds = [
                (entry, EventKind::MethodEntry),
                (true, EventKind::SingleStep),
                (true, EventKind::Breakpoint),
            ];
            for (wanted, kind) in kinds {
                if !wanted {
                    continue;
                }
                let matched = ds.events.match_location_events_with_instance(
                    kind,
                    &location,
                    &class_is,
                    &instance_is,
                );
                for (request_id, suspend_policy) in matched {
                    events.push(crate::debug::DebugEvent {
                        kind,
                        request_id,
                        suspend_policy,
                        thread_id: tid,
                        class_id,
                        method_id,
                        offset,
                        extra: Vec::new(),
                        set_follows: false,
                    });
                }
            }
            let park = apply_debugger_event_policies(shared, thread, &mut ds, tid, pc, &events);
            (events, park)
        };
        send_debugger_events(shared, events);
        if park {
            // A park at a bytecode about to run: the one suspend point a
            // `ThreadReference.ForceEarlyReturn` is served at (interpreter
            // round i1 wave 43, lane L1; `debug::early_return`).
            let depth = thread.frames.len();
            crate::debug::early_return::enter_point(shared, tid, depth, loop_base);
            park_for_debugger(shared, thread, tid, pc);
            crate::debug::early_return::leave_point(shared, tid);
            // Interpreter round i7 wave 139, lane L1 (`VmConfig::jdwp_lambda_target_pop`):
            // a `PopFrames` of the lambda door's entry frame (this loop's)
            // served while parked here: the proxy's re-run invoke calls the
            // target again, so the door's method is pushed again, as this
            // loop's entry frame, with the popped frame's arguments, and the
            // loop runs it from its first bytecode.
            if let Some((cached, args)) = crate::debug::pop_frames::take_door_restart(thread) {
                install_cached_frame_nested(shared, thread, cached, &args, None, Some("door-restart"), false);
                return Some(DebuggerStop::FramesPopped);
            }
            // A `PopFrames` served while parked here (interpreter round i1
            // wave 44, lane L1; `debug::pop_frames`): the frame this park
            // stopped is gone, and the loop re-runs its caller's invoke. A
            // `ThreadReference.Stop` made meanwhile stays pending, for the
            // suspend point at that invoke.
            if thread.frames.len() < depth {
                return Some(DebuggerStop::FramesPopped);
            }
            // A C agent's `PopFrame` made while parked here, taken at the
            // resume as HotSpot takes it (interpreter round i2 wave 61, lane
            // L1; `debug::pop_frames::take_jvmti_pop`).
            if crate::debug::pop_frames::take_jvmti_pop(shared, thread, tid, depth) {
                return Some(DebuggerStop::FramesPopped);
            }
            // A `ThreadReference.Stop` made while the thread was suspended
            // here (wave 24, jdb's `kill`): thrown at this bytecode now (the
            // method then exits by the exception: no exit is reported).
            if let Some(exc) = crate::debug::take_pending_stop(shared, tid) {
                return Some(DebuggerStop::Throw(exc));
            }
            if let Some(forced) = crate::debug::early_return::take(shared, tid, depth) {
                return force_return_here(
                    shared,
                    thread,
                    frame_idx,
                    pc,
                    tid,
                    forced,
                    &location_supers,
                );
            }
        }
        if exit {
            let returned = deliver_method_exit_events(
                shared,
                thread,
                frame_idx,
                pc,
                tid,
                opcode,
                &location_supers,
            );
            return returned.map(DebuggerStop::Throw);
        }
        None
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = (shared, thread, frame_idx, loop_base, pc, step_frame_control);
        None
    }
}

/// The depth a JDWP step measures `thread`'s top interpreter frame at
/// (interpreter round i7 wave 138, lane L1; `VmConfig::jdwp_lambda_proxy_steps`,
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// its interpreter frame count plus the lambda proxy method rows the
/// debugger's listing places below it (`jvmti::native_env::proxy_rows_below`).
/// A step's starting depth counts every listed row that is no compiled
/// activation (`commands::handle_er_set`), the proxy's row among them (since
/// the i54-L1 retirement), so a step made in a lambda's target started one
/// deeper than this suspend point measured the target: a `STEP_OUT` completed
/// at the target's next line, a `STEP_OVER` in a callee's first line. Off
/// (`=0`): the frame count alone, as before. One test while the thread is in
/// no JNI native and no proxy method.
#[cfg(feature = "experimental-debug")]
pub(crate) fn debugger_step_depth(shared: &SharedVm, thread: &JvmThread) -> usize {
    let depth = thread.frames.len();
    if thread.jni_native_frames.is_empty() {
        return depth;
    }
    depth + listed_proxy_rows_below(shared, thread, depth)
}

/// The lambda proxy rows `thread`'s listing places below its interpreter
/// frame count `depth` ([`debugger_step_depth`]): those whose listing facts
/// were noted (`DebugRealm::lambda_proxy_rows`; a row without them is left
/// out of the listing, [`listing_splices_of`]), with the one-frame listing
/// on. The facts' lock only while such a row stands.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn listed_proxy_rows_below(shared: &SharedVm, thread: &JvmThread, depth: usize) -> usize {
    if !shared.debug.debugger_gates.one_frame_listing() {
        return 0;
    }
    let rows = &thread.jni_native_frames;
    if crate::jvmti::native_env::proxy_rows_below(rows, depth, |_, _| true) == 0 {
        return 0;
    }
    let facts = shared.debug.lambda_proxy_rows.lock();
    crate::jvmti::native_env::proxy_rows_below(rows, depth, |class_id, index| {
        facts.contains_key(&(class_id, index))
    })
}

/// The `MethodExit` / `MethodExitWithReturnValue` events of frame
/// `frame_idx`'s return bytecode at `pc` (interpreter round i1 wave 25): its
/// own event set, matched after the events at that location
/// ([`deliver_breakpoint_if_set`]) were reported and, when they suspended
/// the thread, resumed — the requests are read again (the debugger may have
/// deleted the exit request meanwhile, as `L1W24JdiSurface` does) and the
/// returned value is read now, not before the park, where a collection may
/// have moved it. `location_supers` are the `ClassOnly` answers the caller
/// computed for this location. Answers a `ThreadReference.Stop` made while
/// the thread was suspended here, to be thrown at this bytecode.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn deliver_method_exit_events(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    pc: usize,
    tid: u64,
    return_opcode: Option<u8>,
    location_supers: &[u64],
) -> Option<ObjectRef> {
    use crate::debug::events::{EventKind, EventLocation};
    let frame = thread.frames.get(frame_idx)?;
    let returned = debugger_return_value(shared, frame, return_opcode);
    // `InstanceOnly` (interpreter round i4 wave 85, lane L1), read now, after
    // any park, as the return value is.
    let instance_ids = shared
        .debug
        .debug_state
        .lock()
        .events
        .location_instance_filter_ids();
    let frame_this = if instance_ids.is_empty() {
        None
    } else {
        debugger_frame_this(shared, frame)
    };
    let class_id = u64::from(frame.class_id.as_u32());
    let method_id = crate::debug::frame_method_id(frame);
    let offset = pc as u64; // Widening: bytecode index
    let location = EventLocation {
        class_id,
        class_name: frame.class_name(),
        method_id,
        offset,
        thread_id: tid,
        frame_depth: thread.frames.len(),
        line_start: None,
        line: None,
    };
    let (events, park) = {
        let mut ds = shared.debug.debug_state.lock();
        let class_is = |id: u64| location_supers.contains(&id);
        let instance_hits = debugger_instance_hits(shared, &ds, &instance_ids, frame_this);
        let instance_is = |id: u64| frame_this.is_none() || instance_hits.contains(&id);
        let mut events = Vec::new();
        for kind in [EventKind::MethodExit, EventKind::MethodExitWithReturnValue] {
            let matched = ds.events.match_location_events_with_instance(
                kind,
                &location,
                &class_is,
                &instance_is,
            );
            for (request_id, suspend_policy) in matched {
                // `METHOD_EXIT_WITH_RETURN_VALUE` carries the value after its
                // location, tagged with the method's return type (`V` for a
                // `void` method).
                let extra = if kind == EventKind::MethodExitWithReturnValue {
                    let mut pw = crate::debug::protocol::PayloadWriter::new();
                    match returned {
                        Some((desc, value, tag)) => {
                            crate::debug::inspect::put_tagged_with_tag(
                                shared, &mut ds, &mut pw, desc, value, tag,
                            );
                        }
                        None => pw.put_u8(b'V'),
                    }
                    pw.into_bytes()
                } else {
                    Vec::new()
                };
                events.push(crate::debug::DebugEvent {
                    kind,
                    request_id,
                    suspend_policy,
                    thread_id: tid,
                    class_id,
                    method_id,
                    offset,
                    extra,
                    set_follows: false,
                });
            }
        }
        // Interpreter round i7 wave 139, lane L1 (`CRATONVM_JDWP_EVENT_OBJECT_HOLD`,
        // i127-L1's "not covered" part): a returned object a
        // `MethodExitWithReturnValue` named lives until the debugger's first
        // look at it.
        if events
            .iter()
            .any(|e| e.kind == EventKind::MethodExitWithReturnValue)
        {
            if let Some((_, Value::Object(Some(o)), _)) = returned {
                crate::debug::hold_event_object(shared, &mut ds, o);
            }
        }
        let park = apply_debugger_event_policies(shared, thread, &mut ds, tid, pc, &events);
        (events, park)
    };
    send_debugger_events(shared, events);
    if park {
        park_for_debugger(shared, thread, tid, pc);
        return crate::debug::take_pending_stop(shared, tid);
    }
    None
}

/// The value a return bytecode (`opcode`) of `frame` is about to return, as a
/// JDWP `METHOD_EXIT_WITH_RETURN_VALUE` event reports it (interpreter round
/// i1 wave 24): `(signature byte of the method's return type, value, the
/// object's JDWP tag)`, read in place from the top of the operand stack and
/// tagged with the class manager (before the debug-state lock). `None` for
/// `return` (a `void` method: the event carries `V`).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn debugger_return_value(
    shared: &SharedVm,
    frame: &Frame,
    opcode: Option<u8>,
) -> Option<(u8, Value, Option<u8>)> {
    if opcode == Some(0xb1) || frame.stack.is_empty() {
        return None;
    }
    let desc = frame
        .method_descriptor()
        .rsplit(')')
        .next()?
        .as_bytes()
        .first()
        .copied()?;
    if desc == b'V' {
        return None;
    }
    let (cv, kind) = frame.stack.peek_with_kind_at(0);
    let value = watch_operand_value(shared, cv, kind, desc);
    let tag = match value {
        Value::Object(Some(o)) => {
            let cm = shared.classes.class_manager.read();
            Some(crate::debug::inspect::object_tag(shared, &cm, o))
        }
        _ => None,
    };
    Some((desc, value, tag))
}

/// Apply the suspend policy of the event set `events` — its strongest one,
/// once: `SUSPEND_EVENT_THREAD` counts one suspension of thread `tid`,
/// `SUSPEND_ALL` one of the VM (interpreter round i1 wave 22, lane L1: every
/// event counted its own, so two requests at one breakpoint, or a step landing
/// on a breakpoint, left the thread suspended after the debugger resumed the
/// set; HotSpot suspends once per composite) — so a debugger that reacts to
/// the set already sees the suspension; then, if `tid` is suspended, publish
/// its frames (the top one at `top_pc`).
/// Answers whether the thread must park ([`park_for_debugger`]). Called with
/// the debug-state lock held, by every interpreter hook that reports JDWP
/// events at a location (breakpoints and steps, and since wave 10 exceptions
/// and field watches), before the events are sent ([`send_debugger_events`]).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn apply_debugger_event_policies(
    shared: &SharedVm,
    thread: &JvmThread,
    ds: &mut crate::debug::DebugState,
    tid: u64,
    top_pc: usize,
    events: &[crate::debug::DebugEvent],
) -> bool {
    let policy = crate::debug::strongest_suspend_policy(events.iter().map(|e| e.suspend_policy));
    let suspended_now = ds.apply_event_set_policy(policy, tid);
    // Interpreter round i1 wave 46 (lane L1): a request whose `Count` this
    // match spent stops arming the gates (`events::SPENT_COUNT_RELEASES_GATES`).
    let spent = crate::debug::take_spent_requests(ds);
    // A suspension concerns every method (wave 7): republish, so the other
    // threads' loops run their suspend point at their next frame switch or
    // back edge, and the JIT's doors stand down.
    if suspended_now || spent {
        crate::debug::publish_debugger_gates(shared, ds);
    }
    let park = ds.is_thread_suspended(tid);
    if park {
        publish_debugger_frames(shared, thread, ds, tid, top_pc);
    }
    park
}

/// Queue `events`, one event set, on the JDWP server's channel: sent as one
/// composite packet (wave 22, `debug::send_event_set`; dropped when no
/// debugger is attached). Called without the debug-state lock.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn send_debugger_events(shared: &SharedVm, events: Vec<crate::debug::DebugEvent>) {
    crate::debug::send_event_set(shared, events);
}

// ---------------------------------------------------------------------------
// JDWP field watches and exception events (interpreter round i1 wave 10)
// ---------------------------------------------------------------------------
//
// Until wave 10 `EventRequest.Set` accepted `FieldAccess`, `FieldModification`
// and `Exception` requests and nothing ever reported one: the field bytecodes
// consulted only the JVMTI watch table, and the unwinder only JVMTI. See
// docs/feature-designs/jvmti-delivery-threading.md.
//
// Both hooks cost one load of a per-VM flag when nothing is armed
// (`DebuggerGates::field_watch_armed` behind the process-wide
// `jvmti::any_field_watchpoint_active` the field bytecodes already test;
// `DebuggerGates::exception_events_armed` in the unwinder). While either
// kind is armed every method runs interpreted (`requires_interpreter`), since
// compiled code posts neither event.

/// The JDWP half of the four field bytecodes (wave 10): report a
/// `FieldAccess` (`modification == false`) or `FieldModification` event for
/// `field` at the executing bytecode (`last_instr_pc` of frame `frame_idx`)
/// when a watch request matches, and park if its policy suspends the thread.
///
/// Called behind `jvmti::any_field_watchpoint_active()` — which a JDWP watch
/// raises too, turning every quickened field arm off — after resolution
/// (and, for a static, class initialization), BEFORE the bytecode pops its
/// operands: the receiver (`getfield` top, `putfield` under the value) and
/// the new value (top) are read in place, so they stay GC roots across the
/// park, and a debugger stopped at a modification sees the old value in the
/// field, as in HotSpot. A null receiver reports nothing (the bytecode
/// throws `NullPointerException`).
#[inline]
pub(in crate::runtime) fn deliver_field_watch_if_armed(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    field: &ResolvedField,
    modification: bool,
) {
    #[cfg(feature = "experimental-debug")]
    {
        if shared.debug.debugger_gates.field_watch_armed() {
            deliver_field_watch(shared, thread, frame_idx, field, modification);
            // Interpreter round i2 wave 63 (lane L1): a C JVMTI env's watch.
            if shared
                .debug
                .debugger_gates
                .native_event_armed(crate::debug::NativeEvent::FieldWatch)
            {
                post_c_field_watch(shared, thread, frame_idx, field, modification);
            }
        }
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = (shared, thread, frame_idx, field, modification);
    }
}

/// A C JVMTI env's `FieldAccess` / `FieldModification` of `field`
/// (interpreter round i2 wave 63, lane L1): the operands read where they
/// are, as [`deliver_field_watch`] reads them, then
/// `jvmti::native_env::post_field_watch`, which asks the VM's watch set.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn post_c_field_watch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    field: &ResolvedField,
    modification: bool,
) {
    let Some(frame) = thread.frames.get(frame_idx) else {
        return;
    };
    let stack = &frame.stack;
    let object = if field.is_static {
        None
    } else {
        let depth = usize::from(modification);
        if stack.len() <= depth {
            return;
        }
        let (cv, k) = stack.peek_with_kind_at(depth);
        match ref_operand_value(shared, cv, k) {
            Value::Object(Some(o)) => Some(o),
            _ => return,
        }
    };
    let value = if modification {
        if stack.is_empty() {
            return;
        }
        let (cv, k) = stack.peek_with_kind_at(0);
        Some(watch_operand_value(shared, cv, k, field.desc_byte))
    } else {
        None
    };
    let event = crate::jvmti::native_env::FieldEvent {
        declaring: field.declaring_class_id,
        field_index: field.field_index,
        is_static: field.is_static,
        desc: field.desc_byte,
        method: jvmti_method_id(shared.vm_identity, frame),
        location: frame.last_instr_pc as i64, // Cast: a bytecode index
        object,
        value,
    };
    crate::jvmti::native_env::post_field_watch(shared, thread.thread_id.0, &event);
}

/// The position in `class.fields` of the field a [`ResolvedField`] names:
/// its `field_index` counts a static field among the class's statics and an
/// instance field from `first_field_index` among its instance fields
/// (`Class::find_own_field`). `None` for an index the class does not declare.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn declared_field_position(
    class: &crate::classloading::Class,
    is_static: bool,
    field_index: usize,
) -> Option<usize> {
    let mut remaining = if is_static {
        field_index
    } else {
        field_index.checked_sub(class.first_field_index)?
    };
    for (position, f) in class.fields.iter().enumerate() {
        if f.is_static() != is_static {
            continue;
        }
        if remaining == 0 {
            return Some(position);
        }
        remaining -= 1;
    }
    None
}

/// A field bytecode's value operand, read in place, as the value a
/// `FieldModification` event reports for a field of descriptor type `desc`.
/// A `long` / `double` is decoded by the field's type, whatever mark the
/// slot carries (the store itself decodes it the same way).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn watch_operand_value(shared: &SharedVm, cv: CompactValue, kind: u8, desc: u8) -> Value {
    match desc {
        b'L' | b'[' => ref_operand_value(shared, cv, kind),
        b'J' => match stack_slot_value(cv, kind) {
            // Cast: the slot's raw bits are the long.
            Value::Double(d) => Value::Long(d.to_bits() as i64),
            other => other,
        },
        b'D' => match stack_slot_value(cv, kind) {
            // Cast: the slot's raw bits are the double.
            Value::Long(l) => Value::Double(f64::from_bits(l as u64)),
            other => other,
        },
        _ => stack_slot_value(cv, kind),
    }
}

/// The active `this` of `frame` for a JDWP `InstanceOnly` filter
/// (interpreter round i4 wave 85, lane L1): its local 0 when its method is
/// an instance method and the slot holds an object, as the JDK back end's
/// `eventInstance` reads it (`GetLocalObject(thread, 0, 0)` of a non-static
/// method). `None` -- the back end's NULL, which passes every instance
/// filter -- for a static method, a method the class manager cannot find, or
/// a slot 0 that holds no object. Reads the class manager: call it before
/// the debug-state lock.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn debugger_frame_this(shared: &SharedVm, frame: &crate::runtime::frame::Frame) -> Option<ObjectRef> {
    let is_static = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(frame.class_id)?
            .find_method(frame.method_name(), frame.method_descriptor())?
            .is_static()
    };
    if is_static || frame.locals_len() == 0 {
        return None;
    }
    match crate::debug::debugger_local(frame, 0) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// The ids among `instance_ids` (the requests' `InstanceOnly` object ids)
/// that name `frame_this` (interpreter round i4 wave 85, lane L1); empty
/// when there is no `this`.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn debugger_instance_hits(
    shared: &SharedVm,
    ds: &crate::debug::DebugState,
    instance_ids: &[u64],
    frame_this: Option<ObjectRef>,
) -> Vec<u64> {
    let Some(this) = frame_this else {
        return Vec::new();
    };
    instance_ids
        .iter()
        .copied()
        .filter(|&id| crate::debug::object_for_id(shared, ds, id) == Some(this))
        .collect()
}

/// The ids in `filter_classes` that name `class` or one of its supertypes:
/// the `ClassOnly` / `ExceptionOnly` answers an `EventSubject` gives.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn debugger_supertypes_among(
    cm: &crate::classloading::ClassManager,
    class: ClassId,
    filter_classes: &[u64],
) -> Vec<u64> {
    filter_classes
        .iter()
        .copied()
        .filter(|&id| {
            u32::try_from(id).is_ok_and(|raw| cm.is_subclass_of(class, ClassId::new(raw)))
        })
        .collect()
}

/// [`deliver_field_watch_if_armed`] once a watch is armed in this VM.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn deliver_field_watch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    field: &ResolvedField,
    modification: bool,
) {
    use crate::debug::events::{EventKind, EventLocation, EventSubject};
    if crate::debug::debugger_hooks_suppressed() {
        return;
    }
    let kind = if modification {
        EventKind::FieldModification
    } else {
        EventKind::FieldAccess
    };
    let tid = thread.thread_id.0;
    let Some(frame) = thread.frames.get(frame_idx) else {
        return;
    };
    let pc = frame.last_instr_pc;
    // The operands, read where they are (see above). Nothing below reaches
    // a safepoint before they are exported; the park is the first one.
    let stack = &frame.stack;
    let object = if field.is_static {
        None
    } else {
        let depth = usize::from(modification);
        if stack.len() <= depth {
            return;
        }
        let (cv, k) = stack.peek_with_kind_at(depth);
        match ref_operand_value(shared, cv, k) {
            Value::Object(Some(o)) => Some(o),
            _ => return,
        }
    };
    let value = if modification {
        if stack.is_empty() {
            return;
        }
        let (cv, k) = stack.peek_with_kind_at(0);
        Some(watch_operand_value(shared, cv, k, field.desc_byte))
    } else {
        None
    };
    let (filter_classes, instance_ids) = {
        let ds = shared.debug.debug_state.lock();
        if !ds.events.has_requests(kind) {
            return;
        }
        (
            ds.events.filter_class_ids(kind),
            ds.events.instance_filter_ids(kind),
        )
    };
    // What needs the class manager, before the debug-state lock (the lock
    // order `debug::populate_class_metadata` fixes).
    let (field_id, declaring_tag, location_supers, object_tag, value_tag) = {
        let cm = shared.classes.class_manager.read();
        let Some(declaring) = cm.class_store.get(field.declaring_class_id) else {
            return;
        };
        let Some(position) = declared_field_position(declaring, field.is_static, field.field_index)
        else {
            return;
        };
        let tag_of = |o: ObjectRef| crate::debug::inspect::object_tag(shared, &cm, o);
        (
            crate::debug::jdwp_field_id(field.declaring_class_id, position),
            if declaring.is_interface() { 2u8 } else { 1u8 },
            debugger_supertypes_among(&cm, frame.class_id, &filter_classes),
            object.map(tag_of),
            match value {
                Some(Value::Object(Some(o))) => Some(tag_of(o)),
                _ => None,
            },
        )
    };
    let location = EventLocation {
        class_id: u64::from(frame.class_id.as_u32()),
        class_name: frame.class_name(),
        method_id: crate::debug::frame_method_id(frame),
        offset: pc as u64, // Widening: bytecode index
        thread_id: tid,
        frame_depth: thread.frames.len(),
        line_start: None,
        line: None,
    };
    let declaring_id = u64::from(field.declaring_class_id.as_u32());
    let (events, park) = {
        let mut ds = shared.debug.debug_state.lock();
        let instance_hits: Vec<u64> = instance_ids
            .into_iter()
            .filter(|&id| {
                object.is_some_and(|o| crate::debug::object_for_id(shared, &ds, id) == Some(o))
            })
            .collect();
        let subject = EventSubject {
            location_class_is: &|id| location_supers.contains(&id),
            field: Some((declaring_id, field_id)),
            // A static field's event has no object, which passes every
            // `InstanceOnly`, as the back end's NULL `eventInstance` does
            // (interpreter round i4 wave 85, lane L1).
            instance_is: &|id| object.is_none() || instance_hits.contains(&id),
            exception_is: &|_| false,
            caught: false,
        };
        let hits = ds.events.match_subject_events(kind, &location, &subject);
        let mut events = Vec::with_capacity(hits.len());
        for (request_id, suspend_policy) in hits {
            // refTypeTag, typeID, fieldID, object (null for a static), and a
            // modification's valueToBe. Each event exports its own reference
            // to the objects it names.
            let mut extra = crate::debug::protocol::PayloadWriter::new();
            extra.put_u8(declaring_tag);
            extra.put_u64_be(declaring_id);
            extra.put_u64_be(field_id);
            crate::debug::inspect::put_tagged_with_tag(
                shared,
                &mut ds,
                &mut extra,
                b'L',
                Value::Object(object),
                object_tag,
            );
            if let Some(v) = value {
                crate::debug::inspect::put_tagged_with_tag(
                    shared,
                    &mut ds,
                    &mut extra,
                    field.desc_byte,
                    v,
                    value_tag,
                );
            }
            events.push(crate::debug::DebugEvent {
                kind,
                request_id,
                suspend_policy,
                thread_id: tid,
                class_id: location.class_id,
                method_id: location.method_id,
                offset: location.offset,
                extra: extra.into_bytes(),
                set_follows: false,
            });
        }
        // Interpreter round i7 wave 139, lane L1 (`CRATONVM_JDWP_EVENT_OBJECT_HOLD`,
        // the "not covered" part of
        // `interpreter-i127-L1-compiled-exception-event-object-collected-FIXED-20261021.md`): a
        // field event's object and an object value to be written live until
        // the debugger's first look at them, as an `Exception` event's does
        // (a `SUSPEND_NONE` watch otherwise races a collection).
        if !events.is_empty() {
            if let Some(o) = object {
                crate::debug::hold_event_object(shared, &mut ds, o);
            }
            if let Some(Value::Object(Some(v))) = value {
                crate::debug::hold_event_object(shared, &mut ds, v);
            }
        }
        let park = apply_debugger_event_policies(shared, thread, &mut ds, tid, pc, &events);
        (events, park)
    };
    send_debugger_events(shared, events);
    if park {
        park_for_debugger(shared, thread, tid, pc);
    }
}

/// The debugger half of the interpreter's unwinder: post the `Exception`
/// event for `exc`, thrown at `throw_pc` of frame `frame_idx`, to JDWP (wave
/// 10) and JVMTI (wave 11), and park at the throw site — before any frame is
/// popped — if a JDWP request's policy suspends the thread. Answers the
/// exception, re-read after anything that may collect (it is pinned across
/// the JVMTI callbacks and the park).
///
/// An exception is reported once, where the unwinder first sees it: it
/// reaches the unwinder again at every interpreter entry it unwinds into,
/// and those are not throws (`JvmThread::reported_exception`, one per thread
/// for both clients since wave 11; forgotten when a handler catches it,
/// [`note_exception_caught_if_armed`], so a rethrow is reported again, as in
/// HotSpot).
///
/// The catch location is the handler the interpreter frames on the thread
/// would run ([`predicted_catch_frame`]); none (JDWP location 0, JVMTI
/// method 0 / location -1) when none does.
///
/// Unarmed: one load of the process-wide JVMTI `Exception` union and, with
/// `experimental-debug`, one per-VM load of the JDWP gate. The JVMTI event
/// had no producer before wave 11.
#[inline]
pub(in crate::runtime) fn deliver_exception_event_if_armed(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    exc: ObjectRef,
    throw_pc: usize,
) -> ObjectRef {
    // Interpreter round i1 wave 25, lane L1: a `ThreadReference.Stop` left
    // for this thread replaces the exception in flight (the
    // `InterruptedException` of the blocking call the stop woke) before any
    // handler is looked for, as HotSpot's asynchronous exception replaces a
    // pending one. One load while no stop is pending.
    #[cfg(feature = "experimental-debug")]
    let exc = if shared.debug.debugger_gates.stop_pending() {
        crate::debug::exception_or_pending_stop(shared, thread.thread_id.0, exc)
    } else {
        exc
    };
    #[cfg(feature = "experimental-debug")]
    let jdwp = shared.debug.debugger_gates.exception_events_armed();
    #[cfg(not(feature = "experimental-debug"))]
    let jdwp = false;
    // Interpreter round i7 wave 144, lane L1: or `-agentlib:jdwp`'s start is
    // deferred to a matching throw (`onthrow=` / `onuncaught=y`).
    #[cfg(feature = "experimental-debug")]
    let deferred = shared.debug.debugger_gates.jdwp_deferred_start_armed();
    #[cfg(not(feature = "experimental-debug"))]
    let deferred = false;
    if jdwp || deferred || crate::runtime::jvmti::any_exception_listener_active() {
        return deliver_exception_events(shared, thread, frame_idx, exc, throw_pc, jdwp, None);
    }
    exc
}

/// May an `Exception` event client be armed for `shared`: this VM's JDWP gate
/// (`experimental-debug`) or a JVMTI `Exception` listener of this VM?
/// Over-approximates (the JDWP gate outlives its request; the JVMTI event may
/// be enabled for another thread only), never under-approximates; another
/// VM's agent does not count. The compiled local-handler lookup declines while
/// it holds ([`compiled_catch_events_armed`]), so every compiled catch
/// reaches a VM door that posts the event
/// ([`report_exception_caught_by_compiled_door`]). Unarmed: one or two loads.
#[inline]
pub(crate) fn exception_events_may_be_armed(shared: &SharedVm) -> bool {
    #[cfg(feature = "experimental-debug")]
    {
        // Wave 144 (L1): a deferred `-agentlib:jdwp` start watches every
        // throw, a compiled catch's too.
        if shared.debug.debugger_gates.exception_events_armed()
            || shared.debug.debugger_gates.jdwp_deferred_start_armed()
        {
            return true;
        }
    }
    crate::runtime::jvmti::exception_listener_active_for_vm(shared.vm_identity)
}

/// Must a throw that compiled code could catch in its own frame reach a VM
/// door instead, because the catch has events to post: an `Exception` client
/// ([`exception_events_may_be_armed`]) or a JVMTI `ExceptionCatch` listener
/// of this VM? The compiled local-handler lookup declines while it holds
/// (`helpers::jit_local_handler_lookup_body`); the doors post both events
/// ([`report_exception_caught_by_compiled_door`]). Interpreter round i1 wave
/// 21, lane L2. Unarmed: two or three loads.
#[inline]
pub(crate) fn compiled_catch_events_armed(shared: &SharedVm) -> bool {
    exception_events_may_be_armed(shared)
        || crate::runtime::jvmti::exception_catch_listener_active_for_vm(shared.vm_identity)
}

thread_local! {
    /// JIT r24 (CompletableFuture page, item 1): a SPLICED callee's handler
    /// took the pending throwable (`helpers::jit_local_handler_lookup`) while
    /// a catch client may be armed. Such a site must answer
    /// (`JitLocalHandlerSite::SPLICED_SCOPE_BIT`): its handler entry is an
    /// always-failing guard that resumes the interpreter in the callee at the
    /// handler, through an inlined chain. The chain push
    /// (`deopt_resume::push_inlined_chain`) consumes this and posts the
    /// events a door would ([`report_spliced_catch_if_pending`]), which is
    /// what lets a client armed AFTER the compile see that catch.
    pub(in crate::runtime::interpreter) static SPLICED_CATCH_REPORT_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// See [`SPLICED_CATCH_REPORT_PENDING`]: called by the compiled local-handler
/// lookup on a hit at a spliced site, with `armed` =
/// [`compiled_catch_events_armed`]. Unarmed: clears any stale mark.
#[inline]
/// `CRATONVM_JIT_SPLICED_CATCH_REPORT` (default ON, `=0` kill): the
/// positive control, under which such a catch posts nothing, as before.
pub(crate) fn note_spliced_catch_needs_report(armed: bool) {
    let report =
        armed;
    SPLICED_CATCH_REPORT_PENDING.with(|c| c.set(report));
}

/// Consume the mark of [`note_spliced_catch_needs_report`]. Every chain push
/// takes it, reported or not, so a mark whose chain was refused never
/// reaches a later, unrelated resume.
#[inline]
pub(crate) fn take_spliced_catch_report() -> bool {
    SPLICED_CATCH_REPORT_PENDING.with(|c| c.replace(false))
}

/// The chain push's half of [`SPLICED_CATCH_REPORT_PENDING`]: `top` is the
/// innermost frame just pushed. Posts the catch only where the frame really
/// sits at one of its handlers with an object on its stack, the shape the
/// handler trap resumes; anything else is not a spliced catch and posts
/// nothing.
pub(crate) fn report_spliced_catch_if_pending(
    shared: &SharedVm,
    thread: &mut JvmThread,
    top: usize,
) {
    if !take_spliced_catch_report() {
        return;
    }
    let Some(frame) = thread.frames.get(top) else {
        return;
    };
    let handler_pc = frame.pc;
    let at_handler = frame
        .exception_table()
        .iter()
        .any(|row| usize::from(row.handler_pc) == handler_pc);
    if !at_handler || !matches!(frame.stack.peek_checked(), Ok(Value::Object(Some(_)))) {
        return;
    }
    report_exception_caught_by_compiled_door(shared, thread, top, usize::MAX, handler_pc);
}

/// A compiled catch door has resumed frame `frame_idx` — an interpreter frame
/// rebuilt for a method that was running compiled — at its handler
/// `handler_pc`, with the throwable on top of the frame's operand stack.
/// Posts what the unwinder posts for a throw it catches
/// ([`unwind_to_handler`]): `Exception` at the throw site (JDWP and JVMTI,
/// once per throw, park included), then JVMTI `ExceptionCatch` at the
/// handler, then forgets the reported exception so a rethrow is reported
/// again.
///
/// `throw_pc` is the bci compiled code threw at in that method (the precise
/// reason-9 frame's bci or the body's own throw-site stamp); `usize::MAX`
/// when the door does not know it, reported as the start of the protected
/// range whose handler runs — the nearest location in this method the door
/// can vouch for. An exception an interpreted callee threw was reported by
/// that callee's unwinder already and is not reported again (the
/// `reported_exception` rule); one a COMPILED callee threw through a baked
/// call is reported here, at the call site in this method, not at the
/// callee's throw site (see
/// `docs/feature-designs/real-jdk-mode.md`,
/// wave 21).
///
/// The doors (interpreter round i1 wave 21, lane L2):
/// `route_jit_exception_through_method`, `run_jit_callee_handler`
/// (`execute_resumed_handler_frame`), `route_osr_exception_out_of_artifact`
/// and the first-call door's `jit_early_exception` arm of [`execute`].
/// Unarmed: the loads of [`exception_events_may_be_armed`], the JVMTI
/// any-listener union and one thread-local.
#[inline]
pub(crate) fn report_exception_caught_by_compiled_door(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    throw_pc: usize,
    handler_pc: usize,
) {
    if exception_events_may_be_armed(shared) {
        post_exception_at_compiled_catch(shared, thread, frame_idx, throw_pc, handler_pc);
    }
    if frame_idx < thread.frames.len() {
        fire_jvmti_exception_catch(shared.vm_identity, thread, frame_idx, handler_pc);
    }
    note_exception_caught_if_armed(shared, thread);
}

/// [`report_exception_caught_by_compiled_door`]'s `Exception` half, once a
/// client may be listening. The throwable is read from the handler frame's
/// operand stack, which roots it; the value [`deliver_exception_events`]
/// answers is not needed (the frame's slot is the copy a collection updates).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn post_exception_at_compiled_catch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    throw_pc: usize,
    handler_pc: usize,
) {
    let Some(frame) = thread.frames.get(frame_idx) else {
        return;
    };
    let Ok(Value::Object(Some(exc))) = frame.stack.peek_checked() else {
        return;
    };
    let pc = compiled_catch_report_pc(frame, throw_pc, handler_pc);
    #[cfg(feature = "experimental-debug")]
    let jdwp = shared.debug.debugger_gates.exception_events_armed();
    #[cfg(not(feature = "experimental-debug"))]
    let jdwp = false;
    let _ = deliver_exception_events(shared, thread, frame_idx, exc, pc, jdwp, Some(handler_pc));
}

/// The throw location a compiled catch door reports: `throw_pc` when it is an
/// offset inside the method's code, otherwise the `start_pc` of the first
/// exception-table row whose handler is `handler_pc` (and `handler_pc` itself
/// if no row names it).
pub(in crate::runtime::interpreter) fn compiled_catch_report_pc(frame: &Frame, throw_pc: usize, handler_pc: usize) -> usize {
    // `frame.code` carries 2 bytes of speculative-read padding.
    let code_len = frame.code.len().saturating_sub(2);
    if throw_pc < code_len {
        return throw_pc;
    }
    frame
        .exception_table()
        .iter()
        .find(|row| usize::from(row.handler_pc) == handler_pc)
        .map_or(handler_pc, |row| usize::from(row.start_pc))
}

/// The unwinder found a handler for the exception in flight (wave 10): the
/// next time the thread's unwinder sees that object, it is a new throw. One
/// thread-local load (wave 11: the state moved from the JDWP server's table
/// onto the thread, shared with JVMTI).
#[inline]
pub(in crate::runtime) fn note_exception_caught_if_armed(shared: &SharedVm, thread: &mut JvmThread) {
    if thread.reported_exception != 0 {
        forget_reported_exception(shared, thread);
    }
}

/// A thread is leaving the VM (Java thread exit, virtual-thread end, JNI
/// detach): free the weak handle of the exception it last reported, which no
/// handler will now catch (wave 12; the common end of a thread killed by an
/// uncaught exception), and close a debugger inspection window a blocking
/// region left open (a region not left through `end_blocking_region`), so
/// no debugger command reads the `JvmThread` after it is gone. One
/// thread-local load each when there is nothing to do. Runs after the
/// thread's last Java (`Thread.exit()`), which may report again.
#[inline]
pub(crate) fn release_debugger_thread_state(shared: &SharedVm, thread: &mut JvmThread) {
    if thread.reported_exception != 0 {
        forget_reported_exception(shared, thread);
    }
    #[cfg(feature = "experimental-debug")]
    close_blocked_inspection(shared, thread);
    // Wave 41 (lane L1): a `StackFrame.SetValues` left for this thread's
    // wake, which will not come.
    #[cfg(feature = "experimental-debug")]
    crate::debug::inspect::discard_deferred_frame_writes_if_any(shared, thread.thread_id.0);
}

/// [`note_exception_caught_if_armed`] with a reported exception: free its
/// weak handle.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn forget_reported_exception(shared: &SharedVm, thread: &mut JvmThread) {
    let handle = std::mem::take(&mut thread.reported_exception);
    let _ = shared.natives.jni_global_refs.lock().remove(handle);
}

/// Where the handler that catches an exception of class `exc_class`, thrown
/// at `pc` of frame `frame_idx` (that frame's own table is skipped when
/// `search_own` is false), will run: `(frame index, handler pc)`, or `None`
/// when no interpreter frame on the thread catches it (JDWP catch location
/// 0, "uncaught"; JVMTI catch method 0).
///
/// A prediction from the exception tables and the loaded classes only:
/// nothing is loaded or allocated here, while the unwinder may load a catch
/// type, and a catch type that fails to resolve replaces the exception
/// (JVMS §5.4.3) — neither is predicted. A catch type that is not loaded
/// matches by name along the exception's supertypes, the unwinder's own
/// fallback. Native frames are not seen: their handlers are reported as none
/// (and so is an exception a reflective call wraps).
///
/// `compiled`: the thread's compiled activations, outermost first
/// (`conservative_roots::active_compiled_frames`), each with the methods the
/// JIT inlined into it at its current bci. Interpreter round i4 wave 92
/// (lane L1): an activation entered at interpreter depth `d` stands between
/// frames `d - 1` and `d`, so its handlers, and its inlined callees', are
/// asked right after frame `d`'s; an activation whose bci is unknown is
/// passed over, as before. Empty unless the VM serves JVMTI `Exception` from
/// compiled code (`CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`): without it a
/// compiled catch below an interpreted throw is reported as an outer frame's,
/// or as uncaught.
pub(in crate::runtime::interpreter) fn predicted_catch_frame(
    cm: &crate::classloading::ClassManager,
    thread: &JvmThread,
    frame_idx: usize,
    pc: usize,
    search_own: bool,
    exc_class: ClassId,
    compiled: &[crate::jit::conservative_roots::ActiveCompiledFrame],
) -> Option<PredictedCatch> {
    let mut idx = frame_idx;
    let mut at = if search_own { Some(pc) } else { None };
    loop {
        let frame = thread.frames.get(idx)?;
        if let Some(pc) = at {
            let handler =
                catching_handler(cm, frame.class_id, frame.exception_table(), pc, exc_class);
            if let Some(handler_pc) = handler {
                return Some(PredictedCatch::Frame(idx, handler_pc));
            }
        }
        // Widening: a recorded interpreter depth back to `usize`.
        for activation in compiled.iter().rev().filter(|a| a.interp_depth as usize == idx) {
            if let Some(found) = compiled_activation_catch(cm, activation, exc_class) {
                return Some(found);
            }
        }
        idx = idx.checked_sub(1)?;
        at = thread.frames.get(idx).map(|f| f.last_instr_pc);
    }
}

/// Where a predicted catch runs (interpreter round i4 wave 92, lane L1):
/// interpreter frame `.0` at handler `.1`, or a method running compiled (an
/// activation, or a callee inlined into one) at `handler_pc`.
#[derive(Clone, Debug)]
pub(crate) enum PredictedCatch {
    Frame(usize, usize),
    Compiled {
        class_id: ClassId,
        method_name: Arc<str>,
        descriptor: Arc<str>,
        handler_pc: usize,
    },
}

/// The handler of the first row of `table` (a method of class `class_id`)
/// whose range covers `pc` and whose catch type takes `exc_class`: a
/// catch-all, a loaded catch class `exc_class` is a subclass of, or a catch
/// type named along `exc_class`'s supertypes (the unwinder's own fallback).
pub(in crate::runtime::interpreter) fn catching_handler(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
    table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    pc: usize,
    exc_class: ClassId,
) -> Option<usize> {
    table
        .iter()
        .find(|row| {
            if pc < usize::from(row.start_pc) || pc >= usize::from(row.end_pc) {
                return false;
            }
            row.catch_type == 0
                || cm
                    .get_class(class_id)
                    .and_then(|c| c.constant_pool.get_class_name(row.catch_type))
                    .is_some_and(|name| {
                        cm.find_class_by_name_for_class(name, class_id)
                            .is_some_and(|id| cm.is_subclass_of(exc_class, id))
                            || cm.is_subclass_of_by_name(exc_class, name)
                    })
        })
        .map(|row| usize::from(row.handler_pc))
}

/// The catch, if any, of one compiled activation for an exception of class
/// `exc_class` passing through it: its inlined callees first (innermost
/// first, each at its own bci), then the activation's own method at its
/// bci. `None` for an activation whose bci is unknown, or a level whose
/// method the class store does not name. Interpreter round i4 wave 92, lane
/// L1.
pub(in crate::runtime::interpreter) fn compiled_activation_catch(
    cm: &crate::classloading::ClassManager,
    activation: &crate::jit::conservative_roots::ActiveCompiledFrame,
    exc_class: ClassId,
) -> Option<PredictedCatch> {
    let owner = ClassId::new(activation.owner_class_id);
    for level in activation.chain() {
        let class_id = if level.class_id != 0 {
            ClassId::new(level.class_id)
        } else {
            let ((class_name, _), _) = split_compiled_label(&level.label)?;
            cm.find_class_by_name_for_class(class_name, owner)?
        };
        // Widening: a `u32` bytecode index.
        let bci = level.bci as usize;
        if let Some(found) = compiled_level_catch(cm, class_id, &level.label, bci, exc_class) {
            return Some(found);
        }
    }
    let bci = usize::try_from(activation.bci).ok()?;
    compiled_level_catch(cm, owner, &activation.label, bci, exc_class)
}

/// [`compiled_activation_catch`] for one level: the method `label` names in
/// class `class_id`, at `bci`.
pub(in crate::runtime::interpreter) fn compiled_level_catch(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
    label: &str,
    bci: usize,
    exc_class: ClassId,
) -> Option<PredictedCatch> {
    let ((_, method_name), descriptor) = split_compiled_label(label)?;
    let class = cm.get_class(class_id)?;
    let method = class.find_method(method_name, descriptor)?;
    let code = method.code()?;
    let handler_pc = catching_handler(cm, class_id, &code.exception_table, bci, exc_class)?;
    Some(PredictedCatch::Compiled {
        class_id,
        method_name: Arc::clone(&method.name),
        descriptor: Arc::clone(&method.descriptor),
        handler_pc,
    })
}

/// `"class/Name.method:descriptor"` (a compiled method's label) as
/// `((class name, method name), descriptor)`.
pub(in crate::runtime::interpreter) fn split_compiled_label(label: &str) -> Option<((&str, &str), &str)> {
    let (owner_and_name, descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_name.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() || descriptor.is_empty() {
        return None;
    }
    Some(((class_name, method_name), descriptor))
}

/// The last compiled `athrow` of this thread whose event is still to be
/// posted (interpreter round i4 wave 92, lane L1): the throwable's address
/// when it was thrown, the method that threw it and the `athrow`'s bci.
/// Left by [`note_compiled_throw_site`], taken by the next
/// [`deliver_exception_events`], which reports the throw there instead of at
/// the frame a door or the unwinder first sees it in.
pub(crate) struct CompiledThrowSite {
    pub(in crate::runtime::interpreter) exc: usize,
    pub(in crate::runtime::interpreter) method: Arc<CachedBytecodeMethod>,
    pub(in crate::runtime::interpreter) bci: usize,
    /// The throwing VM's collection count when the site was left (`u64::MAX`
    /// when the thread names no VM). A report taken after a collection drops
    /// the site, so a different throwable at the same address is never
    /// reported at this `athrow` (interpreter round i4 wave 95, lane L1's
    /// finding).
    pub(in crate::runtime::interpreter) gc_epoch: u64,
}

thread_local! {
    /// See [`CompiledThrowSite`]. Per thread, as the throw it describes;
    /// holds no object (an address, compared, never read through).
    pub(in crate::runtime::interpreter) static COMPILED_THROW_SITE: std::cell::RefCell<Option<CompiledThrowSite>> =
        const { std::cell::RefCell::new(None) };
}

/// A compiled `athrow` of `exc` at `bci` (`jit::helpers::jit_throw_exception`),
/// while some VM listens for JVMTI `Exception` (interpreter round i4 wave
/// 92, lane L1; page
/// `docs/feature-designs/real-jdk-mode.md`):
/// remember where it was thrown. The method is the innermost compiled
/// activation's, named by the inline frame record (the compile-id mirror);
/// the site is kept only when that method's code holds an `athrow` at `bci`
/// (the optimizing tier refuses `athrow` in a spliced callee, so the bci is
/// the activation's own). Otherwise the site is cleared, and the throw is
/// reported where a door first sees it, as before.
///
/// Runs on the throwing thread inside the helper; takes no lock. The address
/// is compared with the throwable the next report carries: a collection that
/// moved it in between loses the site (the report falls back to the door's
/// frame), never pins anything.
#[cold]
#[inline(never)]
pub(crate) fn note_compiled_throw_site(exc: ObjectRef, bci: i64) {
    let site = compiled_throw_site(exc, bci);
    COMPILED_THROW_SITE.with(|slot| *slot.borrow_mut() = site);
}

/// Must a compiled `athrow` record its site ([`note_compiled_throw_site`])?
/// While some VM listens for JVMTI `Exception`, or (interpreter round i4 wave
/// 93, lane L1) some VM serves a JDWP `Exception` request from compiled code
/// (`debug::compiled_exception_requests_anywhere`). Two relaxed loads; the
/// report that consumes the site checks its own VM.
#[inline(always)]
pub(crate) fn compiled_throw_sites_wanted() -> bool {
    #[cfg(feature = "experimental-debug")]
    {
        if crate::debug::compiled_exception_requests_anywhere() {
            return true;
        }
    }
    crate::runtime::jvmti::any_exception_listener_active()
}

/// Does `shared` serve JDWP `Exception` requests from compiled code when
/// they allow it (interpreter round i4 wave 93, lane L1;
/// `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`, read into its debugger gates)?
/// The compile door then builds no compiled local throw edge in this VM, as
/// for JVMTI (`jit_bridge`, `exception_events_armed_at_compile`): an edge
/// posts nothing, and a request armed later does not withdraw the bodies.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(crate) fn jdwp_exceptions_post_from_compiled_code(shared: &SharedVm) -> bool {
    shared.debug.debugger_gates.jdwp_exceptions_from_compiled()
}

/// [`jdwp_exceptions_post_from_compiled_code`] in a build without the JDWP
/// surface.
#[cfg(not(feature = "experimental-debug"))]
#[inline]
pub(crate) fn jdwp_exceptions_post_from_compiled_code(_shared: &SharedVm) -> bool {
    false
}

/// [`note_compiled_throw_site`]'s site, or `None` when the innermost
/// compiled activation cannot be named or holds no `athrow` at `bci`.
pub(in crate::runtime::interpreter) fn compiled_throw_site(exc: ObjectRef, bci: i64) -> Option<CompiledThrowSite> {
    let bci = usize::try_from(bci).ok()?;
    let (_, _, cm_ptr) = crate::jit::conservative_roots::innermost_compiled_site_key()?;
    // SAFETY: `innermost_compiled_site_key`'s: the compile-id table hands out
    // only live methods, and the activation that called the helper is on
    // this thread's stack while it runs.
    let method = Arc::clone(unsafe { &*cm_ptr }.compiled_source()?);
    // `athrow` is 0xbf.
    if method.code.get(bci) != Some(&0xbf) {
        return None;
    }
    let gc_epoch = crate::native::jni::calling_thread_vm().map_or(u64::MAX, |s| {
        s.mem
            .gc_barrier
            .gc_generation
            .load(std::sync::atomic::Ordering::Acquire)
    });
    Some(CompiledThrowSite {
        exc: exc.as_ptr() as usize,
        method,
        bci,
        gc_epoch,
    })
}

/// Take this thread's [`CompiledThrowSite`]; every report consumes it.
pub(in crate::runtime::interpreter) fn take_compiled_throw_site() -> Option<CompiledThrowSite> {
    COMPILED_THROW_SITE.with(|slot| slot.borrow_mut().take())
}

/// Does `frame`'s code hold an invoke instruction at `pc` -- is the
/// exception the unwinder sees at `pc` one a callee threw?
pub(in crate::runtime::interpreter) fn invokes_at(frame: &Frame, pc: usize) -> bool {
    // invokevirtual, invokespecial, invokestatic, invokeinterface,
    // invokedynamic.
    matches!(frame.code.get(pc).copied(), Some(0xb6..=0xba))
}


/// `-agentlib:jdwp`'s deferred start at a throw (interpreter round i7 wave
/// 144, lane L1; `debug::start_deferred_jdwp_agent`): exception `exc`,
/// thrown at `throw_pc` of frame `frame_idx`, is uncaught when no frame on
/// the thread would catch it (`known_handler` names a compiled door's
/// handler; otherwise the unwinder's own prediction, compiled activations
/// included). The exception is pinned across the start, which may park this
/// thread for the debugger (`suspend=y`), and answered at its current
/// address. Cold: only while the start is deferred.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn deferred_jdwp_start_on_throw(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    exc: ObjectRef,
    throw_pc: usize,
    known_handler: Option<usize>,
) -> ObjectRef {
    let exc_class = shared.mem.heap.class_id_of(exc);
    let wants_uncaught = shared
        .config
        .jdwp_agent
        .as_ref()
        .is_some_and(|agent| agent.on_uncaught);
    let uncaught = wants_uncaught
        && known_handler.is_none()
        && {
            let search_own = throw_pc != OSR_FRAME_DECLINED_TO_CATCH;
            let pc = if search_own {
                throw_pc
            } else {
                thread.frames.get(frame_idx).map_or(0, |f| f.last_instr_pc)
            };
            let compiled = if crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
                crate::jit::conservative_roots::active_compiled_frames()
            } else {
                Vec::new()
            };
            let cm = shared.classes.class_manager.read();
            predicted_catch_frame(&cm, thread, frame_idx, pc, search_own, exc_class, &compiled)
                .is_none()
        };
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let started = crate::debug::start_deferred_jdwp_agent(shared, thread, exc_class, uncaught);
    let current = if started {
        thread.native_pin_roots.get(pin).copied().unwrap_or(exc)
    } else {
        exc
    };
    thread.native_pin_roots.truncate(pin);
    current
}

/// [`deliver_exception_event_if_armed`] once a client may be listening:
/// decides which one is (the JVMTI union can be raised by another VM, and a
/// JDWP gate by a request that is gone), applies the once-per-throw rule and
/// posts.
///
/// `known_handler`: frame `frame_idx` is already known to catch at this
/// handler pc (a compiled catch door, wave 21), so the catch location is not
/// predicted.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn deliver_exception_events(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    exc: ObjectRef,
    throw_pc: usize,
    jdwp: bool,
    known_handler: Option<usize>,
) -> ObjectRef {
    // Wave 92 (lane L1): a compiled `athrow`'s site, consumed by every report
    // (see `CompiledThrowSite`).
    // A site left before a collection is dropped (wave 95): the address it
    // compares may since name another throwable.
    let gc_now = shared
        .mem
        .gc_barrier
        .gc_generation
        .load(std::sync::atomic::Ordering::Acquire);
    let compiled_site = take_compiled_throw_site().filter(|s| s.gc_epoch == gc_now);
    // Interpreter round i7 wave 144, lane L1: `-agentlib:jdwp`'s deferred
    // start, before anything reports the throw (the back end's early
    // `Exception` callback runs `initialize` first).
    #[cfg(feature = "experimental-debug")]
    let exc = if shared.debug.debugger_gates.jdwp_deferred_start_armed() {
        deferred_jdwp_start_on_throw(shared, thread, frame_idx, exc, throw_pc, known_handler)
    } else {
        exc
    };
    let tid = thread.thread_id.0;
    let jvmti = crate::runtime::jvmti::exception_event_enabled_for_vm(shared.vm_identity, tid);
    #[cfg(feature = "experimental-debug")]
    let jdwp = jdwp
        && !crate::debug::debugger_hooks_suppressed()
        && shared
            .debug
            .debug_state
            .lock()
            .events
            .has_requests(crate::debug::events::EventKind::Exception);
    if !(jdwp || jvmti) {
        return exc;
    }
    let Some(frame) = thread.frames.get(frame_idx) else {
        return exc;
    };
    // An OSR frame that already declined to catch hands the unwinder no pc
    // (`OSR_FRAME_DECLINED_TO_CATCH`): report where the frame last was, and
    // look for the handler from its caller on.
    let search_own = throw_pc != OSR_FRAME_DECLINED_TO_CATCH;
    let pc = if search_own {
        throw_pc
    } else {
        frame.last_instr_pc
    };
    let arrived_from_callee = invokes_at(frame, pc);
    {
        let mut globals = shared.natives.jni_global_refs.lock();
        let seen = thread.reported_exception;
        if seen != 0 {
            // The same object again: already reported where it was thrown.
            if globals.resolve(seen) == Some(exc) {
                return exc;
            }
            let _ = globals.remove(seen);
        }
        // Remembered whether or not a request reports it: a JDWP `Count`
        // must not be consumed again at the next interpreter entry.
        thread.reported_exception = globals.add_weak(exc);
    }
    let exc_class = shared.mem.heap.class_id_of(exc);
    // Wave 92 (lane L1, `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`): a VM that
    // serves JVMTI `Exception` from compiled code reports a compiled
    // `athrow` where it ran -- when this report is that throwable's, and a
    // compiled catch door is reporting it or the unwinder sees it arrive
    // from a callee -- and predicts the catch over the compiled activations
    // too.
    // Interpreter round i4 wave 93 (lane L1): or one that serves JDWP
    // `Exception` requests from compiled code (the same switch, read into
    // the VM's debugger gates), for the JDWP report.
    #[cfg(feature = "experimental-debug")]
    let jdwp_from_compiled = jdwp && shared.debug.debugger_gates.jdwp_exceptions_from_compiled();
    #[cfg(not(feature = "experimental-debug"))]
    let jdwp_from_compiled = false;
    let from_compiled = (jvmti
        && crate::runtime::jvmti::exceptions_post_from_compiled_code(shared.vm_identity))
        || jdwp_from_compiled;
    let thrower = compiled_site.filter(|site| {
        from_compiled
            && site.exc == exc.as_ptr() as usize
            && (known_handler.is_some() || arrived_from_callee)
    });
    let catch = match known_handler {
        Some(handler_pc) => Some(PredictedCatch::Frame(frame_idx, handler_pc)),
        None => {
            let compiled = if from_compiled
                && crate::jit::conservative_roots::current_thread_jit_depth() != 0
            {
                crate::jit::conservative_roots::active_compiled_frames()
            } else {
                Vec::new()
            };
            let cm = shared.classes.class_manager.read();
            predicted_catch_frame(&cm, thread, frame_idx, pc, search_own, exc_class, &compiled)
        }
    };
    let mut exc = exc;
    if jvmti {
        let caught = catch.as_ref();
        exc = post_jvmti_exception(shared, thread, frame_idx, exc, pc, thrower.as_ref(), caught);
    }
    #[cfg(feature = "experimental-debug")]
    {
        if jdwp {
            // Wave 93 (lane L1): at the compiled `athrow` when the VM serves
            // JDWP `Exception` from compiled code.
            let thrower = thrower.as_ref().filter(|_| jdwp_from_compiled);
            exc = deliver_jdwp_exception_event(
                shared, thread, frame_idx, exc, pc, exc_class, catch, thrower,
            );
        }
    }
    exc
}

/// The JVMTI half (wave 11): `Exception` on the VM's manager, with the throw
/// location, the exception and the predicted catch location, the exception
/// pinned across the callbacks (an agent's callback may run Java).
///
/// `thrower` (wave 92, lane L1): the compiled `athrow` that threw `exc`,
/// reported as the throw location instead of frame `frame_idx` at `pc`.
pub(in crate::runtime::interpreter) fn post_jvmti_exception(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    exc: ObjectRef,
    pc: usize,
    thrower: Option<&CompiledThrowSite>,
    catch: Option<&PredictedCatch>,
) -> ObjectRef {
    let vm = shared.vm_identity;
    let Some(frame) = thread.frames.get(frame_idx) else {
        return exc;
    };
    let (method, location) = match thrower {
        Some(site) => (
            jvmti_events::jvmti_method_id_of(
                vm,
                site.method.declaring_class_id,
                &site.method.method_name,
                &site.method.method_descriptor,
            ),
            site.bci,
        ),
        None => (jvmti_events::jvmti_method_id(vm, frame), pc),
    };
    let caught = match catch {
        Some(PredictedCatch::Frame(idx, handler_pc)) => thread
            .frames
            .get(*idx)
            .map(|f| (jvmti_events::jvmti_method_id(vm, f), *handler_pc)),
        Some(PredictedCatch::Compiled {
            class_id,
            method_name,
            descriptor,
            handler_pc,
        }) => Some((
            jvmti_events::jvmti_method_id_of(vm, *class_id, method_name, descriptor),
            *handler_pc,
        )),
        None => None,
    };
    let (catch_method, catch_location) = match caught {
        // Widening: bytecode index -> jlocation
        Some((m, handler_pc)) => (m, handler_pc as i64),
        None => (0, -1),
    };
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let tid = thread.thread_id.0;
    // gci i11/ja: delivered under this thread's own handle.
    post_on_own_thread(thread, || {
        crate::runtime::jvmti::fire_exception_for_vm(
            vm,
            tid,
            method,
            // Widening: bytecode index -> jlocation
            location as i64,
            // Cast: the exception's heap address, as the other object events carry it
            exc.as_ptr() as usize as u64,
            catch_method,
            catch_location,
        )
    });
    let exc = thread.native_pin_roots.get(pin).copied().unwrap_or(exc);
    thread.native_pin_roots.truncate(pin);
    exc
}

/// The JDWP half (wave 10): report an `Exception` event when a request
/// matches, and park at the throw site if its policy suspends the thread.
///
/// `thrower` (interpreter round i4 wave 93, lane L1;
/// `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`): the compiled `athrow` that
/// threw `exc`, reported as the event's location (and matched by the
/// requests' class filters) instead of frame `frame_idx` at `pc`, as HotSpot
/// reports a compiled throw. Used only while every `Exception` request in
/// force is one compiled code serves
/// (`EventManager::exception_requests_need_the_interpreter` false): such a
/// request neither parks nor filters by instance, so the frames that unwound
/// past the throw site are never shown.
#[cfg(feature = "experimental-debug")]
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn deliver_jdwp_exception_event(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    exc: ObjectRef,
    pc: usize,
    exc_class: ClassId,
    catch: Option<PredictedCatch>,
    thrower: Option<&CompiledThrowSite>,
) -> ObjectRef {
    use crate::debug::events::{EventKind, EventLocation, EventSubject};
    let tid = thread.thread_id.0;
    let Some(frame) = thread.frames.get(frame_idx) else {
        return exc;
    };
    let (filter_classes, instance_ids, need_interpreter) = {
        let ds = shared.debug.debug_state.lock();
        (
            ds.events.filter_class_ids(EventKind::Exception),
            ds.events.instance_filter_ids(EventKind::Exception),
            ds.events.exception_requests_need_the_interpreter(),
        )
    };
    let thrower = thrower.filter(|_| !need_interpreter);
    // The class the event's location names: the compiled thrower's, or the
    // frame's.
    let location_class = thrower.map_or(frame.class_id, |site| site.method.declaring_class_id);
    // `InstanceOnly` (interpreter round i4 wave 85, lane L1): the throwing
    // frame's active `this`, as the back end's `eventInstance` reads it.
    let frame_this = if instance_ids.is_empty() {
        None
    } else {
        debugger_frame_this(shared, frame)
    };
    let (location_supers, exception_supers, exc_tag) = {
        let cm = shared.classes.class_manager.read();
        (
            debugger_supertypes_among(&cm, location_class, &filter_classes),
            debugger_supertypes_among(&cm, exc_class, &filter_classes),
            crate::debug::inspect::object_tag(shared, &cm, exc),
        )
    };
    // The catch location as JDWP names it: (class, jdwp method id, pc).
    let catch = catch.and_then(|caught| match caught {
        PredictedCatch::Frame(idx, handler_pc) => thread.frames.get(idx).map(|f| {
            (
                u64::from(f.class_id.as_u32()),
                crate::debug::frame_method_id(f),
                handler_pc as u64, // Widening: bytecode index
            )
        }),
        // Wave 92 (lane L1): a catch in a method running compiled.
        PredictedCatch::Compiled {
            class_id,
            method_name,
            descriptor,
            handler_pc,
        } => Some((
            u64::from(class_id.as_u32()),
            crate::debug::jdwp_method_id(&method_name, &descriptor),
            handler_pc as u64, // Widening: bytecode index
        )),
    });
    let location = match thrower {
        Some(site) => EventLocation {
            class_id: u64::from(site.method.declaring_class_id.as_u32()),
            class_name: &site.method.class_name,
            method_id: crate::debug::jdwp_method_id(
                &site.method.method_name,
                &site.method.method_descriptor,
            ),
            offset: site.bci as u64, // Widening: bytecode index
            thread_id: tid,
            frame_depth: thread.frames.len(),
            line_start: None,
            line: None,
        },
        None => EventLocation {
            class_id: u64::from(frame.class_id.as_u32()),
            class_name: frame.class_name(),
            method_id: crate::debug::frame_method_id(frame),
            offset: pc as u64, // Widening: bytecode index
            thread_id: tid,
            frame_depth: thread.frames.len(),
            line_start: None,
            line: None,
        },
    };
    let (events, park) = {
        let mut ds = shared.debug.debug_state.lock();
        let instance_hits = debugger_instance_hits(shared, &ds, &instance_ids, frame_this);
        let subject = EventSubject {
            location_class_is: &|id| location_supers.contains(&id),
            field: None,
            instance_is: &|id| frame_this.is_none() || instance_hits.contains(&id),
            exception_is: &|id| exception_supers.contains(&id),
            caught: catch.is_some(),
        };
        let hits = ds
            .events
            .match_subject_events(EventKind::Exception, &location, &subject);
        let (catch_class, catch_method, catch_pc) = catch.unwrap_or((0, 0, 0));
        let mut events = Vec::with_capacity(hits.len());
        for (request_id, suspend_policy) in hits {
            // The exception (tagged object id), then the catch location.
            let mut extra = crate::debug::protocol::PayloadWriter::new();
            crate::debug::inspect::put_tagged_with_tag(
                shared,
                &mut ds,
                &mut extra,
                b'L',
                Value::Object(Some(exc)),
                Some(exc_tag),
            );
            let mut catch_location =
                crate::debug::build_location_extra(catch_class, catch_method, catch_pc);
            // The catching class's own type tag (wave 15); no catch keeps
            // CLASS, as HotSpot writes a null location.
            if catch.is_some() {
                if let Some(tag) = catch_location.first_mut() {
                    *tag = ds.location_type_tag(catch_class);
                }
            }
            extra.put_bytes(&catch_location);
            events.push(crate::debug::DebugEvent {
                kind: EventKind::Exception,
                request_id,
                suspend_policy,
                thread_id: tid,
                class_id: location.class_id,
                method_id: location.method_id,
                offset: location.offset,
                extra: extra.into_bytes(),
                set_follows: false,
            });
        }
        // Interpreter round i6 wave 127 (lane L1, `CRATONVM_JDWP_EVENT_OBJECT_HOLD`):
        // the exception lives until the debugger's first look at it.
        if !events.is_empty() {
            crate::debug::hold_event_object(shared, &mut ds, exc);
        }
        let park = apply_debugger_event_policies(shared, thread, &mut ds, tid, pc, &events);
        (events, park)
    };
    send_debugger_events(shared, events);
    if !park {
        return exc;
    }
    // The park is a safepoint, and nothing else roots the exception yet.
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    park_for_debugger(shared, thread, tid, pc);
    let exc = thread.native_pin_roots.get(pin).copied().unwrap_or(exc);
    thread.native_pin_roots.truncate(pin);
    exc
}

/// The `LineNumberTable` facts a JDWP LINE step needs at bytecode index `pc`
/// of `frame`'s method: does `pc` start an entry, and which source line does
/// it belong to (the entry with the greatest `start_pc <= pc`)? `(None,
/// None)` when the method has no line information (a LINE step then behaves
/// as a MIN step). For [`deliver_breakpoint_if_set`], only while a step
/// request exists; takes the class-manager read lock.
#[cfg(feature = "experimental-debug")]
#[cold]
pub(in crate::runtime::interpreter) fn debugger_line_info(shared: &SharedVm, frame: &Frame, pc: usize) -> (Option<bool>, Option<i32>) {
    let entries = {
        let cm = shared.classes.class_manager.read();
        crate::runtime::stackwalker::line_number_entries(
            &cm.class_store,
            frame.class_id,
            frame.method_name(),
            frame.method_descriptor(),
        )
    };
    if entries.is_empty() {
        return (None, None);
    }
    let starts = entries.iter().any(|e| usize::from(e.start_pc) == pc);
    let line = entries
        .iter()
        .filter(|e| usize::from(e.start_pc) <= pc)
        .max_by_key(|e| e.start_pc)
        .map(|e| i32::from(e.line_number));
    (Some(starts), line)
}

/// Publish a suspended thread's interpreter frames (top first, frame id =
/// the thread's frame generation over the depth from the top, wave 28:
/// `DebugState::frame_id`) and their locals to the JDWP state. The top frame is
/// at `top_pc`; every caller is at its `last_instr_pc`, the invoke it is
/// waiting in — the location HotSpot reports for a caller frame.
///
/// A reference local is published as its JDWP object id from the session's
/// object table (wave 7), pinned while the snapshot stands and withdrawn by
/// [`park_for_debugger`]; it used to be the object's address, which the next
/// moving collection turned into a name for nothing or for another object.
/// Minting an id allocates nothing on the Java heap (the handle is a JNI
/// global reference), so no collection can run while the frames are read.
///
/// Any snapshot the thread already has is replaced, and its pins dropped
/// after the new ones are taken (an id both name survives). Wave 9: a stop
/// inside a debugger invocation publishes over the snapshot of the stop the
/// invocation runs from; that snapshot's pins used to be lost with it.
///
/// Wave 21 (lane L3): the compiled activations between the interpreter
/// frames (a compiled method that called the interpreted one that stopped)
/// are listed too, and a frame whose body runs compiled (an OSR'd caller)
/// withholds its stale locals ([`capture_blocked_compiled_view`]). Always
/// called on the stopping thread itself, which is what the capture needs.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn publish_debugger_frames(
    shared: &SharedVm,
    thread: &JvmThread,
    ds: &mut crate::debug::DebugState,
    tid: u64,
    top_pc: usize,
) {
    publish_debugger_frames_under(shared, thread, ds, tid, top_pc, None);
}

/// The most JDK Java levels a [`NativeHead`] carries (a stand-in chain is at
/// most `stackwalker`'s `STANDIN_CHAIN_MAX` = 8 frames, the leaf included).
#[cfg(feature = "experimental-debug")]
pub(crate) const NATIVE_HEAD_LEVELS_MAX: usize = 7;

/// One JDK Java level of a [`NativeHead`]: a frame the registered native
/// standing in for a JDK method did not run, listed at its call of the next
/// level (interpreter round i1 wave 47, lane L1).
#[cfg(feature = "experimental-debug")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StandinLevel {
    /// Wire class id of the method's class.
    pub(crate) class_id: u64,
    /// The method's JDWP method id.
    pub(crate) method_id: u64,
    /// The method's slot in its class (a JVMTI `jmethodID`'s low half).
    pub(crate) method_index: u16,
    /// The bytecode index of its call of the next level.
    pub(crate) bci: u64,
    /// What the level's locals are, when they are known without a frame
    /// (interpreter round i6 wave 121, lane L1; i119-L1,
    /// `CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS`, default off). Tagged by
    /// [`NativeHead::from_standin_chain`]; read only by the publisher.
    pub(crate) locals: StandinLevelLocals,
}

/// The locals of a stand-in head's JDK Java level that the stand-in did not
/// run (interpreter round i6 wave 121, lane L1;
/// `docs/feature-designs/real-jdk-mode.md`).
/// A tag, never a value: the head is `Copy` and republished by a park loop
/// across collections, so it holds no reference. The publisher reads the
/// value where a moving collection forwards it, at publish time.
#[cfg(feature = "experimental-debug")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum StandinLevelLocals {
    /// Not known: the row stays opaque (`OPAQUE_FRAME`), as before.
    #[default]
    Unknown,
    /// `Object.wait()V` / `Object.wait(J)V` whose timeout is not known:
    /// slot 0 is the monitor the thread waits on
    /// (`ThreadRegistry::peek_jmx_waiting_monitor`, a forwarded root).
    WaitThis,
    /// `Object.wait(J)V` called by `Object.wait()V`: slot 0 the monitor,
    /// slots 1-2 the `long` timeout, `0L` (`wait()` is `wait(0L)`).
    WaitThisAndTimeout(i64),
    /// An instance method of `java.lang.VirtualThread` among the continuation
    /// rows of an unmounted bound virtual thread
    /// (`continuation_head_of_unmounted`: `yieldContinuation`, `parkNanos`,
    /// `park`, `sleepNanos`; interpreter round i6 wave 129, lane L1,
    /// `L1W129RawJdwpVirtualWaitThisObject`): slot 0 is the virtual thread's
    /// own `Thread` object (`ThreadRegistry::java_thread_obj`, a forwarded
    /// root), as HotSpot 25.0.3 answers `ThisObject` there.
    VirtualThreadThis,
}

/// The native method heading a debugger's listing of a thread blocked in it
/// or suspended at its return, and, when a registered native stands in for a
/// JDK method whose bytecode reaches that native, the JDK method's Java
/// levels between the native and the caller, top first (interpreter round i1
/// wave 47, lane L1;
/// `docs/feature-designs/real-jdk-mode.md`):
/// `Thread.sleepNanos0` over `sleepNanos` and `sleep`, `Object.wait0` over
/// `wait(long)`, `Unsafe.park` over `LockSupport.parkNanos`, as HotSpot lists
/// them. The levels are listed without locals (`DebugState::opaque_frames`,
/// as a compiled activation), so every local command answers `OPAQUE_FRAME`
/// for them and the frame arithmetic of the other commands skips them. `Copy`
/// (no allocation), as the park loop republishes it.
#[cfg(feature = "experimental-debug")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeHead {
    /// Wire class id of the native's class.
    pub(crate) class_id: u64,
    /// The native's JDWP method id.
    pub(crate) method_id: u64,
    pub(in crate::runtime::interpreter) levels: [StandinLevel; NATIVE_HEAD_LEVELS_MAX],
    pub(in crate::runtime::interpreter) level_count: u8,
    /// The head lists its native row (every head but a continuation's,
    /// [`Self::continuation`], interpreter round i1 wave 49, lane L1).
    pub(in crate::runtime::interpreter) has_native: bool,
    /// The native row's code index: -1 (a native method) for every head but
    /// a reviewed-`Intrinsic` shadow's ([`Self::located`]).
    pub(in crate::runtime::interpreter) location: i64,
}

#[cfg(feature = "experimental-debug")]
impl NativeHead {
    /// The native method `(class id, JDWP method id)`, with no Java level.
    pub(crate) fn leaf(class_id: u64, method_id: u64) -> Self {
        Self {
            class_id,
            method_id,
            levels: [StandinLevel::default(); NATIVE_HEAD_LEVELS_MAX],
            level_count: 0,
            has_native: true,
            location: -1,
        }
    }

    /// The head of a Java method with bytecode that a reviewed `Intrinsic`
    /// answers without running it (interpreter round i6 wave 120, lane L1;
    /// `docs/feature-designs/native-dispatch-architecture.md`,
    /// `CRATONVM_JDWP_INTRINSIC_SHADOW_METHOD_EVENTS`): listed on top at
    /// `location` (its entry, 0, or its last return's index) as an opaque
    /// row, not at -1, which JDI refuses for a method that is not native.
    pub(crate) fn located(class_id: u64, method_id: u64, location: i64) -> Self {
        let mut head = Self::leaf(class_id, method_id);
        head.location = location;
        head
    }

    /// A head with no native row: only `levels`, top first (interpreter
    /// round i1 wave 49, lane L1): the JDK frames of HotSpot's unmounted
    /// continuation that a suspended unmounted virtual thread lists above its
    /// kept frames ([`continuation_head_of_unmounted`]). `native()` answers
    /// `(0, 0)`, which names no method. `None` for no level or too many.
    pub(crate) fn continuation(levels: &[StandinLevel]) -> Option<Self> {
        if levels.is_empty() || levels.len() > NATIVE_HEAD_LEVELS_MAX {
            return None;
        }
        let mut head = Self::leaf(0, 0);
        head.levels[..levels.len()].copy_from_slice(levels);
        head.level_count = u8::try_from(levels.len()).ok()?;
        head.has_native = false;
        Some(head)
    }

    /// Does the head list a native row on top?
    pub(crate) fn has_native(&self) -> bool {
        self.has_native
    }

    /// The native, `(class id, JDWP method id)`.
    pub(crate) fn native(&self) -> (u64, u64) {
        (self.class_id, self.method_id)
    }

    /// The JDK Java levels below the native, top first.
    pub(crate) fn levels(&self) -> &[StandinLevel] {
        &self.levels[..usize::from(self.level_count)]
    }

    /// `chain`, OUTERMOST first as `stackwalker::debugger_standin_frames`
    /// answers it (the native leaf last), as a head; `None` when the leaf is
    /// not a native entry or the chain is longer than a head holds.
    pub(crate) fn from_standin_chain(
        chain: &[crate::native::registry::StackTraceEntry],
    ) -> Option<Self> {
        let (leaf, java) = chain.split_last()?;
        if leaf.byte_code_index != -1 || java.len() > NATIVE_HEAD_LEVELS_MAX {
            return None;
        }
        let wire = |entry: &crate::native::registry::StackTraceEntry| -> Option<(u64, u64)> {
            Some((
                u64::from(entry.class_id?.as_u32()),
                crate::debug::jdwp_method_id(
                    &entry.method_name,
                    entry.method_descriptor.as_deref()?,
                ),
            ))
        };
        let (class_id, method_id) = wire(leaf)?;
        // Wave 121 (i119): an `Object.wait` level's locals, by name.
        let is_wait = |entry: &crate::native::registry::StackTraceEntry, descriptor: &str| {
            &*entry.class_name == "java/lang/Object"
                && &*entry.method_name == "wait"
                && entry.method_descriptor.as_deref() == Some(descriptor)
        };
        let mut head = Self::leaf(class_id, method_id);
        for (k, (slot, entry)) in head.levels.iter_mut().zip(java.iter().rev()).enumerate() {
            let (class_id, method_id) = wire(entry)?;
            // Top first: level `k` is `java[java.len() - 1 - k]`, called by
            // `java[java.len() - 2 - k]`.
            let caller = java.len().checked_sub(k + 2).and_then(|j| java.get(j));
            let locals = if is_wait(entry, "()V") {
                StandinLevelLocals::WaitThis
            } else if is_wait(entry, "(J)V") {
                if caller.is_some_and(|c| is_wait(c, "()V")) {
                    StandinLevelLocals::WaitThisAndTimeout(0)
                } else {
                    StandinLevelLocals::WaitThis
                }
            } else {
                StandinLevelLocals::Unknown
            };
            *slot = StandinLevel {
                class_id,
                method_id,
                method_index: u16::try_from(entry.method_index?).ok()?,
                bci: u64::try_from(entry.byte_code_index).ok()?,
                locals,
            };
        }
        head.level_count = u8::try_from(java.len()).ok()?;
        Some(head)
    }
}

/// Rows a debugger's listing splices between a thread's interpreter frames
/// and compiled activations (interpreter round i1 wave 47, lane L1; stage 1
/// of `docs/feature-designs/jvmti-delivery-threading.md`):
/// a JNI native in the middle of the stack (one that called back into Java,
/// `JvmThread::jni_native_frames`) at location -1, or a reflective call's JDK
/// frames (`Method.invoke` and the accessor's `invoke`,
/// `JvmThread::reflective_calls`), anchored where the call was made.
#[cfg(feature = "experimental-debug")]
#[derive(Clone, Debug)]
pub(crate) struct ListingSplice {
    /// The thread's interpreter depth at the call.
    pub(in crate::runtime::interpreter) interp_depth: u32,
    /// The thread's JIT entry-chain length at the call.
    pub(in crate::runtime::interpreter) jit_depth: u32,
    /// At one place a reflective call's frames (0) go below a native's row
    /// (1): a native a reflective call runs is its target.
    pub(in crate::runtime::interpreter) kind: u8,
    /// The rows, OUTERMOST first: `(class id, JDWP method id, location)`,
    /// location -1 for a native.
    pub(in crate::runtime::interpreter) rows: Vec<(u64, u64, i64)>,
    /// A lambda proxy method's row (the i54-L1 retirement): the frame's
    /// locals (the proxy, then the arguments as passed), each reference at
    /// its current address, when the listing is made on the thread itself
    /// (`JvmThread::lambda_proxy_locals`). Listed as [`ListingRow::Proxy`].
    pub(in crate::runtime::interpreter) values: Option<Vec<Value>>,
}

/// The splices of `thread`'s listing ([`ListingSplice`]; wave 47), or none
/// while `CRATONVM_DEBUG_ONE_FRAME_LISTING` is off
/// (`DebuggerGates::one_frame_listing`) or the thread is in no JNI native
/// and no reflective call (two length reads). A reflective call's frames come
/// from the thread's memo (`JvmThread::reflective_frames_named`, no lock). A
/// JNI native's row is named in its class, under the class-manager lock:
/// `may_block` is `false` for a caller holding the debug-state lock, which is
/// taken after that one, and then the lock is only tried (a row it cannot
/// name is left out, as the stack trace leaves out what it cannot name). A
/// row on top that names `head`'s native (the method-event or native-exit
/// park's own row) is not listed twice.
#[cfg(feature = "experimental-debug")]
pub(crate) fn debugger_listing_splices(
    shared: &SharedVm,
    thread: &JvmThread,
    head: Option<&NativeHead>,
    may_block: bool,
) -> Vec<ListingSplice> {
    listing_splices_of(
        shared,
        thread.thread_id.0,
        thread.frames.len(),
        &thread.jni_native_frames,
        &thread.reflective_calls,
        &thread.reflective_frames_named,
        head,
        may_block,
        Some((&thread.lambda_proxy_locals, &thread.native_pin_roots)),
        thread.served_thread_run.call_depths().unwrap_or_default(),
    )
}

/// [`debugger_listing_splices`] of thread `tid` from its rows as given --
/// its own lists, or (interpreter round i2, the i53 retirement) the copy it
/// recorded as it entered the native a native frames window lists
/// (`jvm_thread::NativeRowsSnapshot`) -- with `depth` interpreter frames
/// listed. `proxy_locals`: the thread's own lambda proxy frames' locals and
/// pins, for a listing made on it (none for a copy).
#[cfg(feature = "experimental-debug")]
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn listing_splices_of(
    shared: &SharedVm,
    tid: u64,
    depth_listed: usize,
    natives: &[(usize, ClassId, u64, u32)],
    calls: &[crate::runtime::stackwalker::ReflectiveCallRow],
    named: &crate::runtime::stackwalker::ReflectiveFramesNamed,
    head: Option<&NativeHead>,
    may_block: bool,
    proxy_locals: Option<(
        &[crate::threading::jvm_thread::LambdaProxyLocals],
        &[ObjectRef],
    )>,
    served: &[(u32, u32)],
) -> Vec<ListingSplice> {
    if (natives.is_empty() && calls.is_empty() && served.is_empty())
        || !shared.debug.debugger_gates.one_frame_listing()
    {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(natives.len() + calls.len());
    for (interp_depth, jit_depth, entries) in
        crate::runtime::stackwalker::reflective_frames_memoized(
            named,
            calls,
            // JIT round 20 wave 7, lane revVM: a served walk's frames.
            Some(&shared.threads.throwable_layout_cache.stack_walk_named),
        )
    {
        let rows: Vec<(u64, u64, i64)> = entries
            .iter()
            .filter_map(|entry| {
                Some((
                    u64::from(entry.class_id?.as_u32()),
                    crate::debug::jdwp_method_id(
                        &entry.method_name,
                        entry.method_descriptor.as_deref()?,
                    ),
                    i64::from(entry.byte_code_index),
                ))
            })
            .collect();
        if rows.len() == entries.len() {
            out.push(ListingSplice {
                interp_depth,
                jit_depth,
                kind: 0,
                rows,
                values: None,
            });
        }
    }
    // The i54-L1 retirement: each VM-served `Thread.run` call's frames
    // (`Thread.run` and its `@Hidden` `runWith`), which HotSpot's listing
    // shows (`--compatible` serves the method without running its bytecode;
    // a stack trace lists `Thread.run` alone, from the same rows).
    if !served.is_empty() {
        let cm = if may_block {
            Some(shared.classes.class_manager.read_recursive())
        } else {
            shared.classes.class_manager.try_read_untracked()
        };
        let entries = cm.and_then(|cm| {
            crate::runtime::stackwalker::served_thread_run_debugger_entries(&cm.class_store)
        });
        let rows: Vec<(u64, u64, i64)> = entries
            .iter()
            .flatten()
            .filter_map(|entry| {
                Some((
                    u64::from(entry.class_id?.as_u32()),
                    crate::debug::jdwp_method_id(
                        &entry.method_name,
                        entry.method_descriptor.as_deref()?,
                    ),
                    i64::from(entry.byte_code_index),
                ))
            })
            .collect();
        if !rows.is_empty() && entries.as_ref().is_some_and(|e| e.len() == rows.len()) {
            out.extend(
                served
                    .iter()
                    .map(|&(interp_depth, jit_depth)| ListingSplice {
                        interp_depth,
                        jit_depth,
                        kind: 0,
                        rows: rows.clone(),
                        values: None,
                    }),
            );
        }
    }
    if !natives.is_empty() {
        let cm = if may_block {
            Some(shared.classes.class_manager.read_recursive())
        } else {
            shared.classes.class_manager.try_read_untracked()
        };
        let mut left_out = false;
        for &(depth, class_id, key, jit_depth) in natives {
            let interp_depth = u32::try_from(depth).unwrap_or(u32::MAX);
            // The i54-L1 retirement: a lambda proxy method's row (no class in
            // the store), named from the facts its dispatch recorded
            // (`DebugRealm::lambda_proxy_rows`): no class-manager lock.
            if let Some(index) = crate::jvmti::native_env::proxy_row_index(key) {
                let row = shared
                    .debug
                    .lambda_proxy_rows
                    .lock()
                    .get(&(class_id, index))
                    .cloned();
                let Some(row) = row else {
                    continue;
                };
                let values = proxy_locals.and_then(|(records, pins)| {
                    crate::jvmti::native_env::proxy_frame_values(records, pins, key)
                });
                out.push(ListingSplice {
                    interp_depth,
                    jit_depth,
                    kind: 1,
                    rows: vec![(
                        u64::from(class_id.as_u32()),
                        crate::debug::jdwp_method_id(&row.name, &row.descriptor),
                        row.location,
                    )],
                    values,
                });
                continue;
            }
            let Some(cm) = cm.as_ref() else {
                left_out = true;
                continue;
            };
            let Some(class) = cm.get_class(class_id) else {
                continue;
            };
            let Some((method_id, _)) = crate::jvmti::native_env::jni_row_method(shared, class, key)
            else {
                continue;
            };
            let wire_class = u64::from(class_id.as_u32());
            if depth >= depth_listed && head.is_some_and(|h| h.native() == (wire_class, method_id))
            {
                continue;
            }
            out.push(ListingSplice {
                interp_depth,
                jit_depth,
                kind: 1,
                rows: vec![(wire_class, method_id, -1)],
                values: None,
            });
        }
        if left_out && crate::runtime::env_cache::frame_trace() {
            eprintln!("[DEBUG_LISTING] tid={tid} jni rows left out: class manager busy");
        }
    }
    if crate::runtime::env_cache::frame_trace() && !out.is_empty() {
        eprintln!(
            "[DEBUG_LISTING] tid={tid} splices={} rows={}",
            out.len(),
            out.iter().map(|s| s.rows.len()).sum::<usize>()
        );
    }
    out
}

/// One row of a debugger's listing of a thread ([`build_frame_listing`],
/// wave 47), top first.
#[cfg(feature = "experimental-debug")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListingRow {
    /// The native method heading the listing (location -1, no locals).
    Head { class_id: u64, method_id: u64 },
    /// A row with no interpreter frame and no locals: a stand-in's JDK Java
    /// level, a compiled (or inlined) activation, a reflective call's JDK
    /// frame, or a JNI native in the middle (location -1).
    Opaque {
        class_id: u64,
        method_id: u64,
        location: i64,
        /// A compiled activation's JIT entry-chain index
        /// (`BlockedCompiledRow::chain`; interpreter round i6 wave 120, lane
        /// L1): what the C table's owned-monitor depths attribute a compiled
        /// hold by. `None` for every other row, an inlined callee's too.
        chain: Option<u32>,
    },
    /// A compiled activation whose locals its capture read from their home
    /// slots: entry `home` of the view's `BlockedCompiledView::homes`
    /// (interpreter round i6 wave 120, lane L2; `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`).
    /// Listed exactly as [`Self::Opaque`] is, but with locals.
    CompiledHome {
        class_id: u64,
        method_id: u64,
        location: i64,
        home: u32,
        /// As [`Self::Opaque`]'s `chain` (wave 121, L2, from L1's review):
        /// the activation's JIT entry-chain index, so its monitors keep their
        /// owned-monitor depth under `CRATONVM_JVMTI_C_LISTING_SHARED`.
        chain: Option<u32>,
    },
    /// Interpreter frame `index` (from the bottom) at `location`;
    /// `runs_compiled`: its body runs compiled, and its locals are the values
    /// of its compiled entry.
    Interp {
        index: usize,
        class_id: u64,
        method_id: u64,
        location: i64,
        runs_compiled: bool,
    },
    /// A lambda proxy method's frame (the i54-L1 retirement): no interpreter
    /// frame, its locals those of `splices[splice]` ([`ListingSplice::values`]).
    Proxy {
        class_id: u64,
        method_id: u64,
        location: i64,
        splice: usize,
    },
}

/// A thread's listing, top first, as JDWP (`ThreadReference.Frames`,
/// [`publish_frame_snapshot`]) and the C JVMTI table for a blocked thread
/// ([`read_blocked_rows`]) list it (interpreter round i1 wave 47, lane L1;
/// one builder where each had its own loop): `head` on top (the native the
/// thread blocks in or returns from, then the JDK Java levels a stand-in did
/// not run), then the interpreter frames `frames` (bottom first; the top one
/// at `top_pc`, each other at its `last_instr_pc`) with the compiled
/// activations of `compiled` between them, and each of `splices` right below
/// the first thing its call put on the stack: interpreter frame
/// `interp_depth`, or a compiled activation entered at that depth or deeper
/// whose entry-chain index is at least `jit_depth`, whichever comes first
/// (as `stackwalker::trace_anchor_position` places them in a stack trace);
/// on top of the frames, under `head`, when the call pushed nothing yet.
#[cfg(feature = "experimental-debug")]
pub(crate) fn build_frame_listing(
    frames: &[Frame],
    top_pc: usize,
    head: Option<&NativeHead>,
    compiled: &crate::threading::jvm_thread::BlockedCompiledView,
    splices: &[ListingSplice],
) -> Vec<ListingRow> {
    #[derive(Clone, Copy)]
    enum Slot<'a> {
        Interp(usize),
        Compiled(&'a crate::threading::jvm_thread::BlockedCompiledRow),
    }
    let depth = frames.len();
    let rows = &compiled.rows;
    // Bottom first: a row with `below == b` stands between frames `b - 1`
    // and `b`.
    let mut slots: Vec<Slot<'_>> = Vec::with_capacity(depth + rows.len());
    let mut next = 0usize;
    for index in 0..depth {
        while next < rows.len() && rows[next].below as usize <= index {
            slots.push(Slot::Compiled(&rows[next]));
            next += 1;
        }
        slots.push(Slot::Interp(index));
    }
    slots.extend(rows[next..].iter().map(Slot::Compiled));
    // Each splice's place: the index of the slot it goes right before.
    let mut placed: Vec<(usize, u8, usize)> = splices
        .iter()
        .enumerate()
        .map(|(k, splice)| {
            let at = slots
                .iter()
                .position(|slot| match *slot {
                    Slot::Interp(index) => index >= splice.interp_depth as usize,
                    Slot::Compiled(row) => {
                        row.below >= splice.interp_depth
                            && row.chain.is_some_and(|chain| chain >= splice.jit_depth)
                    }
                })
                .unwrap_or(slots.len());
            (at, splice.kind, k)
        })
        .collect();
    placed.sort_unstable();
    let mut bottom_first: Vec<ListingRow> =
        Vec::with_capacity(slots.len() + splices.iter().map(|s| s.rows.len()).sum::<usize>() + 8);
    let mut placed_iter = placed.iter().peekable();
    let push_splice = |out: &mut Vec<ListingRow>, k: usize| {
        let Some(splice) = splices.get(k) else {
            return;
        };
        out.extend(splice.rows.iter().map(|&(class_id, method_id, location)| {
            if splice.values.is_some() {
                ListingRow::Proxy {
                    class_id,
                    method_id,
                    location,
                    splice: k,
                }
            } else {
                ListingRow::Opaque {
                    class_id,
                    method_id,
                    location,
                    chain: None,
                }
            }
        }));
    };
    for (i, slot) in slots.iter().enumerate() {
        while let Some(&(_, _, k)) = placed_iter.next_if(|&&(at, _, _)| at == i) {
            push_splice(&mut bottom_first, k);
        }
        // j27 wave 4 (lane TRACEROWS, TR16-3): the compiled indy bridge's
        // synthetic frame is in no listing, as in no stack trace.
        let bridge = |index: usize| crate::runtime::stackwalker::frame_is_vm_bridge(&frames[index]);
        if matches!(*slot, Slot::Interp(index) if bridge(index)) {
            continue;
        }
        // Wave 120 (L2): a compiled row whose capture read its home slots
        // carries its locals (`BlockedCompiledView::homes`, empty without
        // `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`).
        let home_of = |row: &crate::threading::jvm_thread::BlockedCompiledRow| {
            compiled
                .homes
                .iter()
                .position(|(at, _, _)| {
                    rows.get(*at as usize)
                        .is_some_and(|r| std::ptr::eq(r, row))
                })
                .and_then(|k| u32::try_from(k).ok())
        };
        bottom_first.push(match *slot {
            Slot::Compiled(row) if home_of(row).is_some() => ListingRow::CompiledHome {
                class_id: u64::from(row.class_id),
                method_id: row.method_id,
                location: row.bci.max(0),
                home: home_of(row).unwrap_or(0),
                chain: row.chain,
            },
            Slot::Compiled(row) => ListingRow::Opaque {
                class_id: u64::from(row.class_id),
                method_id: row.method_id,
                // An activation whose bytecode index is unknown is listed at
                // code index 0, not -1 (wave 22, lane L1). -1 is the
                // native-method marker: JDI maps a concrete method's location
                // through its line table and throws `InternalError("Location
                // with invalid code index")` for an index outside the
                // method's code, so `jdb where` failed for the whole thread;
                // and `commands::is_native_frame` took such a row for a
                // native. The method's first index claims nothing past entry.
                location: row.bci.max(0),
                chain: row.chain,
            },
            Slot::Interp(index) => {
                let f = &frames[index];
                let pc = if index + 1 == depth {
                    top_pc
                } else {
                    f.last_instr_pc
                };
                // Widening: a recorded frame index back to `usize`.
                let runs_compiled = compiled
                    .stale
                    .iter()
                    .find(|&&(at, _)| at as usize == index)
                    .map(|&(_, bci)| bci);
                ListingRow::Interp {
                    index,
                    class_id: u64::from(f.class_id.as_u32()),
                    // An obsolete method is listed with the null method id
                    // (wave 19, lane L3).
                    method_id: crate::debug::frame_method_id(f),
                    location: match runs_compiled {
                        Some(bci) if bci >= 0 => bci,
                        _ => i64::try_from(pc).unwrap_or(-1),
                    },
                    runs_compiled: runs_compiled.is_some(),
                }
            }
        });
    }
    for &(_, _, k) in placed_iter {
        push_splice(&mut bottom_first, k);
    }
    if let Some(head) = head {
        bottom_first.extend(head.levels().iter().rev().map(|level| ListingRow::Opaque {
            class_id: level.class_id,
            method_id: level.method_id,
            location: i64::try_from(level.bci).unwrap_or(-1),
            chain: None,
        }));
        if head.has_native && head.location >= 0 {
            // Wave 120 (i40): a reviewed-Intrinsic shadow's Java method, at
            // its entry or exit index, with no locals.
            bottom_first.push(ListingRow::Opaque {
                class_id: head.class_id,
                method_id: head.method_id,
                location: head.location,
                chain: None,
            });
        } else if head.has_native {
            bottom_first.push(ListingRow::Head {
                class_id: head.class_id,
                method_id: head.method_id,
            });
        }
    }
    bottom_first.reverse();
    bottom_first
}

/// [`publish_debugger_frames`] with `native_top`, `(class id, JDWP method
/// id)` of a native method the top interpreter frame's invoke (at `top_pc`)
/// is entering or leaving, listed first at location -1 (interpreter round i1
/// wave 26, lane L1: a thread suspended by a native method's `MethodEntry` or
/// `MethodExit`, `jvmti_events::report_native_method_entry`).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn publish_debugger_frames_under(
    shared: &SharedVm,
    thread: &JvmThread,
    ds: &mut crate::debug::DebugState,
    tid: u64,
    top_pc: usize,
    native_top: Option<NativeHead>,
) {
    // Wave 120 (L2): with the home-slot locals of its compiled activations
    // (`CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`); published below, before this
    // thread can reach a safepoint.
    let compiled = capture_blocked_compiled_view_with_homes(&thread.frames);
    // Under the debug-state lock: the class-manager lock is only tried.
    let splices = debugger_listing_splices(shared, thread, native_top.as_ref(), false);
    publish_frame_snapshot(
        shared,
        &thread.frames,
        ds,
        tid,
        top_pc,
        native_top,
        Some(&compiled),
        &splices,
        |_, _, v| v,
    );
    // j27 wave 8 (lane W8INTR, `CRATONVM_JVMTI_C_COMPILED_FRAME_LOCALS`): an
    // OSR'd frame's current locals from its body's home slots, read on this
    // owning thread before it can reach a safepoint, as the homes above.
    compiled_frame_locals::publish_osr_home_locals(shared, &thread.frames, ds, tid);
    // Wave 124 (L2; fix 3 of `i124-L9-compiled-home-write-matches-a-stale-take`):
    // a pending compiled-home write whose activation this home-reading
    // capture no longer lists left without its exit, and is dropped. One load
    // while none waits.
    if shared.debug.debugger_gates.compiled_home_writes_pending() {
        let listed: Vec<crate::threading::jvm_thread::CompiledHomeBody> =
            compiled.homes.iter().map(|&(_, _, body)| body).collect();
        crate::debug::inspect::drop_unlisted_compiled_home_writes(shared, ds, tid, &listed);
    }
}

/// [`publish_debugger_frames`] over `frames` (bottom first), each local read
/// through `local(frame index, slot, value)`: the identity for the thread's
/// own frames, the blocked-window remap for a blocked thread's
/// ([`publish_frames_of_blocked_thread`], wave 12).
///
/// `native_top` is `(class id, JDWP method id)` of the native method a
/// blocked thread is in ([`blocked_native_method`], wave 15): listed first,
/// frame id 0, at location -1 (`debug::NATIVE_FRAME_LOCATION`) and with no
/// locals, as HotSpot lists it; the interpreter frames follow from id 1.
///
/// `compiled` is what a thread blocked under compiled code captured of its
/// compiled activations ([`capture_blocked_compiled_view`], interpreter round
/// i1 wave 21, lane L3): each row is listed at its place between the
/// interpreter frames, at its bytecode index (code index 0 when unknown, see
/// [`build_frame_listing`]), with no locals; an interpreter frame whose body
/// runs compiled is listed at the compiled half's bytecode index when known,
/// with its stale locals withheld. Both are recorded in
/// `DebugState::opaque_frames`, so a command reading their locals answers
/// `OPAQUE_FRAME`. `native_top` must name the native the TOP listed Java
/// frame invoked: with a compiled activation above the top interpreter frame
/// that is the activation's invoke, not the frame's
/// ([`blocked_native_top`], wave 22; wave 21 listed no native frame there).
#[cfg(feature = "experimental-debug")]
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn publish_frame_snapshot(
    shared: &SharedVm,
    frames: &[Frame],
    ds: &mut crate::debug::DebugState,
    tid: u64,
    top_pc: usize,
    native_top: Option<NativeHead>,
    compiled: Option<&crate::threading::jvm_thread::BlockedCompiledView>,
    splices: &[ListingSplice],
    local: impl Fn(usize, u16, Value) -> Value,
) {
    let pinned = withdraw_debugger_frames(ds, tid);
    // Interpreter round i2 wave 56, lane L6 (`CRATONVM_DEBUG_SUSPEND_SETTLE_LONG`):
    // a listing stands, so the thread is stopped where a debugger may read
    // it: a laggard of an earlier settle wait is waited for again. Only a
    // settle wait that SAW it stopped cleared the mark before, and a thread
    // resumed and suspended again is on its way at every later wait, so one
    // laggard made every later suspension of that thread reply at once
    // (`L6W56JdiSuspendUnderCpuHogs`).
    if shared.debug.debugger_gates.suspend_settle_long() && !ds.suspend_laggards.is_empty() {
        ds.suspend_laggards.remove(&tid);
    }
    // Interpreter round i6 wave 117, lane L1 (`CRATONVM_DEBUG_SETTLE_PENDING_COUNTER`):
    // the settle wait in progress learns of this arrival without re-walking
    // (the listing is in place before the debug-state lock is released).
    crate::debug::note_settle_arrival(ds, tid);
    let empty = crate::threading::jvm_thread::BlockedCompiledView::default();
    let view = compiled.unwrap_or(&empty);
    // Wave 47: one builder for this listing and the C JVMTI table's
    // ([`build_frame_listing`]).
    let listing = build_frame_listing(frames, top_pc, native_top.as_ref(), view, splices);
    // Interpreter round i6 wave 121, lane L1 (i119-L1 steps 2-3,
    // `CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS`, default off): the values of a
    // stand-in head's JDK Java levels whose locals are known
    // ([`StandinLevelLocals`]), by listing position. The head's levels are
    // the rows right below its native row (`build_frame_listing` lists them
    // last, so on top), position `has_native + k` for level `k`. The waited
    // monitor is read here, at publish time, from the slot a moving
    // collection forwards (`ThreadRegistry::peek_jmx_waiting_monitor`), and
    // exported pinned below before this publisher can reach a safepoint;
    // without one the level stays opaque, as before.
    let level_locals: Vec<(usize, Vec<Value>)> = match native_top.as_ref() {
        Some(head) if shared.debug.debugger_gates.standin_level_locals() => {
            let base = usize::from(head.has_native());
            let waited = || {
                shared
                    .threads
                    .thread_registry
                    .peek_jmx_waiting_monitor(crate::threading::jvm_thread::ThreadId(tid))
            };
            head.levels()
                .iter()
                .enumerate()
                .filter_map(|(k, level)| {
                    let values = match level.locals {
                        StandinLevelLocals::Unknown => return None,
                        StandinLevelLocals::WaitThis => vec![Value::Object(Some(waited()?))],
                        // Wave 129 (lane L1): a bound virtual thread's
                        // `VirtualThread` continuation row; its receiver is
                        // the thread itself.
                        StandinLevelLocals::VirtualThreadThis => {
                            let registry = &shared.threads.thread_registry;
                            let id = crate::threading::jvm_thread::ThreadId(tid);
                            vec![Value::Object(Some(registry.java_thread_obj(id)?))]
                        }
                        StandinLevelLocals::WaitThisAndTimeout(timeout) => vec![
                            Value::Object(Some(waited()?)),
                            Value::Long(timeout),
                            // A `long`'s second slot.
                            Value::Uninitialized,
                        ],
                    };
                    Some((base + k, values))
                })
                .collect()
        }
        _ => Vec::new(),
    };
    let mut entries = Vec::with_capacity(listing.len());
    // Positions, top first; the wire id folds in the thread's frame
    // generation (wave 28, `DebugState::frame_id`).
    for (position, row) in listing.iter().enumerate() {
        // Widening: a listing position.
        let frame_id = ds.frame_id(tid, position as u64);
        match *row {
            // The native the thread is in (wave 15): location -1, no locals.
            ListingRow::Head {
                class_id,
                method_id,
            } => entries.push(crate::debug::FrameEntry {
                frame_id,
                class_id,
                method_id,
                offset: crate::debug::NATIVE_FRAME_LOCATION,
            }),
            // A compiled activation (wave 21; at code index 0 when its index
            // is unknown, wave 22), a stand-in's JDK Java level (wave 47), a
            // spliced JDK frame or a JNI native in the middle (wave 47): no
            // `Frame`, `OPAQUE_FRAME` to every local command.
            ListingRow::Opaque {
                class_id,
                method_id,
                location,
                ..
            } => {
                entries.push(crate::debug::FrameEntry {
                    frame_id,
                    class_id,
                    method_id,
                    offset: u64::try_from(location).unwrap_or(crate::debug::NATIVE_FRAME_LOCATION),
                });
                ds.opaque_frames.insert((tid, frame_id), true);
                // Interpreter round i7 wave 138, lane L1
                // (`VmConfig::jdwp_lambda_proxy_steps`): a lambda proxy
                // method's row listed without its locals (a blocked thread's
                // listing) is still a frame for a step's depth, as the
                // suspend point counts it ([`debugger_step_depth`]).
                if u32::try_from(class_id).is_ok_and(|id| {
                        shared.classes.is_lambda_proxy_class(ClassId::new(id))
                    })
                {
                    ds.proxy_frames.insert((tid, frame_id));
                }
                // Wave 121 (i119): a stand-in level whose locals are known,
                // published as a compiled home's are (opaque, with locals:
                // reads answer, writes refuse).
                if let Some((_, values)) = level_locals.iter().find(|(at, _)| *at == position) {
                    let locals = values
                        .iter()
                        .map(|&v| crate::debug::snapshot_local(shared, ds, v))
                        .collect();
                    ds.update_frame_locals(tid, frame_id, locals);
                }
            }
            // Interpreter round i6 wave 120, lane L2 (`CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`,
            // stage 2 of `i43-L1`): a compiled activation whose capture read
            // its locals from their home slots. Listed as an opaque row is, and
            // recorded in `opaque_frames` as one (it has no `Frame`: every
            // write refuses it, and the interpreter-frame indices skip it), but
            // with its locals published, which `commands::frame_has_no_readable_locals`
            // lets `StackFrame.GetValues` answer from. `SetValues` /
            // `ForceEarlyReturn` / `PopFrames` are stage 3.
            ListingRow::CompiledHome {
                class_id,
                method_id,
                location,
                home,
                ..
            } => {
                entries.push(crate::debug::FrameEntry {
                    frame_id,
                    class_id,
                    method_id,
                    offset: u64::try_from(location).unwrap_or(0),
                });
                ds.opaque_frames.insert((tid, frame_id), true);
                let Some((_, values, body)) = view.homes.get(home as usize) else {
                    continue;
                };
                // Wave 123 (L2, stage 3b of i43-L1): the row's body and frame
                // base, for a `SetValues` to it
                // (`inspect::set_compiled_home_values`), and the values such a
                // write left pending (`DebugState::compiled_home_writes`), read
                // back as HotSpot reads back a written local.
                ds.compiled_home_rows.insert(
                    (tid, frame_id),
                    crate::debug::CompiledHomeRowFacts {
                        body: *body,
                        call_bci: u32::try_from(location).unwrap_or(u32::MAX),
                        locals: values.len(),
                    },
                );
                let pending = crate::debug::inspect::pending_compiled_home_values(
                    shared, ds, tid, *body,
                );
                let locals = values
                    .iter()
                    .enumerate()
                    .map(|(slot, &v)| {
                        let v = pending
                            .iter()
                            .find(|&&(s, _)| usize::from(s) == slot)
                            .map_or_else(|| compiled_home_value(v), |&(_, w)| w);
                        crate::debug::snapshot_local(shared, ds, v)
                    })
                    .collect();
                ds.update_frame_locals(tid, frame_id, locals);
            }
            // The i54-L1 retirement: a lambda proxy method's frame, with the
            // locals its dispatch recorded (the proxy, then the arguments),
            // as HotSpot's spun frame holds them.
            ListingRow::Proxy {
                class_id,
                method_id,
                location,
                splice,
            } => {
                entries.push(crate::debug::FrameEntry {
                    frame_id,
                    class_id,
                    method_id,
                    offset: u64::try_from(location).unwrap_or(0),
                });
                // Interpreter round i7 wave 138, lane L1
                // (`VmConfig::jdwp_lambda_proxy_steps`): no `Frame`, so the
                // commands that find a frame by its place among the thread's
                // interpreter frames skip it (`DebugState::proxy_frames`).
                {
                    ds.proxy_frames.insert((tid, frame_id));
                }
                let values = splices
                    .get(splice)
                    .and_then(|s| s.values.as_deref())
                    .unwrap_or_default();
                let locals = values
                    .iter()
                    .map(|&v| crate::debug::snapshot_local(shared, ds, v))
                    .collect();
                ds.update_frame_locals(tid, frame_id, locals);
            }
            ListingRow::Interp {
                index,
                class_id,
                method_id,
                location,
                runs_compiled,
            } => {
                entries.push(crate::debug::FrameEntry {
                    frame_id,
                    class_id,
                    method_id,
                    // Widening: a bytecode index. A frame whose body runs
                    // compiled is where its compiled half stands, when that
                    // half knows.
                    offset: u64::try_from(location).unwrap_or(0),
                });
                if runs_compiled {
                    // Its `Frame` holds the values of the compiled entry.
                    ds.opaque_frames.insert((tid, frame_id), false);
                    continue;
                }
                let f = &frames[index];
                let mut locals = Vec::with_capacity(f.locals_len());
                // gcf f2/f: a liveness-dead reference slot is no root, so its
                // address may be stale; screened before it is exported pinned
                // (`debug::screen_dead_local`).
                let live = f.live_locals_mask_here();
                for i in 0..f.locals_len() {
                    // Cast: `locals_len` is bounded by the u16 `max_locals`.
                    let slot = i as u16;
                    // Kind-aware (wave 15): a `long` read through `get_local`
                    // came back as a `double` and was published as 0.
                    let v = local(index, slot, crate::debug::debugger_local(f, slot));
                    let v = crate::debug::screen_dead_local(shared, &live, slot, v);
                    locals.push(crate::debug::snapshot_local(shared, ds, v));
                }
                ds.update_frame_locals(tid, frame_id, locals);
            }
        }
    }
    ds.thread_frames.insert(tid, entries);
    for id in pinned {
        ds.objects.unpin(id);
    }
}

/// The compiled activations the CURRENT thread stands under, for a JDWP
/// listing of it while it is blocked in native code or parked at a suspend
/// point (interpreter round i1 wave 21, lane L3;
/// `i18-L3-proposal-jdwp-lists-the-compiled-activations-of-a-blocked-thread`,
/// stages 1-3). Must run on the thread itself: only it can walk its JIT
/// entry chain (`conservative_roots::active_compiled_frames`). Empty when no
/// compiled code is active (one thread-local read).
///
/// The activations are spliced exactly as a stack trace splices them
/// (`stackwalker::interleave_compiled_frames`): the same dedupe decides that
/// a compiled entry and an interpreter frame are one activation
/// (`stackwalker::drop_osr_continuations`); such a frame is recorded in
/// `stale`, at the compiled half's bytecode index when the dedupe's
/// authoritative arm supplied one, and the callees inlined at that point are
/// listed above it. Each compiled activation is followed by the callees it
/// inlined, outermost first. A level whose label or class id is unknown ends
/// its chain; an activation whose label does not name a method is left out,
/// as a stack trace leaves it out.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn capture_blocked_compiled_view(
    frames: &[Frame],
) -> crate::threading::jvm_thread::BlockedCompiledView {
    if crate::jit::conservative_roots::current_thread_jit_depth() == 0 {
        return crate::threading::jvm_thread::BlockedCompiledView::default();
    }
    compiled_view_of(
        frames,
        crate::jit::conservative_roots::active_compiled_frames(),
    )
}

/// [`capture_blocked_compiled_view`] from the compiled activations `all` of
/// the thread whose interpreter frames are `frames` -- the calling thread's,
/// or (interpreter round i2 wave 58, lane L6) another thread's walked from
/// the chain it copied as it entered a native
/// (`conservative_roots::JitChainSnapshot`).
#[cfg(feature = "experimental-debug")]
pub(crate) fn compiled_view_of(
    frames: &[Frame],
    all: Vec<crate::jit::conservative_roots::ActiveCompiledFrame>,
) -> crate::threading::jvm_thread::BlockedCompiledView {
    compiled_view_of_with(frames, all, None)
}

/// [`capture_blocked_compiled_view`] that also reads each compiled
/// activation's locals from its home slots (interpreter round i6 wave 120,
/// lane L2; `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`, stage 2 of
/// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`;
/// [`read_compiled_homes`]). For a caller that publishes the view before its
/// thread can reach a safepoint ([`publish_debugger_frames_under`]): the
/// reference locals it reads are addresses valid until then. Without the
/// switch, exactly [`capture_blocked_compiled_view`].
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn capture_blocked_compiled_view_with_homes(
    frames: &[Frame],
) -> crate::threading::jvm_thread::BlockedCompiledView {
    if crate::jit::conservative_roots::current_thread_jit_depth() == 0 {
        return crate::threading::jvm_thread::BlockedCompiledView::default();
    }
    let read_homes =
        true;
    compiled_view_of_with(
        frames,
        crate::jit::conservative_roots::active_compiled_frames(),
        read_homes.then_some(crate::jit::conservative_roots::FrameWordReader::Own),
    )
}

/// One local of a compiled activation, as the debugger snapshot reads it
/// ([`crate::threading::jvm_thread::CompiledHomeValue`] to a [`Value`]).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn compiled_home_value(v: crate::threading::jvm_thread::CompiledHomeValue) -> Value {
    use crate::threading::jvm_thread::CompiledHomeValue as H;
    match v {
        H::Undefined => Value::Uninitialized,
        H::Int(i) => Value::Int(i),
        H::Long(l) => Value::Long(l),
        H::Float(bits) => Value::Float(f32::from_bits(bits)),
        H::Double(bits) => Value::Double(f64::from_bits(bits)),
        H::RefAddr(0) => Value::Object(None),
        // SAFETY: an address [`read_compiled_homes`] read from a home word the
        // activation's active oop maps name as a live reference, on the owning
        // thread, which has reached no safepoint since (the capture's contract,
        // see `capture_blocked_compiled_view_with_homes`), or (wave 121) of a
        // blocked thread whose window this reader still holds, with no moving
        // collection completed since it opened (`read_native_frames`).
        H::RefAddr(addr) => Value::Object(Some(unsafe { ObjectRef::from_raw(addr as *mut u8) })),
    }
}

/// The locals of compiled activation `f` of the CALLING thread (a walk it
/// just made) read from their home slots `[rbp - (idx + 1) * 8]`, or `None`
/// when any of them cannot be vouched for: no frame pointer, no record at its
/// bytecode index (`CompiledMethod::debugger_home_locals_at`: a compile made
/// without the switch or without a debugger), an `Unknown` local, or a
/// reference word the activation's active oop maps do not name
/// (`conservative_roots::compiled_frame_reference_word`, which also bounds the
/// read to this thread's live frames). The frame then stays `OPAQUE_FRAME`.
///
/// Interpreter round i6 wave 121, lane L1 (stage 4 of
/// `i18-L3-proposal-jdwp-lists-the-compiled-activations-of-a-blocked-thread`):
/// `reader` names whose activation `f` is. `FrameWordReader::Own` is the
/// calling thread's (the floor is its stack pointer, as in wave 120).
/// `FrameWordReader::HeldThread` is a BLOCKED thread's, walked from the chain
/// it copied as it opened its native window and read while the window is
/// held (`read_native_frames`): every word must lie in the activation's own
/// band `[rbp - frame_size, rbp)` (`CompiledMethod::frame_layout`; a body
/// that published no size reads nothing), and a reference word is named
/// through `compiled_frame_reference_word_by(.., HeldThread)`.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn read_compiled_homes(
    f: &crate::jit::conservative_roots::ActiveCompiledFrame,
    reader: crate::jit::conservative_roots::FrameWordReader,
) -> Option<Box<[crate::threading::jvm_thread::CompiledHomeValue]>> {
    use cratonvm_jit::DebuggerHomeLocal as K;
    use crate::jit::conservative_roots::FrameWordReader;
    use crate::threading::jvm_thread::CompiledHomeValue as H;
    let rbp = f.frame_rbp?;
    if f.cm_ptr == 0 {
        return None;
    }
    let bci = u32::try_from(f.bci).ok()?;
    // SAFETY: `cm_ptr` names the artifact this live activation runs, valid for
    // as long as the frame is (`ActiveCompiledFrame::cm_ptr`); the walk was
    // made on this thread just now, or (`HeldThread`) from the blocked
    // thread's chain copy while its window is held, so it stays in its native
    // and its compiled frames and their bodies are as copied.
    let cm = unsafe { &*(f.cm_ptr as *const cratonvm_jit::CompiledMethod) };
    let kinds = cm.debugger_home_locals_at(bci)?;
    let floor = match reader {
        FrameWordReader::Own => crate::jit::conservative_roots::current_stack_pointer(),
        FrameWordReader::HeldThread => {
            let size = cm.frame_layout.frame_size;
            if size <= 0 {
                return None;
            }
            rbp.checked_sub(usize::try_from(size).ok()?)?
        }
    };
    let mut out = Vec::with_capacity(kinds.len());
    for (idx, kind) in kinds.iter().enumerate() {
        let off = i32::try_from((idx + 1) * 8).ok()?;
        let addr = rbp.checked_sub(usize::try_from(off).ok()?)?;
        if addr < floor || addr & 7 != 0 {
            return None;
        }
        let word = || {
            // SAFETY: a word of a live compiled frame, aligned and above the
            // floor (checked above): this thread's own above its stack
            // pointer, or a held thread's within the activation's band.
            unsafe { (addr as *const u64).read() }
        };
        out.push(match *kind {
            K::Unknown => return None,
            K::Undefined => H::Undefined,
            // Cast: the home word's low 32 bits (Int, Float) or all 64 (Long), a bit reinterpretation.
            K::Int => H::Int(word() as u32 as i32),
            K::Long => H::Long(word() as i64),
            K::Float => H::Float(word() as u32),
            K::Double => H::Double(word()),
            // Wave 121 (lane L9): a null too only where the active oop maps
            // name the home (`compiled_frame_reference_slot_by`); a word no
            // map vouches for may be a stale home under a register-held
            // local, and listing it as null is a wrong answer, not an opaque
            // one.
            K::Ref => H::RefAddr(
                crate::jit::conservative_roots::compiled_frame_reference_slot_by(
                    f, cm, off, reader,
                )?,
            ),
            // Cast: a constant's bits, reinterpreted exactly as a home word above.
            K::Constant(b'I', bits) => H::Int(bits as u32 as i32),
            K::Constant(b'J', bits) => H::Long(bits as i64),
            K::Constant(b'F', bits) => H::Float(bits as u32),
            K::Constant(b'D', bits) => H::Double(bits),
            K::Constant(..) => return None,
        });
    }
    // Wave 121 (L2, i43-L1 stage 3a): which door a write to this activation
    // would take. Diagnostic only; every write still answers `OPAQUE_FRAME`.
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] debugger home frame {} bci={bci}: write door {}",
            f.label,
            compiled_home_write_door(cm, bci, kinds.len()).name(),
        );
    }
    Some(out.into_boxed_slice())
}

/// Stages 3a and 3b (first slice) of
/// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`
/// (interpreter round i6 waves 121-122, lane L2;
/// `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`): the door a
/// `StackFrame.SetValues` / `ThreadReference.ForceEarlyReturn` to a compiled
/// activation whose `locals` homes were read ([`read_compiled_homes`]) would
/// leave through. Fail closed: only a kept single-frame post-call exit right
/// after the call the activation is suspended in, whose map describes every
/// local and whose body was not spared as an obsolete activation
/// (`CompiledMethod::debugger_write_door`), can turn the frame into an
/// interpreter frame the moment the call returns, BEFORE the body reads a
/// local again; anything else stays `OPAQUE_FRAME`. Asked for the census line
/// here, and (wave 123) by `jit_bridge::force_compiled_home_exit` before a
/// `SetValues` to the activation is forced and recorded.
#[cfg(feature = "experimental-debug")]
pub(crate) fn compiled_home_write_door(
    cm: &cratonvm_jit::CompiledMethod,
    bci: u32,
    locals: usize,
) -> cratonvm_jit::DebuggerWriteDoor {
    cm.debugger_write_door(bci, locals)
}

/// The body and frame base of an activation whose homes were read
/// ([`read_compiled_homes`], which refuses one with no frame pointer):
/// interpreter round i6 wave 123, lane L2.
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn compiled_home_body(
    f: &crate::jit::conservative_roots::ActiveCompiledFrame,
) -> crate::threading::jvm_thread::CompiledHomeBody {
    crate::threading::jvm_thread::CompiledHomeBody {
        cm_ptr: f.cm_ptr,
        frame_rbp: f.frame_rbp.unwrap_or(0),
    }
}

/// [`compiled_view_of`], also recording each activation's home-slot locals
/// when `read_homes` names a reader ([`read_compiled_homes`]): `Own` for the
/// calling thread's own walk; `HeldThread` (interpreter round i6 wave 121,
/// lane L1) for a blocked thread's chain copy read while its native window
/// is held (`read_native_frames`).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn compiled_view_of_with(
    frames: &[Frame],
    all: Vec<crate::jit::conservative_roots::ActiveCompiledFrame>,
    read_homes: Option<crate::jit::conservative_roots::FrameWordReader>,
) -> crate::threading::jvm_thread::BlockedCompiledView {
    use crate::threading::jvm_thread::{BlockedCompiledRow, BlockedCompiledView};
    let mut view = BlockedCompiledView::default();
    if all.is_empty() {
        return view;
    }
    let before: Vec<(u32, usize)> = all.iter().map(|f| (f.interp_depth, f.cm_ptr)).collect();
    let (kept, overrides) = crate::runtime::stackwalker::drop_osr_continuations(frames, all);
    // The entries the dedupe dropped name the interpreter frame they were
    // pushed from: its body is the compiled one.
    let mut survivors: Vec<(u32, usize)> =
        kept.iter().map(|f| (f.interp_depth, f.cm_ptr)).collect();
    for (interp_depth, cm_ptr) in before {
        if let Some(at) = survivors.iter().position(|&s| s == (interp_depth, cm_ptr)) {
            survivors.swap_remove(at);
            continue;
        }
        let Some(index) = interp_depth.checked_sub(1) else {
            continue;
        };
        if view.stale.iter().any(|&(at, _)| at == index) {
            continue;
        }
        let bci = overrides
            .get(&(index as usize)) // Widening: a frame index
            .map_or(-1, |(bci, _)| i64::from(*bci));
        view.stale.push((index, bci));
    }
    // `chain`: the activation's JIT entry-chain index (wave 47); `None` for
    // an inlined callee.
    let row = |label: &str,
               class_id: u32,
               bci: i64,
               below: u32,
               chain: Option<u32>|
     -> Option<BlockedCompiledRow> {
        let (owner_and_name, descriptor) = label.rsplit_once(':')?;
        let (_, name) = owner_and_name.rsplit_once('.')?;
        if class_id == 0 || name.is_empty() || descriptor.is_empty() {
            return None;
        }
        Some(BlockedCompiledRow {
            below,
            class_id,
            method_id: crate::debug::jdwp_method_id(name, descriptor),
            bci,
            chain,
        })
    };
    let chain_rows = |rows: &mut Vec<BlockedCompiledRow>,
                      chain: &[crate::jit::conservative_roots::InlinedLevel],
                      below: u32| {
        for level in chain.iter().rev() {
            match row(
                &*level.label,
                level.class_id,
                i64::from(level.bci),
                below,
                None,
            ) {
                Some(r) => rows.push(r),
                None => break,
            }
        }
    };
    let mut next = 0usize;
    for index in 0..frames.len() {
        // Cast: a frame index fits u32 (the interpreter's depth limit).
        let below = index as u32;
        while next < kept.len() && kept[next].interp_depth as usize <= index {
            let f = &kept[next];
            if let Some(r) = row(
                &*f.label,
                f.owner_class_id,
                i64::from(f.bci),
                below,
                Some(f.chain_index),
            ) {
                view.rows.push(r);
                // Wave 120 (L2): its home-slot locals, for an activation with
                // nothing inlined at its point (an inlined callee's level
                // stands above it and has no home slots of its own).
                if let (Some(reader), true) = (read_homes, f.inline_chain.is_empty()) {
                    if let Some(values) = read_compiled_homes(f, reader) {
                        // Cast: a row index fits u32 (the depth limit).
                        view.homes.push((
                            (view.rows.len() - 1) as u32,
                            values,
                            compiled_home_body(f),
                        ));
                    }
                }
                chain_rows(&mut view.rows, &f.inline_chain, below);
            }
            next += 1;
        }
        if let Some((_, chain)) = overrides.get(&index) {
            chain_rows(&mut view.rows, chain, below + 1);
        }
    }
    // Cast: a frame count fits u32 (the interpreter's depth limit).
    let top = frames.len() as u32;
    for f in &kept[next..] {
        if let Some(r) = row(
            &*f.label,
            f.owner_class_id,
            i64::from(f.bci),
            top,
            Some(f.chain_index),
        ) {
            view.rows.push(r);
            if let (Some(reader), true) = (read_homes, f.inline_chain.is_empty()) {
                if let Some(values) = read_compiled_homes(f, reader) {
                    // Cast: a row index fits u32 (the depth limit).
                    view.homes.push((
                        (view.rows.len() - 1) as u32,
                        values,
                        compiled_home_body(f),
                    ));
                }
            }
            chain_rows(&mut view.rows, &f.inline_chain, top);
        }
    }
    view
}

/// Remove thread `tid`'s published frame snapshot, answering the object ids
/// it pinned; the caller unpins them ([`publish_debugger_frames`],
/// [`park_for_debugger`]).
#[cfg(feature = "experimental-debug")]
pub(in crate::runtime::interpreter) fn withdraw_debugger_frames(ds: &mut crate::debug::DebugState, tid: u64) -> Vec<u64> {
    ds.thread_frames.remove(&tid);
    if !ds.opaque_frames.is_empty() {
        ds.opaque_frames.retain(|&(t, _), _| t != tid);
    }
    // Wave 138 (L1): the proxy rows among them go with them.
    if !ds.proxy_frames.is_empty() {
        ds.proxy_frames.retain(|&(t, _)| t != tid);
    }
    // Wave 123 (L2): the compiled-home rows' bodies go with the snapshot;
    // their `cm_ptr`s are valid only while it stands.
    if !ds.compiled_home_rows.is_empty() {
        ds.compiled_home_rows.retain(|&(t, _), _| t != tid);
    }
    let mut pinned = Vec::new();
    ds.frame_locals.retain(|&(t, _), locals| {
        if t == tid {
            pinned.extend(
                locals
                    .iter()
                    .filter_map(|v| v.as_object_id())
                    .filter(|&id| id != 0),
            );
        }
        t != tid
    });
    pinned
}

/// Park a debugger-suspended thread until JDWP resumes it
/// (`DebugState::is_thread_suspended` turns false: its suspend count is back
/// at zero — wave 7 — after as many `ThreadReference.Resume` /
/// `VirtualMachine.Resume` commands as suspensions, or at a disconnect).
///
/// The thread stays a mutator the collector can stop: every poll answers a
/// stop-the-world request through `safepoint_check`, exactly as the loop top
/// does, so its frames are scanned and remapped like any thread at a
/// safepoint. The poll period (1 ms, longer on hosts with a coarse sleep
/// quantum) bounds how long a collection waits for a suspended thread; it is
/// a debugger-session cost only. On resume the published snapshot is
/// withdrawn, since the frames it describes start moving again.
///
/// While parked the thread also runs the work the JDWP server hands it
/// (wave 8, `debug::run_on_parked_thread`): a debugger invocation — on this
/// thread, as JDWP specifies, its frames pushed above the parked one — or a
/// heap read or write. Being a registered mutator is what makes that work
/// safe from collections; see `debug::run_on_parked_thread`. Heap work runs
/// with the debugger hooks suppressed (`debug::run_parked_task`); an
/// invocation does not (wave 9): a stop inside it parks the thread here
/// again, one level deeper, and the debugger can inspect, invoke and resume
/// there. When that inner park ends it withdraws the snapshot, so this level
/// publishes its own again (at `pc`, the stop it parked at) for as long as
/// the thread stays suspended here.
#[cfg(feature = "experimental-debug")]
#[cold]
pub(in crate::runtime::interpreter) fn park_for_debugger(shared: &SharedVm, thread: &mut JvmThread, tid: u64, pc: usize) {
    park_for_debugger_under(shared, thread, tid, pc, None);
}

