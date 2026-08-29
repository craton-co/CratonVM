// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! deopt-osr — virtual-object (scalar-replaced) re-materialization for
//! real-frame deopt / OSR-exit.
//!
//! When escape analysis (`jit::escape_analysis`) proves an object non-escaping
//! on the JIT fast path, its allocation is elided. If a later guard deopts, the
//! reconstructed interpreter frame can carry `FrameValue::VirtualObject`
//! placeholders that must be turned back into real heap objects before the
//! interpreter resumes. That operation is heap-mutating and GC-coordinated, so —
//! unlike the abstract `FrameState` model in `jit/src/deopt.rs` — it lives in
//! the vm crate, where the live heap/allocator and the thread's GC-root set are
//! reachable. The jit-crate `materialize_virtual_objects` panic stub points here.
//!
//! STATUS: Phases 1+2 (Steps 5+6) implemented AND wired into the resume sink
//! (Workstream A). [`materialize_virtual_objects`] is now called from
//! `interpreter::build_deopt_frame_inner`: a reconstructed deopt frame carrying
//! `VirtualObject`/`VirtualObjectRef` slots is materialized into a real heap
//! object graph and resumed at the trapping bci, under `CRATONVM_DEOPT_REAL`
//! (default-off). The live path passes `keep_pins = true`, so the shells stay
//! rooted across the frame build + push (the sink owns their release). No
//! *production* emitter writes virtual deopt slots yet (the x64 snapshot records
//! only Register/StackSlot provenance), so the path is reachable today only via
//! the acceptance tests and `CRATONVM_DEOPT_VERIFY`; the `#![allow(dead_code)]`
//! covers test-only helpers (`count_virtual_objects`). Implemented:
//!   * [`TempRootScope`] — RAII temporary GC-root set over `native_pin_roots`.
//!   * [`materialize_virtual_objects`] — two-phase, cycle-safe. **Phase 1**
//!     allocates + header-inits + immediately-roots a shell for every distinct
//!     virtual object (by [`VirtualObjectState::id`]) reachable from the frame;
//!     **Phase 2** fills each shell's fields (primitive / real-object /
//!     nested-virtual / `VirtualObjectRef`) GC-barrier-correct like `putfield`,
//!     then rewrites the frame's top-level slots to real `Object`s.
//! See `docs/feature-designs/deopt-osr.md`.
#![allow(dead_code)]

use std::collections::BTreeMap;

use cratonvm_jit::deopt::{FrameValue, ReconstructedFrame, VirtualObjectState};
use cratonvm_types::ClassId;

use crate::error::{MethodCallFailed, VmError};
use crate::runtime::interpreter::{alloc_object_shared, maybe_gc_forced_pub};
use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;
use cratonvm_gc::heap::ArrayElementType;

/// RAII temporary GC-root scope for virtual-object materialization.
///
/// Materialization allocates a *graph* of shell objects one at a time, and each
/// allocation may trigger a GC. A half-built shell not yet referenced from any
/// interpreter root would be reclaimed by a GC fired during the *next*
/// allocation. The fix (the load-bearing invariant from `jit/src/deopt.rs`,
/// `materialize_virtual_objects` docs) is to register every freshly allocated
/// shell as a GC root the instant it is allocated, in a temporary root set owned
/// by the deopt operation, and release the whole set only once the reconstructed
/// interpreter frame is itself a root.
///
/// This wraps the thread's `native_pin_roots` vector — the same precise,
/// moving-GC-aware root set used to pin arguments across native calls (the
/// collector forwards its entries in place). The scope records the vector's
/// length at creation and truncates back to it on drop, so every shell pinned
/// via [`TempRootScope::alloc_shell`] is released together, including on the
/// panic / early `?`-return paths.
struct TempRootScope<'a> {
    thread: &'a mut JvmThread,
    /// `native_pin_roots.len()` at scope creation; the unpin watermark.
    base: usize,
}

impl<'a> TempRootScope<'a> {
    fn new(thread: &'a mut JvmThread) -> Self {
        let base = thread.native_pin_roots.len();
        Self { thread, base }
    }

    /// Allocate a header-initialised shell of `class_id` (with `num_fields`
    /// zeroed slots) and IMMEDIATELY pin it as a temporary GC root.
    ///
    /// Ordering is load-bearing: `alloc_object_shared` may itself GC on the slow
    /// path, but any shells pinned by *earlier* `alloc_shell` calls are already
    /// roots and survive; the new shell is pinned before control returns, so the
    /// *next* allocation cannot reclaim it.
    fn alloc_shell(
        &mut self,
        shared: &SharedVm,
        class_id: ClassId,
        num_fields: usize,
    ) -> Result<ObjectRef, MethodCallFailed> {
        let obj = alloc_object_shared(shared, self.thread, class_id, num_fields)?;
        self.thread.native_pin_roots.push(obj);
        Ok(obj)
    }

    /// The array analogue of [`Self::alloc_shell`]: a zeroed array of `length`
    /// elements of `element_type`, pinned the instant it exists.
    ///
    /// Same ordering contract, for the same reason -- `gc_alloc_array` can GC
    /// on its slow path, and every shell pinned before this call is already a
    /// root.
    fn alloc_array_shell(
        &mut self,
        shared: &SharedVm,
        element_type: ArrayElementType,
        length: usize,
    ) -> Result<ObjectRef, MethodCallFailed> {
        let arr = crate::runtime::interpreter::gc_alloc_array(
            shared,
            self.thread,
            ClassId::new(0),
            element_type,
            length,
        )?;
        self.thread.native_pin_roots.push(arr);
        Ok(arr)
    }

    /// Mutable access to the underlying thread (Phase-1 alloc / forced GC).
    fn thread(&mut self) -> &mut JvmThread {
        self.thread
    }
}

impl Drop for TempRootScope<'_> {
    fn drop(&mut self) {
        // Release every shell pinned during materialization. The caller must not
        // drop the scope until the reconstructed interpreter Frame is built and
        // itself a GC root (its locals/stack are scanned by the normal root
        // walk), so the now-materialized objects stay reachable afterwards.
        self.thread.native_pin_roots.truncate(self.base);
    }
}

/// Re-materialize every scalar-replaced object reachable from a reconstructed
/// deopt frame as a real heap object, fill its fields, and rewrite the frame's
/// top-level slots to the resulting real references. Two-phase and cycle-safe.
///
/// Returns one `(slot_index, heap_address)` per *top-level* virtual object —
/// indexed locals first, then operand stack — for the caller's convenience (the
/// frame is also rewritten in place).
///
/// **Phase 1 (Step 5):** collect every distinct virtual object by
/// [`VirtualObjectState::id`] (traversing locals/stack and recursively through
/// `VirtualObject` field graphs; `VirtualObjectRef(id)` references an object
/// without defining one, so it terminates cycles), then allocate + header-init +
/// immediately-root a shell per id via [`TempRootScope::alloc_shell`]. Shell
/// addresses are taken from the pin set *after* allocation (and the optional
/// stress GC), so they are correct under a moving collector.
///
/// **Phase 2 (Step 6):** fill each shell's fields — primitive stores directly,
/// `Object` / `VirtualObject` / `VirtualObjectRef` store a real reference
/// (resolving nested / shared / cyclic objects via the Phase-1 shell map), each
/// through the `putfield` write barrier (SATB pre on the overwritten ref + post
/// card barrier inside `set_field`). Then rewrite the frame's top-level
/// `VirtualObject` / `VirtualObjectRef` slots to `Object(addr)`.
///
/// `stress_gc` forces a full GC after each shell is allocated+rooted, exercising
/// the "root before the next alloc" invariant directly; a production caller
/// passes the `CRATONVM_GC_STRESS` gate (`false` on the normal path, since
/// forcing a GC at every deopt would be ruinous).
///
/// `keep_pins` controls the temporary GC-root lifetime. When `false` (unit
/// tests), the shell pins are released before returning (the frame's rewritten
/// `Object` slots are the only references — fine for a test that inspects the
/// heap immediately). When `true` (the live resume sink), the pins are LEFT
/// installed in `native_pin_roots` and the CALLER owns their release: the sink
/// holds them across the GC-capable frame build + `push_frame_and_fire_entry`
/// and truncates only AFTER the resumed frame is on `thread.frames` (itself a GC
/// root) — so the materialized shells are rooted continuously from allocation
/// until the frame roots them, with no unrooted window. On the error path the
/// pins are always released (the scope drops normally before the early return),
/// regardless of `keep_pins`.
///
/// Returns an error only on allocation failure or a malformed frame (an
/// unresolved / unsupported field value, or a `VirtualObjectRef` to an unknown
/// id) — the caller then falls back to the safe re-run path.
pub(crate) fn materialize_virtual_objects(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame: &mut ReconstructedFrame,
    stress_gc: bool,
    keep_pins: bool,
) -> Result<Vec<(usize, u64)>, MethodCallFailed> {
    // Collect the virtual-object graph: id -> canonical state. Cloning releases
    // the borrow on `frame` so its slots can be rewritten at the end.
    let mut states: BTreeMap<usize, VirtualObjectState> = BTreeMap::new();
    collect_virtual_objects(&frame.locals, &mut states);
    collect_virtual_objects(&frame.stack, &mut states);
    // Phase C (monitors): a held monitor's object may be a scalar-replaced one;
    // its defining `VirtualObject` lives in the locals (the `synchronized(obj)`
    // temp), so it is already collected above — but collect from the monitor
    // objects too in case the only reference is the monitor itself.
    let monitor_objs: Vec<FrameValue> = frame.monitors.iter().map(|m| m.object.clone()).collect();
    collect_virtual_objects(&monitor_objs, &mut states);
    if states.is_empty() {
        return Ok(Vec::new());
    }
    // Engagement census (`cratonvm_types::scalar_deopt_census`). Counted HERE,
    // past the empty check, so the number means "objects the compiler deleted
    // and a deopt had to put back" — the only evidence that a soak exercised
    // the descriptor rather than merely the deletion.
    cratonvm_types::scalar_deopt_census::note_materialized(states.len() as u64);

    // ENGAGEMENT COUNTER. `CRATONVM_SCALAR_DEOPT` has two halves, and only one
    // of them is exercised by simply running a workload: the producer elides
    // more allocations (every run pays that), while the CONSUMER -- rebuilding
    // an elided object at a precise resume -- only runs if a deopt actually
    // lands on a frame that names one. A soak that never reaches this line has
    // not tested the half that can hand the interpreter a wrong object, so a
    // green result from it would be vacuous.
    //
    // One line per materialization, under the flag the feature already has, so
    // `grep -c` is the whole instrument.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some() {
        let shells: usize = states.len();
        let fields: usize = states.values().map(|s| s.num_fields).sum();
        let arrays: usize = states
            .values()
            .filter(|s| s.array_element_type.is_some())
            .count();
        eprintln!(
            "[DBG_SCALAR_DEOPT] MATERIALIZE bci={} objects={shells} (arrays={arrays}) slots={fields}",
            frame.bci,
        );
    }

    let mut scope = TempRootScope::new(thread);

    // Phase 1: a pinned shell per distinct id (ordered for determinism). Record
    // the pin index so post-(stress-)GC addresses can be read back from the
    // in-place-forwarded pin set.
    let mut pin_of: BTreeMap<usize, usize> = BTreeMap::new();
    for (&id, state) in &states {
        let pin_index = scope.thread().native_pin_roots.len();
        // An ARRAY state carries its element atype and its LENGTH (in
        // `num_fields`); an object state carries a class id and a field count.
        // The two are different allocations and must not be confused -- an
        // object shell where the frame expects an array is a wrong-shaped
        // header the collector would walk as fields.
        match state.array_element_type {
            Some(atype) => {
                let element_type = match atype_to_element_type(atype) {
                    Some(t) => t,
                    // Only primitive atypes are ever emitted (a reference array
                    // is refused in `escape_analysis`), so this is unreachable
                    // -- and it refuses rather than guesses, which sends the
                    // caller down the safe whole-method re-run path.
                    None => {
                        return Err(MethodCallFailed::InternalError(VmError::Internal {
                            message: format!(
                                "deopt materialize: virtual array with unsupported atype {atype}"
                            ),
                        }))
                    }
                };
                scope.alloc_array_shell(shared, element_type, state.num_fields)?;
            }
            None => {
                scope.alloc_shell(shared, ClassId::new(state.class_id), state.num_fields)?;
            }
        }
        pin_of.insert(id, pin_index);
        if stress_gc {
            // Force a GC with the just-pinned shell (and all prior shells) rooted:
            // proves the temp-root set keeps a half-built object graph alive across
            // a collection, and that a moving collector forwards the pinned entries
            // in place.
            let t = scope.thread();
            maybe_gc_forced_pub(shared, t);
        }
    }

    // Stable id -> shell map at post-(stress-)GC addresses. No GC runs after this
    // point — Phase-2 field stores do not allocate — so these stay valid.
    let shells: BTreeMap<usize, ObjectRef> = pin_of
        .iter()
        .map(|(&id, &pin)| (id, scope.thread().native_pin_roots[pin]))
        .collect();

    // Phase 2: fill each shell's fields, GC-barrier-correct like putfield.
    for (&id, state) in &states {
        let shell = shells[&id];
        for (field_index, fv) in state.field_values.iter().enumerate() {
            let value = field_value_to_value(fv, &shells)?;
            if state.array_element_type.is_some() {
                // An array's slots are ELEMENTS. `set_field` would write them
                // at the object field offsets, which for an array is the
                // length word and past it. No pre-barrier: only primitive
                // element types are emitted, so no reference is overwritten.
                let _ = shared.mem.heap.set_array_element(shell, field_index, value);
            } else {
                store_field_barriered(shared, shell, field_index, value);
            }
        }
    }

    // Rewrite the frame's top-level VirtualObject / VirtualObjectRef slots to the
    // real Object references (locals first, then stack); collect (flat slot, addr).
    let mut result = Vec::new();
    let mut slot = 0usize;
    for fv in frame.locals.iter_mut() {
        if let Some(id) = virtual_id_of(fv) {
            let addr = shells[&id].as_ptr() as usize as u64;
            *fv = FrameValue::Object(addr);
            result.push((slot, addr));
        }
        slot += 1;
    }
    for fv in frame.stack.iter_mut() {
        if let Some(id) = virtual_id_of(fv) {
            let addr = shells[&id].as_ptr() as usize as u64;
            *fv = FrameValue::Object(addr);
            result.push((slot, addr));
        }
        slot += 1;
    }
    // Phase C: rewrite each held monitor's object to the materialized shell so the
    // resume can re-acquire the lock on the real heap object. (Not added to
    // `result` — `result` tracks frame slots; monitors are relocked separately.)
    for m in frame.monitors.iter_mut() {
        if let Some(id) = virtual_id_of(&m.object) {
            let addr = shells[&id].as_ptr() as usize as u64;
            m.object = FrameValue::Object(addr);
        }
    }

    if keep_pins {
        // Live resume: leave the shell pins installed. The caller holds them
        // across the frame build + push and truncates `native_pin_roots` only
        // after the resumed frame is on `thread.frames` — so the shells are
        // rooted continuously, with no unrooted window across a GC-capable build.
        std::mem::forget(scope);
    } else {
        // Tests: release the shell pins now (the rewritten Object slots remain).
        drop(scope);
    }
    Ok(result)
}

/// The [`ArrayElementType`] for a JVM `newarray` atype, or `None` for an atype
/// that is not a primitive element kind. Mirrors `jit_newarray`'s table, which
/// is the one the compiled `newarray` uses -- a materialized array has to have
/// the same element kind the compiled code would have allocated.
fn atype_to_element_type(atype: u8) -> Option<ArrayElementType> {
    Some(match atype {
        4 => ArrayElementType::Boolean,
        5 => ArrayElementType::Char,
        6 => ArrayElementType::Float,
        7 => ArrayElementType::Double,
        8 => ArrayElementType::Byte,
        9 => ArrayElementType::Short,
        10 => ArrayElementType::Int,
        11 => ArrayElementType::Long,
        _ => return None,
    })
}

/// The virtual-object id a top-level `FrameValue` refers to, if any.
fn virtual_id_of(fv: &FrameValue) -> Option<usize> {
    match fv {
        FrameValue::VirtualObject(state) => Some(state.id),
        FrameValue::VirtualObjectRef(id) => Some(*id),
        _ => None,
    }
}

/// Collect every distinct virtual object (keyed by id) reachable from `values`,
/// recursing through `VirtualObject` field graphs. `VirtualObjectRef(id)`
/// references an object without defining one (the cycle / sharing terminator),
/// so it is not recursed into; the canonical `VirtualObject(state)` for each id
/// carries the real fields.
fn collect_virtual_objects(values: &[FrameValue], out: &mut BTreeMap<usize, VirtualObjectState>) {
    for v in values {
        if let FrameValue::VirtualObject(state) = v {
            if out.contains_key(&state.id) {
                continue;
            }
            out.insert(state.id, state.clone());
            collect_virtual_objects(&state.field_values, out);
        }
    }
}

/// Resolve a scalar-replaced object's field value to a runtime [`Value`] for
/// storing into the shell. `VirtualObject` / `VirtualObjectRef` resolve to the
/// already-allocated shell for that id (handles nesting, sharing, and cycles).
fn field_value_to_value(
    fv: &FrameValue,
    shells: &BTreeMap<usize, ObjectRef>,
) -> Result<Value, MethodCallFailed> {
    let value = match fv {
        FrameValue::Int(i) => Value::Int(*i as i32),
        FrameValue::Long(l) => Value::Long(*l),
        // Cast: raw IEEE-754 bits -> f32/f64 (deopt-osr P2).
        FrameValue::Float(bits) => Value::Float(f32::from_bits(*bits as u32)),
        FrameValue::Double(bits) => Value::Double(f64::from_bits(*bits)),
        FrameValue::Object(addr) => Value::Object(object_ref_from_addr(*addr)),
        FrameValue::VirtualObject(state) => Value::Object(Some(shell_for(shells, state.id)?)),
        FrameValue::VirtualObjectRef(id) => Value::Object(Some(shell_for(shells, *id)?)),
        FrameValue::Undefined => Value::Int(0),
        // Unresolved machine forms (Register / RegisterLong / Xmm* / StackSlot* —
        // resolved in-stub before the sink) and `Unsupported` must never reach a
        // materialized field. Refuse so the caller falls back to the safe re-run
        // path rather than store garbage.
        other => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!("deopt materialize: unsupported field value {other:?}"),
            }));
        }
    };
    Ok(value)
}

/// Look up the Phase-1 shell for a virtual-object id, or error (a malformed
/// frame referencing an undefined object).
fn shell_for(
    shells: &BTreeMap<usize, ObjectRef>,
    id: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    shells.get(&id).copied().ok_or_else(|| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("deopt materialize: field references unknown virtual object id {id}"),
        })
    })
}

/// Reconstruct an `ObjectRef` from a raw frame address (`0` == null).
fn object_ref_from_addr(addr: u64) -> Option<ObjectRef> {
    if addr == 0 {
        None
    } else {
        // SAFETY: `addr` is a live heap object address captured in the deopt
        // frame (an already-real `Object` field). `from_raw` only debug-asserts
        // non-null / alignment.
        Some(unsafe { ObjectRef::from_raw(addr as usize as *mut u8) })
    }
}

/// Store `value` into `obj`'s field `index`, GC-barrier-correct exactly like the
/// interpreter's `putfield` (mirrors `Vm::set_instance_field`): an SATB
/// pre-barrier on the overwritten reference (a no-op for a fresh shell's zero
/// field, but kept for correctness/future-proofing), then `set_field`, which
/// fires the post / card-marking barrier internally.
fn store_field_barriered(shared: &SharedVm, obj: ObjectRef, index: usize, value: Value) {
    let old = shared.mem.heap.get_field(obj, index);
    if let Value::Object(Some(old_ref)) = old {
        shared
            .mem
            .heap
            .write_barrier_pre(std::ptr::null_mut(), old_ref);
    }
    shared.mem.heap.set_field(obj, index, value);
}

/// Count top-level `FrameValue::VirtualObject` occurrences in a frame's locals +
/// operand stack. NB: the number of *shells* the materializer allocates is the
/// number of *distinct* `VirtualObjectState::id`s in the whole reachable graph
/// (see [`collect_virtual_objects`]), which differs once nested or shared
/// objects are involved; this helper is the simple top-level count used by the
/// unit tests.
fn count_virtual_objects(frame: &ReconstructedFrame) -> usize {
    frame
        .locals
        .iter()
        .chain(frame.stack.iter())
        .filter(|v| matches!(v, FrameValue::VirtualObject(_)))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    /// A scalar-replaced object placeholder: id `id`, class `class_id`,
    /// `num_fields` default-`Undefined` fields.
    fn vobj(id: usize, class_id: u32, num_fields: usize) -> FrameValue {
        FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id,
            class_id,
            num_fields,
            field_values: vec![FrameValue::Undefined; num_fields],
        })
    }

    fn frame_of(locals: Vec<FrameValue>, stack: Vec<FrameValue>) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: "T.m".to_string(),
            bci: 0,
            locals,
            stack,
            monitors: Vec::new(),
            caller_frames: Vec::new(),
        }
    }

    /// Flat slot accessor over locals-then-stack (test helper).
    fn frame_slot(frame: &ReconstructedFrame, slot: usize) -> &FrameValue {
        if slot < frame.locals.len() {
            &frame.locals[slot]
        } else {
            &frame.stack[slot - frame.locals.len()]
        }
    }

    #[test]
    fn counts_virtual_objects_across_locals_and_stack() {
        let frame = frame_of(
            vec![FrameValue::Int(7), vobj(0, 1, 0)],
            vec![vobj(1, 1, 0), FrameValue::Int(3), vobj(2, 1, 0)],
        );
        assert_eq!(count_virtual_objects(&frame), 3);
    }

    #[test]
    fn counts_zero_when_no_virtual_objects() {
        let frame = frame_of(
            vec![FrameValue::Int(1), FrameValue::Undefined],
            vec![FrameValue::Int(2)],
        );
        assert_eq!(count_virtual_objects(&frame), 0);
    }

    /// Step-5: shells materialize as real, distinct, header-valid objects that
    /// survive a forced GC while temporarily rooted; the frame's top-level slots
    /// are rewritten to real `Object`s.
    #[test]
    fn shells_materialize_and_survive_forced_gc() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");

        // 3 virtual objects at flat slots 1, 2, 3 (locals first, then stack).
        let mut frame = frame_of(
            vec![FrameValue::Int(1), vobj(0, 0, 2), vobj(1, 0, 0)],
            vec![vobj(2, 0, 3), FrameValue::Int(9)],
        );
        assert_eq!(count_virtual_objects(&frame), 3);

        let pin_base = thread.native_pin_roots.len();
        let mapped = materialize_virtual_objects(
            &shared,
            &mut thread,
            &mut frame,
            /* stress_gc */ true,
            /* keep_pins */ false,
        )
        .expect("materialization should succeed");

        assert_eq!(mapped.len(), 3);
        assert_eq!(
            mapped.iter().map(|&(s, _)| s).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        let mut seen = std::collections::HashSet::new();
        for &(slot, addr) in &mapped {
            assert_ne!(addr, 0, "shell address must be non-null");
            assert!(seen.insert(addr), "shells must be distinct objects");
            // Frame slot rewritten to the real object.
            assert_eq!(frame_slot(&frame, slot), &FrameValue::Object(addr));
            let obj = unsafe { ObjectRef::from_raw(addr as usize as *mut u8) };
            assert_eq!(
                shared.mem.heap.class_id_of(obj),
                ClassId::new(0),
                "shell header must carry the requested class id post-GC"
            );
        }
        assert_eq!(thread.native_pin_roots.len(), pin_base);
    }

    /// Step-6: a virtual object's primitive + already-real-object fields are
    /// stored into the shell; an `Undefined` field defaults to 0.
    #[test]
    fn materializes_primitive_and_object_fields() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");

        // A pre-existing real object to reference from a field.
        let real = shared.mem.heap.alloc_object(ClassId::new(7), 0);
        let real_addr = real.as_ptr() as usize as u64;

        // One virtual object (id 0, class 5, 3 fields): [Int(42), Object(real), Undefined].
        let vo = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 0,
            class_id: 5,
            num_fields: 3,
            field_values: vec![
                FrameValue::Int(42),
                FrameValue::Object(real_addr),
                FrameValue::Undefined,
            ],
        });
        let mut frame = frame_of(vec![vo], vec![]);

        let mapped = materialize_virtual_objects(
            &shared,
            &mut thread,
            &mut frame,
            /* stress_gc */ false,
            /* keep_pins */ false,
        )
        .expect("materialization should succeed");
        assert_eq!(mapped.len(), 1);
        let (slot, addr) = mapped[0];
        assert_eq!(slot, 0);
        assert_eq!(frame.locals[0], FrameValue::Object(addr));

        let shell = unsafe { ObjectRef::from_raw(addr as usize as *mut u8) };
        assert_eq!(shared.mem.heap.class_id_of(shell), ClassId::new(5));
        assert_eq!(shared.mem.heap.get_field(shell, 0), Value::Int(42));
        assert_eq!(
            shared.mem.heap.get_field(shell, 1),
            Value::Object(Some(real))
        );
        assert_eq!(shared.mem.heap.get_field(shell, 2), Value::Int(0)); // Undefined -> 0
    }

    /// Step-6 cyclic graph: two objects referencing each other materialize to two
    /// heap objects whose fields point at each other — the two-phase
    /// shells-first design makes the back-edge resolvable. Run with stress GC so
    /// the half-built graph is proven to survive a collection.
    #[test]
    fn materializes_two_object_cycle() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");

        // A (id 0).field0 -> B ;  B (id 1).field0 -> A
        let a = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 0,
            class_id: 1,
            num_fields: 1,
            field_values: vec![FrameValue::VirtualObjectRef(1)],
        });
        let b = FrameValue::VirtualObject(VirtualObjectState {
            array_element_type: None,
            id: 1,
            class_id: 1,
            num_fields: 1,
            field_values: vec![FrameValue::VirtualObjectRef(0)],
        });
        let mut frame = frame_of(vec![a, b], vec![]);

        let mapped = materialize_virtual_objects(
            &shared,
            &mut thread,
            &mut frame,
            /* stress_gc */ true,
            /* keep_pins */ false,
        )
        .expect("materialization should succeed");
        assert_eq!(mapped.len(), 2);

        let addr_a = mapped.iter().find(|&&(s, _)| s == 0).unwrap().1;
        let addr_b = mapped.iter().find(|&&(s, _)| s == 1).unwrap().1;
        let oa = unsafe { ObjectRef::from_raw(addr_a as usize as *mut u8) };
        let ob = unsafe { ObjectRef::from_raw(addr_b as usize as *mut u8) };

        // The cycle is wired: A.field0 == B and B.field0 == A.
        assert_eq!(shared.mem.heap.get_field(oa, 0), Value::Object(Some(ob)));
        assert_eq!(shared.mem.heap.get_field(ob, 0), Value::Object(Some(oa)));
        // Frame slots rewritten.
        assert_eq!(frame.locals[0], FrameValue::Object(addr_a));
        assert_eq!(frame.locals[1], FrameValue::Object(addr_b));
    }

    /// Phase C (monitors): a held monitor whose object is a scalar-replaced one
    /// (referenced by `VirtualObjectRef`) is rewritten to the SAME materialized
    /// shell as the local that defines it, so the resume can relock the real object.
    #[test]
    fn materialize_rewrites_held_monitor_object() {
        use cratonvm_jit::deopt::MonitorInfo;
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(ThreadId(0), "test");

        // local 1 defines virtual object id 0; a held monitor references it.
        let mut frame = ReconstructedFrame {
            method_key: "T.m".to_string(),
            bci: 0,
            locals: vec![FrameValue::Int(0), vobj(0, 1, 0)],
            stack: vec![],
            monitors: vec![MonitorInfo {
                object: FrameValue::VirtualObjectRef(0),
                lock_depth: 2,
            }],
            caller_frames: Vec::new(),
        };

        let mapped = materialize_virtual_objects(
            &shared,
            &mut thread,
            &mut frame,
            /* stress_gc */ true,
            /* keep_pins */ false,
        )
        .expect("materialization should succeed");

        // The defining local was materialized to a real Object.
        let local_addr = mapped
            .iter()
            .find(|&&(s, _)| s == 1)
            .expect("local 1 materialized")
            .1;
        assert_eq!(frame.locals[1], FrameValue::Object(local_addr));
        // The monitor's object was rewritten to the SAME shell; depth preserved.
        assert_eq!(frame.monitors[0].object, FrameValue::Object(local_addr));
        assert_eq!(frame.monitors[0].lock_depth, 2);
    }
}
