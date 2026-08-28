// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The per-opcode dispatch: `execute_instruction`.
//!
//! One function, ~3,900 lines, one arm per JVM opcode. It was the largest
//! single item in `interpreter.rs` and it is the one a reader most often
//! wants open on its own, because almost every question about interpreter
//! behaviour is really a question about one of its arms.
//!
//! Opcodes that need more than an arm delegate outward: invocation to
//! `interpreter/invoke.rs` and its dispatch siblings, field access to
//! `interpreter/field_access.rs`, casts to `interpreter/typecheck.rs`,
//! allocation and GC to the frame loop in `interpreter.rs`. What is left
//! here is the decode-and-do.

use super::*;
use super::site_cache::{site_stats, CastSiteCache, ClassSiteCache, ResolvedNewSite};

/// `CRATONVM_JIT_NO_NEW_SITE_CACHE=1` — withdraw the per-thread `new`-site
/// cache, so one binary can be A/B'd against its own pre-change behaviour.
/// Comparing against a separately built branch would confound this with
/// everything else that landed.
///
/// Default-ON, unlike its field and method siblings. Those are off because
/// their hit rate does not generalise — a Spring Boot class measured 52%, and a
/// miss there still leaves the authoritative path to run. This one is on for a
/// different reason: what a `new` site miss re-derives is not a revalidation
/// but a `String` allocation plus a full class resolution plus four
/// `class_manager` read acquisitions, and a `new` executed once is a `new`
/// whose class was just loaded — the very case whose resolution cost the most.
/// `CRATONVM_DBG=field-site` reports `new: hit/miss/fill/reject_loader` beside
/// the other two arms; read it before quoting a timing number.
/// Also withdrawn whenever one of the `new`-arm traces is armed. A hit skips
/// the class-name derivation those three print from, so leaving the cache on
/// would make each of them report a SUBSET of the `new` sites it is being asked
/// about — an instrument that silently under-reports is worse than none, and
/// these three exist precisely to answer loader- and class-identity questions
/// where a missing line reads as an absent event. They are debug gates, so a
/// run that sets one is not a run whose throughput anybody is measuring.
fn new_site_cache_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_NEW_SITE_CACHE").is_none()
            && !crate::runtime::env_cache::dbg_h2trace()
            && !crate::runtime::env_cache::dbg_loader_trace()
            && !crate::runtime::env_cache::nsee_trace()
    })
}

/// `CRATONVM_JIT_NO_CAST_SITE_CACHE` — opt out of the per-thread resolved
/// `checkcast`/`instanceof` target cache.
///
/// Default-ON, and gated the same way `new_site_cache_enabled` is, including the
/// diagnostic exclusions: those tracers print the resolved class NAME on every
/// execution, and a cache hit never materializes one, so a hit would silence
/// them. A lever that quietly blinds a diagnostic is worse than one that costs
/// a lock.
fn cast_site_cache_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_CAST_SITE_CACHE").is_none()
            && !crate::runtime::env_cache::dbg_h2trace()
            && !crate::runtime::env_cache::dbg_loader_trace()
    })
}

/// Does this referencing class resolve constant-pool class names in a loader
/// namespace of its own?
///
/// The admission test for [`ClassSiteCache`]. `true` means
/// `resolve_class_loader_aware` may consult the initiating-resolution memo or
/// drive a user `loadClass`, and the site is refused. `false` means the answer
/// comes from the global name → `ClassId` mapping, which the entry's two epochs
/// cover completely.
///
/// Both halves are immutable for a class: a defining loader is fixed when the
/// class is defined, and the class-manager loader id it is cross-checked
/// against never becomes `UserDefined` afterwards. That is what lets the fill
/// site be the only place this is asked — a hit needs no re-check.
///
/// The side-table probe comes first because it is lock-free; the
/// `class_manager` read behind it runs once per site, on the miss that fills it.
fn referencing_class_has_loader_namespace(shared: &SharedVm, class_id: ClassId) -> bool {
    if cratonvm_native_builtins::classloader::defining_loader_for(
        shared.vm_identity,
        class_id.as_u32(),
    )
    .is_some()
    {
        return true;
    }
    matches!(
        shared.classes.class_manager.read().get_loader_id(class_id),
        Some(cratonvm_types::ClassLoaderId::UserDefined(_))
    )
}

// ---------------------------------------------------------------------------
// Deoptimisation and OSR-exit frame reconstruction
// ---------------------------------------------------------------------------
//
// Rebuilding an interpreter frame from a compiled one, verifying it before
// resuming on it, and withdrawing the assumption that failed:
// `interpreter/deopt_resume.rs`.


// ---------------------------------------------------------------------------
// Instruction dispatch
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
pub(super) fn execute_instruction(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    instruction: &Instruction,
    saved_pc: usize,
) -> Result<InstructionResult, MethodCallFailed> {
    hotpath_counts::bump(&hotpath_counts::DECODED_INSTRUCTIONS);
    match instruction {
        // -- Constants (T10.9.D direct CompactValue push) --
        Instruction::Nop => {}
        Instruction::AconstNull => thread.frames[frame_idx].stack.push_null()?,
        Instruction::IconstM1 => thread.frames[frame_idx].stack.push_int(-1)?,
        Instruction::Iconst0 => thread.frames[frame_idx].stack.push_int(0)?,
        Instruction::Iconst1 => thread.frames[frame_idx].stack.push_int(1)?,
        Instruction::Iconst2 => thread.frames[frame_idx].stack.push_int(2)?,
        Instruction::Iconst3 => thread.frames[frame_idx].stack.push_int(3)?,
        Instruction::Iconst4 => thread.frames[frame_idx].stack.push_int(4)?,
        Instruction::Iconst5 => thread.frames[frame_idx].stack.push_int(5)?,
        Instruction::Lconst0 => thread.frames[frame_idx].stack.push_long(0)?,
        Instruction::Lconst1 => thread.frames[frame_idx].stack.push_long(1)?,
        Instruction::Fconst0 => thread.frames[frame_idx].stack.push_float(0.0)?,
        Instruction::Fconst1 => thread.frames[frame_idx].stack.push_float(1.0)?,
        Instruction::Fconst2 => thread.frames[frame_idx].stack.push_float(2.0)?,
        Instruction::Dconst0 => thread.frames[frame_idx].stack.push_double(0.0)?,
        Instruction::Dconst1 => thread.frames[frame_idx].stack.push_double(1.0)?,

        Instruction::Bipush(val) => thread.frames[frame_idx].stack.push_int(*val as i32)?, // Cast: bytecode operand decoding
        Instruction::Sipush(val) => thread.frames[frame_idx].stack.push_int(*val as i32)?, // Cast: bytecode operand decoding

        Instruction::Ldc(index) => {
            // Cast: bytecode operand decoding. B5: malformed-CP ClassFormatError
            // is surfaced as a catchable Java exception, not a hard VM abort.
            execute_ldc(shared, thread, frame_idx, *index as u16)
                .map_err(|e| convert_ldc_class_format_error(shared, thread, e))?
        }
        Instruction::LdcW(index) => execute_ldc(shared, thread, frame_idx, *index)
            .map_err(|e| convert_ldc_class_format_error(shared, thread, e))?,
        Instruction::Ldc2W(index) => {
            execute_ldc2w(shared, thread, frame_idx, *index)
                .map_err(|e| convert_ldc_class_format_error(shared, thread, e))?
        }

        // -- Loads (T10.9.D direct CompactValue path) --
        Instruction::Iload(idx) => {
            let cv = thread.frames[frame_idx].get_local_compact(*idx);
            thread.frames[frame_idx].stack.push_compact_checked(cv)?;
        }
        Instruction::Lload(idx) => {
            let cv = thread.frames[frame_idx].get_local_compact(*idx);
            // Mark KIND_LONG so a NaN-tag-colliding long pops bit-exact.
            thread.frames[frame_idx]
                .stack
                .push_compact_long_checked(cv)?;
        }
        Instruction::Fload(idx) => {
            let cv = thread.frames[frame_idx].get_local_compact(*idx);
            thread.frames[frame_idx].stack.push_compact_checked(cv)?;
        }
        Instruction::Dload(idx) => {
            let cv = thread.frames[frame_idx].get_local_compact(*idx);
            thread.frames[frame_idx]
                .stack
                .push_compact_double_checked(cv)?;
        }
        Instruction::Aload(idx) => {
            // Mirror `Instruction::Astore` / fast-path `aload`: JNI may leave a
            // jobject in a reference local as `VTAG_LONG`; `push_compact` would
            // keep raw bits and the next `if_acmpeq` / `invokevirtual` can AV
            // (Letsgo after `ConfigurationClassEnhancer.enhance` in
            // `ConfigurationClassPostProcessor.enhanceConfigurationClasses`).
            let v = coerce_value_for_return_validated(
                shared,
                thread.frames[frame_idx].get_local(*idx),
                b'L',
            );
            thread.frames[frame_idx].stack.push(v)?;
        }

        // -- Array loads --
        Instruction::Iaload
        | Instruction::Faload
        | Instruction::Aaload
        | Instruction::Baload
        | Instruction::Caload
        | Instruction::Saload
        | Instruction::Laload
        | Instruction::Daload => {
            let index = thread.frames[frame_idx].stack.pop_int()?;
            // JEP 358 increment 2: `Cannot load from <T> array because
            // "<expr>" is null`. Source bytecode stack is `[..., arrayref,
            // index]` so the array is one slot below the top (depth 1). The
            // element kind comes from the opcode itself.
            let array_ref = if crate::runtime::env_cache::helpful_npe_opcodes() {
                use crate::runtime::exceptions::helpful_npe::ArrayElemKind;
                let elem = match instruction {
                    Instruction::Iaload => ArrayElemKind::Int,
                    Instruction::Laload => ArrayElemKind::Long,
                    Instruction::Faload => ArrayElemKind::Float,
                    Instruction::Daload => ArrayElemKind::Double,
                    Instruction::Aaload => ArrayElemKind::Object,
                    Instruction::Baload => ArrayElemKind::Byte,
                    Instruction::Caload => ArrayElemKind::Char,
                    _ => ArrayElemKind::Short, // Saload
                };
                let action = crate::runtime::exceptions::helpful_npe::action_array_load(elem);
                let npe_code = Arc::clone(&thread.frames[frame_idx].code);
                let npe_cid = thread.frames[frame_idx].class_id;
                let npe_mname = thread.frames[frame_idx].method_name_arc();
                let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
                let npe_bci = thread.frames[frame_idx].last_instr_pc;
                pop_object_ref_ctx_with(
                    &mut thread.frames[frame_idx].stack,
                    &shared.mem.heap,
                    || {
                        helpful_npe_opcode_message_parts(
                            shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, 1,
                        )
                    },
                )?
            } else {
                pop_object_ref_ctx(
                    &mut thread.frames[frame_idx].stack,
                    Some("Cannot load from null array"),
                )?
            };
            let value = shared
                .mem
                .heap
                .get_array_element(array_ref, index as usize) // Widening: index conversion
                .map_err(|i| {
                    if aioobe_dbg() {
                        let cls = thread.frames[frame_idx].class_name().to_string();
                        let mth = thread.frames[frame_idx].method_name().to_string();
                        let pc = thread.frames[frame_idx].pc;
                        let alen = shared.mem.heap.array_length(array_ref);
                        eprintln!(
                            "AIOOBE-LOAD class={cls} method={mth} pc={pc} idx={i} len={alen}"
                        );
                    }
                    RuntimeError::aioobe(i, shared.mem.heap.array_length(array_ref) as i32)
                })?;
            if remap_trace_on() && matches!(instruction, Instruction::Aaload) {
                if let Value::Object(Some(o)) = &value {
                    push_prov_record(o.as_ptr() as usize, "aaload");
                }
            }
            thread.frames[frame_idx].stack.push(value)?;
        }

        // -- Stores (T10.9.D direct CompactValue path) --
        Instruction::Istore(idx) | Instruction::Fstore(idx) => {
            // Direct compact round-trip — tag on the stack slot is preserved
            // through compact_to_local_slot.
            let cv = thread.frames[frame_idx].stack.pop_compact_checked()?;
            thread.frames[frame_idx].set_local_compact(*idx, cv);
        }
        // `astore` must NOT use the raw compact round-trip: JNI / invoke
        // bridges can leave a jobject as `Value::Long`; storing that as
        // VTAG_LONG corrupts reference locals (Spring slow-path after CCE;
        // fast path `astore_*` / `0x3a` already use `coerce_value_for_return`).
        Instruction::Astore(idx) => {
            let v = thread.frames[frame_idx].stack.pop()?;
            let coerced = coerce_value_for_return_validated(shared, v, b'L');
            thread.frames[frame_idx].set_local(*idx, coerced);
        }
        Instruction::Lstore(idx) => {
            // JVM spec: lstore always consumes a Long (category-2).
            // pop_long widens an Int if the stack accidentally carries one
            // (preserving the prior behaviour in set_local via encode_value);
            // the result is then stored with VTAG_LONG so downstream
            // get_local decoders see the correct Java type.
            let v = thread.frames[frame_idx].stack.pop_long()?;
            thread.frames[frame_idx].set_local(*idx, Value::Long(v));
        }
        Instruction::Dstore(idx) => {
            // JVM spec: dstore always consumes a Double.  pop_double widens
            // Int/Long/Float so bytecode that leaves a smaller numeric type
            // where a double was expected still round-trips.
            let d = thread.frames[frame_idx].stack.pop_double()?;
            thread.frames[frame_idx].set_local(*idx, Value::Double(d));
        }

        // -- Array stores --
        Instruction::Aastore => {
            // Reference array store — needs write barrier for generational GC
            // Mirror fast-path 0x53: JNI / invoke bridges may leave jobject bits as
            // `Value::Long` on the stack; both decoded and raw handlers now use
            // the same validated normalization.
            let raw_value = coerce_value_for_return_validated(
                shared,
                thread.frames[frame_idx].stack.pop()?,
                b'L',
            );
            let value = normalize_aastore_value(shared, raw_value);
            let index = thread.frames[frame_idx].stack.pop_int()?;
            let _diag_pc = thread.frames[frame_idx].pc;
            // The class and method NAMES used to be materialised here, as two
            // `String` allocations per array store, for a message that is only
            // ever built when the array reference turns out to be null. They
            // are resolved inside the closure now, from `npe_cid` and
            // `npe_mname` below — both of which this site already had to
            // capture for the JEP 358 arm. Hoisting them was not a mistake to
            // begin with: the closure cannot borrow `thread` while
            // `thread.frames[..].stack` is mutably borrowed by
            // `pop_object_ref_ctx_with`. `npe_cid` is a plain `ClassId` and
            // needs no such borrow, which is what makes the lazy form possible.
            // JEP 358 increment 2: `Cannot store to object array because
            // "<expr>" is null`. Source stack `[..., arrayref, index, value]`
            // → array at depth 2.
            let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
            let npe_code = Arc::clone(&thread.frames[frame_idx].code);
            let npe_cid = thread.frames[frame_idx].class_id;
            let npe_mname = thread.frames[frame_idx].method_name_arc();
            let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
            let npe_bci = thread.frames[frame_idx].last_instr_pc;
            let array_ref = pop_object_ref_ctx_with(
                &mut thread.frames[frame_idx].stack,
                &shared.mem.heap,
                || {
                    if jep358 {
                        let action = crate::runtime::exceptions::helpful_npe::action_array_store(
                            crate::runtime::exceptions::helpful_npe::ArrayElemKind::Object,
                        );
                        helpful_npe_opcode_message_parts(
                            shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, 2,
                        )
                    } else {
                        let class_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(npe_cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        format!("aastore in {}.{} pc={}", class_name, npe_mname, _diag_pc)
                    }
                },
            )?;
            // JVMS §6.5 aastore fixes the order of the three checks:
            // NullPointerException (done above, by `pop_object_ref_ctx_with`),
            // THEN ArrayIndexOutOfBoundsException, THEN ArrayStoreException.
            // The bounds test used to be nothing but `set_array_element`'s error
            // return, which runs AFTER the covariance block below — so an
            // out-of-range index with an incompatible value reported
            // `ArrayStoreException` where HotSpot reports
            // `ArrayIndexOutOfBoundsException` (measured: `RArrayStoreTiers` s15).
            // `jit_aastore` already had this order; the interpreter did not.
            // See docs/known-issues/jdk-only/W8-C10-1-typecheck-hatch-audit-and-aastore-precedence.md
            {
                let alen = shared.mem.heap.array_length(array_ref) as i32;
                if index < 0 || index >= alen {
                    return Err(RuntimeError::aioobe(index, alen).into());
                }
            }
            // JVMS §aastore covariance check: a reference store into an
            // Object[]-family array whose element's runtime type is NOT
            // assignment-compatible with the array's component type throws
            // ArrayStoreException. Only checked for a non-null element stored
            // into a reference-component array (primitive arrays never reach
            // aastore; a null element is always storable). `aastore_element_assignable`
            // fails open (allows the store) on any imprecise type info, so this is
            // additive and never produces a false ArrayStoreException.
            if let Value::Object(Some(elem_ref)) = value {
                if shared.mem.heap.kind_of(array_ref) == cratonvm_types::ObjectKind::Array
                    && shared.mem.heap.element_type_of(array_ref) == ArrayElementType::Reference
                    && !aastore_element_assignable(shared, array_ref, elem_ref)
                {
                    // HotSpot's message is `Klass::external_name()` of the
                    // VALUE'S OWN class. For an array value that is the JVMS
                    // descriptor — `[Ljava.lang.Integer;`, never the component
                    // `java.lang.Integer` (measured on JDK 25.0.3:
                    // `Object[] o = new String[1][]; o[0] = new Integer[1];`).
                    //
                    // The raw lookup below cannot produce that, because on a
                    // reference array the header class id holds the COMPONENT
                    // class (`typecheck::array_descriptor_of`, and the same
                    // trap is written out at length on `cce_display_class_name`
                    // — it cost a session as a class-identity split). So this
                    // arm was off by exactly one array dimension for every
                    // array-valued element, and only for those: the plain-class
                    // shapes (`RArrayStoreTiers` s01/s02/s03/s05) were always
                    // right, which is why only s04 diverged.
                    //
                    // Reuse `cce_display_class_name` rather than re-deriving the
                    // descriptor here: it is the same "Java-visible class name
                    // for a VM-minted type error" question `checkcast` asks a
                    // few hundred lines below, and it also carries the
                    // `UnmodifiableMap` stamp translation that keeps a
                    // VM-internal storage class out of an app-visible message.
                    // It returns the INTERNAL (slashed) name; `throw_runtime_
                    // error`'s funnel dots it (`exceptions::hotspot_external_
                    // name`) and correctly leaves a primitive descriptor such as
                    // `[I` alone, since that contains no `/`.
                    //
                    // Two statements, not one: `cce_display_class_name` takes
                    // the class-manager read lock itself, so the guard from the
                    // name lookup must be dropped before the call.
                    let raw_elem_name = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(shared.mem.heap.class_id_of(elem_ref))
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let elem_cls = cce_display_class_name(shared, elem_ref, &raw_elem_name);
                    return Err(RuntimeError::ArrayStoreException { message: elem_cls }.into());
                }
            }
            // SATB barrier: log old array element before overwriting
            // Widening: index conversion
            if let Ok(old_elem) = shared.mem.heap.get_array_element(array_ref, index as usize) {
                shared.mem.heap.satb_barrier(old_elem);
            }
            shared
                .mem.heap
                .set_array_element(array_ref, index as usize, value) // Widening: index conversion
                .map_err(|i| {
                    if aioobe_dbg() {
                        let alen = shared.mem.heap.array_length(array_ref);
                        let _diag_class = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(npe_cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        eprintln!("AIOOBE-AASTORE class={_diag_class} method={npe_mname} pc={_diag_pc} idx={i} len={alen}");
                    }
                    RuntimeError::aioobe(i, shared.mem.heap.array_length(array_ref) as i32)
                })?;
            // write_barrier fires automatically inside set_array_element
        }
        Instruction::Iastore
        | Instruction::Fastore
        | Instruction::Bastore
        | Instruction::Castore
        | Instruction::Sastore => {
            let value = thread.frames[frame_idx].stack.pop()?;
            let index = thread.frames[frame_idx].stack.pop_int()?;
            let _diag_pc = thread.frames[frame_idx].pc;
            // The class and method NAMES used to be materialised here, as two
            // `String` allocations per array store, for a message that is only
            // ever built when the array reference turns out to be null. They
            // are resolved inside the closure now, from `npe_cid` and
            // `npe_mname` below — both of which this site already had to
            // capture for the JEP 358 arm. Hoisting them was not a mistake to
            // begin with: the closure cannot borrow `thread` while
            // `thread.frames[..].stack` is mutably borrowed by
            // `pop_object_ref_ctx_with`. `npe_cid` is a plain `ClassId` and
            // needs no such borrow, which is what makes the lazy form possible.
            // JEP 358 increment 2: array at depth 2 (`[..., arrayref, index,
            // value]`); element kind from the opcode.
            let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
            let npe_code = Arc::clone(&thread.frames[frame_idx].code);
            let npe_cid = thread.frames[frame_idx].class_id;
            let npe_mname = thread.frames[frame_idx].method_name_arc();
            let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
            let npe_bci = thread.frames[frame_idx].last_instr_pc;
            let array_ref = pop_object_ref_ctx_with(
                &mut thread.frames[frame_idx].stack,
                &shared.mem.heap,
                || {
                    if jep358 {
                        use crate::runtime::exceptions::helpful_npe::ArrayElemKind;
                        let elem = match instruction {
                            Instruction::Iastore => ArrayElemKind::Int,
                            Instruction::Fastore => ArrayElemKind::Float,
                            Instruction::Bastore => ArrayElemKind::Byte,
                            Instruction::Castore => ArrayElemKind::Char,
                            _ => ArrayElemKind::Short, // Sastore
                        };
                        let action =
                            crate::runtime::exceptions::helpful_npe::action_array_store(elem);
                        helpful_npe_opcode_message_parts(
                            shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, 2,
                        )
                    } else {
                        let class_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(npe_cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        format!("Xastore in {}.{} pc={}", class_name, npe_mname, _diag_pc)
                    }
                },
            )?;
            // bc math-ec 0x4 smear hunt — see the Lastore twin below.
            if arrstore_enabled() {
                arrstore_check(shared, thread, array_ref, index, "iastore");
            }
            shared
                .mem.heap
                .set_array_element(array_ref, index as usize, value) // Widening: index conversion
                .map_err(|i| {
                    if aioobe_dbg() {
                        let alen = shared.mem.heap.array_length(array_ref);
                        let _diag_class = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(npe_cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        eprintln!("AIOOBE-XASTORE class={_diag_class} method={npe_mname} pc={_diag_pc} idx={i} len={alen}");
                    }
                    RuntimeError::aioobe(i, shared.mem.heap.array_length(array_ref) as i32)
                })?;
            // Phase 10 #2: the host just wrote this array, so any device
            // buffer mirroring it is stale.
            #[cfg(feature = "gpu-offload")]
            crate::runtime::offload::input_cache::invalidate(array_ref);
        }
        // WP4.3 fix: long[] / double[] store must use typed pop so that the
        // CompactValue type-erasure (raw long bits decoding as Value::Double via
        // `to_value()`) does not silently write zero into the array slot.
        // Mirrors the existing `Lstore` / `Dstore` (locals) pattern at the
        // operand-stack level.
        Instruction::Lastore => {
            let v = thread.frames[frame_idx].stack.pop_long()?;
            let index = thread.frames[frame_idx].stack.pop_int()?;
            let _diag_pc = thread.frames[frame_idx].pc;
            // The class and method NAMES used to be materialised here, as two
            // `String` allocations per array store, for a message that is only
            // ever built when the array reference turns out to be null. They
            // are resolved inside the closure now, from `npe_cid` and
            // `npe_mname` below — both of which this site already had to
            // capture for the JEP 358 arm. Hoisting them was not a mistake to
            // begin with: the closure cannot borrow `thread` while
            // `thread.frames[..].stack` is mutably borrowed by
            // `pop_object_ref_ctx_with`. `npe_cid` is a plain `ClassId` and
            // needs no such borrow, which is what makes the lazy form possible.
            // JEP 358 increment 2: long array store, array at depth 2.
            let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
            let npe_code = Arc::clone(&thread.frames[frame_idx].code);
            let npe_cid = thread.frames[frame_idx].class_id;
            let npe_mname = thread.frames[frame_idx].method_name_arc();
            let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
            let npe_bci = thread.frames[frame_idx].last_instr_pc;
            let array_ref = pop_object_ref_ctx_with(
                &mut thread.frames[frame_idx].stack,
                &shared.mem.heap,
                || {
                    if jep358 {
                        let action = crate::runtime::exceptions::helpful_npe::action_array_store(
                            crate::runtime::exceptions::helpful_npe::ArrayElemKind::Long,
                        );
                        helpful_npe_opcode_message_parts(
                            shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, 2,
                        )
                    } else {
                        let class_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(npe_cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        format!("lastore in {}.{} pc={}", class_name, npe_mname, _diag_pc)
                    }
                },
            )?;
            // bc math-ec 0x4 smear hunt (CRATONVM_DBG_ARRSTORE): validate the
            // receiver's header AT THE WRITE. A stale (GC-moved) long[] ref
            // points at reused memory whose "header" is garbage math data —
            // kind byte (offset 4) is then almost never the Array(1) it must
            // be. `set_array_element` would still bounds-check against the
            // garbage array_length and SMEAR longs over neighboring objects
            // (the headline corruption). Dump the receiver + Java stack at the
            // first such store — theory-free attribution of the writer.
            if arrstore_enabled() {
                arrstore_check(shared, thread, array_ref, index, "lastore");
            }
            shared
                .mem
                .heap
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                .set_array_element(array_ref, index as usize, Value::Long(v))
                .map_err(|i| {
                    RuntimeError::aioobe(i, shared.mem.heap.array_length(array_ref) as i32)
                })?;
            // Phase 10 #2 — see the `Iastore` arm.
            #[cfg(feature = "gpu-offload")]
            crate::runtime::offload::input_cache::invalidate(array_ref);
        }
        Instruction::Dastore => {
            let d = thread.frames[frame_idx].stack.pop_double()?;
            let index = thread.frames[frame_idx].stack.pop_int()?;
            let _diag_pc = thread.frames[frame_idx].pc;
            // The class and method NAMES used to be materialised here, as two
            // `String` allocations per array store, for a message that is only
            // ever built when the array reference turns out to be null. They
            // are resolved inside the closure now, from `npe_cid` and
            // `npe_mname` below — both of which this site already had to
            // capture for the JEP 358 arm. Hoisting them was not a mistake to
            // begin with: the closure cannot borrow `thread` while
            // `thread.frames[..].stack` is mutably borrowed by
            // `pop_object_ref_ctx_with`. `npe_cid` is a plain `ClassId` and
            // needs no such borrow, which is what makes the lazy form possible.
            // JEP 358 increment 2: double array store, array at depth 2.
            let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
            let npe_code = Arc::clone(&thread.frames[frame_idx].code);
            let npe_cid = thread.frames[frame_idx].class_id;
            let npe_mname = thread.frames[frame_idx].method_name_arc();
            let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
            let npe_bci = thread.frames[frame_idx].last_instr_pc;
            let array_ref = pop_object_ref_ctx_with(
                &mut thread.frames[frame_idx].stack,
                &shared.mem.heap,
                || {
                    if jep358 {
                        let action = crate::runtime::exceptions::helpful_npe::action_array_store(
                            crate::runtime::exceptions::helpful_npe::ArrayElemKind::Double,
                        );
                        helpful_npe_opcode_message_parts(
                            shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, 2,
                        )
                    } else {
                        let class_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(npe_cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        format!("dastore in {}.{} pc={}", class_name, npe_mname, _diag_pc)
                    }
                },
            )?;
            shared
                .mem
                .heap
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                .set_array_element(array_ref, index as usize, Value::Double(d))
                .map_err(|i| {
                    RuntimeError::aioobe(i, shared.mem.heap.array_length(array_ref) as i32)
                })?;
            // Phase 10 #2 — see the `Iastore` arm.
            #[cfg(feature = "gpu-offload")]
            crate::runtime::offload::input_cache::invalidate(array_ref);
        }

        // -- Stack manipulation (T10.9.D direct CompactValue path) --
        Instruction::Pop => {
            thread.frames[frame_idx].stack.pop_compact_checked()?;
        }
        Instruction::Pop2 => {
            // Kind-aware: a collision-shaped long is one logical cat-2 value.
            let (val, kind) = thread.frames[frame_idx].stack.pop_with_kind()?;
            if !crate::runtime::ValueStack::is_cat2_kind(kind, val) {
                thread.frames[frame_idx].stack.pop_with_kind()?;
            }
        }
        Instruction::Dup => {
            // Preserve the kind so a duplicated collision-long stays KIND_LONG
            // (otherwise the copy lands KIND_UNKNOWN and the GC mis-roots it).
            let (val, kind) = thread.frames[frame_idx].stack.peek_with_kind()?;
            record_shuffle_push(val, "dup");
            thread.frames[frame_idx].stack.push_with_kind(val, kind)?;
        }
        Instruction::DupX1 => {
            let (val1, k1) = thread.frames[frame_idx].stack.pop_with_kind()?;
            let (val2, k2) = thread.frames[frame_idx].stack.pop_with_kind()?;
            record_shuffle_push(val1, "dup_x1");
            record_shuffle_push(val2, "dup_x1");
            thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
            thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
        }
        Instruction::DupX2 => {
            let (val1, k1) = thread.frames[frame_idx].stack.pop_with_kind()?;
            let (val2, k2) = thread.frames[frame_idx].stack.pop_with_kind()?;
            record_shuffle_push(val1, "dup_x2");
            record_shuffle_push(val2, "dup_x2");
            if crate::runtime::ValueStack::is_cat2_kind(k2, val2) {
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            } else {
                let (val3, k3) = thread.frames[frame_idx].stack.pop_with_kind()?;
                record_shuffle_push(val3, "dup_x2");
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val3, k3)?;
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            }
        }
        Instruction::Dup2 => {
            let (val1, k1) = thread.frames[frame_idx].stack.pop_with_kind()?;
            record_shuffle_push(val1, "dup2");
            if crate::runtime::ValueStack::is_cat2_kind(k1, val1) {
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            } else {
                let (val2, k2) = thread.frames[frame_idx].stack.pop_with_kind()?;
                record_shuffle_push(val2, "dup2");
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            }
        }
        Instruction::Dup2X1 => {
            let (val1, k1) = thread.frames[frame_idx].stack.pop_with_kind()?;
            let (val2, k2) = thread.frames[frame_idx].stack.pop_with_kind()?;
            record_shuffle_push(val1, "dup2_x1");
            record_shuffle_push(val2, "dup2_x1");
            if crate::runtime::ValueStack::is_cat2_kind(k1, val1) {
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            } else {
                let (val3, k3) = thread.frames[frame_idx].stack.pop_with_kind()?;
                record_shuffle_push(val3, "dup2_x1");
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val3, k3)?;
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            }
        }
        Instruction::Dup2X2 => {
            let (val1, k1) = thread.frames[frame_idx].stack.pop_with_kind()?;
            let (val2, k2) = thread.frames[frame_idx].stack.pop_with_kind()?;
            record_shuffle_push(val1, "dup2_x2");
            record_shuffle_push(val2, "dup2_x2");
            let v1c2 = crate::runtime::ValueStack::is_cat2_kind(k1, val1);
            let v2c2 = crate::runtime::ValueStack::is_cat2_kind(k2, val2);
            if v1c2 && v2c2 {
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            } else if v1c2 {
                let (val3, k3) = thread.frames[frame_idx].stack.pop_with_kind()?;
                record_shuffle_push(val3, "dup2_x2");
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val3, k3)?;
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            } else if v2c2 {
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            } else {
                let (val3, k3) = thread.frames[frame_idx].stack.pop_with_kind()?;
                let (val4, k4) = thread.frames[frame_idx].stack.pop_with_kind()?;
                record_shuffle_push(val3, "dup2_x2");
                record_shuffle_push(val4, "dup2_x2");
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
                thread.frames[frame_idx].stack.push_with_kind(val4, k4)?;
                thread.frames[frame_idx].stack.push_with_kind(val3, k3)?;
                thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
                thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            }
        }
        Instruction::Swap => {
            let (val1, k1) = thread.frames[frame_idx].stack.pop_with_kind()?;
            let (val2, k2) = thread.frames[frame_idx].stack.pop_with_kind()?;
            record_shuffle_push(val1, "swap");
            record_shuffle_push(val2, "swap");
            thread.frames[frame_idx].stack.push_with_kind(val1, k1)?;
            thread.frames[frame_idx].stack.push_with_kind(val2, k2)?;
        }

        // -- Integer arithmetic --
        Instruction::Iadd => int_binop(&mut thread.frames[frame_idx], |a, b| a.wrapping_add(b))?,
        Instruction::Isub => int_binop(&mut thread.frames[frame_idx], |a, b| a.wrapping_sub(b))?,
        Instruction::Imul => int_binop(&mut thread.frames[frame_idx], |a, b| a.wrapping_mul(b))?,
        Instruction::Idiv => {
            let b = thread.frames[frame_idx].stack.pop_int()?;
            let a = thread.frames[frame_idx].stack.pop_int()?;
            if b == 0 {
                return Err(RuntimeError::ArithmeticException {
                    message: "/ by zero".to_string(),
                }
                .into());
            }
            thread.frames[frame_idx].stack.push_int(a.wrapping_div(b))?;
        }
        Instruction::Irem => {
            let b = thread.frames[frame_idx].stack.pop_int()?;
            let a = thread.frames[frame_idx].stack.pop_int()?;
            if b == 0 {
                return Err(RuntimeError::ArithmeticException {
                    message: "/ by zero".to_string(),
                }
                .into());
            }
            thread.frames[frame_idx].stack.push_int(a.wrapping_rem(b))?;
        }
        Instruction::Ineg => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            thread.frames[frame_idx].stack.push_int(v.wrapping_neg())?;
        }
        Instruction::Ishl => {
            let shift = thread.frames[frame_idx].stack.pop_int()? & 0x1F;
            let v = thread.frames[frame_idx].stack.pop_int()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Int(v << shift))?;
        }
        Instruction::Ishr => {
            let shift = thread.frames[frame_idx].stack.pop_int()? & 0x1F;
            let v = thread.frames[frame_idx].stack.pop_int()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Int(v >> shift))?;
        }
        Instruction::Iushr => {
            let shift = thread.frames[frame_idx].stack.pop_int()? & 0x1F;
            let v = thread.frames[frame_idx].stack.pop_int()? as u32; // Widening: unsigned conversion
            thread.frames[frame_idx]
                .stack
                .push(Value::Int((v >> shift) as i32))?; // Cast: JIT ABI -- JVM int value
        }
        Instruction::Iand => int_binop(&mut thread.frames[frame_idx], |a, b| a & b)?,
        Instruction::Ior => int_binop(&mut thread.frames[frame_idx], |a, b| a | b)?,
        Instruction::Ixor => int_binop(&mut thread.frames[frame_idx], |a, b| a ^ b)?,
        Instruction::Iinc { index, constant } => {
            // Direct compact read + write.  The local bounds check lives
            // inside get_local_compact/set_local_compact; a stale or
            // wrong-typed slot decodes to Uninitialized, whose as_int
            // returns None and degrades to 0 — mirrors the prior behaviour.
            let val = thread.frames[frame_idx]
                .get_local_compact(*index)
                .as_int()
                .unwrap_or(0);
            thread.frames[frame_idx].set_local_compact(
                *index,
                // Cast: operand reinterpreted as i32 (JVM 32-bit stack word)
                CompactValue::int(val.wrapping_add(*constant as i32)),
            ); // Cast: bytecode operand decoding
        }

        // -- Long arithmetic --
        Instruction::Ladd => long_binop(&mut thread.frames[frame_idx], |a, b| a.wrapping_add(b))?,
        Instruction::Lsub => long_binop(&mut thread.frames[frame_idx], |a, b| a.wrapping_sub(b))?,
        Instruction::Lmul => long_binop(&mut thread.frames[frame_idx], |a, b| a.wrapping_mul(b))?,
        Instruction::Ldiv => {
            let b = thread.frames[frame_idx].stack.pop_long()?;
            let a = thread.frames[frame_idx].stack.pop_long()?;
            if b == 0 {
                return Err(RuntimeError::ArithmeticException {
                    message: "/ by zero".to_string(),
                }
                .into());
            }
            thread.frames[frame_idx]
                .stack
                .push_long(a.wrapping_div(b))?;
        }
        Instruction::Lrem => {
            let b = thread.frames[frame_idx].stack.pop_long()?;
            let a = thread.frames[frame_idx].stack.pop_long()?;
            if b == 0 {
                return Err(RuntimeError::ArithmeticException {
                    message: "/ by zero".to_string(),
                }
                .into());
            }
            thread.frames[frame_idx]
                .stack
                .push_long(a.wrapping_rem(b))?;
        }
        Instruction::Lneg => {
            let v = thread.frames[frame_idx].stack.pop_long()?;
            thread.frames[frame_idx].stack.push_long(v.wrapping_neg())?;
        }
        Instruction::Lshl => {
            let shift = thread.frames[frame_idx].stack.pop_int()? & 0x3F;
            let v = thread.frames[frame_idx].stack.pop_long()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Long(v << shift))?;
        }
        Instruction::Lshr => {
            let shift = thread.frames[frame_idx].stack.pop_int()? & 0x3F;
            let v = thread.frames[frame_idx].stack.pop_long()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Long(v >> shift))?;
        }
        Instruction::Lushr => {
            let shift = thread.frames[frame_idx].stack.pop_int()? & 0x3F;
            let v = thread.frames[frame_idx].stack.pop_long()? as u64; // Widening: unsigned conversion
            thread.frames[frame_idx]
                .stack
                .push(Value::Long((v >> shift) as i64))?; // Cast: JIT ABI -- i64 register convention
        }
        Instruction::Land => long_binop(&mut thread.frames[frame_idx], |a, b| a & b)?,
        Instruction::Lor => long_binop(&mut thread.frames[frame_idx], |a, b| a | b)?,
        Instruction::Lxor => long_binop(&mut thread.frames[frame_idx], |a, b| a ^ b)?,

        // -- Float arithmetic --
        Instruction::Fadd => float_binop(&mut thread.frames[frame_idx], |a, b| a + b)?,
        Instruction::Fsub => float_binop(&mut thread.frames[frame_idx], |a, b| a - b)?,
        Instruction::Fmul => float_binop(&mut thread.frames[frame_idx], |a, b| a * b)?,
        Instruction::Fdiv => float_binop(&mut thread.frames[frame_idx], |a, b| a / b)?,
        Instruction::Frem => float_binop(&mut thread.frames[frame_idx], |a, b| a % b)?,
        Instruction::Fneg => {
            let v = thread.frames[frame_idx].stack.pop_float()?;
            thread.frames[frame_idx].stack.push_float(-v)?;
        }

        // -- Double arithmetic --
        Instruction::Dadd => double_binop(&mut thread.frames[frame_idx], |a, b| a + b)?,
        Instruction::Dsub => double_binop(&mut thread.frames[frame_idx], |a, b| a - b)?,
        Instruction::Dmul => double_binop(&mut thread.frames[frame_idx], |a, b| a * b)?,
        Instruction::Ddiv => double_binop(&mut thread.frames[frame_idx], |a, b| a / b)?,
        Instruction::Drem => double_binop(&mut thread.frames[frame_idx], |a, b| a % b)?,
        Instruction::Dneg => {
            let v = thread.frames[frame_idx].stack.pop_double()?;
            thread.frames[frame_idx].stack.push_double(-v)?;
        }

        // -- Conversions --
        // T10.K5: conversions producing long/double push the result directly
        // as a CompactValue so the 8-byte slot carries the correct tag without
        // a Value-enum round-trip.  Widenings use `i64::from`/`f64::from` to
        // avoid silent truncation; float→integer narrowings defer to the
        // saturation helpers (`float_to_long`, `double_to_long`) which
        // implement JVM §2.8.3 NaN→0, +inf→MAX, -inf→MIN semantics.
        Instruction::I2l => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            // JVM spec: i2l sign-extends int to long (lossless).
            thread.frames[frame_idx].stack.push_long(i64::from(v))?;
        }
        Instruction::I2f => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            // JVM spec: i2f converts int to float (may lose precision)
            thread.frames[frame_idx]
                .stack
                .push(Value::Float(v as f32))?; // JVM spec: i2f converts int to float (may lose precision)
        }
        Instruction::I2d => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            // JVM spec: i2d widens int to double (lossless).
            thread.frames[frame_idx].stack.push_double(f64::from(v))?;
        }
        Instruction::L2i => {
            let v = thread.frames[frame_idx].stack.pop_long()?;
            // JVM spec: l2i narrows long to int (truncates upper 32 bits)
            thread.frames[frame_idx].stack.push(Value::Int(v as i32))?;
        }
        Instruction::L2f => {
            let v = thread.frames[frame_idx].stack.pop_long()?;
            // JVM spec: l2f converts long to float (may lose precision)
            thread.frames[frame_idx]
                .stack
                .push(Value::Float(v as f32))?; // JVM spec: l2f converts long to float (may lose precision)
        }
        Instruction::L2d => {
            let v = thread.frames[frame_idx].stack.pop_long()?;
            // JVM spec: l2d converts long to double (may lose precision on
            // magnitudes above 2^53).  The `as f64` cast matches the JVM's
            // round-to-nearest-even rule on all Rust-supported targets.
            thread.frames[frame_idx].stack.push_double(v as f64)?;
        }
        Instruction::F2i => {
            let v = thread.frames[frame_idx].stack.pop_float()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Int(float_to_int(v)))?;
        }
        Instruction::F2l => {
            let v = thread.frames[frame_idx].stack.pop_float()?;
            // JVM spec §2.8.3: NaN → 0, +inf → Long::MAX, -inf → Long::MIN;
            // in-range values truncate toward zero.  `float_to_long` handles
            // all four branches; a plain `as i64` cast would also saturate on
            // x86-64 but the helper keeps the semantics target-independent.
            thread.frames[frame_idx].stack.push_long(float_to_long(v))?;
        }
        Instruction::F2d => {
            let v = thread.frames[frame_idx].stack.pop_float()?;
            // JVM spec: f2d widens float to double (lossless).
            thread.frames[frame_idx].stack.push_double(f64::from(v))?;
        }
        Instruction::D2i => {
            let v = thread.frames[frame_idx].stack.pop_double()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Int(double_to_int(v)))?;
        }
        Instruction::D2l => {
            let v = thread.frames[frame_idx].stack.pop_double()?;
            // JVM spec §2.8.3 semantics applied by `double_to_long`.
            thread.frames[frame_idx]
                .stack
                .push_long(double_to_long(v))?;
        }
        Instruction::D2f => {
            let v = thread.frames[frame_idx].stack.pop_double()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Float(v as f32))?; // JVM spec: d2f narrows double to float (may lose precision)
        }
        Instruction::I2b => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Int((v as i8) as i32))?; // Cast: JIT ABI -- JVM int value
        }
        Instruction::I2c => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Int((v as u16) as i32))?; // Cast: JIT ABI -- JVM int value
        }
        Instruction::I2s => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Int((v as i16) as i32))?; // Cast: JIT ABI -- JVM int value
        }

        // -- Comparisons --
        Instruction::Lcmp => {
            let b = thread.frames[frame_idx].stack.pop_long()?;
            let a = thread.frames[frame_idx].stack.pop_long()?;
            let result = if a > b {
                1
            } else if a == b {
                0
            } else {
                -1
            };
            thread.frames[frame_idx].stack.push(Value::Int(result))?;
        }
        Instruction::Fcmpl => {
            let b = thread.frames[frame_idx].stack.pop_float()?;
            let a = thread.frames[frame_idx].stack.pop_float()?;
            let result = if a.is_nan() || b.is_nan() {
                -1
            } else if a > b {
                1
            } else if a == b {
                0
            } else {
                -1
            };
            thread.frames[frame_idx].stack.push(Value::Int(result))?;
        }
        Instruction::Fcmpg => {
            let b = thread.frames[frame_idx].stack.pop_float()?;
            let a = thread.frames[frame_idx].stack.pop_float()?;
            let result = if a.is_nan() || b.is_nan() || a > b {
                1
            } else if a == b {
                0
            } else {
                -1
            };
            thread.frames[frame_idx].stack.push(Value::Int(result))?;
        }
        Instruction::Dcmpl => {
            let b = thread.frames[frame_idx].stack.pop_double()?;
            let a = thread.frames[frame_idx].stack.pop_double()?;
            let result = if a.is_nan() || b.is_nan() {
                -1
            } else if a > b {
                1
            } else if a == b {
                0
            } else {
                -1
            };
            thread.frames[frame_idx].stack.push(Value::Int(result))?;
        }
        Instruction::Dcmpg => {
            let b = thread.frames[frame_idx].stack.pop_double()?;
            let a = thread.frames[frame_idx].stack.pop_double()?;
            let result = if a.is_nan() || b.is_nan() || a > b {
                1
            } else if a == b {
                0
            } else {
                -1
            };
            thread.frames[frame_idx].stack.push(Value::Int(result))?;
        }

        // -- Conditional branches (int) --
        Instruction::Ifeq(offset) => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            if v == 0 {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::Ifne(offset) => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            if v != 0 {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::Iflt(offset) => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            if v < 0 {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::Ifge(offset) => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            if v >= 0 {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::Ifgt(offset) => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            if v > 0 {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::Ifle(offset) => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            if v <= 0 {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }

        // -- Conditional branches (int comparison) --
        Instruction::IfIcmpeq(offset) => {
            let b = thread.frames[frame_idx].stack.pop_int()?;
            let a = thread.frames[frame_idx].stack.pop_int()?;
            if a == b {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::IfIcmpne(offset) => {
            let b = thread.frames[frame_idx].stack.pop_int()?;
            let a = thread.frames[frame_idx].stack.pop_int()?;
            if a != b {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::IfIcmplt(offset) => {
            let b = thread.frames[frame_idx].stack.pop_int()?;
            let a = thread.frames[frame_idx].stack.pop_int()?;
            if a < b {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::IfIcmpge(offset) => {
            let b = thread.frames[frame_idx].stack.pop_int()?;
            let a = thread.frames[frame_idx].stack.pop_int()?;
            if a >= b {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::IfIcmpgt(offset) => {
            let b = thread.frames[frame_idx].stack.pop_int()?;
            let a = thread.frames[frame_idx].stack.pop_int()?;
            if a > b {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::IfIcmple(offset) => {
            let b = thread.frames[frame_idx].stack.pop_int()?;
            let a = thread.frames[frame_idx].stack.pop_int()?;
            if a <= b {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }

        // -- Conditional branches (reference comparison) --
        Instruction::IfAcmpeq(offset) => {
            let b = thread.frames[frame_idx].stack.pop()?;
            let a = thread.frames[frame_idx].stack.pop()?;
            if refs_equal(&a, &b) {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::IfAcmpne(offset) => {
            let b = thread.frames[frame_idx].stack.pop()?;
            let a = thread.frames[frame_idx].stack.pop()?;
            let eq = refs_equal(&a, &b);
            if crate::runtime::env_cache::active_profiles_identity_trace() {
                let class_name = |value: &Value| match value {
                    Value::Object(Some(mirror)) => crate::vm::class_id_from_mirror(shared, *mirror)
                        .and_then(|cid| {
                            shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                        })
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let an = class_name(&a);
                let bn = class_name(&b);
                if an.contains("ActiveProfilesResolver") || bn.contains("ActiveProfilesResolver") {
                    eprintln!(
                        "[ACTIVE-PROFILES-IDENTITY] acmpne a={} b={} equal={}",
                        an, bn, eq
                    );
                }
            }
            if !eq {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::Ifnull(offset) => {
            let v = thread.frames[frame_idx].stack.pop()?;
            // Use `ref_operand_is_null` (not `Value::is_null`) so a JNI jobject
            // null carried as `Value::Long(0)` is also recognised as null.
            if ref_operand_is_null(&v) {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }
        Instruction::Ifnonnull(offset) => {
            let v = thread.frames[frame_idx].stack.pop()?;
            // Mirror Ifnull: a `Value::Long(0)` jobject-null is null, so
            // ifnonnull must NOT take the branch for it.
            if !ref_operand_is_null(&v) {
                thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            }
        }

        // -- Unconditional branches --
        Instruction::Goto(offset) => {
            thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
            // Safepoint check on backward branches (loop iterations)
            if *offset < 0 {
                safepoint_check(shared, thread);
            }
        }
        Instruction::GotoW(offset) => {
            thread.frames[frame_idx].pc = (saved_pc as i64 + *offset as i64) as usize; // Widening: index conversion
            if *offset < 0 {
                safepoint_check(shared, thread);
            }
        }

        // -- Switch --
        Instruction::Tableswitch(ts) => {
            let index = thread.frames[frame_idx].stack.pop_int()?;
            let offset = if index >= ts.low && index <= ts.high {
                ts.offsets[(index - ts.low) as usize] // Widening: index conversion
            } else {
                ts.default
            };
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            thread.frames[frame_idx].pc = (saved_pc as i64 + offset as i64) as usize;
            // Widening: index conversion
        }
        Instruction::Lookupswitch(ls) => {
            let key = thread.frames[frame_idx].stack.pop_int()?;
            // Binary search over the JVMS-mandated sorted key table, with a
            // linear fallback for unverified bytecode — see `LookupSwitch::target`.
            let offset = ls.target(key);
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            thread.frames[frame_idx].pc = (saved_pc as i64 + offset as i64) as usize;
            // Widening: index conversion
        }

        // -- Returns --
        Instruction::Return => {
            // T17.Δ.2 — MethodExit fires on every normal return. No-op fast
            // path when no agent listens.
            fire_jvmti_method_exit_normal(
                shared.vm_identity,
                thread,
                &thread.frames[frame_idx],
                &None,
            );
            return Ok(InstructionResult::Return(None));
        }
        Instruction::Ireturn => {
            let v = thread.frames[frame_idx].stack.pop_int()?;
            let rv = Some(Value::Int(v));
            fire_jvmti_method_exit_normal(
                shared.vm_identity,
                thread,
                &thread.frames[frame_idx],
                &rv,
            );
            return Ok(InstructionResult::Return(rv));
        }
        Instruction::Lreturn => {
            let v = thread.frames[frame_idx].stack.pop_long()?;
            let rv = Some(Value::Long(v));
            fire_jvmti_method_exit_normal(
                shared.vm_identity,
                thread,
                &thread.frames[frame_idx],
                &rv,
            );
            return Ok(InstructionResult::Return(rv));
        }
        Instruction::Freturn => {
            let v = thread.frames[frame_idx].stack.pop_float()?;
            let rv = Some(Value::Float(v));
            fire_jvmti_method_exit_normal(
                shared.vm_identity,
                thread,
                &thread.frames[frame_idx],
                &rv,
            );
            return Ok(InstructionResult::Return(rv));
        }
        Instruction::Dreturn => {
            let v = thread.frames[frame_idx].stack.pop_double()?;
            let rv = Some(Value::Double(v));
            fire_jvmti_method_exit_normal(
                shared.vm_identity,
                thread,
                &thread.frames[frame_idx],
                &rv,
            );
            return Ok(InstructionResult::Return(rv));
        }
        Instruction::Areturn => {
            let v = thread.frames[frame_idx].stack.pop()?;
            let ret = crate::jit::return_type(thread.frames[frame_idx].method_descriptor());
            let v = coerce_value_for_return_validated(shared, v, ret);
            let rv = Some(v);
            fire_jvmti_method_exit_normal(
                shared.vm_identity,
                thread,
                &thread.frames[frame_idx],
                &rv,
            );
            return Ok(InstructionResult::Return(rv));
        }

        // -- Field access --
        Instruction::Getstatic(index) => op_getstatic(shared, thread, frame_idx, *index)?,
        Instruction::Putstatic(index) => op_putstatic(shared, thread, frame_idx, *index)?,
        Instruction::Getfield(index) => op_getfield(shared, thread, frame_idx, *index)?,
        Instruction::Putfield(index) => op_putfield(shared, thread, frame_idx, *index)?,

        // -- Method invocation (slow path) --
        //
        // Decoded fallback for opcodes and guarded edge cases not handled by
        // the common raw-byte loop. This is no longer selected by class-name
        // prefix: identical bytecode executes through identical handlers.
        // The monomorphic `InvokeCache` consulted by `execute_invokevirtual_cached`
        // does NOT share that risk: its arg decode already goes through
        // `pop_arg_for_descriptor_checked` (descriptor-aware, checked), and a
        // miss/edge-case (receiver-class change, lambda proxy, annotation
        // proxy, stack-depth limit, JVMTI redefine) just returns `CacheMiss`
        // and falls through to the exact same slow path used before this fix.
        // Previously this arm called `execute_invoke`/`execute_invoke_kind`
        // UNCONDITIONALLY, so every single invoke instruction executed by
        // JDK-internal bytecode (java.xml/Xerces, java.util, java.io, …) paid
        // full method resolution every time — native-registry hash lookup,
        // `force_native_over_real_jdk_bytecode` / `synthetic_stub_should_yield_
        // to_real_bytecode` special-case checks, a superclass hierarchy walk,
        // `class_manager` RwLock reads — instead of the lock-free O(1) cache
        // hit non-JDK bytecode already enjoyed. Measured impact: a standalone
        // SAX/DTD parse-loop repro (no Spring/Tomcat involved) went from
        // ~155ms/parse to ~0.6ms/parse on CratonVM after this fix (HotSpot:
        // ~0.46ms/parse) — see CRATONVM-SPRING-GENUINE-
        // BUGLIST, RequestMappingMessageConversionIntegrationTests entry.
        Instruction::Invokevirtual(index) | Instruction::Invokespecial(index) => {
            let is_special_invoke = matches!(instruction, Instruction::Invokespecial(_));
            match execute_invokevirtual_cached(
                shared,
                thread,
                frame_idx,
                *index,
                saved_pc,
                is_special_invoke,
                false,
            )? {
                CachedCallResult::FramePushed => {
                    return Ok(InstructionResult::FramePushed);
                }
                CachedCallResult::Handled => {}
                CachedCallResult::CacheMiss => {
                    match execute_invoke(shared, thread, frame_idx, *index, is_special_invoke, saved_pc)? {
                        CachedCallResult::FramePushed => {
                            return Ok(InstructionResult::FramePushed);
                        }
                        _ => {}
                    }
                }
            }
        }
        Instruction::Invokestatic(index) => {
            // Mirrors the Invokevirtual/Invokespecial arm above: JDK-internal
            // classes (java.xml/Xerces, java.util, java.io, …) run through
            // this dispatcher rather than the raw-byte-peek fast loop at the
            // top of `execute_frame` (which already consulted
            // `execute_invokestatic_cached` — see its call site's history),
            // so every invokestatic previously paid full method resolution
            // on every single call: native-registry hash lookup,
            // `force_native_over_real_jdk_bytecode` /
            // `synthetic_stub_should_yield_to_real_bytecode` checks, a
            // `split_method_descriptor` heap allocation, `class_manager`
            // RwLock reads. `execute_invokestatic_cached` already exists and
            // is exercised by the other dispatch loop; wiring it in here
            // gives JDK-internal invokestatic call sites the same lock-free
            // O(1) cache hit non-JDK bytecode and Invokevirtual/Invokespecial
            // already enjoyed. A miss/edge-case (JVMTI redefine, synthetic
            // stub upgrade) falls through to the exact same slow path used
            // before this fix.
            match execute_invokestatic_cached(shared, thread, frame_idx, *index, saved_pc)? {
                CachedCallResult::FramePushed => {
                    return Ok(InstructionResult::FramePushed);
                }
                CachedCallResult::Handled => {}
                CachedCallResult::CacheMiss => {
                    match execute_invokestatic(shared, thread, frame_idx, *index, saved_pc)? {
                        CachedCallResult::FramePushed => {
                            return Ok(InstructionResult::FramePushed);
                        }
                        _ => {}
                    }
                }
            }
        }
        Instruction::Invokeinterface { index, count: _ } => {
            // WFLYCTL0079 round 3 (2026-07-23): the bytecode of
            // `TransactionSubsystemRootResourceDefinition.registerAttributes`
            // builds a `HashSet<AttributeDefinition>` from a static array,
            // removes ~11 specific attributes from it (including
            // `HORNETQ_STORE_ENABLE_ASYNC_IO`, which needs special
            // `AliasedHandler` treatment applied later via an explicit call
            // at a separate pc), then registers whatever remains via a
            // generic loop, THEN makes the explicit HORNETQ_STORE_ENABLE_
            // ASYNC_IO registration call. If `Set.remove()` for that
            // attribute ever silently returns `false` (return value is
            // discarded in the bytecode — `pop` after every `.remove()`
            // call), the attribute would get registered TWICE: once by the
            // generic loop, once by the explicit call — "already
            // registered". Both `DUPCALL` (executor-dispatch level) and
            // `DUPREG` (registerAttributes-entry level) diagnostics tested
            // clean across 1600 boots, which only rules out re-ENTERING the
            // method twice — NOT this same-invocation double-registration
            // shape. Trace every `invokeinterface` call made BY this one
            // caller frame, with the AttributeDefinition argument's raw
            // identity, so a within-one-invocation repeat is directly
            // visible without needing to resolve field offsets.
            if crate::runtime::env_cache::dbg_dupcall_filter() {
                let caller = &thread.frames[frame_idx];
                if caller.method_name() == "registerAttributes"
                    && caller.class_name()
                        == "org/jboss/as/txn/subsystem/TransactionSubsystemRootResourceDefinition"
                {
                    // Safety guard: other invokeinterface calls inside this
                    // same method (Set.remove/iterator/Iterator.hasNext/next)
                    // have shallower stack shapes at their call site — only
                    // peek when there's plausibly a 4-slot call
                    // (registration, attrDef, handler, handler) in flight,
                    // to avoid `peek_at` panicking on an out-of-range depth
                    // for those unrelated calls.
                    if caller.stack.len() >= 4 {
                        let attr_def = caller.stack.peek_at(2);
                        let addr = match attr_def {
                            Value::Object(Some(o)) => o.as_ptr() as usize,
                            _ => 0,
                        };
                        if addr != 0 {
                            eprintln!(
                                "[REGCALL] caller_pc={} attr=0x{:x} tid={}",
                                caller.pc, addr, thread.thread_id.0,
                            );
                        }
                    }
                }
            }
            // is_interface=true threads γ's stash so the default-method
            // rescue can fire on NSME for invokeinterface only. Same cache
            // consultation as invokevirtual/invokespecial above (PERF FIX
            // 2026-07-15) — the vtable slot resolved by the cache is
            // identical for invokevirtual and invokeinterface call sites
            // once a receiver's concrete class is known (see the
            // `execute_invokevirtual_vtable_fast` "miss path" comment in the
            // raw fast-dispatch loop, which already relies on this fact).
            match execute_invokevirtual_cached(
                shared, thread, frame_idx, *index, saved_pc, false, true,
            )? {
                CachedCallResult::FramePushed => {
                    return Ok(InstructionResult::FramePushed);
                }
                CachedCallResult::Handled => {}
                CachedCallResult::CacheMiss => {
                    match execute_invoke_kind(shared, thread, frame_idx, *index, false, true, saved_pc)? {
                        CachedCallResult::FramePushed => {
                            return Ok(InstructionResult::FramePushed);
                        }
                        _ => {}
                    }
                }
            }
        }

        // -- Object creation --
        Instruction::New(index) => op_new(shared, thread, frame_idx, *index)?,

        // -- Array creation --
        Instruction::Newarray(atype) => {
            let length = thread.frames[frame_idx].stack.pop_int()?;
            if length < 0 {
                return Err(RuntimeError::NegativeArraySizeException { size: length }.into());
            }
            let element_type = match atype {
                4 => ArrayElementType::Boolean,
                5 => ArrayElementType::Char,
                6 => ArrayElementType::Float,
                7 => ArrayElementType::Double,
                8 => ArrayElementType::Byte,
                9 => ArrayElementType::Short,
                10 => ArrayElementType::Int,
                11 => ArrayElementType::Long,
                _ => {
                    return Err(VmError::Internal {
                        message: format!("invalid newarray atype: {atype}"),
                    }
                    .into());
                }
            };
            // Widening: index conversion
            let arr = gc_alloc_array(
                shared,
                thread,
                ClassId::new(0),
                element_type,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                length as usize,
            )?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(arr)))?;
            maybe_gc(shared, thread);
        }
        Instruction::Anewarray(index) => {
            let length = thread.frames[frame_idx].stack.pop_int()?;
            if length < 0 {
                return Err(RuntimeError::NegativeArraySizeException { size: length }.into());
            }
            let referencing_class_id = thread.frames[frame_idx].class_id;
            let component_class_name = {
                let cm = shared.classes.class_manager.read();
                let class =
                    cm.get_class(referencing_class_id)
                        .ok_or_else(|| VmError::Internal {
                            message: "current class not found".to_string(),
                        })?;
                class
                    .constant_pool
                    .get_class_name(*index)
                    .ok_or_else(|| VmError::Internal {
                        message: format!("invalid class ref at cp#{index}"),
                    })?
                    .to_string()
            };
            let component_class_id = resolve_class_loader_aware(
                shared,
                thread,
                referencing_class_id,
                &component_class_name,
            )
            .map_err(|e| convert_class_not_found(shared, thread, &component_class_name, e))?;
            // Widening: index conversion
            let arr = gc_alloc_array(
                shared,
                thread,
                component_class_id,
                ArrayElementType::Reference,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                length as usize,
            )?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(arr)))?;
            maybe_gc(shared, thread);
        }
        Instruction::Arraylength => {
            let pc = thread.frames[frame_idx].pc;
            // Perf: the class name is already cached on the Frame as an
            // `Arc<str>` (see `Frame::class_name`), so read it from there
            // instead of re-acquiring the `class_manager` RwLock on every
            // `arraylength` opcode. `frame.class_name()` returns exactly the
            // same value as `class_manager.get_class(class_id).name`.
            // The three names below feed ONE format string, in the non-JEP-358
            // branch of a closure that runs only when the array reference is
            // null. Building them eagerly cost three `String` allocations on
            // every executed `arraylength` — invisible while the raw-bytecode
            // arm handles the opcode, but this handler IS the arraylength path
            // under `-Xverify:none`. Clone the frame's `Arc<str>` names (one
            // atomic bump each, no copy) and format inside the closure.
            let current_class_name = thread.frames[frame_idx].class_name_arc();
            let mname = thread.frames[frame_idx].method_name_arc();
            let mdesc = thread.frames[frame_idx].method_descriptor_arc();
            // JEP 358 increment 2: `Cannot read the array length because
            // "<expr>" is null`. The array ref is at the top of the operand
            // stack (depth 0).
            let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
            let npe_code = Arc::clone(&thread.frames[frame_idx].code);
            let npe_cid = thread.frames[frame_idx].class_id;
            let npe_mname = thread.frames[frame_idx].method_name_arc();
            let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
            let npe_bci = thread.frames[frame_idx].last_instr_pc;
            // S111r14 diag: print full Java stack trace on arraylength failure
            let arr_ref = match pop_object_ref_ctx_with(
                &mut thread.frames[frame_idx].stack,
                &shared.mem.heap,
                || {
                    if jep358 {
                        let action = crate::runtime::exceptions::helpful_npe::action_array_length();
                        helpful_npe_opcode_message_parts(
                            shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, 0,
                        )
                    } else {
                        format!("arraylength null (in {current_class_name}.{mname}{mdesc} pc={pc})")
                    }
                },
            ) {
                Ok(r) => r,
                Err(e) => {
                    if crate::runtime::env_cache::iae_trace_os() {
                        eprintln!("[ARRAYLEN-DIAG] failure in {current_class_name}.{mname}{mdesc} pc={pc}");
                        for (i, f) in thread.frames.iter().enumerate().rev() {
                            eprintln!(
                                "  frame[{i}]: {}.{}{} pc={}",
                                f.class_name(),
                                f.method_name(),
                                f.method_descriptor(),
                                f.pc
                            );
                        }
                    }
                    return Err(e);
                }
            };
            let len = shared.mem.heap.array_length(arr_ref);
            thread.frames[frame_idx]
                .stack
                .push(Value::Int(len as i32))?; // Cast: array length to JVM int
        }
        Instruction::Multianewarray { index, dimensions } => {
            let dims = *dimensions as usize; // Widening: index conversion
            if dims == 0 {
                return Err(VmError::Internal {
                    message: "multianewarray: dimensions must be >= 1".to_string(),
                }
                .into());
            }

            let mut sizes = Vec::with_capacity(dims);
            for _ in 0..dims {
                let size = thread.frames[frame_idx].stack.pop_int()?;
                if size < 0 {
                    return Err(RuntimeError::NegativeArraySizeException { size }.into());
                }
                sizes.push(size as usize); // Widening: index conversion
            }
            sizes.reverse();

            // Descriptor parse, the JVMS §4.9.1 bracket-count guard, the
            // per-level component-class resolution and the allocation itself
            // all live in `multianewarray_alloc`, which the JIT's
            // `jit_multianewarray_2d` helper calls too. Keeping one body is the
            // point: when this arm and the JIT helper were separate
            // transcriptions, only this one resolved component classes, and a
            // JIT-compiled `new String[a][b]` came back as `[Ljava.lang.Object;`.
            let referencing_class_id = thread.frames[frame_idx].class_id;
            let arr = crate::runtime::interpreter::multianewarray_alloc(
                shared,
                thread,
                referencing_class_id,
                *index,
                &sizes,
            )?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(arr)))?;
            maybe_gc(shared, thread);
        }

        // -- Exceptions --
        Instruction::Athrow => {
            let exc_value = thread.frames[frame_idx].stack.pop()?;
            match exc_value {
                Value::Object(Some(obj_ref)) => {
                    // DBG (CRATONVM_DBG_IMSE): autopsy for the RRWL "attempt
                    // to unlock read lock" hold-count loss (ES testAllEqual
                    // face). At the throw site, dump the Sync's complete
                    // read-hold bookkeeping so the broken invariant is named
                    // directly: firstReader identity, the cached hold
                    // counter, and the current thread's readHolds
                    // ThreadLocalMap entry.
                    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IMSE").is_some() {
                        dump_imse_holdcount_state(shared, thread, obj_ref);
                    }
                    // S111r19+: trace IAE thrown from Java bytecode (ATHROW opcode)
                    // This catches IAEs that don't go through throw_runtime_error,
                    // e.g. Spring's Assert.notNull / validateBeanDefinition etc.
                    if crate::runtime::env_cache::iae_trace() {
                        let exc_class_id = shared.mem.heap.class_id_of(obj_ref);
                        let exc_class_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(exc_class_id)
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        if exc_class_name.contains("IllegalArgumentException") {
                            // Try to read the detail message (field 0 = detailMessage)
                            let msg = match shared.mem.heap.get_field(obj_ref, 0) {
                                Value::Object(Some(msg_ref)) => {
                                    read_java_string(&shared.mem.heap, msg_ref)
                                        .unwrap_or_else(|| "<non-string>".to_string())
                                }
                                Value::Object(None) => "<null message>".to_string(),
                                _ => "<no message field>".to_string(),
                            };
                            eprintln!("IAE-ATHROW class={exc_class_name} message={msg:?}");
                            for (i, f) in thread.frames.iter().enumerate().rev().take(30) {
                                let cn = shared
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(f.class_id)
                                    .map(|c| c.name.clone())
                                    .unwrap_or_default();
                                eprintln!(
                                    "IAE-ATHROW-STK[{i}] {}.{} pc={}",
                                    cn,
                                    f.method_name(),
                                    f.pc
                                );
                            }
                        }
                    }
                    // CRATONVM_DBG_ATHROW=1 — env-gated dump of every Java
                    // exception throw (class name, detailMessage, and a
                    // short stack trace). Useful when an exception is
                    // caught by an outer handler that swallows it and the
                    // app exits silently (Kafka 4.2.0 main()'s catch-all
                    // around buildServer/startup is the canonical case).
                    if crate::runtime::env_cache::athrow_dbg() {
                        let exc_class_id = shared.mem.heap.class_id_of(obj_ref);
                        let exc_class_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(exc_class_id)
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        let mut msg = String::from("<no msg>");
                        for fi in 0..8 {
                            if let Value::Object(Some(msg_ref)) =
                                shared.mem.heap.get_field(obj_ref, fi)
                            {
                                if let Some(s) = read_java_string(&shared.mem.heap, msg_ref) {
                                    if !s.is_empty() {
                                        msg = format!("field{fi}={s}");
                                        break;
                                    }
                                }
                            }
                        }
                        eprintln!("ATHROW class={exc_class_name} msg={msg:?}");
                        for (i, f) in thread.frames.iter().enumerate().rev().take(15) {
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(f.class_id)
                                .map(|c| c.name.clone())
                                .unwrap_or_default();
                            eprintln!("  ATHROW-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                        }
                    }
                    // charset-NPE diagnostic (2026-05-21) — gated by
                    // `CRATONVM_DBG_CHARSET=1`. When a `NullPointerException`
                    // whose detail message is exactly `charset` is thrown via
                    // an `athrow` (the genuine JDK `OutputStreamWriter(out, cs)`
                    // / `PrintStream`/`PrintWriter` `new NullPointerException(
                    // "charset")` site), dump the ENTIRE live Java thread
                    // stack — `class.method:pc` for every frame, deepest
                    // first. The frames are still fully intact here (athrow
                    // has not unwound anything yet), so this is the ground
                    // truth of which JDK method and which app/JDK call site
                    // dereferenced the null Charset. This works even when the
                    // CLI uncaught-exception renderer prints zero `\tat`
                    // frames (its `throwable_stacks` lookup having missed).
                    if crate::runtime::env_cache::charset_dbg() {
                        let exc_class_id = shared.mem.heap.class_id_of(obj_ref);
                        let exc_class_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(exc_class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        // Detail message: scan slots 0..8 for a String
                        // (Throwable layout puts `detailMessage` at slot 1,
                        // but synthetic stubs may differ — so probe a range).
                        let mut detail = String::new();
                        for fi in 0..8 {
                            if let Value::Object(Some(msg_ref)) =
                                shared.mem.heap.get_field(obj_ref, fi)
                            {
                                if let Some(s) = read_java_string(&shared.mem.heap, msg_ref) {
                                    if !s.is_empty() {
                                        detail = s;
                                        break;
                                    }
                                }
                            }
                        }
                        if exc_class_name == "java/lang/NullPointerException" && detail == "charset"
                        {
                            eprintln!(
                                "[CHARSET-NPE] NullPointerException(\"charset\") thrown via athrow \
                                 — full live Java thread stack ({} frames, deepest first):",
                                thread.frames.len()
                            );
                            for (i, f) in thread.frames.iter().enumerate().rev() {
                                let cn = shared
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(f.class_id)
                                    .map(|c| c.name.to_string())
                                    .unwrap_or_default();
                                eprintln!(
                                    "[CHARSET-NPE-STK {i}] {}.{}{} pc={}",
                                    cn,
                                    f.method_name(),
                                    f.method_descriptor(),
                                    f.pc
                                );
                            }
                        }
                    }
                    // Round-5 MED-fix (Bug 6, 2026-05-17): emit
                    // `jdk.JavaErrorThrow` for `java.lang.Error` subclasses.
                    // The emit fn was previously dead code. Gated by
                    // `cratonvm_jfr::is_enabled()` so the disabled path is
                    // ~3 ns (one Acquire load + branch). We only fire for a
                    // small whitelist of well-known `Error` types so the
                    // `class_name` argument can stay `&'static str` (per
                    // the emit-fn API contract — Errors are a fixed
                    // taxonomy).
                    if cratonvm_jfr::is_enabled() {
                        let exc_class_id = shared.mem.heap.class_id_of(obj_ref);
                        let exc_class_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(exc_class_id)
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        let static_name: Option<&'static str> = match &*exc_class_name {
                            "java/lang/OutOfMemoryError" => Some("java.lang.OutOfMemoryError"),
                            "java/lang/StackOverflowError" => Some("java.lang.StackOverflowError"),
                            "java/lang/AssertionError" => Some("java.lang.AssertionError"),
                            "java/lang/NoClassDefFoundError" => {
                                Some("java.lang.NoClassDefFoundError")
                            }
                            "java/lang/NoSuchFieldError" => Some("java.lang.NoSuchFieldError"),
                            "java/lang/NoSuchMethodError" => Some("java.lang.NoSuchMethodError"),
                            "java/lang/AbstractMethodError" => {
                                Some("java.lang.AbstractMethodError")
                            }
                            "java/lang/IncompatibleClassChangeError" => {
                                Some("java.lang.IncompatibleClassChangeError")
                            }
                            "java/lang/LinkageError" => Some("java.lang.LinkageError"),
                            "java/lang/VerifyError" => Some("java.lang.VerifyError"),
                            "java/lang/ClassFormatError" => Some("java.lang.ClassFormatError"),
                            "java/lang/UnsatisfiedLinkError" => {
                                Some("java.lang.UnsatisfiedLinkError")
                            }
                            "java/lang/ExceptionInInitializerError" => {
                                Some("java.lang.ExceptionInInitializerError")
                            }
                            "java/lang/InternalError" => Some("java.lang.InternalError"),
                            _ => None,
                        };
                        if let Some(class_name) = static_name {
                            // Best-effort detailMessage extraction (slot 1
                            // per Throwable layout). Round-5 HIGH-4 fix
                            // (2026-05-17): cache the empty-message Arc<str>
                            // in a `OnceLock` so the no-detail path is a
                            // refcount bump instead of an allocation.
                            static EMPTY_MSG: std::sync::OnceLock<Arc<str>> =
                                std::sync::OnceLock::new();
                            let empty_msg = EMPTY_MSG.get_or_init(|| Arc::from(""));
                            let message: Arc<str> = match shared.mem.heap.get_field(obj_ref, 1) {
                                Value::Object(Some(msg_ref)) => Arc::from(
                                    read_java_string(&shared.mem.heap, msg_ref).unwrap_or_default(),
                                ),
                                _ => Arc::clone(empty_msg),
                            };
                            let now_ns = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                                .as_nanos() as u64;
                            let mut jfr = shared.debug.flight_recorder.lock();
                            cratonvm_jfr::builtin::emit_java_error_throw_event(
                                &mut jfr,
                                class_name,
                                message,
                                now_ns,
                                // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                                thread.thread_id.0 as u64,
                            );
                        }
                    }
                    return Err(MethodCallFailed::ExceptionThrown(obj_ref));
                }
                Value::Object(None) => {
                    // JEP 358 increment 2: `Cannot throw exception because
                    // "<expr>" is null`. The thrown ref was at the top of the
                    // operand stack (depth 0). Falls back to the legacy
                    // `cannot throw null` text when the flag is off.
                    let message = if crate::runtime::env_cache::helpful_npe_opcodes() {
                        let action = crate::runtime::exceptions::helpful_npe::action_throw();
                        Some(helpful_npe_opcode_message(
                            shared, thread, frame_idx, &action, 0,
                        ))
                    } else {
                        Some("cannot throw null".to_string())
                    };
                    return Err(RuntimeError::NullPointerException { message }.into());
                }
                _ => {
                    return Err(VmError::Internal {
                        message: "athrow: not an object reference".to_string(),
                    }
                    .into());
                }
            }
        }

        // -- Type checking --
        Instruction::Checkcast(index) => op_checkcast(shared, thread, frame_idx, *index)?,
        Instruction::Instanceof(index) => op_instanceof(shared, thread, frame_idx, *index)?,

        // -- Monitor --
        //
        // PERF (round-5 vm #6): on the uncontended fast path JFR is almost
        // always disabled, so all of this cold work used to dominate
        // monitorenter cost:
        //   * `Instant::now()` (~20 ns on Windows QPC) on every op;
        //   * two `.to_string()` clones of the class/method name even though
        //     the `pop_object_ref_ctx_with` closure only fires on a
        //     stack-shape error (the names live in the frame as `&str`).
        // Gate every cold piece of work behind a single cached AtomicBool
        // (`cratonvm_jfr::is_enabled()`) and rebuild the diagnostic context
        // lazily from `&str` borrows inside the closure that actually needs
        // it.  Borrowing through a fresh borrow scope avoids the
        // `thread.frames[..]`-borrow-while-also-mut-borrowing-stack issue.
        Instruction::Monitorenter => op_monitorenter(shared, thread, frame_idx)?,
        Instruction::Monitorexit => op_monitorexit(shared, thread, frame_idx)?,

        // -- Unsupported / deprecated --
        Instruction::Jsr(offset) => {
            let return_addr = thread.frames[frame_idx].pc as u32; // Widening: unsigned conversion
            thread.frames[frame_idx]
                .stack
                .push(Value::ReturnAddress(return_addr))?;
            thread.frames[frame_idx].pc = branch_target(saved_pc, *offset);
        }
        Instruction::JsrW(offset) => {
            let return_addr = thread.frames[frame_idx].pc as u32; // Widening: unsigned conversion
            thread.frames[frame_idx]
                .stack
                .push(Value::ReturnAddress(return_addr))?;
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            thread.frames[frame_idx].pc = (saved_pc as i64 + *offset as i64) as usize;
            // Widening: index conversion
        }
        Instruction::Ret(index) => {
            let val = thread.frames[frame_idx].get_local(*index);
            match val {
                Value::ReturnAddress(addr) => {
                    thread.frames[frame_idx].pc = addr as usize; // Widening: index conversion
                }
                _ => {
                    return Err(VmError::Internal {
                        message: format!(
                            "ret: expected ReturnAddress in local {}, got {val}",
                            index
                        ),
                    }
                    .into());
                }
            }
        }
        Instruction::Invokedynamic(index) => {
            crate::runtime::invokedynamic::execute_invokedynamic(
                shared, thread, frame_idx, *index,
            )?;
        }
        Instruction::Wide => {
            return Err(VmError::Internal {
                message: "Wide should not appear as a standalone instruction".to_string(),
            }
            .into());
        }
    }

    Ok(InstructionResult::Continue)
}

/// `monitorexit` — release the receiver monitor.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
#[inline]
pub(super) fn op_monitorexit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Result<(), MethodCallFailed> {
            let pc_snap = thread.frames[frame_idx].pc;
            // JEP 358 increment 2: monitor object at top of stack (depth 0).
            let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
            let npe_code = Arc::clone(&thread.frames[frame_idx].code);
            let npe_cid = thread.frames[frame_idx].class_id;
            let npe_mname = thread.frames[frame_idx].method_name_arc();
            let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
            let npe_bci = thread.frames[frame_idx].last_instr_pc;
            let obj_ref = {
                let frame_ref = &thread.frames[frame_idx];
                // Cast: reinterpret pointer/address to typed pointer
                let cls_ptr = frame_ref.class_name() as *const str;
                // Cast: reinterpret pointer/address to typed pointer
                let mth_ptr = frame_ref.method_name() as *const str;
                let stack = &mut thread.frames[frame_idx].stack;
                pop_object_ref_ctx_with(stack, &shared.mem.heap, || {
                    if jep358 {
                        let action = crate::runtime::exceptions::helpful_npe::action_monitor();
                        helpful_npe_opcode_message_parts(
                            shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, 0,
                        )
                    } else {
                        // SAFETY: see Monitorenter — frame metadata is stable
                        // across the stack pop performed by `pop_object_ref_ctx_with`.
                        let cls = unsafe { &*cls_ptr };
                        let mth = unsafe { &*mth_ptr };
                        format!("monitorexit in {cls}.{mth} pc={pc_snap}")
                    }
                })?
            };
            // Round-9 JFR MED-6 fix (audit `round9-jfr.md`): the previous
            // code read `monitors.jfr_enter_recorded(obj_ref)` and discarded
            // the result with `let _ =`, justifying it as a "load-bearing
            // probe for a future exit-event emission". That justification was
            // wrong on two counts: (a) the read happens behind
            // `is_enabled()`, so disabling JFR mid-critical-section would
            // have skipped the probe entirely (defeating its stated purpose),
            // and (b) the monitor table's per-entry flag is reset atomically
            // by `exit()` on the entry_count→0 transition anyway. The probe
            // produced zero side effects and only paid for an extra lock
            // acquire. If/when a paired `monitorexit` event is wired in, the
            // emission site must consult the snapshot itself — there's no
            // value in a dead pre-read here.
            // Release + JMX retract as ONE call — see
            // `vm_exec::monitor_exit_and_retract_jmx` for why the pairing is a
            // function rather than a convention repeated at five sites.
            crate::vm::vm_exec::monitor_exit_and_retract_jmx(
                shared,
                obj_ref,
                thread.thread_id,
            )?;
    Ok(())
}

/// `monitorenter` — acquire the receiver monitor.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
#[inline]
pub(super) fn op_monitorenter(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Result<(), MethodCallFailed> {
            let pc_snap = thread.frames[frame_idx].pc;
            // JEP 358 increment 2: `Cannot enter synchronized block because
            // "<expr>" is null`. The monitor object is at the top of the
            // operand stack (depth 0).
            let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
            let npe_code = Arc::clone(&thread.frames[frame_idx].code);
            let npe_cid = thread.frames[frame_idx].class_id;
            let npe_mname = thread.frames[frame_idx].method_name_arc();
            let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
            let npe_bci = thread.frames[frame_idx].last_instr_pc;
            let obj_ref = {
                // Capture &str borrows of class/method into the closure
                // without allocating Strings on the hot path. The frame
                // borrow is dropped before `monitors.enter` runs.
                let frame_ref = &thread.frames[frame_idx];
                let cls = frame_ref.class_name();
                let mth = frame_ref.method_name();
                // SAFETY: extend lifetime of borrowed names to the closure
                // body — both are inside `frame_ref.inner` which is not
                // mutated by `pop_object_ref_ctx_with` (which only touches
                // the stack vec). We re-borrow `stack` from a fresh index.
                // Cast: reinterpret pointer/address to typed pointer
                let cls_ptr = cls as *const str;
                // Cast: reinterpret pointer/address to typed pointer
                let mth_ptr = mth as *const str;
                let stack = &mut thread.frames[frame_idx].stack;
                pop_object_ref_ctx_with(stack, &shared.mem.heap, || {
                    if jep358 {
                        let action = crate::runtime::exceptions::helpful_npe::action_monitor();
                        helpful_npe_opcode_message_parts(
                            shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, 0,
                        )
                    } else {
                        // SAFETY: cls/mth originate from `frame_ref.inner`,
                        // which is not mutated by stack ops; the pointers are
                        // valid for the duration of the closure call.
                        let cls = unsafe { &*cls_ptr };
                        let mth = unsafe { &*mth_ptr };
                        format!("monitorenter in {cls}.{mth} pc={pc_snap}")
                    }
                })?
            };
            // Snapshot the JFR-enabled flag *before* the acquire so the
            // matching exit can decide whether to emit a paired event
            // without re-reading the global flag (which may have flipped
            // mid-critical-section — round-7 vm #5).
            let jfr_on = cratonvm_jfr::is_enabled();
            let mon_start = if jfr_on {
                Some(std::time::Instant::now())
            } else {
                None
            };
            // GC-safe contended acquire — an unmarked contended wait here is
            // counted in the STW barrier's `expected` and deadlocks the
            // collector against a safepoint-parked owner (see
            // vm_exec::monitor_enter_blocking). Safe HERE because the only
            // raw copy is `obj_ref` (pinned + remapped inside) — the object
            // also lives in a frame local (javac's synchronized-block temp),
            // which the wake-side fixup remaps, and the paired monitorexit
            // re-reads it from that fixed local. The returned (possibly
            // relocated) ref is not needed afterwards.
            let _ = crate::vm::monitor_enter_blocking(shared, thread, obj_ref);
            if let Some(start) = mon_start {
                let mon_dur = start.elapsed();
                if mon_dur.as_micros() > 1000 {
                    let now_ns = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64; // Cast: duration to u64 nanoseconds
                    let mut jfr = shared.debug.flight_recorder.lock();
                    // Round-5 HIGH-fix (Bug 4, 2026-05-17): use the `_arc`
                    // variant so the monitor class name avoids a per-event
                    // `Arc::from(&str)` allocation inside `emit_*`. The
                    // frame already holds an `Arc<str>` for the class name
                    // (`frame.class_name_arc()`), so this is a refcount
                    // bump instead of a `memcpy + alloc`.
                    cratonvm_jfr::builtin::emit_monitor_enter_event_arc(
                        &mut jfr,
                        thread.frames[frame_idx].class_name_arc(),
                        "unknown",
                        obj_ref.as_ptr() as i64, // Cast: JIT ABI -- pointer to i64 register
                        thread.thread_id.0 as u64, // Widening: unsigned conversion
                        now_ns.saturating_sub(mon_dur.as_nanos() as u64), // Cast: duration to u64 nanoseconds
                        mon_dur.as_nanos() as u64, // Cast: duration to u64 nanoseconds
                    );
                    // Drop the recorder lock before reaching into MonitorTable
                    // to avoid a lock-acquisition-order inversion with the
                    // monitor's own state mutex.
                    drop(jfr);
                    // Stash the per-monitor flag so a future paired exit-side
                    // emission knows the enter event was recorded. Today no
                    // exit event is emitted, but the gate is in place so the
                    // exit handler below can safely consult it without
                    // re-checking the (potentially flipped) global flag.
                    shared
                        .threads
                        .monitors
                        .set_jfr_enter_recorded(obj_ref, thread.thread_id);
                }
            }
    Ok(())
}

/// `instanceof` — the non-throwing sibling of `checkcast`.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
#[inline]
pub(super) fn op_instanceof(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    // `index` is rebound as a reference so the moved body's `*index`
    // reads unchanged — this is a pure move, not a rewrite.
    let index = &index;
            let val = thread.frames[frame_idx].stack.pop()?;
            match val {
                Value::Object(None) => {
                    thread.frames[frame_idx].stack.push(Value::Int(0))?;
                }
                Value::Object(Some(mut obj_ref)) => {
                    let referencing_class_id = thread.frames[frame_idx].class_id;
                    // ── Resolved cast site ────────────────────────────────
                    // Everything from here to `is_subclass_of` below is
                    // re-derivation on EVERY execution: a `String` for the
                    // target class name, a full loader-aware
                    // `resolve_class_loader_aware`, and two separate
                    // `class_manager` read acquisitions. A hit answers the
                    // RESOLUTION with an array index and two integer compares,
                    // and then still performs the assignability test.
                    //
                    // Taken only for a non-array receiver, and only when
                    // `is_subclass_of` says yes: a refusal needs the name for
                    // the five fail-open fallbacks below, so it falls through
                    // to the full path unchanged. See `CastSiteCache`.
                    let cast_epochs = if cast_site_cache_enabled() {
                        match thread.cast_sites.get(referencing_class_id, *index).copied() {
                            Some(target_id) => {
                                // GC-safety: this whole block is safepoint-free
                                // — `kind_of` and `class_id_of` are object-header
                                // reads and `is_subclass_of` is a read lock over
                                // an id table. Nothing allocates, loads a class or
                                // resolves anything, so `obj_ref` cannot move and
                                // needs no `native_pin_roots` entry. The slow path
                                // below still pins, because loader-aware
                                // resolution there genuinely can safepoint.
                                //
                                // `kind_of` rather than `array_descriptor_of`: the
                                // latter builds a `String` descriptor for an array
                                // receiver, which this test would only discard.
                                let mut answered = false;
                                if shared.mem.heap.kind_of(obj_ref)
                                    != cratonvm_types::ObjectKind::Array
                                {
                                    let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
                                    answered = shared
                                        .classes
                                        .class_manager
                                        .read()
                                        .is_subclass_of(obj_class_id, target_id);
                                }
                                if answered {
                                    site_stats::bump(site_stats::CAST_HIT);
                                    thread.frames[frame_idx].stack.push(Value::Int(1))?;
                                    return Ok(());
                                }
                                site_stats::bump(site_stats::CAST_UNUSABLE);
                                None
                            }
                            None => {
                                site_stats::bump(site_stats::CAST_MISS);
                                // Read BEFORE resolving; see `SiteCache::put`.
                                Some(CastSiteCache::epochs_now())
                            }
                        }
                    } else {
                        None
                    };
                    let target_class_name = {
                        let cm = shared.classes.class_manager.read();
                        let class = cm.get_class(referencing_class_id).ok_or_else(|| {
                            VmError::Internal {
                                message: "current class not found".to_string(),
                            }
                        })?;
                        class
                            .constant_pool
                            .get_class_name(*index)
                            .ok_or_else(|| VmError::Internal {
                                message: format!("invalid class ref at cp#{index}"),
                            })?
                            .to_string()
                    };
                    // Arrays: use descriptor-based assignability. instanceof is
                    // strict (SBR-03): a genuine `Object[]` is not an instance of
                    // an unrelated `T[]`.
                    let pin = thread.native_pin_roots.len();
                    thread.native_pin_roots.push(obj_ref);
                    let result = if let Some(src_desc) = array_descriptor_of(shared, obj_ref) {
                        let result = if array_is_instance_of(shared, &src_desc, &target_class_name)
                        {
                            1
                        } else {
                            0
                        };
                        obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
                        thread.native_pin_roots.truncate(pin);
                        result
                    } else if target_class_name.starts_with('[') {
                        // Non-array object is not instanceof any array type.
                        obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
                        thread.native_pin_roots.truncate(pin);
                        0
                    } else {
                        // GC-safety: see the Checkcast arm above; this object has
                        // been popped from the operand stack and is only rooted by
                        // `native_pin_roots` until the type check completes.
                        let resolved = resolve_class_loader_aware(
                            shared,
                            thread,
                            referencing_class_id,
                            &target_class_name,
                        );
                        obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
                        thread.native_pin_roots.truncate(pin);
                        let target_class_id = resolved.map_err(|e| {
                            convert_class_not_found(shared, thread, &target_class_name, e)
                        })?;
                        // Offer the resolution to the cast-site cache. Same
                        // admissibility rule as the `new` site cache: a
                        // loader-namespaced referencing class resolves cp class
                        // names through its own loader, so its answer is not a
                        // property of the (class, index) pair alone. Array
                        // targets are excluded because the array path never
                        // consults the cache.
                        if let Some(epochs_at_entry) = cast_epochs {
                            if target_class_name.starts_with('[')
                                || referencing_class_has_loader_namespace(
                                    shared,
                                    referencing_class_id,
                                )
                            {
                                site_stats::bump(site_stats::CAST_REJECT_LOADER);
                            } else {
                                thread.cast_sites.put(
                                    referencing_class_id,
                                    *index,
                                    epochs_at_entry,
                                    target_class_id,
                                );
                                site_stats::bump(site_stats::CAST_FILL);
                            }
                        }
                        let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
                        // Bound to a `let` rather than left inline in the `if`
                        // condition, and that is load-bearing: the
                        // `class_manager.read()` temporary below is dropped at
                        // the end of THIS statement, so it is not still held
                        // when `display_class_satisfies_target` runs. That arm
                        // can take the class-manager WRITE lock (it loads the
                        // display class on first use) and `parking_lot::RwLock`
                        // is not reentrant, so evaluating it inside the same
                        // expression would self-deadlock. Same trap, and the
                        // same remedy, as `resolve_component` in
                        // `typecheck::array_is_assignable_to_impl`.
                        let assignable = shared
                            .classes
                            .class_manager
                            .read()
                            .is_subclass_of(obj_class_id, target_class_id)
                            || loader_aware_name_assignable(
                                shared,
                                obj_class_id,
                                target_class_id,
                                &target_class_name,
                            )
                            || lambda_proxy_satisfies(shared, obj_class_id, target_class_id)
                            || synthetic_implements(shared, obj_class_id, &target_class_name)
                            || proxy_instance_satisfies_target(shared, obj_ref, &target_class_name)
                            || annotation_proxy_satisfies_target(
                                shared,
                                obj_ref,
                                &target_class_name,
                            );
                        // LAST, after every cheap predicate: the receiver's
                        // `getClass()` display class. `Class.isInstance` has
                        // consulted it since the Spring `GenericConversionService`
                        // fix and the opcodes never did, so the two doors
                        // disagreed about one object at one instant — MEASURED
                        // on seven of seven immutable/unmodifiable receivers.
                        // H18-1.
                        if assignable
                            || display_class_satisfies_target(
                                shared,
                                thread,
                                &mut obj_ref,
                                obj_class_id,
                                target_class_id,
                            )
                        {
                            1
                        } else {
                            0
                        }
                    };
                    thread.frames[frame_idx].stack.push(Value::Int(result))?;
                }
                _ => thread.frames[frame_idx].stack.push(Value::Int(0))?,
            }
    Ok(())
}

/// `checkcast` — JVMS 6.5 assignability, or `ClassCastException`.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
#[inline]
pub(super) fn op_checkcast(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    // `index` is rebound as a reference so the moved body's `*index`
    // reads unchanged — this is a pure move, not a rewrite.
    let index = &index;
            let val = thread.frames[frame_idx].stack.pop()?;
            match val {
                Value::Object(None) => {
                    thread.frames[frame_idx].stack.push(Value::Object(None))?;
                }
                Value::Object(Some(mut obj_ref)) => {
                    let referencing_class_id = thread.frames[frame_idx].class_id;
                    // ── Resolved cast site ────────────────────────────────
                    // Everything from here to `is_subclass_of` below is
                    // re-derivation on EVERY execution: a `String` for the
                    // target class name, a full loader-aware
                    // `resolve_class_loader_aware`, and two separate
                    // `class_manager` read acquisitions. A hit answers the
                    // RESOLUTION with an array index and two integer compares,
                    // and then still performs the assignability test.
                    //
                    // Taken only for a non-array receiver, and only when
                    // `is_subclass_of` says yes: a refusal needs the name for
                    // the five fail-open fallbacks below, so it falls through
                    // to the full path unchanged. See `CastSiteCache`.
                    let cast_epochs = if cast_site_cache_enabled() {
                        match thread.cast_sites.get(referencing_class_id, *index).copied() {
                            Some(target_id) => {
                                // GC-safety: this whole block is safepoint-free
                                // — `kind_of` and `class_id_of` are object-header
                                // reads and `is_subclass_of` is a read lock over
                                // an id table. Nothing allocates, loads a class or
                                // resolves anything, so `obj_ref` cannot move and
                                // needs no `native_pin_roots` entry. The slow path
                                // below still pins, because loader-aware
                                // resolution there genuinely can safepoint.
                                //
                                // `kind_of` rather than `array_descriptor_of`: the
                                // latter builds a `String` descriptor for an array
                                // receiver, which this test would only discard.
                                let mut answered = false;
                                if shared.mem.heap.kind_of(obj_ref)
                                    != cratonvm_types::ObjectKind::Array
                                {
                                    let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
                                    answered = shared
                                        .classes
                                        .class_manager
                                        .read()
                                        .is_subclass_of(obj_class_id, target_id);
                                }
                                if answered {
                                    site_stats::bump(site_stats::CAST_HIT);
                                    thread.frames[frame_idx]
                                        .stack
                                        .push(Value::Object(Some(obj_ref)))?;
                                    return Ok(());
                                }
                                site_stats::bump(site_stats::CAST_UNUSABLE);
                                None
                            }
                            None => {
                                site_stats::bump(site_stats::CAST_MISS);
                                // Read BEFORE resolving; see `SiteCache::put`.
                                Some(CastSiteCache::epochs_now())
                            }
                        }
                    } else {
                        None
                    };
                    let target_class_name = {
                        let cm = shared.classes.class_manager.read();
                        let class = cm.get_class(referencing_class_id).ok_or_else(|| {
                            VmError::Internal {
                                message: "current class not found".to_string(),
                            }
                        })?;
                        class
                            .constant_pool
                            .get_class_name(*index)
                            .ok_or_else(|| VmError::Internal {
                                message: format!("invalid class ref at cp#{index}"),
                            })?
                            .to_string()
                    };
                    // GC-safety: `obj_ref` has been popped off the operand stack,
                    // so keep it in a remappable native root until this opcode either
                    // pushes it back or decides to throw. Array assignability and
                    // loader-aware resolution can both take paths that may safepoint.
                    let pin = thread.native_pin_roots.len();
                    thread.native_pin_roots.push(obj_ref);
                    // If the object is an array, use descriptor-based assignability
                    // to correctly reject invalid casts (e.g. int[] -> Object[]).
                    let cast_ok = if let Some(src_desc) = array_descriptor_of(shared, obj_ref) {
                        let ok = array_is_assignable_to(shared, &src_desc, &target_class_name);
                        obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
                        thread.native_pin_roots.truncate(pin);
                        ok
                    } else if target_class_name.starts_with('[') {
                        // Non-array object cannot be cast to an array type.
                        obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
                        thread.native_pin_roots.truncate(pin);
                        false
                    } else {
                        // Use load_class_concurrent (read-lock fast path) not
                        // class_manager.write().load_class() — the write lock
                        // would block if any JIT thread holds a read lock during
                        // compilation, causing interpreter hangs under concurrent JIT.
                        //
                        let resolved = resolve_class_loader_aware(
                            shared,
                            thread,
                            referencing_class_id,
                            &target_class_name,
                        );
                        obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
                        thread.native_pin_roots.truncate(pin);
                        let target_class_id = resolved.map_err(|e| {
                            convert_class_not_found(shared, thread, &target_class_name, e)
                        })?;
                        // Offer the resolution to the cast-site cache. Same
                        // admissibility rule as the `new` site cache: a
                        // loader-namespaced referencing class resolves cp class
                        // names through its own loader, so its answer is not a
                        // property of the (class, index) pair alone. Array
                        // targets are excluded because the array path never
                        // consults the cache.
                        if let Some(epochs_at_entry) = cast_epochs {
                            if target_class_name.starts_with('[')
                                || referencing_class_has_loader_namespace(
                                    shared,
                                    referencing_class_id,
                                )
                            {
                                site_stats::bump(site_stats::CAST_REJECT_LOADER);
                            } else {
                                thread.cast_sites.put(
                                    referencing_class_id,
                                    *index,
                                    epochs_at_entry,
                                    target_class_id,
                                );
                                site_stats::bump(site_stats::CAST_FILL);
                            }
                        }
                        let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
                        // Bound to a `let`, not left as the block's trailing
                        // expression: that drops the `class_manager.read()`
                        // temporary before the display arm below, which can
                        // take the WRITE lock. See the twin in `op_instanceof`.
                        let assignable = shared
                            .classes
                            .class_manager
                            .read()
                            .is_subclass_of(obj_class_id, target_class_id)
                            || loader_aware_name_assignable(
                                shared,
                                obj_class_id,
                                target_class_id,
                                &target_class_name,
                            )
                            || lambda_proxy_satisfies(shared, obj_class_id, target_class_id)
                            || synthetic_implements(shared, obj_class_id, &target_class_name)
                            || proxy_instance_satisfies_target(shared, obj_ref, &target_class_name)
                            || annotation_proxy_satisfies_target(
                                shared,
                                obj_ref,
                                &target_class_name,
                            );
                        // `&mut obj_ref` is not decoration: the failure path
                        // below reads the receiver AGAIN (`class_id_of`, then
                        // `cce_display_class_name`). The display arm can load a
                        // class and therefore safepoint, so it pins and hands
                        // back the possibly-moved reference. H18-1.
                        assignable
                            || display_class_satisfies_target(
                                shared,
                                thread,
                                &mut obj_ref,
                                obj_class_id,
                                target_class_id,
                            )
                    };
                    if !cast_ok {
                        let actual_class_id = shared.mem.heap.class_id_of(obj_ref);
                        let obj_class_name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(actual_class_id)
                            .map(|c| c.name.to_string())
                            // A bare `?` here cost a whole triage round: it is
                            // the only thing the message says about a receiver
                            // whose class id resolves to nothing, and "?" is
                            // consistent with a reclaimed header, a foreign
                            // layout domain, and the synthetic auto-box wrapper
                            // alike. The id tells those apart on the first
                            // sighting (`AUTOBOX_CLASS_ID` is `u32::MAX`), so
                            // name it rather than counting the question marks.
                            .unwrap_or_else(|| format!("?class_id={actual_class_id}"));
                        // Spring's ConfigurationClassParser reaches this cast
                        // only after requesting annotation attributes with
                        // `classValuesAsString=true`. Under its forked
                        // class-path loader the normal annotation adapter can
                        // leave a VM-owned Class[] at that boundary. Convert
                        // that exact representation to the requested String[]
                        // rather than weakening general array assignability.
                        let source_class_parser_boundary = target_class_name == "[Ljava/lang/String;"
                            && thread.frames[frame_idx].class_name()
                                == "org/springframework/context/annotation/ConfigurationClassParser$SourceClass"
                            && thread.frames[frame_idx].method_name()
                                == "getAnnotationAttributes"
                            && obj_class_name == "java/lang/Class"
                            && shared.mem.heap.kind_of(obj_ref)
                                == cratonvm_types::ObjectKind::Array;
                        if source_class_parser_boundary {
                            if let Value::Object(Some(strings)) =
                                crate::vm::convert_class_values_to_strings(
                                    shared,
                                    Value::Object(Some(obj_ref)),
                                )
                            {
                                thread.frames[frame_idx]
                                    .stack
                                    .push(Value::Object(Some(strings)))?;
                                // Early exit from the moved body: in the match arm this returned
                                // `Continue` from `execute_instruction`; here it returns from the
                                // helper and the caller's `?` reaches the same `Ok(Continue)` tail.
                                return Ok(());
                            }
                        }
                        // `ServletComponentHandler` asks Spring metadata for
                        // a nested `WebInitParam[]`. The forked class-path
                        // reader can surface a lone annotation proxy instead
                        // of that array; materialize the one element through
                        // the same AnnotationAttributes map contract Spring
                        // uses for regular annotation arrays.
                        let servlet_init_params_boundary = target_class_name
                            == "[Lorg/springframework/core/annotation/AnnotationAttributes;"
                            && thread.frames[frame_idx].class_name()
                                == "org/springframework/boot/web/server/servlet/context/ServletComponentHandler"
                            && thread.frames[frame_idx].method_name() == "extractInitParameters";
                        if servlet_init_params_boundary {
                            if let Some(attributes) =
                                crate::vm::annotation_proxy_to_annotation_attributes_array(
                                    shared, thread, obj_ref,
                                )?
                            {
                                thread.frames[frame_idx]
                                    .stack
                                    .push(Value::Object(Some(attributes)))?;
                                return Ok(());
                            }
                        }
                        if crate::runtime::env_cache::cce_dbg() {
                            eprintln!(
                                "[CCE_DBG] checkcast fail: obj_cid={} obj_class={} target={} caller={}.{}{}",
                                actual_class_id,
                                obj_class_name,
                                target_class_name,
                                thread.frames[frame_idx].class_name(),
                                thread.frames[frame_idx].method_name(),
                                thread.frames[frame_idx].method_descriptor(),
                            );
                        }
                        // S-trinity #1: when the runtime class is a bare
                        // `Object` / cid=0 (synthetic alloc that lost
                        // class_id) and the checkcast target is a
                        // ClassLoader-shaped type — i.e. log4j's
                        // `LoaderUtil.getThreadContextClassLoader` / the
                        // `(ClassLoader) priv.run()` chain in
                        // `Logger.doGetMessageLogger` — substitute the
                        // singleton app ClassLoader. The unidentifiable
                        // value can only have come from one of our
                        // classloader-returning natives (every concrete
                        // `Class.getClassLoader` / `Thread.getContextClassLoader`
                        // path ends in `get_or_create_app_loader`), so the
                        // app loader is the spec-correct standin and lets
                        // the caller's `loadClass` chain proceed instead
                        // of poisoning `<clinit>` with an EIIE.
                        let is_classloader_target = target_class_name == "java/lang/ClassLoader"
                            || target_class_name == "java/security/SecureClassLoader"
                            || target_class_name == "jdk/internal/loader/BuiltinClassLoader"
                            || target_class_name
                                == "jdk/internal/loader/ClassLoaders$AppClassLoader"
                            || target_class_name
                                == "jdk/internal/loader/ClassLoaders$PlatformClassLoader";
                        let obj_is_bare_object = actual_class_id == ClassId::new(0)
                            || obj_class_name == "java/lang/Object";
                        if is_classloader_target && obj_is_bare_object {
                            if let Some(loader_obj) =
                                cratonvm_native_builtins::classloader::peek_app_loader(
                                    shared.vm_identity,
                                )
                            {
                                tracing::debug!(
                                    target: "cratonvm::interp::checkcast",
                                    "S-trinity #1 — substituting app ClassLoader for cid=0 \
                                     Object on checkcast → {}",
                                    target_class_name,
                                );
                                thread.frames[frame_idx]
                                    .stack
                                    .push(Value::Object(Some(loader_obj)))?;
                                return Ok(());
                            }
                        }
                        // Render both operands as binary (dotted) class names,
                        // matching HotSpot's `ClassCastException` message.
                        // Tools parse this message: mockk's `JvmAutoHinter`
                        // applies the regex `cannot be cast to (class )?(.+/)?
                        // (.+?)( \(...\))?$` and reads group 3 as the target
                        // type, then `Class.forName`s it to learn a mock's
                        // return type. With our former *internal* (slashed)
                        // names — e.g. `... cast to java/lang/String` — the
                        // `(.+/)?` group greedily ate `java/lang/`, leaving
                        // group 3 = `String`, so `Class.forName("String")`
                        // threw `ClassNotFoundException: String` and every
                        // reified Kotlin extension test that records a mock
                        // (`getBean<T>()`, `getProperty<T>()`) failed. Dotted
                        // names contain no `/`, so group 3 captures the full
                        // FQN exactly as on HotSpot.
                        let obj_binary = cce_display_class_name(shared, obj_ref, &obj_class_name)
                            .replace('/', ".");
                        let target_binary = target_class_name.replace('/', ".");
                        // CRATONVM_DBG_CCE_BT: identify the failing receiver
                        // (address + classes) at the moment a checkcast CCE
                        // is constructed — attribution for the WildFly
                        // `parallel-extension-add` stale-object CCE family,
                        // correlated against GC logs / the
                        // CRATONVM_DBG_STALE_OBJREF quarantine ring.
                        if crate::runtime::interpreter::dbg_cce_bt_enabled() {
                            eprintln!(
                                "CRATONVM_DBG_CCE_BT: site=checkcast obj={obj_binary} @0x{:x} target={target_binary}",
                                obj_ref.as_ptr() as usize
                            );
                            // Java frame stack at the failing checkcast — a
                            // checkcast CCE is VM-raised (no `athrow`
                            // bytecode), so the ATHROW tracer never sees it
                            // and, uncaught during parallel-extension-add,
                            // the rollback path prints no stack either. The
                            // frames are fully intact here (nothing has
                            // unwound yet) — this names the exact producing
                            // frame for the family's residual shapes.
                            for (i, f) in thread.frames.iter().enumerate().rev().take(15) {
                                let cn = shared
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(f.class_id)
                                    .map(|c| c.name.clone())
                                    .unwrap_or_default();
                                eprintln!(
                                    "  CCE-BT-STK[{i}] {}.{} pc={}",
                                    cn,
                                    f.method_name(),
                                    f.pc
                                );
                            }
                            // stw-residual-close FIX (2026-07-23): a checkcast
                            // CCE against a bare `java.lang.Object` (this
                            // family's signature -- the checked value reads
                            // back with an all-zero/degraded header instead
                            // of its real class) is mechanistically the SAME
                            // "stale ref surfaces after a nested allocating
                            // call" shape `[stale-recv]` already forensics,
                            // just consumed at a site with no healing path.
                            // Reuse the exact same forensic probes here so
                            // the NEXT capture (rare -- observed 2x so far,
                            // once fatal/PathAddress, once non-fatal/
                            // FieldElement$Label) doesn't need a follow-up
                            // campaign just to get gcpart/pushprov/zeroed
                            // data. Only meaningful when the receiver
                            // degraded to bare Object (obj_binary check);
                            // a genuine app-level CCE (real type A vs B) has
                            // nothing useful to probe here.
                            if obj_binary == "java.lang.Object" && remap_trace_on() {
                                let addr = obj_ref.as_ptr() as usize;
                                for (e, moved_to, mlen, as_dest) in
                                    crate::memory::gc::gcpart_probe(addr)
                                {
                                    eprintln!(
                                        "  CCE-BT-GCPART epoch={e} map_len={mlen} moved_to={moved_to:x?} appears_as_dest={as_dest}"
                                    );
                                }
                                for (ago, site) in push_prov_find(addr) {
                                    eprintln!(
                                        "  CCE-BT-PUSHPROV pushed {ago} pushes ago at {site}"
                                    );
                                }
                                for (age, site, tag, s, l) in
                                    cratonvm_gc::zero_forensics::probe(addr)
                                {
                                    eprintln!(
                                        "  CCE-BT-ZEROED age={age} site={} tag={tag} range=0x{s:x}+0x{l:x}",
                                        if site == 1 { "sweep-span" } else { "fromspace-reset" },
                                    );
                                }
                                for (ago, parent, fidx) in getfield_ring_find(addr) {
                                    eprintln!(
                                        "  CCE-BT-GETFIELD pushed {ago} getfields ago from parent=0x{parent:x} fld[{fidx}]"
                                    );
                                }
                            }
                        }
                        // RECLAIMED-LIVE-OBJECT reporter. `java.lang.Object`
                        // is `ClassId(0)` — also the ALL-ZERO header the young
                        // sweep writes over every span it reclaims — so a
                        // checkcast that fails with `java.lang.Object` as the
                        // ACTUAL class is ambiguous: an ordinary
                        // `(String) new Object()`, or a reference to an object
                        // the collector freed while it was still reachable.
                        //
                        // Nothing available on this path distinguishes the two
                        // for free (`zero_forensics` and the sweep ring are
                        // both gated), so this deliberately does NOT guess.
                        // It consults the sweep ring, which is populated only
                        // under `CRATONVM_DBG_SWEEP_ZERO`, and speaks only on a
                        // HIT — where the address provably was a swept span and
                        // the ring still knows what class it held. That names
                        // the root-coverage gap, which is the whole question.
                        //
                        // Wiring this here is the point: the ring already had a
                        // consumer on the stale-RECEIVER (invoke) path, but a
                        // reclaimed object usually surfaces as a failed CAST
                        // first, and that path reported nothing — so setting
                        // the flag and reproducing still produced silence. See
                        // audits/old-sweep-liveness.md section 7.
                        // H2-CID0 follow-up (2026-08-01): the `ClassId(0)` gate
                        // below is too narrow. A block freed while still
                        // referenced only reads back as `java.lang.Object`
                        // while it stays on the free list; once the allocator
                        // REUSES it the same stale reference sees a perfectly
                        // valid object of some unrelated class, and the cast
                        // fails with that class instead. Both faces were
                        // observed in one A/B: `java.lang.Object cannot be cast
                        // to java.nio.ByteBuffer` (still free) and
                        // `java.util.BitSet cannot be cast to
                        // org.h2.mvstore.Chunk` (reused). Gating the reporter on
                        // `ClassId(0)` reports the first and stays silent on the
                        // second, which is the same defect one step later in the
                        // block's life.
                        //
                        // So ask on EVERY failing cast. Both queries are reached
                        // only after a cast has already failed, and the
                        // reclamation ring is bounded, so a match means the
                        // address really was freed recently.
                        //
                        // MIGRATED 2026-08-17. `memory::reclaim_guard`'s header
                        // named this the "remaining un-migrated" copy of the
                        // verdict, and the divergence had teeth: the shared
                        // reporter asks the free-list question on ZGC (the
                        // default collector) and consults ZGC's relocation
                        // ledger, and this copy asked neither -- so the H2
                        // `TestMultiThread` MVStore-writer failure, which
                        // surfaces as `ClassCastException: java.math.BigDecimal
                        // cannot be cast to org.h2.mvstore.Page` on a
                        // COMPACTING heap, printed nothing at all. `_forced`
                        // rather than the gated entry point for the reason the
                        // comment above gives: the re-served face has a
                        // non-zero class id, which is exactly what the gate
                        // suppresses.
                        crate::memory::reclaim_guard::report_reclaimed_receiver_forced(
                            shared,
                            obj_ref.as_ptr() as usize,
                            "checkcast",
                            &target_binary,
                            shared.mem.heap.class_id_of(obj_ref).as_u32(),
                        );
                        // WHICH SLOT still holds it. The reporters above say the
                        // address is wrong and where its object went; they cannot
                        // say who is naming it, and on the surviving H2
                        // MVStore-writer residual that is the whole remaining
                        // question — the stale reference reaches this cast
                        // without ever being pushed as a vacated address, stored
                        // into a local as one, or used as a field-read receiver
                        // as one, because by then the allocator has re-issued the
                        // address. The frame walk is the one view left, and it
                        // names the Java slot, hence the bytecode that put it
                        // there.
                        crate::memory::reclaim_guard::report_root_slice_provenance(
                            shared,
                            thread,
                            obj_ref.as_ptr() as usize,
                            "checkcast",
                        );
                        {
                            let addr = obj_ref.as_ptr() as usize;
                            if let Some((cid, kind, site, seq, fbase, fsize, fflags)) =
                                cratonvm_gc::gen_heap::old_freed_lookup_covering(addr)
                            {
                                static F: std::sync::atomic::AtomicU64 =
                                    std::sync::atomic::AtomicU64::new(0);
                                if F.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                                    let orig = shared
                                        .classes
                                        .class_manager
                                        .try_read()
                                        .and_then(|cm| {
                                            cm.class_store
                                                .get(cratonvm_types::ClassId::new(cid))
                                                .map(|c| c.name.to_string())
                                        })
                                        .unwrap_or_else(|| format!("class_id={cid}"));
                                    let (old_alloc, young_surv, region) =
                                        shared.mem.heap.liveness_arms(addr);
                                    tracing::error!(
                                        target: "cratonvm::gc::guard",
                                        obj = format!("{addr:#x}"),
                                        actual_class = %obj_binary,
                                        target_class = %target_binary,
                                        original_class = %orig,
                                        original_kind = kind,
                                        freed_block = format!("{fbase:#x}+{fsize:#x}"),
                                        interior_off = addr - fbase,
                                        was_weak_referent =
                                            fflags & cratonvm_gc::gen_heap::OLD_FREED_FLAG_WATCHED
                                                != 0,
                                        interior_root_pointed_in = fflags
                                            & cratonvm_gc::gen_heap::OLD_FREED_FLAG_INTERIOR_ROOT
                                            != 0,
                                        freed_by = if site == 1 {
                                            "in-place old-gen sweep"
                                        } else {
                                            "old-gen mark-compact"
                                        },
                                        free_seq = seq,
                                        region = %region,
                                        old_gen_allocated = old_alloc,
                                        young_survivor = young_surv,
                                        "checkcast receiver is an OLD-GEN block this process \
                                         RECLAIMED while it was still referenced. `original_class` \
                                         is what the block held when it was freed; `actual_class` \
                                         is whatever occupies it now (`java.lang.Object` means the \
                                         block is still on the free list, anything else means the \
                                         allocator has already re-served it). `freed_by` names the \
                                         mark phase with the gap.",
                                    );
                                }
                            }
                        }
                        if obj_class_name == "java/lang/Object"
                            && target_class_name != "java/lang/Object"
                        {
                            let addr = obj_ref.as_ptr() as usize;
                            // H2-CID0 (2026-08-01): the FLAG-FREE verdict.
                            //
                            // Everything else on this path needs a debug gate
                            // to have been set before the run, which is never
                            // true of the run that actually reproduces. Ask the
                            // heap instead: is this address inside a free-list
                            // hole, past the allocation frontier, or in the
                            // inactive semispace? A live `new Object()` is in
                            // none of those, so a hit is proof the collector
                            // reclaimed an object that is still referenced —
                            // and it costs nothing until a cast has already
                            // failed.
                            if let Some((what, span, size)) =
                                shared.mem.heap.reclaimed_hole_at(addr)
                            {
                                static R: std::sync::atomic::AtomicU64 =
                                    std::sync::atomic::AtomicU64::new(0);
                                if R.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                                    tracing::error!(
                                        target: "cratonvm::gc::guard",
                                        obj = format!("{addr:#x}"),
                                        location = %what,
                                        span = format!("{span:#x}+{size:#x}"),
                                        target_class = %target_binary,
                                        "checkcast receiver points into RECLAIMED memory — a \
                                         still-referenced object was collected. `java.lang.Object` \
                                         here is the all-zero header the collector left behind, \
                                         not a real Object.",
                                    );
                                    // H2-CID0: and WHAT was reclaimed. The
                                    // old-gen ring is unconditional, so unlike
                                    // the young sweep ring this answers on the
                                    // FIRST occurrence rather than only on a
                                    // re-run with the right flag pre-set.
                                    if let Some((cid, kind, site, seq, _b, _s, wflags)) =
                                        cratonvm_gc::gen_heap::old_freed_lookup_covering(addr)
                                    {
                                        let orig = shared
                                            .classes
                                            .class_manager
                                            .try_read()
                                            .and_then(|cm| {
                                                cm.class_store
                                                    .get(cratonvm_types::ClassId::new(cid))
                                                    .map(|c| c.name.to_string())
                                            })
                                            .unwrap_or_else(|| format!("class_id={cid}"));
                                        tracing::error!(
                                            target: "cratonvm::gc::guard",
                                            obj = format!("{addr:#x}"),
                                            original_class = %orig,
                                            original_kind = kind,
                                            was_weak_referent = wflags
                                                & cratonvm_gc::gen_heap::OLD_FREED_FLAG_WATCHED
                                                != 0,
                                            freed_by = if site == 1 {
                                                "in-place old-gen sweep"
                                            } else {
                                                "old-gen mark-compact"
                                            },
                                            free_seq = seq,
                                            "…and the old-gen reclamation ring knows what that \
                                             block held. The original class names the mark-phase \
                                             gap that freed it while it was still referenced.",
                                        );
                                    }
                                }
                            }
                            if let Some((cid, kind, cycle, reason, initiator, blocked)) =
                                cratonvm_gc::gen_heap::sweep_zero_lookup(addr)
                            {
                                static N: std::sync::atomic::AtomicU64 =
                                    std::sync::atomic::AtomicU64::new(0);
                                if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                                    let orig = shared
                                        .classes
                                        .class_manager
                                        .try_read()
                                        .and_then(|cm| {
                                            cm.class_store
                                                .get(cratonvm_types::ClassId::new(cid))
                                                .map(|c| c.name.to_string())
                                        })
                                        .unwrap_or_else(|| format!("class_id={cid}"));
                                    tracing::error!(
                                        target: "cratonvm::gc::guard",
                                        obj = format!("{addr:#x}"),
                                        original_class = %orig,
                                        original_kind = kind,
                                        sweep_cycle = cycle,
                                        gc_reason = reason,
                                        gc_initiator = initiator,
                                        threads_blocked = blocked,
                                        target = %target_binary,
                                        "checkcast receiver was RECLAIMED BY THE YOUNG SWEEP while \
                                         still reachable — its header is the all-zero span the \
                                         sweep wrote. The original class names the root-coverage \
                                         gap.",
                                    );
                                }
                            }
                            // H2-CID0: the sweep ring above only knows the
                            // NON-MOVING sweep's dead spans. An all-zero header
                            // is equally the signature of a MOVING cycle that
                            // failed to evacuate a live object and then reset
                            // from-space over it, and the two demand different
                            // investigations. `zero_forensics`
                            // (`CRATONVM_DBG_ZERO_RANGES`) records BOTH sites,
                            // so consult it too — separately, because the doc
                            // this reporter serves attributes the fault to the
                            // non-moving sweep on evidence that never
                            // distinguished them. Gated by its own flag, so it
                            // is silent unless asked for.
                            for (age, site, tag, s, l) in
                                cratonvm_gc::zero_forensics::probe(addr)
                            {
                                static Z: std::sync::atomic::AtomicU64 =
                                    std::sync::atomic::AtomicU64::new(0);
                                if Z.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 16 {
                                    break;
                                }
                                tracing::error!(
                                    target: "cratonvm::gc::guard",
                                    obj = format!("{addr:#x}"),
                                    zeroing_site = if site == 1 {
                                        "non-moving-sweep-dead-span"
                                    } else {
                                        "moving-gc-fromspace-reset"
                                    },
                                    sweep_cycle = tag,
                                    range = format!("{s:#x}+{l:#x}"),
                                    events_ago = age,
                                    target = %target_binary,
                                    "checkcast receiver lies inside a range the collector ZEROED \
                                     — the site names which collector reclaimed it.",
                                );
                            }
                        }
                        return Err(RuntimeError::ClassCastException {
                            message: format!("{obj_binary} cannot be cast to {target_binary}"),
                        }
                        .into());
                    }
                    thread.frames[frame_idx]
                        .stack
                        .push(Value::Object(Some(obj_ref)))?;
                }
                other => {
                    // A non-reference where the verifier guarantees a
                    // reference. Name the value AND the site: the bare
                    // "not an object reference" text left nothing to work
                    // with, and the usual producer is a deopt resume that
                    // rebuilt a ref-typed operand-stack slot as an `Int`
                    // (a `stack_oop_marks` / `FrameValue` typing miss).
                    let f = &thread.frames[frame_idx];
                    return Err(VmError::Internal {
                        message: format!(
                            "checkcast: not an object reference (got {other:?}) \
                             at {}.{}{} pc={} cp#{index}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor(),
                            f.last_instr_pc,
                        ),
                    }
                    .into());
                }
            }
    Ok(())
}

/// `new` — resolve, access-check, initialise, allocate.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
#[inline]
pub(super) fn op_new(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    // `index` is rebound as a reference so the moved body's `*index`
    // reads unchanged — this is a pure move, not a rewrite.
    let index = &index;
            let referencing_class_id = thread.frames[frame_idx].class_id;
            // ---- Resolved constant pool: per-thread `new`-site cache --------
            //
            // Everything between here and `gc_alloc_object` below is
            // re-derivation: a `String` for the class name, a full
            // `resolve_class_loader_aware`, the JVMS 5.4.4 access check, the
            // initialization check and the field count — four separate
            // `class_manager` read acquisitions and one heap allocation, on
            // EVERY execution of a `new`. A hit answers all of it with an array
            // index and four integer compares.
            //
            // See `site_cache::ClassSiteCache` for which sites are admissible
            // (loader-blind referencing classes only, a property immutable per
            // class) and why a hit may skip the access and initialization
            // checks. `maybe_gc` at the end of this arm still runs on both
            // paths, so the cache changes what is COMPUTED, never when a
            // collection can happen.
            let new_site_epochs = if new_site_cache_enabled() {
                if let Some(hit) = thread.class_sites.get(referencing_class_id, *index) {
                    let hit = *hit;
                    site_stats::bump(site_stats::NEW_HIT);
                    let obj_ref = gc_alloc_object(
                        shared,
                        thread,
                        hit.class_id,
                        // Widening: u32 → usize
                        hit.num_fields as usize,
                    )?;
                    if remap_trace_on() {
                        push_prov_record(obj_ref.as_ptr() as usize, "new");
                    }
                    thread.frames[frame_idx]
                        .stack
                        .push(Value::Object(Some(obj_ref)))?;
                    maybe_gc(shared, thread);
                    return Ok(());
                }
                site_stats::bump(site_stats::NEW_MISS);
                // Read BEFORE resolving; see `SiteCache::put`.
                Some(ClassSiteCache::epochs_now())
            } else {
                None
            };
            let class_name = {
                let cm = shared.classes.class_manager.read();
                let class =
                    cm.get_class(referencing_class_id)
                        .ok_or_else(|| VmError::Internal {
                            message: "current class not found".to_string(),
                        })?;
                class
                    .constant_pool
                    .get_class_name(*index)
                    .ok_or_else(|| VmError::Internal {
                        message: format!("invalid class ref at cp#{index}"),
                    })?
                    .to_string()
            };

            let target_class_id =
                resolve_class_loader_aware(shared, thread, referencing_class_id, &class_name)
                    .map_err(|e| convert_class_not_found(shared, thread, &class_name, e))?;

            if crate::runtime::env_cache::dbg_h2trace()
                && (class_name == "org/h2/command/Parser"
                    || class_name == "org/h2/command/ParserBase"
                    || class_name == "org/h2/command/Token")
            {
                let cm = shared.classes.class_manager.read();
                let ref_loader = cm.get_loader_id(referencing_class_id);
                let target_loader = cm.get_loader_id(target_class_id);
                let ref_name = cm
                    .get_class(referencing_class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                drop(cm);
                eprintln!(
                    "[h2trace-new] BYTECODE-NEW class_name={class_name} referencing_class={ref_name} referencing_class_id={referencing_class_id:?} referencing_loader={ref_loader:?} target_class_id={target_class_id:?} target_loader={target_loader:?}",
                );
            }

            if crate::runtime::env_cache::dbg_loader_trace() && class_name.contains("RootReference")
            {
                let cm = shared.classes.class_manager.read();
                let ref_loader = cm.get_loader_id(referencing_class_id);
                let target_loader = cm.get_loader_id(target_class_id);
                eprintln!(
                    "[LOADER-TRACE] new thread={:?} class_name={class_name} referencing_class_id={referencing_class_id:?} referencing_loader={ref_loader:?} target_class_id={target_class_id:?} target_loader={target_loader:?}",
                    thread.thread_id
                );
                if matches!(ref_loader, Some(cratonvm_types::ClassLoaderId::Application)) {
                    eprintln!(
                        "[LOADER-TRACE-STACK] full Java stack for this Application-context 'new':"
                    );
                    for (i, f) in thread.frames.iter().enumerate().rev() {
                        eprintln!(
                            "[LOADER-TRACE-STACK]   [{i}] {}.{}{} pc={}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc
                        );
                    }
                }
            }

            // JVMS 6.5 `new`, run-time exceptions: IllegalAccessError if the
            // referencing class does not have permission to access the
            // resolved class (JVMS 5.4.4 -- public, or same *runtime*
            // package as the referencing class). `check_class_access`
            // compares runtime package identity as the JVMS 5.3 tuple
            // (defining class loader, package name), so a package-private
            // class defined by a DIFFERENT `ClassLoader` instance is
            // correctly rejected even when the package NAME matches --
            // while ordinary same-loader package-private instantiation
            // (by far the common case) is unaffected.
            {
                let cm = shared.classes.class_manager.read();
                if let (Some(accessor), Some(target)) = (
                    cm.get_class(referencing_class_id),
                    cm.get_class(target_class_id),
                ) {
                    crate::classloading::access_control::check_class_access(accessor, target)?;
                }
            }

            ensure_class_initialized_shared(shared, thread, target_class_id)?;

            let num_fields = shared
                .classes
                .class_manager
                .read()
                .get_class(target_class_id)
                .map(|c| c.num_total_fields)
                .unwrap_or(0);
            // Offer the site now that resolution, the access check and
            // initialization have all succeeded. Refused for a referencing
            // class with a loader namespace, and for an array name — `new` on
            // an array type is not legal bytecode, but the resolver's `[` arm
            // is loader-faithful in its own way and nothing here should be the
            // first to assume otherwise.
            if let Some(epochs_at_entry) = new_site_epochs {
                // The initialization state must be `Initialized`, not merely
                // "`ensure_class_initialized_shared` returned Ok". Those differ
                // in exactly one window: that call also answers Ok for a class
                // THIS thread is already initializing, i.e. a `new C()` reached
                // from C's own `<clinit>`. If that `<clinit>` then fails, C is
                // Erroneous and every later `new C()` must throw
                // NoClassDefFoundError — which an entry filled from inside the
                // window would silently allocate past. Both real states are
                // terminal, so filling only from `Initialized` is what makes
                // the hit path's skip sound rather than nearly sound.
                if !crate::vm::is_class_initialized_via_manager(shared, target_class_id) {
                    site_stats::bump(site_stats::NEW_REJECT_LOADER);
                } else if class_name.starts_with('[') || referencing_class_has_loader_namespace(
                    shared,
                    referencing_class_id,
                ) {
                    site_stats::bump(site_stats::NEW_REJECT_LOADER);
                } else {
                    thread.class_sites.put(
                        referencing_class_id,
                        *index,
                        epochs_at_entry,
                        ResolvedNewSite {
                            class_id: target_class_id,
                            // Narrowing: a class-file field table is u16-sized,
                            // so this is unreachable for a verified class; the
                            // saturating form keeps a corrupt synthetic caller
                            // out of a panic on the allocation path, exactly as
                            // `init_object_header` does with the same value.
                            num_fields: u32::try_from(num_fields).unwrap_or(u32::MAX),
                        },
                    );
                    site_stats::bump(site_stats::NEW_FILL);
                }
            }
            let obj_ref = gc_alloc_object(shared, thread, target_class_id, num_fields)?;
            // SPORTME-NSEE-TRACE: print full Java stack when NoSuchElementException is constructed.
            if class_name == "java/util/NoSuchElementException"
                && crate::runtime::env_cache::nsee_trace()
            {
                eprintln!("[NSEE-TRACE] new java/util/NoSuchElementException at:");
                let cm = shared.classes.class_manager.read();
                for (i, f) in thread.frames.iter().enumerate().rev().take(30) {
                    let cn = cm
                        .get_class(f.class_id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    eprintln!("[NSEE-STK {i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                }
            }
            if remap_trace_on() {
                push_prov_record(obj_ref.as_ptr() as usize, "new");
            }
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(obj_ref)))?;
            maybe_gc(shared, thread);
    Ok(())
}

/// `putfield` — including the SATB pre-barrier and the write barrier.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
#[inline]
pub(super) fn op_putfield(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    // Self-test only, and deliberately a site with NO watch: `putfield` is a
    // bytecode every program executes, so if the safepoint BACKSTOP cannot
    // report a cell injected here it cannot report a real one either.
    crate::memory::reclaim_guard::corrupt_cell_selftest_inject("putfield");
    // `index` is rebound as a reference so the moved body's `*index`
    // reads unchanged — this is a pure move, not a rewrite.
    let index = &index;
            let current_class_id = thread.frames[frame_idx].class_id;
            // Resolve the field early (cached) so its descriptor byte is in hand
            // for the tag-exact value pop below. The loader-aware resolver is
            // stack-neutral, and surfacing a resolution error here (before the
            // value/objectref pop) is spec-compliant for putfield.
            let mut field =
                resolve_field_ref_loader_aware(shared, thread, current_class_id, *index)?;
            // K2 (T10.9.E) — tag-exact pop for category-2 primitives.
            //
            // The stack top before putfield is [..., objectref, value] (with
            // `value` a single CompactValue slot for both category-1 and
            // category-2 primitives, since our `CompactValue` stores the
            // full 64-bit payload in one slot).  For J/D we pop the raw
            // CompactValue and decode it based on its tag; the generic
            // `pop()?` path would first decode untagged long bits as
            // `Value::Double` via `to_value()` and then re-encode on the
            // heap write — silently corrupting the long payload.
            //
            // A tag-mismatched slot (e.g. an Uninitialized or Object slot
            // landing where a Long was expected) coerces to 0 rather than
            // panicking, matching the defensive pop_int/pop_long convention
            // in value_stack.rs; a truly bogus upstream producer is already
            // flagged by the verifier.
            let desc_byte = Some(field.desc_byte);
            let value: Value = match desc_byte {
                Some(b'J') => {
                    // Kinds-aware bit-exact pop: a slot marked KIND_LONG (the
                    // genuine long producer case) is read verbatim, so a
                    // collision-shaped long whose `0xFFFC_…` top bits + low
                    // 32-bit payload masquerade as a tagged int (SHA-512
                    // H1..H8 working variables) is stored without truncation.
                    // The KIND_UNKNOWN fallback inside `pop_long` keeps the
                    // i2l-widening crutch for synthetic int-where-long.
                    Value::Long(thread.frames[frame_idx].stack.pop_long()?)
                }
                Some(b'D') => {
                    use crate::types::CompactTag;
                    let cv = thread.frames[frame_idx].stack.pop_compact_checked()?;
                    let dv = match cv.tag() {
                        CompactTag::Double => f64::from_bits(cv.raw_bits()),
                        // Cast: integer word reinterpreted as float/double bit pattern
                        CompactTag::Long => f64::from_bits(cv.as_long_unchecked() as u64),
                        CompactTag::Int => match cv.to_value() {
                            // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                            Value::Int(x) => x as f64,
                            _ => 0.0,
                        },
                        CompactTag::Null | CompactTag::Uninitialized => 0.0,
                        other => {
                            return Err(VmError::Internal {
                                message: format!(
                                    "putfield: tag {other:?} incompatible with D-descriptor field",
                                ),
                            }
                            .into());
                        }
                    };
                    Value::Double(dv)
                }
                _ => {
                    let v = thread.frames[frame_idx].stack.pop()?;
                    // Same jobject-as-Long contract as `astore` / `coerce_value_for_return`
                    // in vm_exec: invoke returns can sit on the stack as compact long bits.
                    match desc_byte {
                        Some(d @ (b'L' | b'[')) => coerce_value_for_return_validated(shared, v, d),
                        // JVMS putfield: narrow the popped int to the field's
                        // declared sub-int width (byte/boolean/char/short) before
                        // storing, so a wide int producer can't leave out-of-range
                        // bits in a byte/short field. `I` and the rest pass through.
                        Some(d) => narrow_int_to_field_type(v, d),
                        None => v,
                    }
                }
            };
            // Perf: resolve the field name LAZILY (it locks + allocates) — see
            // the matching Getfield comment. Only the rare null-NPE message and
            // cached-off debug blocks need it; putfield is a hot opcode.
            // JEP 358 increment 2: `Cannot assign field "x" because "<expr>"
            // is null`. In the source bytecode the putfield stack is
            // `[..., objectref, value]`, so the receiver sits one slot below
            // the top (the stored value) — depth 1. Clone the frame parts
            // only when the opt-in flag is set (putfield is a hot opcode).
            let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
            let npe_parts = if jep358 {
                Some((
                    Arc::clone(&thread.frames[frame_idx].code),
                    thread.frames[frame_idx].method_name_arc(),
                    thread.frames[frame_idx].method_descriptor_arc(),
                    thread.frames[frame_idx].last_instr_pc,
                ))
            } else {
                None
            };
            let obj_ref = pop_object_ref_ctx_with(
                &mut thread.frames[frame_idx].stack,
                &shared.mem.heap,
                || {
                    let field_name = resolve_field_name(shared, current_class_id, *index);
                    if let Some((code, mname, mdesc, bci)) = &npe_parts {
                        let action = crate::runtime::exceptions::helpful_npe::action_assign_field(
                            field_name.as_deref().unwrap_or("?"),
                        );
                        helpful_npe_opcode_message_parts(
                            shared,
                            current_class_id,
                            code,
                            mname,
                            mdesc,
                            *bci,
                            &action,
                            1,
                        )
                    } else {
                        format!(
                            "Cannot write field '{}' because the object is null",
                            field_name.as_deref().unwrap_or("?")
                        )
                    }
                },
            );
            // CRATONVM_DBG_NULLTHIS — dump the Java frame stack + current-frame
            // locals when a putfield pops a null receiver. Diagnoses the
            // "Cannot write field X because the object is null" family (a JIT'd
            // or misdispatched caller losing the freshly allocated receiver,
            // cf. gap-jit-fastmath-transform-miscompile.md Bug 4).
            // Perf: short-circuit on the consolidated field-diagnostics gate
            // first (a single cached bool) so the common no-diagnostics path
            // never even tests `obj_ref.is_err()` for this block. `any_field_diag`
            // is `true` whenever `CRATONVM_DBG_NULLTHIS` is set, so the inner
            // var check still selects exactly this block — semantics unchanged.
            if crate::runtime::env_cache::any_field_diag()
                && obj_ref.is_err()
                && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NULLTHIS").is_some()
            {
                let field_name = resolve_field_name(shared, current_class_id, *index);
                let fr0 = &thread.frames[frame_idx];
                eprintln!(
                    "[nullthis] putfield '{}' on null receiver in {}.{}{} pc={}",
                    field_name.as_deref().unwrap_or("?"),
                    fr0.class_name(),
                    fr0.method_name(),
                    fr0.method_descriptor(),
                    fr0.pc
                );
                for (i, fr) in thread.frames.iter().enumerate().rev().take(30) {
                    eprintln!(
                        "  [{}] {}.{}{} pc={}",
                        i,
                        fr.class_name(),
                        fr.method_name(),
                        fr.method_descriptor(),
                        fr.pc
                    );
                }
                let fr0 = &thread.frames[frame_idx];
                for li in 0..fr0.locals_len().min(8) {
                    eprintln!("  local[{}] = 0x{:x}", li, fr0.get_local_raw(li));
                }
            }
            let obj_ref = obj_ref?;
            // GCBARRIER-CDLWAIT-FIX (2026-07-17): heal a receiver that went
            // stale while resolving the field above. resolve_field_ref at
            // the top of this opcode handler runs BEFORE this pop (the
            // receiver was still nominally live on the Java operand stack
            // during that call), but a cache-miss-cold resolution can load
            // and link the field's declaring class for the first time --
            // real allocating work -- and a moving collection triggered
            // during it (confirmed live: java.lang.Thread$FieldHolder's
            // very first "putfield task" under CRATONVM_DBG_GC_STRESS,
            // which silently dropped the write, leaving Thread.holder.task
            // permanently null and that worker's Runnable never invoked)
            // can leave this local pointing at a forwarded-but-structurally-
            // intact from-space copy that CRATONVM_DBG_STRAYSTACK's
            // num_slots/class_id sanity check does not catch. Heal it the
            // same way native-call arguments and the getfield RECEIVER
            // (see the identical fix just above) already are.
            let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
            if let Some(retargeted) = retarget_instance_field_to_receiver(
                shared,
                current_class_id,
                *index,
                shared.mem.heap.class_id_of(obj_ref),
                &field,
            ) {
                field = retargeted;
            }
            // Perf: ALL of the per-putfield diagnostic blocks below are gated
            // behind a SINGLE cached "any field diagnostic enabled" branch, so
            // the common no-diagnostics case (the overwhelmingly hot path) does
            // exactly one branch instead of stepping through each individual
            // gate (FIELDADDR, STRAYSTACK, HashtableOfInt, BAOS). Inside, every
            // block still re-checks its own cached gate, so behaviour is
            // byte-for-byte identical to the original sequence. See
            // `env_cache::any_field_diag`.
            // ES-FAIL-FAMILY-20260710 hunt: java-level-stack companion to
            // `cratonvm_gc::heap::dynamic_watch_addr()` (armed by
            // `NativeContext::dbg_set_watch_cell`, see
            // `native_builtins::lang_misc::write_throwable_cause`). The
            // watch's own `[CELLWATCH]` report captures a Rust backtrace,
            // which is unreliable here (JIT frames lack Windows unwind
            // info and the walk comes back garbled) — this prints the
            // actual JAVA call stack instead, which is always available.
            {
                let watch = cratonvm_gc::heap::dynamic_watch_addr();
                if watch != 0 {
                    let addr = obj_ref.as_ptr() as usize
                        + cratonvm_types::HEADER_SIZE
                        + field.field_index * cratonvm_types::SLOT_SIZE;
                    if addr == watch {
                        eprintln!(
                            "[WATCHFIELD] putfield HIT watch={watch:#x} obj=0x{:x} field_index={} value={:?} in {}.{}{} pc={}",
                            obj_ref.as_ptr() as usize,
                            field.field_index,
                            value,
                            thread.frames[frame_idx].class_name(),
                            thread.frames[frame_idx].method_name(),
                            thread.frames[frame_idx].method_descriptor(),
                            thread.frames[frame_idx].pc,
                        );
                        eprintln!("[WATCHFIELD] Java stack (top first):");
                        for f in thread.frames.iter().rev().take(30) {
                            eprintln!(
                                "[WATCHFIELD]   {}.{}{} pc={}",
                                f.class_name(),
                                f.method_name(),
                                f.method_descriptor(),
                                f.pc,
                            );
                        }
                    }
                }
            }
            if crate::runtime::env_cache::any_field_diag() {
                // Gated diagnostic (CRATONVM_DBG_FIELDADDR): trace put for specific
                // fields — object address + resolved slot — to localize a write
                // that doesn't reach the read site.
                if crate::runtime::env_cache::field_addr_dbg() {
                    let field_name = resolve_field_name(shared, current_class_id, *index);
                    if let Some(fname) = field_name.as_deref() {
                        if matches!(
                            fname,
                            "unsharedLongs"
                                | "threadFactory"
                                | "runningThreads"
                                | "submittedTaskCounter"
                        ) {
                            eprintln!(
                            "[FIELDADDR] PUT {} obj=0x{:x} slot={} num_slots={} valObj={} in {}",
                            fname,
                            // Cast: object/code pointer to integer address
                            obj_ref.as_ptr() as usize,
                            field.field_index,
                            shared.mem.heap.get_header(obj_ref).num_slots(),
                            matches!(value, Value::Object(Some(_))),
                            thread.frames[frame_idx].class_name(),
                        );
                        }
                    }
                }
                // bc math-ec 0x4 (CRATONVM_DBG_STRAYSTACK): catch a STRAY/STALE
                // receiver reaching putfield. The corruption's reliable face is the
                // `set_field out-of-bounds` flood: a relocated-but-unremapped (or
                // wild) `objectref` whose header reads `num_slots=0` (real obj NOT
                // forwarded; class_id reads a Value-disc 0/1/4) or `num_slots` huge
                // (real obj forwarded → forwarding_ptr low bits). Dump the Java
                // stack + receiver so we can trace where the stale ref originates
                // (operand-stack slot not remapped after a young GC). Rate-limited.
                if straystack_enabled() {
                    let h = shared.mem.heap.get_header(obj_ref);
                    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                    let ns = h.num_slots() as usize;
                    if field.field_index >= ns || h.num_slots() > (1 << 24) {
                        use std::sync::atomic::{AtomicUsize, Ordering};
                        static N: AtomicUsize = AtomicUsize::new(0);
                        let k = N.fetch_add(1, Ordering::Relaxed);
                        if k < 12 {
                            let field_name = resolve_field_name(shared, current_class_id, *index);
                            eprintln!(
                            "[straystack] #{k} STRAY putfield recv@0x{:x} cid={} num_slots={} array_len={} kind={} -> field '{}' idx={} is_ref={} value={:?}",
                            // Cast: object/code pointer to integer address
                            obj_ref.as_ptr() as usize,
                            // Truncation: integer -> u8 (intentional low 8 bits)
                            h.class_id.as_u32(), h.num_slots(), h.array_length(), cratonvm_types::ObjectHeader::kind_tag(h.mark_word.load(std::sync::atomic::Ordering::Relaxed)),
                            field_name.as_deref().unwrap_or("?"),
                            field.field_index, field.is_reference, value,
                        );
                            eprintln!("[straystack] Java stack (top first):");
                            for f in thread.frames.iter().rev().take(28) {
                                eprintln!(
                                    "[straystack]   {}.{}{} pc={}",
                                    f.class_name(),
                                    f.method_name(),
                                    f.method_descriptor(),
                                    f.pc,
                                );
                            }
                        }
                    }
                }
                if crate::runtime::env_cache::hashtableofint_trace() {
                    let cname = thread.frames[frame_idx].class_name();
                    let mname = thread.frames[frame_idx].method_name();
                    if cname.contains("HashtableOfInt") {
                        let field_name = resolve_field_name(shared, current_class_id, *index);
                        let nf = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(field.declaring_class_id)
                            .map(|c| c.num_total_fields)
                            .unwrap_or(0);
                        eprintln!(
                        "[HTI-PUT] {cname}.{mname} cp#{idx} fld={fn2:?} declaring={decl:?} field_index={fi} is_ref={ir} num_total_fields={nf} obj_ptr={op:p} value={v:?}",
                        idx = *index,
                        fn2 = field_name,
                        decl = field.declaring_class_id,
                        fi = field.field_index,
                        ir = field.is_reference,
                        op = obj_ref.as_ptr(),
                        v = value,
                    );
                    }
                }
                if crate::runtime::env_cache::baos_dbg() {
                    let field_name = resolve_field_name(shared, current_class_id, *index);
                    if matches!(field_name.as_deref(), Some("buf") | Some("count")) {
                        let cm = shared.classes.class_manager.read();
                        let recv_cid = shared.mem.heap.class_id_of(obj_ref);
                        let rn = cm
                            .get_class(recv_cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        let rnf = cm
                            .get_class(recv_cid)
                            .map(|c| c.num_total_fields)
                            .unwrap_or(0);
                        let rffi = cm
                            .get_class(recv_cid)
                            .map(|c| c.first_field_index)
                            .unwrap_or(0);
                        let dn = cm
                            .get_class(field.declaring_class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        let dnf = cm
                            .get_class(field.declaring_class_id)
                            .map(|c| c.num_total_fields)
                            .unwrap_or(0);
                        let dffi = cm
                            .get_class(field.declaring_class_id)
                            .map(|c| c.first_field_index)
                            .unwrap_or(0);
                        eprintln!(
                        "[BAOS-DBG] putfield {fld:?} recv={rn}(nf={rnf},ffi={rffi}) decl={dn}(nf={dnf},ffi={dffi}) field_index={fi} value={v:?}",
                        fld = field_name, fi = field.field_index, v = value,
                    );
                    }
                }
            } // end `if any_field_diag()` — consolidated putfield diagnostics
              // T17.Δ.4 — JVMTI FieldModification watchpoint, scoped to this VM.
            {
                let method_id = synth_method_id(&thread.frames[frame_idx]);
                crate::runtime::jvmti::fire_field_modification_if_watched_for_vm(
                    shared.vm_identity,
                    thread.thread_id.0,
                    method_id,
                    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                    field.declaring_class_id.as_u32() as u64,
                    field.field_index,
                );
            }

            // Task #25 SATB triad: route the pre-barrier through the
            // new `GarbageCollector::write_barrier_pre` trait method via
            // `VmHeap::write_barrier_pre` so the debug-build triad
            // ordering assertion fires (and so non-SATB collectors pay
            // genuinely zero cost, not even a `Value` match). We still
            // need the old `Value` for the pre-store check, but only
            // call the trait method on the ref-typed case — primitives
            // and nulls don't enter SATB.
            let old_value = shared.mem.heap.get_field(obj_ref, field.field_index);
            if let Value::Object(Some(old_ref)) = old_value {
                shared
                    .mem
                    .heap
                    .write_barrier_pre(std::ptr::null_mut(), old_ref);
            }
            // FIELD-WATCH (TestUpgrade RootReference/MVMap residual) — every
            // real putfield to any Page/RootReference field, unconditional
            // (not tied to a construction-site guess or a GC-move-fragile
            // address watch list). See fixed-suite-bugs/h2-suite-bugs/
            // bug-h2-suite-residual-fail-triage-FIXED.md.
            if crate::runtime::env_cache::dbg_field_watch() {
                let decl_name = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(field.declaring_class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                let field_name = resolve_field_name(shared, current_class_id, *index);
                if crate::runtime::env_cache::field_watch_class_matches(&format!(
                    "{}.{}",
                    decl_name,
                    field_name.as_deref().unwrap_or("?")
                )) {
                    let fr = &thread.frames[frame_idx];
                    eprintln!(
                        "[PUTFIELD-WATCH] obj={:p} decl_class={} field={:?} field_index={} old={:?} new={:?} in {}.{} pc={} thread={}",
                        obj_ref.as_ptr(),
                        decl_name,
                        field_name,
                        field.field_index,
                        old_value,
                        value,
                        fr.class_name(),
                        fr.method_name(),
                        fr.pc,
                        thread.thread_id.0,
                    );
                    // A watched field almost always raises a "who did this?"
                    // question next, and the answer is the Java call chain.
                    for f in thread.frames.iter().rev().take(24) {
                        eprintln!("    at {}.{} pc={}", f.class_name(), f.method_name(), f.pc);
                    }
                }
            }
            // CRATONVM_DBG_CORRUPT_CELL, the interpreter's own WRITE door.
            // The read doors were instrumented first, and a producer that only
            // ever WRITES was invisible to every one of them: the first
            // array-receiver refusal this instrument caught on a real workload
            // (`CacheAutoConfigurationTests`, 2026-08-23) was a `set_field`,
            // and only the safepoint BACKSTOP saw it -- two minutes later, with
            // the frames of whatever that thread was doing by then, which is
            // exactly the caveat the backstop prints about itself. A write door
            // names it at the store. Off: one cached bool.
            let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
            if field.is_volatile {
                // CRATONVM_DBG_AQS_TRACE (2026-07-21): ledger entry for the
                // synchronizer family's plain volatile state writes
                // (`AQLS.setState` — the writer-exclusive release path),
                // matching the CAS-side ledger in native_unsafe_cas_long.
                if cratonvm_native_builtins::aqs_trace_enabled() {
                    let cid = shared.mem.heap.class_id_of(obj_ref);
                    let cls = {
                        let cm = shared.classes.class_manager.read();
                        cm.get_class(cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default()
                    };
                    if cls.starts_with("java/util/concurrent/locks/") {
                        cratonvm_native_builtins::aqs_trace_line(&format!(
                            "[AQS] tid={} putvol obj={:p} cls={} slot={} old={:?} new={:?}",
                            thread.thread_id.0,
                            obj_ref.as_ptr(),
                            cls.rsplit('/').next().unwrap_or(cls.as_str()),
                            field.field_index,
                            old_value,
                            value
                        ));
                    }
                }
                shared
                    .mem
                    .heap
                    .set_field_volatile(obj_ref, field.field_index, value);
            } else {
                shared.mem.heap.set_field(obj_ref, field.field_index, value);
            }
            crate::memory::reclaim_guard::corrupt_cell_watch_close(
                shared,
                thread,
                corrupt_before,
                "interpreter putfield",
                Some(obj_ref),
                Some(field.field_index),
            );
            // DBG (bc math-ec, CRATONVM_DBG_ECWATCH): when a *valid* reference is
            // stored into an EC object's reference field, arm a software
            // watchpoint on the field's payload so the later raw `0x4` overwrite
            // (which bypasses this very set_field) is attributed to the native
            // that does it (checked in `safe_native_call`). See runtime::ec_watch.
            if crate::runtime::ec_watch::enabled() {
                if let (true, Value::Object(Some(p))) = (field.is_reference, value) {
                    let recv_cid = shared.mem.heap.class_id_of(obj_ref);
                    if ec_is_watched_class(shared, recv_cid) {
                        crate::runtime::ec_watch::record(
                            shared.vm_identity,
                            obj_ref,
                            field.field_index,
                            // Cast: object/code pointer to integer address
                            p.as_ptr() as usize,
                            recv_cid.as_u32(),
                        );
                    }
                }
            }
            // write_barrier fires automatically inside set_field / set_field_volatile.
            // TODO(orchestrator): migrate the remaining `heap.satb_barrier(...)`
            // call sites (interpreter aastore lines ~4093/5164/5961, JIT
            // putfield_object / aastore_object in jit/helpers.rs, the
            // `vm_exec` putfield path) to use `write_barrier_pre` so the
            // debug-build triad assertion covers every reference store.
    Ok(())
}

/// `getfield` — the single most common opcode in OO bytecode.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
#[inline]
pub(super) fn op_getfield(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    // `index` is rebound as a reference so the moved body's `*index`
    // reads unchanged — this is a pure move, not a rewrite.
    let index = &index;
            let current_class_id = thread.frames[frame_idx].class_id;
            // Perf: `resolve_field_name` takes a class_manager RwLock and
            // allocates a `String` — but the name is only needed for the
            // (rare) null-receiver NPE message and the (cached-off) debug
            // blocks below. Resolve it LAZILY in those paths instead of on
            // every getfield (the single most common opcode in OO bytecode).
            // JEP 358 increment 2 (opt-in via CRATONVM_HELPFUL_NPE_OPCODES):
            // emit the HotSpot shape `Cannot read field "x" because "<expr>"
            // is null`. The getfield receiver is at the top of the operand
            // stack (depth 0). Pre-extract the `thread`-independent frame
            // parts so the closure (which holds a &mut borrow of the operand
            // stack) can build the message without re-borrowing `thread`.
            // Only clone the frame parts (Arc bumps) when the opt-in flag is
            // set — keep the default getfield path (the hottest OO opcode)
            // allocation-free.
            let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
            let npe_parts = if jep358 {
                Some((
                    Arc::clone(&thread.frames[frame_idx].code),
                    thread.frames[frame_idx].method_name_arc(),
                    thread.frames[frame_idx].method_descriptor_arc(),
                    thread.frames[frame_idx].last_instr_pc,
                ))
            } else {
                None
            };
            let obj_ref = pop_object_ref_ctx_with(
                &mut thread.frames[frame_idx].stack,
                &shared.mem.heap,
                || {
                    let field_name = resolve_field_name(shared, current_class_id, *index);
                    if let Some((code, mname, mdesc, bci)) = &npe_parts {
                        let action = crate::runtime::exceptions::helpful_npe::action_read_field(
                            field_name.as_deref().unwrap_or("?"),
                        );
                        helpful_npe_opcode_message_parts(
                            shared,
                            current_class_id,
                            code,
                            mname,
                            mdesc,
                            *bci,
                            &action,
                            0,
                        )
                    } else {
                        format!(
                            "Cannot read field '{}' because the object is null",
                            field_name.as_deref().unwrap_or("?")
                        )
                    }
                },
            );
            if crate::runtime::env_cache::any_field_diag()
                && obj_ref.is_err()
                && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NULLTHIS").is_some()
            {
                let field_name = resolve_field_name(shared, current_class_id, *index);
                let fr0 = &thread.frames[frame_idx];
                eprintln!(
                    "[nullthis] getfield '{}' on null receiver in {}.{}{} pc={}",
                    field_name.as_deref().unwrap_or("?"),
                    fr0.class_name(),
                    fr0.method_name(),
                    fr0.method_descriptor(),
                    fr0.pc
                );
                for (i, fr) in thread.frames.iter().enumerate().rev().take(30) {
                    eprintln!(
                        "  [{}] {}.{}{} pc={}",
                        i,
                        fr.class_name(),
                        fr.method_name(),
                        fr.method_descriptor(),
                        fr.pc
                    );
                }
                let fr0 = &thread.frames[frame_idx];
                for li in 0..fr0.locals_len().min(8) {
                    eprintln!("  local[{}] = 0x{:x}", li, fr0.get_local_raw(li));
                }
            }
            let obj_ref = obj_ref?;
            // GCBARRIER-CDLWAIT-FIX (2026-07-17): heal a receiver that went
            // stale (relocated by a moving GC) while it sat mid-flight
            // between the operand-stack pop above and here. resolve_field_ref
            // below is a cache-miss-cold path on a field's FIRST-ever
            // resolution: it can call load_class_concurrent, which loads
            // and links (and may run clinit for) the field's declaring
            // class -- real work that allocates, and under GC-stress
            // (or ordinary allocation pressure) can trigger a moving
            // collection. obj_ref was popped into this bare Rust local
            // BEFORE that call and is therefore invisible to the collector's
            // root scan for its duration; a relocated receiver leaves this
            // local pointing at an intact (structurally valid, so it evades
            // the CRATONVM_DBG_STRAYSTACK num_slots/class_id sanity check)
            // but dead from-space copy -- same shape as the native-call-arg
            // and getfield-loaded-value barriers elsewhere in this file, just
            // never applied to the getfield/putfield RECEIVER itself.
            let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
            let mut field =
                resolve_field_ref_loader_aware(shared, thread, current_class_id, *index)?;
            if let Some(retargeted) = retarget_instance_field_to_receiver(
                shared,
                current_class_id,
                *index,
                shared.mem.heap.class_id_of(obj_ref),
                &field,
            ) {
                field = retargeted;
            }
            // CRATONVM_DBG_STRAYSTACK, the getfield door -- the READ twin of
            // the putfield dump above. Deliberately outside `any_field_diag()`,
            // exactly like its putfield sibling, so the two doors are armed by
            // one variable and cannot drift.
            if straystack_enabled() {
                let h = shared.mem.heap.get_header(obj_ref);
                let ns = h.num_slots() as usize;
                if field.field_index >= ns || h.num_slots() > (1 << 24) {
                    use std::sync::atomic::{AtomicUsize, Ordering};
                    static N: AtomicUsize = AtomicUsize::new(0);
                    let k = N.fetch_add(1, Ordering::Relaxed);
                    if k < 12 {
                        let field_name = resolve_field_name(shared, current_class_id, *index);
                        eprintln!(
                            "[straystack] #{k} OOB getfield recv@0x{:x} cid={} num_slots={} kind={} -> field '{}' idx={} declaring={:?}",
                            obj_ref.as_ptr() as usize,
                            h.class_id.as_u32(),
                            h.num_slots(),
                            cratonvm_types::ObjectHeader::kind_tag(
                                h.mark_word.load(std::sync::atomic::Ordering::Relaxed)
                            ),
                            field_name.as_deref().unwrap_or("?"),
                            field.field_index,
                            field.declaring_class_id,
                        );
                        eprintln!("[straystack] Java stack (top first):");
                        for f in thread.frames.iter().rev().take(28) {
                            eprintln!(
                                "[straystack]   {}.{}{} pc={}",
                                f.class_name(),
                                f.method_name(),
                                f.method_descriptor(),
                                f.pc,
                            );
                        }
                    }
                }
            }
            // Perf: ALL of the per-getfield diagnostic blocks below are gated
            // behind a SINGLE cached "any field diagnostic enabled" branch, so
            // the common no-diagnostics case (the overwhelmingly hot path) does
            // exactly one branch instead of stepping through each individual
            // gate. Inside, every block still re-checks its own cached gate, so
            // behaviour is byte-for-byte identical to the original sequence —
            // including the `CRATONVM_DBG_BADRECV` wild-pointer guard, which is
            // only ever reachable when its var is set (and `any_field_diag()` is
            // then `true`). See `env_cache::any_field_diag`.
            if crate::runtime::env_cache::any_field_diag() {
                if crate::runtime::env_cache::field_addr_dbg() {
                    let field_name = resolve_field_name(shared, current_class_id, *index);
                    if let Some(fname) = field_name.as_deref() {
                        if matches!(
                            fname,
                            "unsharedLongs"
                                | "threadFactory"
                                | "runningThreads"
                                | "submittedTaskCounter"
                        ) {
                            let raw = shared.mem.heap.get_field(obj_ref, field.field_index);
                            eprintln!(
                            "[FIELDADDR] GET {} obj=0x{:x} slot={} num_slots={} readObj={} in {}",
                            fname,
                            // Cast: object/code pointer to integer address
                            obj_ref.as_ptr() as usize,
                            field.field_index,
                            shared.mem.heap.get_header(obj_ref).num_slots(),
                            matches!(raw, Value::Object(Some(_))),
                            thread.frames[frame_idx].class_name(),
                        );
                        }
                    }
                }
                // CRATONVM_DBG_BADRECV — localize the H2 TestScript SEGV: a getfield
                // whose receiver is a corrupted `Object(Some(ptr))` not pointing into
                // any managed arena (e.g. ptr=6) faults in `get_field`'s header read.
                // Log the Java frame stack + field + a Rust backtrace (to name the
                // native that drove this method via invoke_virtual), then raise NPE
                // instead of dereferencing the wild pointer.
                if crate::runtime::env_cache::badrecv_dbg() {
                    // Cast: object/code pointer to integer address
                    let p = obj_ref.as_ptr() as usize;
                    if p != 0 && shared.mem.heap.is_heap_addr(p).is_none() {
                        use std::sync::atomic::{AtomicUsize, Ordering};
                        static N: AtomicUsize = AtomicUsize::new(0);
                        let n = N.fetch_add(1, Ordering::Relaxed);
                        if n < 8 {
                            let field_name = resolve_field_name(shared, current_class_id, *index);
                            let cn = thread.frames[frame_idx].class_name().to_string();
                            let mn = thread.frames[frame_idx].method_name().to_string();
                            let pc = thread.frames[frame_idx].pc;
                            eprintln!(
                                "[BADRECV #{n}] getfield receiver=0x{p:x} field={field_name:?} \
                             field_index={} is_ref={} in {cn}.{mn} pc={pc}",
                                field.field_index, field.is_reference,
                            );
                            eprintln!("[BADRECV #{n}] Java frames (innermost first):");
                            for f in thread.frames.iter().rev().take(24) {
                                eprintln!("    {}.{}", f.class_name(), f.method_name());
                            }
                            eprintln!(
                                "[BADRECV #{n}] Rust backtrace:\n{}",
                                std::backtrace::Backtrace::force_capture()
                            );
                            use std::io::Write;
                            let _ = std::io::stderr().flush();
                        }
                        return Err(RuntimeError::NullPointerException {
                            message: Some(format!("[BADRECV] non-heap getfield receiver 0x{p:x}")),
                        }
                        .into());
                    }
                    // DoHead freed-while-live forensics (2026-07-15): the other
                    // stale-receiver face — a VALID heap address whose object
                    // was zeroed (all-zero header: ClassId(0), num_slots=0)
                    // while a long-lived holder kept serving it. The gc guard
                    // contains each read but names no Java context; print it
                    // here (capped) so the holder structure is identifiable.
                    // A getfield on a 0-slot object is always OOB, so this
                    // never fires for a legitimate zero-hash ClassId(0)
                    // container with fields.
                    if p != 0 && shared.mem.heap.is_heap_addr(p).is_some() {
                        let h = shared.mem.heap.get_header(obj_ref);
                        if h.class_id.as_u32() == 0 && h.num_slots() == 0 {
                            use std::sync::atomic::{AtomicUsize, Ordering};
                            static NZ: AtomicUsize = AtomicUsize::new(0);
                            let n = NZ.fetch_add(1, Ordering::Relaxed);
                            if n < 12 {
                                let field_name =
                                    resolve_field_name(shared, current_class_id, *index);
                                let cn = thread.frames[frame_idx].class_name().to_string();
                                let mn = thread.frames[frame_idx].method_name().to_string();
                                let pc = thread.frames[frame_idx].pc;
                                eprintln!(
                                    "[BADRECV-Z #{n}] getfield ZEROED receiver=0x{p:x} \
                                 field={field_name:?} field_index={} in {cn}.{mn} pc={pc}",
                                    field.field_index,
                                );
                                eprintln!("[BADRECV-Z #{n}] Java frames (innermost first):");
                                for f in thread.frames.iter().rev().take(24) {
                                    eprintln!("    {}.{}", f.class_name(), f.method_name());
                                }
                                use std::io::Write;
                                let _ = std::io::stderr().flush();
                            }
                        }
                    }
                }
                if crate::runtime::env_cache::hashtableofint_trace() {
                    let cname = thread.frames[frame_idx].class_name();
                    let mname = thread.frames[frame_idx].method_name();
                    if cname.contains("HashtableOfInt") {
                        let field_name = resolve_field_name(shared, current_class_id, *index);
                        let v = shared.mem.heap.get_field(obj_ref, field.field_index);
                        let nf = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(field.declaring_class_id)
                            .map(|c| c.num_total_fields)
                            .unwrap_or(0);
                        eprintln!(
                        "[HTI-GET] {cname}.{mname} cp#{idx} fld={field_name:?} declaring={decl:?} field_index={fi} is_ref={ir} num_total_fields={nf} obj_ptr={op:p} value={v:?}",
                        idx = *index,
                        decl = field.declaring_class_id,
                        fi = field.field_index,
                        ir = field.is_reference,
                        op = obj_ref.as_ptr(),
                    );
                    }
                }
                if crate::runtime::env_cache::bd_debug() {
                    let mname = thread.frames[frame_idx].method_name().to_string();
                    let cname = thread.frames[frame_idx].class_name().to_string();
                    if cname.contains("BigDecimal") && mname == "intValue" {
                        let field_name = resolve_field_name(shared, current_class_id, *index);
                        let v = if field.is_volatile {
                            shared
                                .mem
                                .heap
                                .get_field_volatile(obj_ref, field.field_index)
                        } else {
                            shared.mem.heap.get_field(obj_ref, field.field_index)
                        };
                        eprintln!("[Getfield in BigDecimal.intValue] cp_index={} field_name={:?} field_index={} is_ref={} obj={:p} value={:?}",
                              *index, field_name, field.field_index, field.is_reference, obj_ref.as_ptr(), v);
                    }
                }
            } // end `if any_field_diag()` — consolidated getfield diagnostics
              // Read side of the [PUTFIELD-WATCH] ledger further down: with both
              // halves on one filter a "the constructor stored it but the reader
              // sees null" question is answerable from a single log, without
              // guessing which of the two sides is wrong. Same class filter
              // (`CRATONVM_DBG_FIELD_WATCH=<substr>[,<substr>…]`).
            if crate::runtime::env_cache::dbg_field_watch() {
                let decl_name = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(field.declaring_class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                let field_name = resolve_field_name(shared, current_class_id, *index);
                if crate::runtime::env_cache::field_watch_class_matches(&format!(
                    "{}.{}",
                    decl_name,
                    field_name.as_deref().unwrap_or("?")
                )) {
                    let watched = if field.is_volatile {
                        shared
                            .mem
                            .heap
                            .get_field_volatile(obj_ref, field.field_index)
                    } else {
                        shared.mem.heap.get_field(obj_ref, field.field_index)
                    };
                    let fr = &thread.frames[frame_idx];
                    eprintln!(
                        "[GETFIELD-WATCH] obj={:p} decl_class={} field={:?} field_index={} value={:?} in {}.{} pc={} thread={}",
                        obj_ref.as_ptr(),
                        decl_name,
                        field_name,
                        field.field_index,
                        watched,
                        fr.class_name(),
                        fr.method_name(),
                        fr.pc,
                        thread.thread_id.0,
                    );
                    // A watched field almost always raises a "who did this?"
                    // question next, and the answer is the Java call chain.
                    for f in thread.frames.iter().rev().take(24) {
                        eprintln!("    at {}.{} pc={}", f.class_name(), f.method_name(), f.pc);
                    }
                }
            }
            // K2 (T10.9.E) — category-2 primitive tag hint.  `ResolvedField`
            // records only is_reference/is_volatile, so we re-read the first
            // byte of the descriptor from the constant pool to choose the
            // direct CompactValue push path for J/D.  Two field loads — no
            // hashmap work on the fast path.
            let desc_byte = Some(field.desc_byte);
            // T17.Δ.4 — JVMTI FieldAccess watchpoint, scoped to this VM.
            // Fast path: no watchpoint registered ⇒ one atomic load; when the
            // process-wide union says some VM is watching, one HashMap read
            // against *this* VM's row returning None.
            {
                let method_id = synth_method_id(&thread.frames[frame_idx]);
                crate::runtime::jvmti::fire_field_access_if_watched_for_vm(
                    shared.vm_identity,
                    thread.thread_id.0,
                    method_id,
                    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                    field.declaring_class_id.as_u32() as u64,
                    field.field_index,
                );
            }
            // T1.7.1 — Brooks-pointer read barrier on the receiver.
            // If the object was evacuated by a concurrent compaction
            // cycle, `load_and_forward` returns the forwarded address
            // so subsequent field access hits the live copy. Under
            // stop-the-world GC this is always a no-op fast path.
            let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
            // CRATONVM_DBG_CORRUPT_CELL, the interpreter's own door. The
            // native funnel was instrumented first and is NOT where most
            // field reads happen -- this is, and a producer reached only from
            // bytecode was invisible until now. Off: one cached bool.
            let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
            crate::memory::reclaim_guard::corrupt_cell_selftest_inject("getfield");
            let mut value = if field.is_volatile {
                shared
                    .mem
                    .heap
                    .get_field_volatile(obj_ref, field.field_index)
            } else {
                shared.mem.heap.get_field(obj_ref, field.field_index)
            };
            crate::memory::reclaim_guard::corrupt_cell_watch_close(
                shared,
                thread,
                corrupt_before,
                "interpreter getfield",
                Some(obj_ref),
                Some(field.field_index),
            );
            // K2 (T10.9.E) — J/D direct-CompactValue fast path.
            //
            // For long/double fields, build the CompactValue with the exact
            // tag (`CompactValue::long` / `::double`) and push via
            // `push_compact`.  This is the KC26 fix: the legacy non-
            // reference branch below coerces any zero-initialized heap slot
            // (`Value::Object(None)`) to `Value::Int(0)`, so a long field
            // that was never explicitly written would leave the stack with
            // an Int slot — which a following long-arithmetic consumer
            // would reject with "expected long on stack, got
            // <uninitialized>" (the KC26 boot-path fingerprint).
            //
            // Covers both the freshly-allocated case (`Value::Object(None)`
            // ⇒ 0-valued long/double) and the written-back case
            // (`Value::Long(x)` / `Value::Double(x)`).  Non-matching input
            // tags (e.g. an Int stored into a J slot by buggy upstream
            // code) widen the bits rather than panic, matching the
            // defensive posture of the pre-existing coercions.
            if matches!(desc_byte, Some(b'J')) {
                let bits: i64 = match value {
                    Value::Long(x) => x,
                    // Cast: float/double raw bit pattern stored in integer word (no value conversion)
                    Value::Double(x) => x.to_bits() as i64,
                    // Widening: i32 -> i64 (sign-extended, JVM i2l)
                    Value::Int(x) => x as i64,
                    Value::Object(None) | Value::Uninitialized => 0,
                    // Cast: object/code pointer to integer address
                    Value::Object(Some(raw)) => raw.as_ptr() as usize as i64,
                    // Cast: float/double raw bit pattern stored in integer word (no value conversion)
                    Value::Float(x) => x.to_bits() as i64,
                    // Widening: i32 -> i64 (sign-extended, JVM i2l)
                    Value::ReturnAddress(pc) => pc as i64,
                };
                thread.frames[frame_idx]
                    .stack
                    .push_compact_long_checked(CompactValue::long(bits))?;
            } else if matches!(desc_byte, Some(b'D')) {
                let d: f64 = match value {
                    Value::Double(x) => x,
                    // Cast: integer word reinterpreted as float/double bit pattern
                    Value::Long(x) => f64::from_bits(x as u64),
                    // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                    Value::Int(x) => x as f64,
                    Value::Object(None) | Value::Uninitialized => 0.0,
                    // Cast: object/code pointer to integer address
                    Value::Object(Some(raw)) => f64::from_bits(raw.as_ptr() as usize as u64),
                    // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                    Value::Float(x) => x as f64,
                    // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                    Value::ReturnAddress(pc) => pc as f64,
                };
                thread.frames[frame_idx]
                    .stack
                    .push_compact_double_checked(CompactValue::double_raw(d))?;
            } else {
                // T12/T14: Coerce zero-initialized heap slots for reference fields.
                // The GC heap zeroes memory on allocation; for reference-typed
                // fields the JVM spec mandates a default of null.  Our tagged
                // representation decodes raw zeros as Int(0), so we fix up here.
                if field.is_reference {
                    match value {
                        Value::Int(0) | Value::Long(0) => value = Value::Object(None),
                        _ => {}
                    }
                } else {
                    // Primitive field read back as a tagged-object slot — see
                    // comment in Getstatic for rationale.  Reinterpret as i32.
                    match value {
                        Value::Object(None) => value = Value::Int(0),
                        Value::Object(Some(raw)) => {
                            // Cast: object/code pointer to integer address
                            let bits = raw.as_ptr() as usize as u64;
                            // Cast: operand reinterpreted as i32 (JVM 32-bit stack word)
                            value = Value::Int(bits as i32);
                        }
                        _ => {}
                    }
                    // JVMS getfield: a sub-int field (byte/boolean/char/short)
                    // loads sign/zero-extended to its declared width. Re-narrow
                    // the read int so a slot that was widened by some other write
                    // path still yields the spec-mandated value (B/S sign-extend,
                    // C zero-extends, Z masks to bit 0; I is unchanged).
                    if let Some(d) = desc_byte {
                        value = narrow_int_to_field_type(value, d);
                    }
                }
                // T1.7.1 — apply the barrier to the LOADED reference too.
                // Loading a forwarded reference into the operand stack
                // would otherwise leak a stale pointer into the next
                // safepoint's root set.
                if let Value::Object(Some(inner)) = value {
                    value = Value::Object(Some(shared.mem.heap.load_and_forward(inner)));
                }
                if remap_trace_on() {
                    if let Value::Object(Some(inner)) = value {
                        getfield_ring_record(
                            obj_ref.as_ptr() as usize,
                            field.field_index,
                            inner.as_ptr() as usize,
                        );
                    }
                }

                thread.frames[frame_idx].stack.push(value)?;
            }
    Ok(())
}

/// `putstatic` — resolve, initialise the holder if needed, store the value.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
#[inline]
pub(super) fn op_putstatic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    // `index` is rebound as a reference so the moved body's `*index`
    // reads unchanged — this is a pure move, not a rewrite.
    let index = &index;
            let current_class_id = thread.frames[frame_idx].class_id;
            let field = {
                let res = resolve_field_ref_loader_aware(shared, thread, current_class_id, *index);
                match res {
                    Ok(f) => f,
                    Err(e) => {
                        let name = field_ref_class_name(shared, current_class_id, *index)
                            .unwrap_or_default();
                        return Err(convert_class_not_found(shared, thread, &name, e));
                    }
                }
            };
            // T17.Δ.4 — JVMTI FieldModification watchpoint, scoped to this VM.
            {
                let method_id = synth_method_id(&thread.frames[frame_idx]);
                crate::runtime::jvmti::fire_field_modification_if_watched_for_vm(
                    shared.vm_identity,
                    thread.thread_id.0,
                    method_id,
                    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                    field.declaring_class_id.as_u32() as u64,
                    field.field_index,
                );
            }
            ensure_class_initialized_shared(shared, thread, field.declaring_class_id)?;
            // T10.9.D K3 — Pop via the descriptor-aware path so category-2
            // primitives (J/D) keep their exact 64-bit payload.  The naïve
            // `pop()?` decodes untagged long bits as Value::Double, which
            // then re-encodes as a double on the next push — silently
            // corrupting every J/D static.
            let desc_byte = Some(field.desc_byte);
            let value = pop_static_field_value(&mut thread.frames[frame_idx].stack, desc_byte)?;
            // SATB barrier: log old static field value before overwriting
            let old_static = get_static_shared(shared, field.declaring_class_id, field.field_index);
            shared.mem.heap.satb_barrier(old_static);
            // Volatile static fields: emit memory fence before write
            if field.is_volatile {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            }
            set_static_shared(shared, field.declaring_class_id, field.field_index, value);
            if field.is_volatile {
                std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
            }
    Ok(())
}

/// `getstatic` — resolve, initialise the holder if needed, push the value.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
#[inline]
pub(super) fn op_getstatic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    // `index` is rebound as a reference so the moved body's `*index`
    // reads unchanged — this is a pure move, not a rewrite.
    let index = &index;
            let current_class_id = thread.frames[frame_idx].class_id;
            let field = {
                let res = resolve_field_ref_loader_aware(shared, thread, current_class_id, *index);
                match res {
                    Ok(f) => f,
                    Err(e) => {
                        let name = field_ref_class_name(shared, current_class_id, *index)
                            .unwrap_or_default();
                        return Err(convert_class_not_found(shared, thread, &name, e));
                    }
                }
            };
            // T17.Δ.4 — JVMTI FieldAccess watchpoint.  Consults *this VM's*
            // watchpoint row; a single HashMap read + branch on the no-watch
            // path. The process-wide `any_field_watchpoint_active()` union
            // gate inside the callee stays as the cheap pre-filter — it may
            // say "yes" because another VM is watching, and the per-VM lookup
            // then answers exactly.
            {
                let method_id = synth_method_id(&thread.frames[frame_idx]);
                crate::runtime::jvmti::fire_field_access_if_watched_for_vm(
                    shared.vm_identity,
                    thread.thread_id.0,
                    method_id,
                    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                    field.declaring_class_id.as_u32() as u64,
                    field.field_index,
                );
            }

            // Bootstrap intercept: System.out / System.err / System.in
            //
            // The real JDK's `System.<clinit>` depends on a complex
            // initialization chain (SecurityManager, Charset, etc.)
            // that isn't fully bootable yet. To allow real JDK classes
            // to call `System.out.println`, we intercept the getstatic
            // on the three standard streams and return our pre-built
            // synthetic PrintStream objects. `System.in` uses the same
            // early pinning strategy via [`ensure_system_stdin_object`].
            let field_name_for_intercept = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(field.declaring_class_id)
                    .filter(|c| &*c.name == "java/lang/System")
                    .and_then(|c| c.fields.get(field.field_index).map(|f| f.name.to_string()))
            };
            // T10.9.D K3 — Resolve the field's declared descriptor byte so
            // category-2 primitives (J/D) get pushed with the correct
            // CompactValue tag instead of round-tripping through `Value`.
            // The Value boundary would encode `Value::Long(x)` as untagged
            // raw bits; a later `to_value()` decodes those bits as
            // `Value::Double`, silently corrupting the long on every read.
            let desc_byte = Some(field.desc_byte);
            if let Some(ref fname) = field_name_for_intercept {
                if fname == "out" || fname == "err" {
                    // Honor System.setOut/setErr: a user-installed stream wins
                    // over the canonical synthetic fd-backed stream. Read it
                    // from the *static field* (which setOut0/setErr0 populate
                    // via set_static_field) rather than the native-builtins
                    // override map: the static field is a GC root that the
                    // collector updates on object motion, whereas the map holds
                    // a raw ObjectRef that goes stale when GC moves/frees the
                    // user stream (DaCapo's TeePrintStream → "stale pointer …
                    // falling back to CP class PrintStream", empty stdout.log).
                    // A null/absent field means no override yet → canonical.
                    let overridden = match get_static_shared(
                        shared,
                        field.declaring_class_id,
                        field.field_index,
                    ) {
                        Value::Object(Some(s)) => Some(s),
                        _ => None,
                    };
                    let stream = match overridden {
                        Some(s) => s,
                        None => {
                            let (out, err) = shared.ensure_system_streams();
                            if fname == "out" {
                                out
                            } else {
                                err
                            }
                        }
                    };
                    if remap_trace_on() {
                        push_prov_record(stream.as_ptr() as usize, "getstatic-stream");
                    }
                    thread.frames[frame_idx]
                        .stack
                        .push(Value::Object(Some(stream)))?;
                    // Skip the normal getstatic path — we've already pushed.
                } else if fname == "in" {
                    let stdin = ensure_system_stdin_object(shared, thread)?;
                    if remap_trace_on() {
                        push_prov_record(stdin.as_ptr() as usize, "getstatic-stream");
                    }
                    thread.frames[frame_idx]
                        .stack
                        .push(Value::Object(Some(stdin)))?;
                } else {
                    // Normal getstatic for other System fields
                    ensure_class_initialized_shared(shared, thread, field.declaring_class_id)?;
                    let value =
                        get_static_shared(shared, field.declaring_class_id, field.field_index);
                    if field.is_volatile {
                        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                    }
                    push_static_field_value(
                        &mut thread.frames[frame_idx].stack,
                        value,
                        field.is_reference,
                        desc_byte,
                    )?;
                }
            } else {
                ensure_class_initialized_shared(shared, thread, field.declaring_class_id)?;
                let mut value =
                    get_static_shared(shared, field.declaring_class_id, field.field_index);
                if matches!(value, Value::Object(None)) {
                    let boolean_const = {
                        let cm = shared.classes.class_manager.read();
                        cm.get_class(field.declaring_class_id)
                            .filter(|c| &*c.name == "java/lang/Boolean")
                            .and_then(|c| {
                                let mut static_idx = 0usize;
                                for f in &c.fields {
                                    if f.is_static() {
                                        if static_idx == field.field_index {
                                            return match &*f.name {
                                                "TRUE" => Some(true),
                                                "FALSE" => Some(false),
                                                _ => None,
                                            };
                                        }
                                        static_idx += 1;
                                    }
                                }
                                None
                            })
                    };
                    if let Some(b) = boolean_const {
                        let obj = gc_alloc_object(shared, thread, field.declaring_class_id, 1)?;
                        shared.mem.heap.set_field(obj, 0, Value::Int(i32::from(b)));
                        value = Value::Object(Some(obj));
                        set_static_shared(
                            shared,
                            field.declaring_class_id,
                            field.field_index,
                            value,
                        );
                    }
                }
                if field.is_volatile {
                    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
                }
                push_static_field_value(
                    &mut thread.frames[frame_idx].stack,
                    value,
                    field.is_reference,
                    desc_byte,
                )?;
            }
    Ok(())
}
