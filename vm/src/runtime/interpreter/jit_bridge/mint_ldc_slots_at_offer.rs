#[allow(unused_imports)]
use super::*;


/// nio20 wave 6, lane L3 (`R17NioRows getclass`, `Nio20GetClass getclasseq`;
/// page `docs/feature-designs/real-jdk-mode.md`):
/// a tier-up offer (method entry, [`offer_invocation_to_tiered_manager`], or a
/// loop's OSR request in `interpreter::try_osr_offer`) is about to hand the
/// method to the GC-neutral background worker. Mint the GC-maintained `ldc`
/// slots of its `ldc <Class>` / `ldc <String>` sites now, on this MUTATOR,
/// from the resolutions the interpreter already recorded
/// (`helpers::compiled_ldc_slot_for`: a mutator may create a slot, the worker
/// may only find one).
///
/// Without it a body the worker compiles before any slot exists bakes slot `0`
/// for every such site and calls `jit_ldc_class_cp` / `jit_ldc_string_cp` on
/// EVERY execution for its life: the helper's first recorded hit mints the
/// slot, but only a recompile reads it, and an OSR body is compiled once. A
/// `getClass() == Foo.class` compare therefore cost a helper CALL per literal
/// per iteration (`getclasseq` 6 ns against `getclass` 3 ns on w5; R17's row
/// compares each getClass with up to four literals).
///
/// The rooting rule is unchanged in kind: a slot keeps its constant reachable
/// for the VM's life, which a compiled site's first recorded hit already did;
/// this moves it from "a compiled body executed the site" to "a compile of the
/// method was offered", and only for sites the interpreter has resolved (an
/// unresolved or failed entry has no record and gets no slot). Only `Class` and
/// `String` entries: those are the two kinds compiled code reads a slot for.
///
/// Call with NO class-manager lock held (`compiled_ldc_slot_for`'s contract);
/// the entry kinds are read under a short guard that is dropped first.
/// Returns the number of sites that have a slot afterwards.
pub(crate) fn mint_ldc_slots_at_offer(shared: &SharedVm, holder: ClassId, code: &[u8]) -> usize {
    if code.is_empty() {
        return 0;
    }
    // Pass 1, no lock: the `ldc` / `ldc_w` operands.
    let mut idxs: Vec<u16> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let cp_idx = match code[pc] {
            0x12 => code.get(pc + 1).map(|&b| u16::from(b)),
            0x13 => match (code.get(pc + 1), code.get(pc + 2)) {
                (Some(&hi), Some(&lo)) => Some(u16::from_be_bytes([hi, lo])),
                _ => None,
            },
            _ => None,
        };
        if let Some(i) = cp_idx {
            idxs.push(i);
        }
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    if idxs.is_empty() {
        return 0;
    }
    idxs.sort_unstable();
    idxs.dedup();
    // Pass 2, under a short class-manager guard: keep the `Class` / `String`
    // entries only.
    {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(holder) else {
            return 0;
        };
        // JIT r23 wave 4b, lane L2: and the reference-typed condy entries
        // `CRATONVM_JIT_LDC_CONDY_SLOT` compiles (`JitLdcConstant::DynamicSlot`);
        // an unresolved one has no record and mints nothing.
        let condy = ldc_condy_slot_enabled();
        idxs.retain(|&i| match class.constant_pool.get(i) {
            Some(ConstantPoolEntry::ClassReference { .. })
            | Some(ConstantPoolEntry::StringReference { .. }) => true,
            Some(ConstantPoolEntry::Dynamic {
                name_and_type_index,
                ..
            }) => condy && ldc_condy_is_reference(&class.constant_pool, *name_and_type_index),
            _ => false,
        });
    }
    idxs.truncate(LDC_MINT_AT_OFFER_MAX_SITES);
    // Pass 3, no class-manager lock: mint.
    let minted = idxs
        .iter()
        .filter(|&&i| crate::jit::helpers::compiled_ldc_slot_for(shared, holder, i) != 0)
        .count();
    if crate::runtime::env_cache::dbg_jitc() && !idxs.is_empty() {
        eprintln!(
            "[cratonvm-jitc] ldc slots at offer class_id={}: {minted}/{} Class/String sites have a slot",
            holder.as_u32(),
            idxs.len(),
        );
    }
    minted
}

pub(in crate::runtime::interpreter::jit_bridge) fn is_trusted_final_pkg(class_name: &str) -> bool {
    class_name.starts_with("java/lang/reflect/")
        || class_name.starts_with("jdk/internal/reflect/")
        || class_name.starts_with("java/lang/invoke/")
}

pub(in crate::runtime::interpreter::jit_bridge) fn count_desc_params_and_ret(desc: &str) -> Option<(usize, u8)> {
    let bytes = desc.as_bytes();
    if bytes.first().copied() != Some(b'(') {
        return None;
    }
    let mut i = 1usize;
    let mut count = 0usize;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => {
                count += 1;
                i += 1;
            }
            b'L' => {
                count += 1;
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                i += 1;
            }
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                if bytes[i] == b'L' {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return None;
                    }
                }
                count += 1;
                i += 1;
            }
            _ => return None,
        }
    }
    let ret = *bytes.get(i + 1)?;
    Some((count, ret))
}

pub(in crate::runtime::interpreter::jit_bridge) fn walk_const_obj_fields(
    shared: &SharedVm,
    mut thread: Option<&mut crate::threading::jvm_thread::JvmThread>,
    offerer_class_id: ClassId,
    obj: ObjectRef,
    slot_addr: usize,
    depth: u8,
    visited: &mut std::collections::HashSet<usize>,
) {
    if depth >= 4 || slot_addr == 0 || !visited.insert(slot_addr) {
        return;
    }
    let (obj, validated) = shared.mem.heap.load_and_forward_checked(obj);
    if !validated {
        return;
    }
    if shared.mem.heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array {
        return;
    }
    let exact_cid = shared.mem.heap.class_id_of_validated(obj);
    if exact_cid.as_u32() == 0 {
        return;
    }
    let (exact_name, is_class_mirror, is_mh, fields_to_scan): (
        String,
        bool,
        bool,
        Vec<(usize, u8, bool, bool)>,
    ) = {
        let cm = shared.classes.class_manager.read();
        let Some(cls) = cm.get_class(exact_cid) else {
            return;
        };
        let exact_name = cls.name.to_string();
        if exact_name.starts_with('[') {
            return;
        }
        let is_class_mirror = exact_name == "java/lang/Class";
        let is_mh = exact_name == "java/lang/invoke/MethodHandle"
            || cm.is_subclass_of_by_name(exact_cid, "java/lang/invoke/MethodHandle");
        let mut list = Vec::new();
        let mut cur = Some(exact_cid);
        while let Some(cid) = cur {
            if let Some(c) = cm.get_class(cid) {
                let trusted_pkg = is_trusted_final_pkg(&c.name);
                for (inst_idx, f) in c.fields.iter().filter(|f| !f.is_static()).enumerate() {
                    let is_stable = cratonvm_classloading::annotations::field_annotations(f)
                        .find_by_type_descriptor("Ljdk/internal/vm/annotation/Stable;", |i| {
                            c.constant_pool.get_utf8(i)
                        })
                        .is_some();
                    let trusted_final = f.is_final() && trusted_pkg;
                    if is_stable || trusted_final {
                        let tag = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                        list.push((c.first_field_index as usize + inst_idx, tag, is_stable, trusted_final));
                    }
                }
                cur = c.superclass;
            } else {
                break;
            }
        }
        (exact_name, is_class_mirror, is_mh, list)
    };

    if is_class_mirror {
        let mirrored = crate::vm::class_id_from_mirror(shared, obj).and_then(|target_cid| {
            let cm = shared.classes.class_manager.read();
            cm.get_class(target_cid)
                .map(|c| (target_cid.as_u32(), c.name.to_string(), c.access_flags.bits() as i32))
        });
        if let Some((cid, name, _)) = mirrored.as_ref().filter(|(cid, name, _)| {
            *cid != 0
                && !name.starts_with('[')
                && !matches!(
                    name.as_str(),
                    "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void"
                )
        }) {
            shared
                .jit
                .bridge
                .const_folds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .const_mirror_classes
                .insert(slot_addr, (*cid, name.clone()));
        }
        let flags_opt = mirrored.map(|(_, _, flags)| flags);
        if let Some(flags) = flags_opt {
            shared
                .jit
                .bridge
                .const_folds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .const_class_access_flags
                .insert(slot_addr, flags);
        }
    }

    if is_mh {
        let raw = obj.as_ptr() as u64;
        let epoch = shared.mem.address_epoch();
        if let Some(t) = thread.as_deref_mut() {
            if mh_constant_compile_enabled(shared) {
                let site_opt = {
                    let mut ctx = crate::vm::NativeContextImpl { shared, thread: t };
                    cratonvm_native_builtins::lang_invoke::mh_real_declared_descriptor(&mut ctx, obj)
                };
                if let Some(site_desc) = site_opt {
                    if mh_constant_target_of(shared, raw, epoch, &site_desc).is_none() {
                        let exact = {
                            let mut ctx = crate::vm::NativeContextImpl { shared, thread: t };
                            cratonvm_native_builtins::lang_invoke::mh_compiled_bind_target_unbound(
                                &mut ctx, obj, &site_desc,
                            )
                        };
                        if let Some(target) = exact.filter(|tgt| {
                            !tgt.bound && mh_constant_records_kind(tgt.invoke_kind)
                        }) {
                            if shared.mem.address_epoch() == epoch
                                && mh_constant_bind_loaders_agree(
                                    shared,
                                    offerer_class_id.as_u32(),
                                    target.class_id,
                                    &site_desc,
                                )
                            {
                                note_mh_constant_target_kind(
                                    shared,
                                    raw,
                                    epoch,
                                    &site_desc,
                                    (&target.class, &target.name, &target.desc),
                                    target.class_id,
                                    target.invoke_kind,
                                );
                            }
                        } else if mh_constant_compile_converting_enabled()
                            && cratonvm_native_builtins::lang_invoke::mh_constant_bind_real_converting_enabled()
                        {
                            let real_conv = {
                                let mut ctx = crate::vm::NativeContextImpl { shared, thread: t };
                                cratonvm_native_builtins::lang_invoke::mh_real_converting_compiled_bind(
                                    &mut ctx, obj, &site_desc,
                                )
                            };
                            if let Some((target, c)) = real_conv {
                                if !target.bound
                                    && matches!(target.invoke_kind, 0 | 2 | 3)
                                    && shared.mem.address_epoch() == epoch
                                    && mh_constant_bind_loaders_agree(
                                        shared,
                                        offerer_class_id.as_u32(),
                                        target.class_id,
                                        &site_desc,
                                    )
                                {
                                    if let Some(rc) = crate::jit::helpers::mh_real_conv_of(shared, &c) {
                                        if let Some(row) = crate::jit::helpers::mh_real_convert_compile_row(
                                            shared,
                                            target.invoke_kind,
                                            &site_desc,
                                            &rc,
                                        ) {
                                            note_mh_constant_target_converting(
                                                shared,
                                                raw,
                                                epoch,
                                                &site_desc,
                                                (&target.class, &target.name, &target.desc),
                                                target.class_id,
                                                row,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let matches: Vec<(String, MhConstantTarget)> = {
            let rows = shared
                .jit
                .bridge
                .mh_constant_targets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            rows.iter()
                .filter(|((h, _), t)| *h == raw && t.epoch == epoch)
                .map(|((_, site), t)| (site.to_string(), t.clone()))
                .collect()
        };
        for (site_desc, t) in matches {
            if let Some((t_pcount, t_ret_type)) = count_desc_params_and_ret(&t.desc) {
                let convert = if t.invoke_kind == 7 {
                    t.convert.as_deref().cloned()
                } else {
                    None
                };
                let main_kind = if let Some(ref c) = convert {
                    c.main_kind
                } else {
                    t.invoke_kind
                };
                if !matches!(main_kind, 0 | 2 | 3) {
                    continue;
                }
                let num_jit_args = if main_kind == 3 {
                    t_pcount
                } else {
                    1 + t_pcount
                };
                let info: &'static cratonvm_jit::JitInvokeInfo =
                    Box::leak(Box::new(cratonvm_jit::JitInvokeInfo {
                        class_name: Box::leak(t.class),
                        method_name: Box::leak(t.name),
                        descriptor: Box::leak(t.desc),
                        num_jit_args,
                        return_type: t_ret_type,
                        invoke_kind: main_kind,
                        declaring_class_id: t.class_id,
                        owner_class_id: 0,
                    }));
                let info_ptr = info as *const cratonvm_jit::JitInvokeInfo as usize;
                let box_info_ptr = if let Some(ref c) = convert {
                    if c.box_return != 0 && c.box_class_id != 0 {
                        let (cls, desc) = if c.box_return == 2 {
                            ("java/lang/Long", "(J)Ljava/lang/Long;")
                        } else {
                            ("java/lang/Integer", "(I)Ljava/lang/Integer;")
                        };
                        let box_info: &'static cratonvm_jit::JitInvokeInfo =
                            Box::leak(Box::new(cratonvm_jit::JitInvokeInfo {
                                class_name: cls,
                                method_name: "valueOf",
                                descriptor: desc,
                                num_jit_args: 1,
                                return_type: b'L',
                                invoke_kind: 3,
                                declaring_class_id: c.box_class_id,
                                owner_class_id: 0,
                            }));
                        box_info as *const cratonvm_jit::JitInvokeInfo as usize
                    } else {
                        0
                    }
                } else {
                    0
                };
                shared
                    .jit
                    .bridge
                    .const_folds
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .const_mh_invoke_infos
                    .insert(
                        (slot_addr, site_desc),
                        cratonvm_jit::ir::ConstMhFold {
                            info_ptr,
                            ret_type: t_ret_type,
                            convert,
                            box_info_ptr,
                        },
                    );
            }
        }
    }

    let _ = exact_name;
    for (f_idx, tag, is_stable, trusted_final) in fields_to_scan {
        let desc_byte = match tag {
            b'[' => b'L',
            other => other,
        };
        let val = shared.mem.heap.get_field_as(obj, f_idx, desc_byte);
        match val {
            Value::Object(Some(child)) => {
                let child_slot = crate::jit::helpers::mint_jit_const_ref_slot(shared, child);
                if child_slot != 0 {
                    let (child_fwd, ok) = shared.mem.heap.load_and_forward_checked(child);
                    let child_cid = if ok {
                        exact_class_id_of_const_ref(shared, child_fwd)
                    } else {
                        0
                    };
                    shared
                        .jit
                        .bridge
                        .const_folds
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .const_field_folds
                        .insert(
                            (slot_addr, f_idx as u32),
                            cratonvm_jit::ir::ConstFieldFold::Ref {
                                slot_addr: child_slot,
                                exact_class_id: child_cid,
                            },
                        );
                    walk_const_obj_fields(shared, thread.as_deref_mut(), offerer_class_id, child_fwd, child_slot, depth + 1, visited);
                }
            }
            Value::Int(v) if matches!(desc_byte, b'I' | b'Z' | b'B' | b'S' | b'C') => {
                if trusted_final || (is_stable && v != 0) {
                    shared
                        .jit
                        .bridge
                        .const_folds
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .const_field_folds
                        .insert(
                            (slot_addr, f_idx as u32),
                            cratonvm_jit::ir::ConstFieldFold::I32(v),
                        );
                }
            }
            Value::Long(v) if desc_byte == b'J' => {
                if trusted_final || (is_stable && v != 0) {
                    shared
                        .jit
                        .bridge
                        .const_folds
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .const_field_folds
                        .insert(
                            (slot_addr, f_idx as u32),
                            cratonvm_jit::ir::ConstFieldFold::I64(v),
                        );
                }
            }
            _ => {}
        }
    }
}

fn exact_class_id_of_const_ref(shared: &SharedVm, fwd: ObjectRef) -> u32 {
    if shared.mem.heap.kind_of(fwd) == crate::memory::heap::ObjectKind::Array {
        let desc = crate::runtime::interpreter::array_descriptor_of(shared, fwd).or_else(|| {
            (shared.mem.heap.element_type_of(fwd) == cratonvm_types::ArrayElementType::Reference)
                .then(|| "[Ljava/lang/Object;".to_string())
        });
        return desc
            .and_then(|d| shared.load_class_concurrent(&d).ok())
            .map_or(0, |id| id.as_u32());
    }
    shared.mem.heap.class_id_of_validated(fwd).as_u32()
}

pub(crate) fn mint_const_ref_folds_at_offer(
    shared: &SharedVm,
    mut thread: Option<&mut crate::threading::jvm_thread::JvmThread>,
    class_id: ClassId,
    code: &[u8],
) {
    let mut sites: Vec<(ClassId, usize)> = Vec::new();
    let mut guesses: Vec<(ClassId, usize)> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let op = code[pc];
        if op == 0xb2 && pc + 2 < code.len() {
            let cp_idx = u16::from_be_bytes([code[pc + 1], code[pc + 2]]);
            if let Ok(f) = jit_resolve_field_site(shared, class_id, cp_idx) {
                if f.is_static
                    && f.is_final
                    && crate::vm::is_class_initialized_via_manager(shared, f.declaring_class_id)
                {
                    sites.push((f.declaring_class_id, f.field_index));
                } else if f.is_static
                    && f.is_reference
                    && !f.is_volatile
                    && thread.is_some()
                    && crate::vm::is_class_initialized_via_manager(shared, f.declaring_class_id)
                {
                    guesses.push((f.declaring_class_id, f.field_index));
                }
            }
        }
        let Some(len) = cratonvm_jit::bytecode_analysis::insn_len(code, pc) else {
            break;
        };
        pc += len;
    }
    let mut visited = std::collections::HashSet::new();
    for (owner_cid, f_idx) in sites {
        if let Value::Object(Some(obj)) = crate::vm::get_static_shared(shared, owner_cid, f_idx) {
            let existing = shared
                .jit
                .bridge
                .const_folds
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .static_final_const_refs
                .get(&(owner_cid.as_u32(), f_idx as u32))
                .copied();
            let (slot_addr, exact_cid) = if let Some((s, c)) = existing {
                (s, c)
            } else {
                let slot = crate::jit::helpers::mint_jit_const_ref_slot(shared, obj);
                if slot == 0 {
                    continue;
                }
                let (fwd, ok) = shared.mem.heap.load_and_forward_checked(obj);
                let (cid, prim_array) = if ok {
                    let cid = exact_class_id_of_const_ref(shared, fwd);
                    let prim_array = (shared.mem.heap.kind_of(fwd)
                        == crate::memory::heap::ObjectKind::Array)
                        .then(|| {
                            let elem = shared.mem.heap.element_type_of(fwd);
                            (elem != cratonvm_types::ArrayElementType::Reference)
                                .then(|| {
                                    i32::try_from(shared.mem.heap.array_length(fwd)).ok().map(
                                        |len| {
                                            let tag = (cratonvm_types::ObjectKind::Array as u8)
                                                | ((elem as u8) << 2);
                                            (tag, len)
                                        },
                                    )
                                })
                                .flatten()
                        })
                        .flatten();
                    (cid, prim_array)
                } else {
                    (0, None)
                };
                let mut folds = shared
                    .jit
                    .bridge
                    .const_folds
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                folds
                    .static_final_const_refs
                    .insert((owner_cid.as_u32(), f_idx as u32), (slot, cid));
                if let Some(pa) = prim_array {
                    folds
                        .static_final_prim_arrays
                        .insert((owner_cid.as_u32(), f_idx as u32), pa);
                }
                (slot, cid)
            };
            let _ = exact_cid;
            walk_const_obj_fields(shared, thread.as_deref_mut(), class_id, obj, slot_addr, 0, &mut visited);
        }
    }
    for (owner_cid, f_idx) in guesses {
        if let Some(slot_addr) = mint_static_guess_slot(shared, owner_cid, f_idx) {
            if let Value::Object(Some(obj)) = crate::vm::get_static_shared(shared, owner_cid, f_idx) {
                walk_const_obj_fields(shared, thread.as_deref_mut(), class_id, obj, slot_addr, 0, &mut visited);
            }
        }
    }
}

/// Most slots [`mint_static_guess_slot`] mints per VM: each roots the handle
/// it was minted for until the VM ends.
const STATIC_GUESS_SLOTS_MAX: usize = 256;

/// The `ConstFoldTables::static_guess_const_refs` slot of the non-final
/// static `(owner, f_idx)` when it holds a `MethodHandle` now: the filed slot
/// while it still holds the field's handle, else, for a field with none yet,
/// a new one (at most [`STATIC_GUESS_SLOTS_MAX`]). On a mutator: minting is
/// refused on the worker.
fn mint_static_guess_slot(shared: &SharedVm, owner: ClassId, f_idx: usize) -> Option<usize> {
    let Value::Object(Some(obj)) = crate::vm::get_static_shared(shared, owner, f_idx) else {
        return None;
    };
    let (obj, ok) = shared.mem.heap.load_and_forward_checked(obj);
    let cid = ok.then(|| shared.mem.heap.class_id_of_validated(obj)).filter(|c| c.as_u32() != 0)?;
    if !shared.classes.class_manager.read().is_subclass_of_by_name(cid, "java/lang/invoke/MethodHandle") {
        return None;
    }
    let key = (owner.as_u32(), f_idx as u32);
    let (filed, count) = {
        let folds = shared.jit.bridge.const_folds.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (folds.static_guess_const_refs.get(&key).copied(), folds.static_guess_const_refs.len())
    };
    if let Some((slot, _)) = filed {
        // shortcut: a field rewritten after its slot was minted keeps the old guess (its guard
        // fails and de-speculates); re-mint only with a bound on leaked roots if a row needs it.
        let held = shared.natives.jni_global_refs.lock().resolve((slot | 1) as _);
        let same = held.is_some_and(|h| shared.mem.heap.load_and_forward_checked(h).0.as_ptr() == obj.as_ptr());
        return same.then_some(slot);
    }
    if count >= STATIC_GUESS_SLOTS_MAX {
        return None;
    }
    let slot = crate::jit::helpers::mint_jit_const_ref_slot(shared, obj);
    if slot == 0 {
        return None;
    }
    shared
        .jit
        .bridge
        .const_folds
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .static_guess_const_refs
        .insert(key, (slot, cid.as_u32()));
    Some(slot)
}




/// Round 13 wave 6 (lane sync4, patch
/// `r13w6-sync3-env-cache-door-guard-skip-patch-FIXED-20260928.md`): does an
/// interpreter door entering `compiled` for `cached` hold the method monitor
/// itself? Not for a SELF-LOCKING body, which takes it and releases it on
/// every exit (the backend refuses any other body, `x64/driver.rs`
/// `self_lock_exits_are_closed`), so the door would only lock it twice.
/// `false` for every unsynchronized method.
/// `CRATONVM_JIT_SELF_LOCKING_DOOR_SKIP=0` restores the door's hold.
pub(in crate::runtime::interpreter::jit_bridge) fn door_holds_method_monitor(
    compiled: &crate::jit::CompiledMethod,
    cached: &CachedBytecodeMethod,
) -> bool {
    cached.is_synchronized
        && !(compiled.self_locks_monitor && crate::runtime::env_cache::jit_self_locking_door_skip())
}

/// Most Java arguments the register-only JIT ABI carries, the `needs_heap`
/// context pointer included.
///
/// The JIT backend wires Java params via ARG_REGS only (no stack-arg
/// marshalling). If the method's parameter count exceeds the available
/// register slots, the JIT codegen would silently truncate (see the x64
/// prologue — `ARG_REGS.iter()...take(self.num_params)`), producing a method
/// body that reads uninitialised locals for the missing tail params. Every door
/// below declines to the bytecode interpreter in that case instead of
/// dispatching a miscompiled call.
///
/// Reproducer (before this gate): Spring Boot 2.x's
/// `ExecutableArchiveLauncher.getMainClass` → `ZipFile`/`JarFile` chain calls
/// into a 5+-arg JIT'd method on Windows and panics with "index out of bounds:
/// the len is 4 but the index is 4" at the pop-into-`jit_args` loop.
///
/// Shared buffer capacity for interpreter doors and compiled lambda callers.
/// The entry helpers separately enforce the usable parameter count, including
/// the heap-context word and the optional narrow-ABI diagnostic setting.
pub(crate) const JIT_ABI_MAX_JAVA_ARGS: usize = if cfg!(target_arch = "x86_64") {
    cratonvm_jit::TRY_CALL_MAX_WORDS
} else {
    8
};

/// How many Java arguments a body with this `needs_heap` can take: the context
/// pointer occupies one of the [`JIT_ABI_MAX_JAVA_ARGS`] slots.
#[inline]
pub(in crate::runtime::interpreter) fn jit_abi_max_java_params(needs_heap: bool) -> usize {
    // Round 12 wave 2: the doors pass up to sixteen words on x86-64;
    // `CRATONVM_JIT_HELPER_WIDE_ENTRY_CALLS=0` restores the eight-word cap.
    let words = if crate::jit::helpers::helper_wide_entry_calls_enabled() {
        JIT_ABI_MAX_JAVA_ARGS
    } else {
        JIT_ABI_MAX_JAVA_ARGS.min(8)
    };
    words - usize::from(needs_heap)
}

/// The compiled method's own `needs_heap`.
///
/// cceres2: never trust a separately-cached ABI flag over the compiled
/// method's own record — a (compiled, needs_heap) pair captured at two
/// different times can disagree after a recompile, and marshalling with the
/// wrong flag shifts every argument by one inside the callee. See
/// `try_call_compiled_entry_reentrant` for the matching guard + rationale.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn jit_entry_needs_heap(
    compiled: &crate::jit::CompiledMethod,
    cached_flag: bool,
    cached: &CachedBytecodeMethod,
    door: &'static str,
) -> bool {
    let own = compiled.needs_heap();
    if own != cached_flag {
        warn_jit_abi_flag_mismatch(cached, door, cached_flag, own);
    }
    own
}

#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn warn_jit_abi_flag_mismatch(
    cached: &CachedBytecodeMethod,
    door: &'static str,
    cached_flag: bool,
    own: bool,
) {
    tracing::warn!(
        "JIT ABI flag mismatch in {door}: cached needs_heap={cached_flag} but compiled \
         {}.{}{} says {own} — using the compiled method's own flag",
        cached.class_name,
        cached.method_name,
        cached.method_descriptor,
    );
}

/// Must every method of this VM run interpreted, because a JVMTI agent listens
/// for an event only the interpreter posts (`SingleStep`, `MethodEntry`,
/// `MethodExit`, `FramePop`, `FieldAccess`, `FieldModification`, and since
/// wave 18 `Exception` — `jvmti::EXCEPTION_EVENTS_NEED_THE_INTERPRETER`), or
/// because a JDWP breakpoint, step request or suspension is in force?
///
/// HotSpot's `interp_only_mode`, at VM rather than thread granularity. Each
/// interpreter→compiled door asks it before entering a body and declines to
/// its interpreted fallback, so a whole compiled call subtree can no longer
/// run without producing a single event. One `Acquire` load when no agent
/// listens for any of the six; see `jvmti::interp_only_events_active_for_vm`.
///
/// The JDWP half (interpreter round i1 wave 5, L7): breakpoints, steps and
/// the debugger's suspend point are delivered by the dispatch loop only
/// (`interpreter::deliver_breakpoint_if_set`, lane L1), so a method entered
/// compiled never stopped at a breakpoint set in it, and a thread running
/// compiled never parked for `VirtualMachine.Suspend`. The same stand-down
/// serves both: one `Relaxed` load of `shared.debug.breakpoints_active`, and
/// the constant `false` in a build without the JDWP surface. A loop already
/// running in a single-pass OSR body leaves it at its next `goto` back-edge
/// poll (wave 12, `jvmti_events::compiled_loop_must_leave`); other frames
/// already running compiled when the request is made still finish compiled
/// (no lazy deopt exists; see
/// `docs/feature-designs/jvmti-delivery-threading.md`).
#[inline]
pub(crate) fn jvmti_requires_interpreter(shared: &SharedVm) -> bool {
    crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity)
        || jdwp_requires_interpreter(shared)
        // JIT round 22 wave 1, lane review: a JNI re-`<init>` witness window
        // (`CRATONVM_JNI_REINIT_WITNESS_INTERPRETS`; one relaxed load).
        || super::jni_reinit_witness_window_open(shared)
}

/// The JDWP half of [`jvmti_requires_interpreter`].
#[cfg(feature = "experimental-debug")]
#[inline(always)]
pub(in crate::runtime::interpreter::jit_bridge) fn jdwp_requires_interpreter(shared: &SharedVm) -> bool {
    shared
        .debug
        .breakpoints_active
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// [`jdwp_requires_interpreter`] in a build without the JDWP surface.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(in crate::runtime::interpreter::jit_bridge) fn jdwp_requires_interpreter(_shared: &SharedVm) -> bool {
    false
}

/// [`jvmti_requires_interpreter`] for one callee: the doors that know the
/// method they are about to enter ask this (interpreter round i1 wave 7,
/// lane L1). The JVMTI half is unchanged (VM-wide); the JDWP half stands down
/// only for a method the requests in force concern
/// (`debug::DebuggerGates::concerns_method`): a method holding a breakpoint,
/// or every method while a step request or a suspension is in force. Until
/// wave 7 one breakpoint anywhere kept every method of the VM out of compiled
/// code for the whole session. The OSR and first-call gates ask the same
/// per-method question from the method's identity
/// (`interpreter::jvmti_requires_interpreter_for_method`, wave 8); the
/// compiled-code helpers, with no method in hand, keep the VM-wide answer.
#[inline]
pub(crate) fn jvmti_requires_interpreter_for(
    shared: &SharedVm,
    cached: &CachedBytecodeMethod,
) -> bool {
    crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity)
        || jdwp_requires_interpreter_for(shared, cached)
        // JIT round 22 wave 1, lane review: see `jvmti_requires_interpreter`.
        || super::jni_reinit_witness_window_open(shared)
}

/// The JDWP half of [`jvmti_requires_interpreter_for`].
#[cfg(feature = "experimental-debug")]
#[inline(always)]
pub(in crate::runtime::interpreter::jit_bridge) fn jdwp_requires_interpreter_for(shared: &SharedVm, cached: &CachedBytecodeMethod) -> bool {
    // `requires_interpreter`, not `concerns_method` (wave 10): a JDWP field
    // watch or exception request keeps every method interpreted without
    // arming the loop's per-bytecode suspend point.
    jdwp_requires_interpreter(shared)
        && shared.debug.debugger_gates.requires_interpreter(
            cached.declaring_class_id.as_u32(),
            &cached.method_name,
            &cached.method_descriptor,
        )
}

/// [`jdwp_requires_interpreter_for`] in a build without the JDWP surface.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(in crate::runtime::interpreter::jit_bridge) fn jdwp_requires_interpreter_for(_shared: &SharedVm, _cached: &CachedBytecodeMethod) -> bool {
    false
}

/// The JIT-ABI bit pattern of one decoded argument: `int` sign-extended,
/// `long` raw, `float`/`double` as their raw bits (no NaN-box tag), a
/// reference as its address, `null` as 0.
#[inline]
pub(in crate::runtime::interpreter) fn jit_abi_arg_bits(v: &Value) -> i64 {
    match *v {
        // Widening: i32 -> i64 (sign-extended, JVM i2l)
        Value::Int(x) => x as i64,
        Value::Long(x) => x,
        // Cast: float/double raw bit pattern stored in integer word (no value conversion)
        Value::Float(x) => x.to_bits() as i64,
        // Cast: float/double raw bit pattern stored in integer word (no value conversion)
        Value::Double(x) => x.to_bits() as i64,
        // Cast: object/code pointer to integer address
        Value::Object(Some(obj)) => obj.as_ptr() as i64,
        Value::Object(None) => 0,
        _ => 0,
    }
}

/// Interpreter round i7 wave 136, lane L2 (suspect 1 of the wave-135 L2
/// report): is a decoded argument free of the one disagreement with its
/// descriptor tag that the ABI encoding makes dangerous -- a non-null
/// reference in a primitive slot? (A numeric kind mismatch, or a null, encodes
/// as a plain number and is not asserted here.)
/// [`jit_abi_arg_bits`] encodes a reference as its address whatever slot it
/// sits in, and `CompactValue::decode_by_descriptor` hands an `I`/`F` tag a
/// slot that holds an object as `Value::Object` (its reference arms null a
/// primitive instead, so only that direction can reach the ABI). Verified
/// bytecode never puts a reference in a primitive slot; only a VM-side
/// producer (a native pushing the wrong kind, a reflective or method-handle
/// door that forgot to unbox) could, and the compiled body would then read an
/// address as an `int`. Checked by `debug_assert!` at every
/// interpreter-to-compiled door that encodes arguments (no release cost); a
/// `false` names the producer to fix, not this door. Unknown tags answer
/// `true`.
#[inline]
pub(in crate::runtime::interpreter) fn jit_abi_arg_kind_agrees(v: &Value, desc_byte: u8) -> bool {
    !(matches!(v, Value::Object(Some(_)))
        && matches!(
            desc_byte,
            b'J' | b'D' | b'F' | b'I' | b'B' | b'C' | b'S' | b'Z'
        ))
}

/// The descriptor tag of each Java argument slot of `descriptor` (`L` for an
/// instance method's receiver first), one byte per argument as the
/// interpreter-to-compiled doors pass them (a `long`/`double` is one
/// argument). Debug-assertion helper for [`jit_abi_arg_kind_agrees`].
pub(in crate::runtime::interpreter::jit_bridge) fn jit_abi_arg_tags(descriptor: &str, is_static: bool) -> Vec<u8> {
    let mut tags = Vec::new();
    if !is_static {
        tags.push(b'L');
    }
    let bytes = descriptor.as_bytes();
    let mut i = usize::from(bytes.first() == Some(&b'('));
    while i < bytes.len() && bytes[i] != b')' {
        let tag = bytes[i];
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        tags.push(tag);
    }
    tags
}

/// `debug_assert!` that every decoded argument of a call into `cached`'s
/// compiled body agrees with its descriptor ([`jit_abi_arg_kind_agrees`]).
/// Compiled out of a release build.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn debug_assert_jit_args_agree(cached: &CachedBytecodeMethod, args: &[Value]) {
    if cfg!(debug_assertions) {
        let tags = jit_abi_arg_tags(&cached.method_descriptor, cached.is_static);
        for (i, (v, tag)) in args.iter().zip(tags.iter()).enumerate() {
            debug_assert!(
                jit_abi_arg_kind_agrees(v, *tag),
                "interpreter-to-compiled door: argument {i} of {}.{}{} is {v:?} for tag {}",
                cached.class_name,
                cached.method_name,
                cached.method_descriptor,
                *tag as char,
            );
        }
    }
}

/// Re-encode every pinned reference argument from its pin, which the
/// collector forwards in place. Used after a `synchronized` method's monitor
/// was acquired: a static method's `get_or_create_class_mirror` can allocate
/// (and so move) on its slow path, after `jit_args` was encoded.
pub(in crate::runtime::interpreter::jit_bridge) fn reencode_pinned_jit_args(thread: &JvmThread, arg_pins: &[usize], jit_args: &mut [i64]) {
    for (slot, &pin) in jit_args.iter_mut().zip(arg_pins) {
        if let Some(obj) = thread.native_pin_roots.get(pin) {
            *slot = obj.as_ptr() as i64; // Cast: JIT ABI -- pointer to i64
        }
    }
}

/// A compiled activation's result as a `Value`, converted by the descriptor's
/// return tag; `None` for `void`.
#[inline]
pub(in crate::runtime::interpreter) fn jit_return_value(return_type: u8, raw: i64) -> Option<Value> {
    match return_type {
        b'I' | b'B' | b'C' | b'S' | b'Z' => Some(Value::Int(narrow_int_return(return_type, raw))),
        b'J' => Some(Value::Long(raw)),
        b'F' => Some(Value::Float(f32::from_bits(raw as u32))), // Cast: JIT ABI -- i64 register convention
        b'D' => Some(Value::Double(f64::from_bits(raw as u64))), // Cast: JIT ABI -- i64 register convention
        b'[' | b'L' => Some(Value::Object(if raw == 0 {
            None
        } else {
            // SAFETY: a non-zero reference return is the address of a live object header.
            Some(unsafe { crate::types::ObjectRef::from_raw(raw as *mut u8) }) // Cast: JIT ABI -- i64 register convention
        })),
        _ => None, // void
    }
}

/// How a compiled activation ended, once every out-of-band signal it left was
/// drained. Produced by [`run_jit_body`]; each door only decides how to hand
/// it to its own caller.
pub(in crate::runtime::interpreter::jit_bridge) enum JitBodyOutcome {
    /// Normal completion with these raw JIT-ABI return bits.
    Returned(i64),
    /// A throwable to route through the callee's own exception table.
    Throw {
        exc: ObjectRef,
        throw_pc: JitThrowPc,
    },
    /// The body trapped and stashed a reconstructed frame (IR-path deopt),
    /// with the cause of the point that fired when the stash kept one, and
    /// that point's address (0 when unknown; see
    /// `cratonvm_jit::deopt::take_last_deopt_with_point`).
    Stashed(
        cratonvm_jit::deopt::ReconstructedFrame,
        Option<cratonvm_jit::deopt::DeoptCause>,
        usize,
    ),
    /// The deopt sentinel with no stashed frame: re-run from entry.
    Deopt,
}

/// [`JitBodyOutcome`] before the throw site is classified against a method's
/// exception table: `Throw` carries the raw stamped bci (`-1` = none), which
/// each door classifies against whatever it holds — a
/// [`CachedBytecodeMethod`] ([`run_jit_body`]) or the frame it pushed
/// (`execute`'s first-call door, `jit_local_athrow_pc_in_frame`). The two
/// classifiers run the same two tests on the same code and table.
pub(in crate::runtime::interpreter) enum RawJitBodyOutcome {
    /// Normal completion with these raw JIT-ABI return bits.
    Returned(i64),
    /// A throwable to route; `athrow_bci` is the stamped throw site or `-1`.
    Throw { exc: ObjectRef, athrow_bci: i64 },
    /// The body trapped and stashed a reconstructed frame (IR-path deopt),
    /// with the cause and the address of the point that fired, as
    /// [`JitBodyOutcome::Stashed`].
    Stashed(
        cratonvm_jit::deopt::ReconstructedFrame,
        Option<cratonvm_jit::deopt::DeoptCause>,
        usize,
    ),
    /// The deopt sentinel with no stashed frame: re-run from entry.
    Deopt,
}

/// Enter `compiled` with the already-encoded `args_jit` and drain everything
/// it signalled — the part the three interpreter→compiled doors
/// ([`execute_jit_call`], [`execute_jit_call_decoded`],
/// [`execute_jit_call_oneshot`]) have in common.
///
/// This used to be three hand-kept copies of ~500 lines, and they drifted:
/// round i1 found two of the three implicit-exception drains not dropping a
/// stashed deopt frame, and one IR-deopt sink consuming the stash without
/// de-speculating (`interpreter-L7-tier-up-gates-duplicated-in-disagreeing-copies-FIXED`).
/// The body run, the signal drain and the sentinel classification now exist
/// once; the doors differ only in where the arguments come from and where the
/// outcome goes.
///
/// Reads and writes neither the caller's operand stack nor any argument
/// buffer.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn run_jit_body(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &CachedBytecodeMethod,
    needs_heap: bool,
    args_jit: &[i64],
) -> Result<JitBodyOutcome, MethodCallFailed> {
    Ok(
        match run_jit_body_raw(
            shared,
            thread,
            compiled,
            JitBodyName::of(cached),
            needs_heap,
            args_jit,
        )? {
            RawJitBodyOutcome::Returned(v) => JitBodyOutcome::Returned(v),
            RawJitBodyOutcome::Throw { exc, athrow_bci } => JitBodyOutcome::Throw {
                exc,
                throw_pc: jit_local_athrow_pc_kind(cached, athrow_bci),
            },
            RawJitBodyOutcome::Stashed(rframe, cause, point) => {
                JitBodyOutcome::Stashed(rframe, cause, point)
            }
            RawJitBodyOutcome::Deopt => JitBodyOutcome::Deopt,
        },
    )
}

/// Is `thread` a virtual thread's continuation (`ThreadKind::Virtual`: the
/// JDK-driven path's and the bound path's alike)?
#[inline]
pub(crate) fn thread_is_continuation(thread: &JvmThread) -> bool {
    matches!(thread.kind, crate::threading::ThreadKind::Virtual)
}

/// gci i11/vt item 10: a continuation never runs compiled code. Its frames
/// must all be interpreter frames when it yields: the yield unwinds them into
/// the boxed `JvmThread` and the remount resumes them on another OS thread,
/// so a compiled activation (its spill slots, register image and shadow-stack
/// window) would be left on a runner stack nothing returns to, its roots
/// unscanned and unremapped. The rule is enforced by per-door
/// `ThreadKind::Virtual` declines (method entry `env_disable_jit`, OSR, the
/// cached static / virtual / lambda doors, the wrapped-body door); this is
/// the one place both interpreter->compiled entries pass (`run_jit_body_raw`
/// for every method-entry door, `try_osr` for OSR), so a new door that forgets
/// its decline fails loudly in a debug build instead of silently.
///
/// gci i19/vd: in every build it also records the breach on the thread
/// (`GcBlockState::continuation_entered_compiled`, sticky), so the precise G1
/// continuation deposit (`thread_registry::continuation_is_interpreted_only`)
/// rests on what this thread actually ran, not on every door's decline. One
/// predictable byte compare per compiled entry; the store runs only on a
/// breach.
#[inline]
pub(in crate::runtime::interpreter) fn assert_no_compiled_continuation(thread: &JvmThread) {
    if thread_is_continuation(thread) {
        // Interpreter round i5 wave 114, lane L6: load before the store. Since
        // waves 92-113 a virtual thread enters compiled leaves (and, under the
        // learned switches, other bodies) on every such call, so the sticky
        // store ran per entry and dirtied the thread's `gc_block_state` line,
        // which safepoint and GC threads read; the mark is set once.
        let mark = &thread.gc_block_state.continuation_entered_compiled;
        if !mark.load(std::sync::atomic::Ordering::Relaxed) {
            mark.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    // Interpreter round i5 wave 114, lane L6 (page
    // `i114-L6-debug-build-asserts-on-every-admitted-virtual-thread-compiled-entry`):
    // a JDK continuation's thread is admitted compiled bodies on purpose since
    // wave 92 (`jdk_continuation::vthread_may_enter_compiled_method`, the
    // learned OSR door; a yield beneath pins), so a debug build aborted on the
    // first hot static leaf any virtual thread called. Every admission
    // requires `jdk_continuation.jdk()`; a bound (VM-scheduled) virtual thread
    // is still never admitted, and the assertion keeps guarding it.
    debug_assert!(
        !thread_is_continuation(thread) || thread.jdk_continuation.jdk(),
        "a virtual thread's continuation (tid {}) is entering compiled code: \
         a door is missing its ThreadKind::Virtual decline (gci i11/vt item 10)",
        thread.thread_id.0
    );
}

/// The `[door-cycles]` exit line ([`door_cycles`]).
pub fn door_cycles_report() {
    door_cycles::report();
}

/// The body run, the signal drain and the sentinel classification of
/// [`run_jit_body`], for every interpreter→compiled door — the three above and
/// `interpreter.rs::execute`'s first-call door, which used to keep a fourth,
/// hand-written copy that drifted (it never took the deopt flag, drained each
/// implicit signal separately so two could build two exceptions, routed an
/// implicit exception without its stamped bci, and left a stashed deopt frame
/// in place under a routed implicit exception, for its stash sink to resume
/// instead of throwing). See
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// `name` is used only by the stranded-exception warning.
///
/// `#[inline]` as `run_jit_body` was before the split: it is the per-call body
/// of the three hot doors.
#[inline]
pub(in crate::runtime::interpreter) fn run_jit_body_raw(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    name: JitBodyName<'_>,
    needs_heap: bool,
    args_jit: &[i64],
) -> Result<RawJitBodyOutcome, MethodCallFailed> {
    let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::JitExecution);
    let vm_ptr = shared as *const _ as i64; // Cast: JIT ABI -- pointer to i64 register
    // Interpreter round i7 wave 136, lane L2 (`CRATONVM_DBG_DOOR_CYCLES`,
    // diagnostic): stage 0 of `i25-L2`. Dropped on every return below.
    let door_span = door_cycles::DoorSpan::enter();

    // Fast path: dispatch-free methods skip catch_unwind.
    // task #44: `try_call`/`try_call_with_context` (not the panicking shims):
    // JIT runtime invocation failures (invalid code pointer / too-many-args)
    // surface as `MethodCallFailed::InternalError` instead of silent 0-returns.
    //
    // NEW-1.5 + T1.1.a: even a dispatch-free JIT call may transitively trigger
    // GC via a helper, so the entry guard is pushed on both arms; the root
    // scanner then finds spill slots in this frame (precise oop maps when the
    // compiled method has them).
    //
    // The `JIT_THREAD` TLS is installed on BOTH arms too, and its absence on
    // the dispatch-free arm was a GC-safety hole rather than an optimization:
    // `jit_safepoint_slow_path_body` is `if let Some((thread, _guard)) =
    // jit_thread_mut() { ... }`, and `jit_thread_mut` reads the TLS only
    // `set_jit_thread` writes. Without it a pure hot loop reached its back-edge
    // poll, saw the stop-the-world flag, called the helper, and returned
    // WITHOUT retiring its TLAB, draining SATB, publishing its roots or
    // arriving at the barrier — survivable only through `xt_root_scan`'s
    // forcible `SuspendThread`, which exists on Windows and Linux-x86_64 and
    // nowhere else. `restore_jit_thread` additionally resets the shadow-stack
    // `top` watermark, healing any push an abnormal JIT exit left unbalanced.
    // Only the TLS is shared with the dispatch arm, NOT `catch_unwind`: a
    // dispatch-free body calls no helper that can unwind.
    //
    // A deopt flag pending before the body runs cannot describe it; retire it so
    // neither the body's `J`/`D` call sites nor the `deopt_signaled`
    // classification below read a stale one as a sentinel and re-run a body that
    // returned a genuine `Long.MIN_VALUE` (the same entry hygiene `try_osr`
    // applies; interpreter round i1 wave 4, L7). One thread-local store.
    //
    // t1 wave 5, lane S (`CRATONVM_JIT_DOOR_THREAD_BLOCK`, per VM, default ON, `=0` kills;
    // i25-L2 stage 2): the deopt retire and the stamp clear below in one
    // `JIT_SIGNALS` access, and the exceptional-stash depth read once for this
    // door's floor and its entry guard's (`JitEntryGuard::enter_door`).
    let door_block = shared.config.jit_door_thread_block;
    if door_block {
        crate::jit::helpers::retire_door_entry_signals();
    } else {
        let _ = crate::jit::helpers::take_jit_deopt_pending();
        // Interpreter round i5 wave 104, lane L2: a throw-site stamp pending
        // before the body runs cannot describe it either. Left set, it rode out
        // with an implicit signal raised by a runtime helper (those set no
        // stamp) and the drain below read another method's bci (the w104
        // `foreign throw stamp refused` census).
        crate::jit::helpers::clear_jit_athrow_bci();
    }
    // The exceptional stash's depth before the body runs: every reason-9 frame
    // above it at the sentinel arm below was published by an activation this
    // door's body started (interpreter round i1 wave 25, lane L2). One
    // thread-local read, and none for a body with no point a pad could publish
    // (`body_may_publish_a_pad_frame`: most leaf bodies), whose own frames the
    // sentinel arm could never claim.
    let exc_floor = if body_may_publish_a_pad_frame(compiled) {
        cratonvm_jit::deopt::exceptional_stash_depth()
    } else {
        usize::MAX
    };
    let door_exc_depth = (door_block && exc_floor != usize::MAX).then_some(exc_floor);
    // HANDLER11-1 (JIT round 19 wave 11, lane handler11): the ordinary
    // stash's push mark before the body runs. At the sentinel arm only a frame
    // pushed after it is this body's ([`door_stash_since_entry_enabled`]). One
    // thread-local read.
    let stash_mark = cratonvm_jit::deopt::deopt_stash_push_mark();
    // gci i11/vt item 10: every method-entry door runs its body here.
    assert_no_compiled_continuation(thread);
    let saved_jit_thread = crate::jit::helpers::set_jit_thread(thread);
    // Interpreter round i6 wave 119, lane L2 (`CRATONVM_JIT_POLL_THREAD_WORD`,
    // default off): compiled polls of THIS VM may read this OS thread's loop
    // poll word, registered with this VM's barrier first; restored (to the
    // enclosing door's value, or null) when this door returns. Off: one
    // cached-flag load.
    let _poll_word_mirror = crate::jit::helpers::PollWordMirrorScope::enter(shared);
    door_cycles::DoorSpan::mark(door_span.as_ref(), 1);
    let raw = {
        let _jit_root_guard = if door_block {
            // t1 wave 8, lane I25 (`CRATONVM_JIT_DOOR_ONE_DEPTH_COUNTER`, per
            // VM, default ON since j27): the entry's three process-wide counts in one.
            crate::jit::conservative_roots::JitEntryGuard::enter_door(
                compiled,
                thread.frames.len(),
                door_exc_depth,
                shared.config.jit_door_one_depth_counter,
            )
        } else {
            crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
                compiled,
                Some(thread.frames.len()),
            )
        };
        let enter = || {
            // SAFETY: compiled is a finalized JIT CompiledMethod whose entry
            // point was validated; `args_jit` was encoded from the method's JVM
            // descriptor (receiver-aware) and fits the register ABI.
            door_cycles::DoorSpan::time_body(door_span.as_ref(), || unsafe {
                if needs_heap {
                    compiled.try_call_with_context(vm_ptr, args_jit)
                } else {
                    compiled.try_call(args_jit)
                }
            })
        };
        if compiled.has_dispatch {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(enter))
        } else {
            Ok(enter())
        }
    };
    door_cycles::DoorSpan::mark(door_span.as_ref(), 3);
    crate::jit::helpers::restore_jit_thread(saved_jit_thread);
    door_cycles::DoorSpan::mark(door_span.as_ref(), 4);

    // PERF (JIT-entry drain consolidation): snapshot-and-clear the whole signal
    // block in ONE TLS access (`take_all_jit_signals`); the drains below
    // consume the local snapshot. Draining everything on every path IS the
    // Round-8..11 leak-fix discipline — a signal surviving into the next,
    // unrelated JIT call surfaced at the wrong pc in the wrong method.
    let mut sig = crate::jit::helpers::take_all_jit_signals(thread);
    // Interpreter round i5 wave 103, lane L2: the body was retired between
    // this door's entrant check and the call, and the not-entrant stub
    // answered the call through the VM, where the throwable already left the
    // method (`DrainedJitSignals::escaped_a_redispatch_of`). Read on the RAW
    // stamp, before the owner check below folds a foreign one into `-1`.
    let escaped_redispatch = sig.escaped_a_redispatch_of(name.method_name, name.descriptor);
    // Interpreter round i5 wave 102, lane L2: a stamp another method left is
    // not this body's throw pc (`DrainedJitSignals::athrow_bci_owned_by`).
    // Wave 104: asked only when the body left something to route. On an
    // ordinary return the stamp means nothing; it is named (no census tag of
    // the close criterion) so a leaking producer can still be found.
    if sig.exception.is_some() || sig.npe || sig.aioobe.is_some() || sig.arithmetic {
        sig.athrow_bci = sig.athrow_bci_owned_by_label(&compiled.method_label);
    } else if sig.athrow_bci >= 0 && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] JITC-STALE-STAMP on an ordinary return: bci={} owner={:#x} label={}{}",
            sig.athrow_bci,
            sig.athrow_owner,
            compiled.method_label,
            crate::jit::helpers::jitc_census_backtrace()
        );
    }
    if escaped_redispatch {
        sig.athrow_bci = super::exception_dispatch::JIT_THROW_ESCAPED_REDISPATCH;
        // Interpreter round i5 wave 104, lane L2: how often the wave-103 race
        // (a supersede between the entrant check and the call) is met. Not a
        // defect line: the throwable propagates correctly. Separate tag from
        // `JITC-PC-CENSUS`, which counts only the suspect answers.
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] JITC-RACE-CENSUS door met a not-entrant re-dispatch escape: {}.{}{}",
                name.class_name, name.method_name, name.descriptor
            );
        }
    }

    // A pending Java exception from a JIT dispatch callback. The JIT ran the
    // entire method, so in general the throw-site pc inside it is unknown.
    // RBC.6 correctness fix: the ONE case where the pc IS known is a local
    // `athrow` — the x64 codegen bakes its own bci into the call to
    // `jit_throw_exception`, stashed as `sig.athrow_bci`. Without it, a method
    // with 2+ exception-table entries whose catch types are in a subtype
    // relationship could route ANY matching-by-type exception to the FIRST
    // declared entry regardless of which try-region threw
    // (`AthrowCountBisect.twoThrowsSequential`,
    // `vm/tests/jit_local_exception_handler_tests.rs`). The exception consumes
    // the deopt: the drain above already cleared the out-of-band deopt signal.
    //
    // r9-vmside: a dispatch-free body is routed too — the drain TAKES the
    // throwable, so ignoring it lost it. See
    // `note_fast_entry_stranded_exception`.
    if let Some(exc) = sig.exception.take() {
        if !compiled.has_dispatch {
            note_fast_entry_stranded_exception(name);
        }
        // HANDLER11-3 (JIT round 19 wave 12, lane handler12): a deopt frame
        // pushed inside this body and still stashed now is an orphan: the
        // call completes here with its throwable, and the interpreter routes
        // that without ever taking a stash. Named (`door-throw`), and dropped
        // under `CRATONVM_JIT_COMPLETED_CALL_DROPS_LEFT_STASH`.
        crate::jit::helpers::note_completed_call_left_stash(
            cratonvm_jit::deopt::deopt_stash_top_seq().filter(|&seq| seq > stash_mark),
            "door-throw",
        );
        return Ok(RawJitBodyOutcome::Throw {
            exc,
            athrow_bci: sig.athrow_bci,
        });
    }
    let result = match raw {
        Ok(Ok(v)) => v,
        Ok(Err(jit_err)) => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!("JIT call failed: {jit_err}"),
            }));
        }
        Err(panic_payload) => return Err(jit_panic_to_exception(shared, thread, panic_payload)),
    };

    // MEDIUM fix (i64::MIN deopt-sentinel collision): the JIT signals
    // exception/deopt by returning `i64::MIN`, but a method that legitimately
    // returns `Long.MIN_VALUE` (or a J/D/F/I value whose JIT-ABI bits equal
    // `i64::MIN`) returns the same value WITHOUT setting this flag. Every JIT
    // path that produces the genuine sentinel also sets it (`jit_throw_aioobe`
    // / `jit_uncommon_trap` / the dispatch helpers and their x64 stubs), so the
    // `Deopt` classification below consults it instead of overloading the
    // value; re-running a method that really returned `i64::MIN` would
    // double-execute its side effects.
    let deopt_signaled = sig.deopt;

    // Round-8 CRIT / Round-9/10 HIGH / Round-11 (NPE, AIOOBE and
    // divide-by-zero leaks): a void-return store helper, or a nested dispatch
    // helper, can set one of these and return normally, so they are drained on
    // EVERY return path, before the sentinel arms, and routed through the
    // compiled method's own exception table.
    if sig.npe || sig.aioobe.is_some() || sig.arithmetic {
        return implicit_exception_from_signals(shared, thread, sig);
    }

    // real-frame-deopt: IR-path deopt detection. The IR lowerer's trampoline
    // (`ir_deopt_entry`) stashes a reconstructed frame in `LAST_DEOPT` and
    // returns `i64::MIN` WITHOUT setting `JIT_DEOPT_PENDING` (it lives in the
    // jit crate, with no access to the VM flag), so an IR-path deopt is
    // invisible to `deopt_signaled` and would otherwise be pushed as a real
    // return. It always returns `i64::MIN` when it stashes, so gating the
    // thread-local take on the sentinel keeps the common return path free of
    // it while never missing a stash.
    if result == i64::MIN {
        // r11w10-irexc-reason9-frame-published-for-a-deopt-sentinel: a protected
        // exit keyed on "the call returned the sentinel" publishes this
        // method's reason-9 frame even when the sentinel was a callee's DEOPT
        // (no exception is pending on any path that reaches here). No drain
        // takes it; left on `LAST_EXCEPTIONAL` it would be claimed by the NEXT
        // exception of this method and route it with a stale bci and stale
        // locals. A callee's frame is kept.
        //
        // Interpreter round i1 wave 25, lane L2
        // (`interpreter-L2-a-dropped-own-reason-9-frame-leaves-its-compiled-locks-held-for-the-rerun`):
        // such a frame, published above `exc_floor` by THIS body, is the last
        // description of the locks the activation's compiled code took and
        // still holds — its pad returned without a `monitorexit`, and nothing
        // resumes it (no exception is pending, so no handler runs). Whatever
        // the door does next (re-run from entry, resume a stash, push the
        // value), the activation that took them is gone, so they are released
        // here, innermost first. A frame below the floor is an earlier
        // activation's leftover, whose fate its own sink decided: dropped
        // below, as before, without a release (released since JIT round 17
        // wave 4, lane stashdrop, under the exceptional hand-over's switches).
        let owns =
            |point: usize| super::deopt_resume::deopt_stash_is_from_artifact(compiled, point);
        super::deopt_resume::release_locks_of_own_pad_exits(
            shared,
            thread,
            &owns,
            name.class_name,
            name.method_name,
            name.descriptor,
            exc_floor,
            None,
        );
        // JIT round 17 wave 4, lane stashdrop (sites 1 and 4 of
        // `r17-syncdoor3-exceptional-frames-discarded-beneath-a-take-keep-their-holds-FIXED-20261005.md`):
        // under the exceptional hand-over's switches the frame dropped here,
        // and every frame its take discards, has its holds released: no
        // live interpreter frame accounts for them, and nothing else will
        // ever release them. Off, the old unreleasing drop.
        super::deopt_resume::CompiledLocksOfAStash::drop_own_exceptional_frame_releasing(
            shared,
            thread,
            name.class_name,
            name.method_name,
            name.descriptor,
            &|_, _| false,
            "door sentinel arm",
        );
        if let Some((rframe, cause, point)) = take_door_stash_since_entry(stash_mark, name) {
            return Ok(RawJitBodyOutcome::Stashed(rframe, cause, point));
        }
        if deopt_signaled {
            return Ok(RawJitBodyOutcome::Deopt);
        }
    }
    Ok(RawJitBodyOutcome::Returned(result))
}

/// Can `compiled` publish a reason-9 frame of its own — does it own any deopt
/// point a pad could bake (interpreter round i1 wave 25, lane L2)? The
/// optimizing tier's pads, guards and poll exits bake `_deopt_point_boxes`
/// addresses (its `deopt_points` are resume points no stub names); the
/// single-pass tier's stubs name `deopt_points`. `false` means
/// `run_jit_body_raw`'s sentinel arm could claim no frame for this body
/// (`deopt_resume::release_locks_of_own_pad_exits` asks
/// `deopt_stash_is_from_artifact`), so its door skips the stash-depth read.
/// Two length loads.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn body_may_publish_a_pad_frame(compiled: &crate::jit::CompiledMethod) -> bool {
    !compiled._deopt_point_boxes.is_empty()
        || (!compiled.used_ir_backend && !compiled.deopt_points.is_empty())
}

/// `CRATONVM_JIT_DOOR_STASH_SINCE_ENTRY` (HANDLER11-1, JIT round 19 wave 11,
/// lane handler11; **default ON**, correctness, `=0` is the kill word): does a
/// door's sentinel arm take only a deopt frame pushed after the body it entered
/// started ([`take_door_stash_since_entry`])? Read on the sentinel arm only.
pub(in crate::runtime::interpreter) fn door_stash_since_entry_enabled() -> bool {
    true
}

/// The ordinary-stash frame a door's sentinel arm may claim for the body it
/// entered when the stash's push mark was `mark` (HANDLER11-1;
/// `r19-outside-manager-c2-upgrade-crashes-under-load-FIXED-20261012.md`).
///
/// The take used to be unconditional: whatever lay on top of the stash was
/// handed to the door as its body's trap. A frame that an EARLIER trap left
/// unclaimed -- a consumer that answered its trap without taking its frame --
/// then reached the next door on the thread whose body returned a bare
/// sentinel (a frameless trap, an owned-arguments hand-off), and was judged
/// there: a foreign stash with the whole-body replay rule, i.e. `InternalError:
/// precise deoptimization unavailable for java/util/Locale.forLanguageTag(..)
/// after a trap of another method (java/util/Locale.isUnicodeExtensionKey bci
/// 1) (door execute-first-call, ForeignStash)` in the loaded `R16LocaleWide`
/// stress (`forLanguageTag` cannot reach `isUnicodeExtensionKey`: only
/// `Locale.getUnicodeLocaleType` calls it, from the previous tag's
/// `DecimalFormatSymbols` / `NumberFormat` work). Left on the stash, the same
/// orphan is claimed BY NAME by a later call-site service whose callee has the
/// same name and parameters (`try_resume_trapped_callee`, which ignores the
/// class and the return type), and resumed as that callee's frame -- the
/// reading of the `0x7`-reference SIGSEGV of the same stress.
///
/// Now the door takes only a frame pushed after `mark`, read before the body
/// ran (`cratonvm_jit::deopt::take_last_deopt_with_point_pushed_after`). An
/// older frame describes nothing the body did: it was on the stash before the
/// body's first instruction, and every consumer that services a trap takes
/// the frame before it runs any Java, so no consumer outside this door can be
/// waiting for it either. It is discarded (counted with the stash's other
/// unclaimed discards, and named under `CRATONVM_DBG_DEOPT`), and the door
/// answers the sentinel as the bare one it is: the deopt flag's re-run, judged
/// by the trap site the thread recorded, or the historical re-run of a
/// non-wide result.
///
/// Off (`CRATONVM_JIT_DOOR_STASH_SINCE_ENTRY=0`): the unconditional take.
pub(in crate::runtime::interpreter::jit_bridge) fn take_door_stash_since_entry(
    mark: u64,
    name: JitBodyName<'_>,
) -> Option<(
    cratonvm_jit::deopt::ReconstructedFrame,
    Option<cratonvm_jit::deopt::DeoptCause>,
    usize,
)> {
    if !door_stash_since_entry_enabled() {
        return cratonvm_jit::deopt::take_last_deopt_with_point();
    }
    let (taken, discarded) = cratonvm_jit::deopt::take_last_deopt_with_point_pushed_after(mark);
    if discarded > 0 && dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] door sentinel arm of {}.{}{}: {discarded} stashed frame(s) older \
             than the body's entry discarded, not claimed",
            name.class_name, name.method_name, name.descriptor,
        );
    }
    taken
}



/// The body that really ran a door's call and trapped, when it is not the
/// body the door entered (HANDLER11-2).
///
/// A door checks [`door_declines_not_entrant_body`] BEFORE it enters, and the
/// tiered manager's C1 -> C2 supersede (`supersede_retire_superseded`, hm5),
/// a late-bind retire or a trap eviction can make the body not entrant on
/// another thread between that check and the call. The entry then runs the
/// not-entrant stub, which FORWARDS to the method's current body; when that
/// body traps, its stash names a point the door's `compiled` does not own,
/// and every sink arm judged it a foreign frame: the frame is not resumed,
/// the method itself is charged as the frame's "real owner", and the door
/// re-runs it from entry -- or, when the whole-body replay rule says the
/// re-run commits twice, raises `InternalError: precise deoptimization
/// unavailable ... ForeignStash` (the wave-17 `R16CallbindUnbox` signature,
/// back as a race). `CRATONVM_JIT_OUTSIDE_MANAGER_C2_UPGRADE` multiplies
/// these supersedes (dozens of C1 eager callees promoted while running).
///
/// `Some` only when all of these hold, so that the answer is exactly the body
/// the stub forwarded to: the switch is on; `compiled` is not entrant (it is
/// never made entrant again, so a body that was forwarded from is not entrant
/// now) and does not own `point`; the take named its trapping body
/// (`cratonvm_jit::deopt::taken_trapping_body`, which also checks that the
/// body owns the point); and that body is another artifact published under
/// `compiled`'s own key (`CompiledMethod::published_as_same_method`), which is
/// the only body the stub forwards to (`jit_not_entrant_entry_body`'s
/// `jit_cache.get` of the record's names). The door then judges, resumes and
/// charges the stash with that body's facts, as it would had it entered that
/// body itself. Released through `RetainedCode` (it may become the last owner).
pub(in crate::runtime::interpreter) fn forwarded_trap_body(
    compiled: &crate::jit::CompiledMethod,
    point: usize,
) -> Option<cratonvm_jit::RetainedCode> {
    if point == 0 || !compiled.is_not_entrant() || compiled.owns_deopt_point(point) {
        return None;
    }
    // j27 wave 7 (lane T1LAM3): a `RetainedCode` from the first moment, so
    // the refused case releases through the queue too.
    let body = crate::jit::helpers::stale_trap_not_entrant::taken_trapping_body_retained(point)?;
    let same = body.artifact_id != compiled.artifact_id && compiled.published_as_same_method(&body);
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] door entered not-entrant {} id={} and took a trap of id={} \
             ({}): {}",
            compiled.method_label,
            compiled.compile_id,
            body.compile_id,
            body.method_label,
            if same {
                "the body its stub forwarded to; judged with that body"
            } else {
                "another method's body; judged as before"
            },
        );
    }
    same.then_some(body)
}

/// Build the implicit exception a compiled body signalled out of band (NPE,
/// then AIOOBE, then `/ by zero` — a frame cannot have two pending at once)
/// and hand it back for routing through the body's own exception table.
///
/// Every arm synthesizes a FRESH exception and consumes no stashed deopt
/// frame, so any stash is dropped first: left in place, the next
/// `take_last_deopt` on this thread — another method's sink — would claim it
/// and resume a frame that is not its own.
///
/// The drop releases no lock a dropped frame names, deliberately (interpreter
/// round i1 wave 24, lane L2; `CompiledLocksOfAStash`). This activation is not
/// abandoned here: the exception is routed through its own table next, and the
/// reason-9 frame that route takes (`route_jit_signal_exception`) is the one
/// that describes the locks the activation holds at the throw — it hands them
/// to the handler frame or releases them when the exception leaves. A dropped
/// `LAST_DEOPT` frame naming the same activation would release them a second
/// time, and one naming an older, already-handled trap describes holds whose
/// fate that trap's own sink decided.
///
/// The compiled frames the trap fired in have already left the stack (the
/// stub called its helper, loaded the `i64::MIN` sentinel and ran the
/// epilogue), so `fillInStackTrace` sees only what the interpreter still
/// holds. `sig.trap_frames` is the snapshot the helper took while they were
/// live; it is attached here, and draining it also keeps it from being handed
/// to the next, unrelated throwable.
///
/// `ArithmeticException` is thrown here rather than by re-running the method
/// from entry (the prior `uncommon_trap` behaviour, a HotSpot divergence that
/// double-executed any side effect preceding the trap).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn implicit_exception_from_signals(
    shared: &SharedVm,
    thread: &mut JvmThread,
    mut sig: crate::jit::helpers::DrainedJitSignals,
) -> Result<RawJitBodyOutcome, MethodCallFailed> {
    let _ = cratonvm_jit::deopt::take_last_deopt();
    let trap_snapshot = sig.trap_frames.take();
    // Round 12 wave 4 (lane exc2 patch page
    // `r12w4-exc2-jit-bridge-implicit-door-one-walk-patch`): build the throwable
    // with the trap snapshot armed as the compiled half of its capture, so the
    // construction walks once and nothing is spliced afterwards
    // (`create_exception_object_with_trap_frames`; it splices as before when the
    // snapshot cannot stand in, or under `CRATONVM_JIT_TRAP_CAPTURE_ONE_WALK=0`).
    // The debug dumps of `throw_runtime_error` keep the old route.
    if crate::runtime::exceptions::dbg_rterr_or_npe_none_dumps_enabled() {
        return implicit_exception_from_signals_via_runtime_error(
            shared,
            thread,
            sig,
            trap_snapshot,
        );
    }
    use crate::runtime::exceptions::{
        create_exception_object_with_trap_frames, try_fast_throw, FastThrowKind,
    };
    let (kind, message) = if sig.npe {
        // See `super::jit_npe_message`: the trapping method's own bytecode
        // first, the action-only string as the fallback. An empty message is
        // "none", as `RuntimeError::as_java_throwable` maps it.
        (FastThrowKind::NullPointer, None)
    } else if sig.aioobe.is_some() {
        (FastThrowKind::ArrayIndexOutOfBounds, None)
    } else {
        (FastThrowKind::Arithmetic, Some("/ by zero".to_string()))
    };
    let site = crate::runtime::exceptions::fast_throw_site_of(trap_snapshot.as_deref());
    let exc = match try_fast_throw(shared, thread, kind, site.as_ref()) {
        Some(exc) => exc,
        None => {
            // Round 15 wave 7 (lane throw2, TH1-5): the NPE through
            // `create_compiled_implicit_npe`, which builds this message as
            // before or defers it to `getMessage()` under `--real-jdk`.
            let created = if kind == FastThrowKind::NullPointer {
                crate::runtime::exceptions::create_compiled_implicit_npe(
                    shared,
                    thread,
                    trap_snapshot,
                    sig.npe_action,
                )
            } else {
                let message = match (kind, sig.aioobe) {
                    (FastThrowKind::ArrayIndexOutOfBounds, Some((index, length))) => Some(
                        cratonvm_types::error::out_of_bounds_message::check_index(index, length),
                    ),
                    _ => message,
                };
                create_exception_object_with_trap_frames(
                    shared,
                    thread,
                    kind.class_name(),
                    message.as_deref(),
                    trap_snapshot,
                )
            };
            match created {
                Ok(exc) => exc,
                Err((err, _)) => {
                    // As `throw_runtime_error` does for the NPE and `/ by zero`
                    // throwables: a heap too full to build one throws the
                    // preallocated `OutOfMemoryError`.
                    if kind != FastThrowKind::ArrayIndexOutOfBounds
                        && matches!(
                            &err,
                            MethodCallFailed::InternalError(VmError::Runtime(
                                RuntimeError::OutOfMemoryError { .. }
                            ))
                        )
                    {
                        if let Some(oom) = *shared.mem.singleton_oom.read() {
                            return Ok(RawJitBodyOutcome::Throw {
                                exc: oom,
                                athrow_bci: sig.athrow_bci,
                            });
                        }
                    }
                    return Err(err);
                }
            }
        }
    };
    // The signal's stamped bci gives location-aware handler selection: a throw
    // outside every protected range propagates, one inside a range reaches only
    // that range's handler.
    Ok(RawJitBodyOutcome::Throw {
        exc,
        athrow_bci: sig.athrow_bci,
    })
}

/// The pre-wave-4 route of [`implicit_exception_from_signals`]: the throwable
/// through `throw_runtime_error` (so its opt-in debug dumps still fire), then
/// the trap snapshot spliced onto the stored trace.
pub(in crate::runtime::interpreter::jit_bridge) fn implicit_exception_from_signals_via_runtime_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sig: crate::jit::helpers::DrainedJitSignals,
    trap_snapshot: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
) -> Result<RawJitBodyOutcome, MethodCallFailed> {
    let site = crate::runtime::exceptions::fast_throw_site_of(trap_snapshot.as_deref());
    let exc = if sig.npe {
        let npe_action = sig.npe_action;
        match crate::runtime::exceptions::throw_implicit_runtime_error(
            shared,
            thread,
            crate::runtime::exceptions::FastThrowKind::NullPointer,
            site.as_ref(),
            || RuntimeError::NullPointerException {
                message: super::jit_npe_message::jit_npe_message(
                    shared,
                    trap_snapshot.as_deref(),
                    npe_action,
                ),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => exc,
            other => return Err(other),
        }
    } else if let Some((index, length)) = sig.aioobe {
        crate::runtime::exceptions::create_implicit_exception_object(
            shared,
            thread,
            crate::runtime::exceptions::FastThrowKind::ArrayIndexOutOfBounds,
            site.as_ref(),
            || cratonvm_types::error::out_of_bounds_message::check_index(index, length),
        )?
    } else {
        match crate::runtime::exceptions::throw_implicit_runtime_error(
            shared,
            thread,
            crate::runtime::exceptions::FastThrowKind::Arithmetic,
            site.as_ref(),
            || RuntimeError::ArithmeticException {
                message: "/ by zero".to_string(),
            },
        ) {
            MethodCallFailed::ExceptionThrown(exc) => exc,
            other => return Err(other),
        }
    };
    crate::runtime::exceptions::attach_snapshotted_trap_frames(
        shared,
        &thread.frames,
        exc,
        trap_snapshot,
    );
    Ok(RawJitBodyOutcome::Throw {
        exc,
        athrow_bci: sig.athrow_bci,
    })
}

/// Spend a stashed deopt frame: resume the trapping activation in the
/// interpreter when possible (the frame-pushing result is returned), otherwise
/// de-speculate whichever method the stash belongs to and return `None`, after
/// which the door re-runs the body from entry.
///
/// The one sink for all three doors (the one-shot door reaches it through
/// [`resume_deopted_body_at_point`]). As separate copies, one of them consumed the
/// stash without de-speculating, so a statically invoked method that could not
/// resume trapped and re-ran from entry on EVERY call, its artifact never
/// evicted and never escalated.
///
/// `point_addr` is the address of the point that stashed `rframe` (0 when the
/// taker did not keep it). Only a foreign frame's charge reads it.
///
/// `Err` (round 12 wave 5, lane replay3) is the trapped method's own outcome
/// when its scalar-replaced objects could not be re-materialised in a full
/// heap: an `OutOfMemoryError` the door propagates to its caller, as HotSpot
/// pops a frame whose reallocation failed. The door must not re-run then.
///
/// Every `Ok(None)` abandons `rframe` for a re-run from entry, so the locks its
/// compiled code took (`relock == false`) are released first; a resumed frame
/// keeps them (interpreter round i1 wave 23, lane L2;
/// [`super::deopt_resume::CompiledLocksOfAStash`]). An `Err` abandons it too,
/// but the frame build released those locks already. The door's own
/// `ACC_SYNCHRONIZED` hold is not in the frame and stays the door's.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn resume_or_despeculate_stash(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
    sink_label: &str,
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    // Pinned before anything below can allocate: the refusal arms run after
    // the materialiser and the pool refill, either of which can collect.
    let locks = super::deopt_resume::CompiledLocksOfAStash::pin_compiled_locks(thread, rframe);
    let resumed = resume_or_despeculate_stash_inner(
        shared, thread, compiled, cached, rframe, cause, point_addr, sink_label,
    );
    match &resumed {
        Ok(Some(_)) => locks.unpin_keeping_the_locks(thread),
        Ok(None) => {
            locks.release_for_a_rerun(shared, thread);
        }
        // The heap-failure answer: `build_deopt_frame_or_refusal` (asked with
        // `release_held_on_heap_failure`) already released the frame's
        // compiled locks through its own pins; releasing them again here could
        // drop a recursive hold an outer frame owns on the same object. Only
        // the pins go.
        Err(_) => locks.unpin_keeping_the_locks(thread),
    }
    resumed
}

/// The bytecode a stash of `class_name.method_name descriptor` must be
/// resumed in, and its constant-pool generation, when the stash is the exit
/// the safepoint verdict granted a METHOD-ENTRY body forced to leave as its
/// class's obsolete activation after a renumbering redefinition
/// ([`super::granted_obsolete_source`]; interpreter round i1 wave 46, lane
/// L2, `cratonvm_jit::not_entrant::OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`),
/// the grant's source names that method, and a frame of it stamped with that
/// generation can still be moved onto the class's current pool
/// (`obsolete_frames::rebuilt_body_translates`, asked again here: a later
/// redefinition may have landed since the verdict). The sink then rebuilds
/// the frame in that bytecode and restamps it with that generation; the
/// frame is translated before its first bytecode and runs the rest of the
/// old activation interpreted, against the old constants.
///
/// `None` (the sink keeps its own rules) for every other stash, and at once
/// while no class of the process was ever redefined. Off the hot path: only
/// a sink that holds a stash asks.
pub(crate) fn granted_obsolete_activation_source(
    shared: &SharedVm,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(Arc<CachedBytecodeMethod>, u64)> {
    if !crate::classloading::any_class_redefined() {
        return None;
    }
    let (source, stamp) = super::granted_obsolete_source(shared, cause, point_addr)?;
    if !obsolete_entry_resume_switches_on() {
        return None;
    }
    if *source.class_name != *class_name
        || *source.method_name != *method_name
        || *source.method_descriptor != *descriptor
    {
        return None;
    }
    let translates = super::obsolete_frames::rebuilt_body_translates(
        shared,
        source.declaring_class_id,
        &source.code,
        &source.exception_table,
        stamp,
    );
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] {class_name}.{method_name}:{descriptor}: a renumbered obsolete \
             activation's exit, rebuilt in the bytecode it was compiled from \
             (stamp {stamp}, translates={translates})"
        );
    }
    translates.then_some((source, stamp))
}

/// Are the kill switches a renumbered obsolete activation's resume depends
/// on all on (their default)? `cratonvm_jit::deopt_sink_resume_enabled`,
/// `CRATONVM_DEOPT_RESUME_OBSOLETE_ACTIVATION`
/// (the sinks' own-source resume, `deopt_resume::obsolete_activation_source`)
/// and `CRATONVM_DEOPT_TIERUP_SINK_CP_STAMP` (the restamp
/// `execute_rebuilt_frame_releasing_pins` gives the call-site service's
/// rebuilt frame; without it the frame would read the new pool with its old
/// indices). The safepoint verdict asks it too, so a body is never told to
/// leave for a sink that would then refuse it (interpreter round i1 wave 46,
/// lane L2). Read only on a forced body's slow path and in the sinks.
pub(crate) fn obsolete_entry_resume_switches_on() -> bool {
    // And the sinks' additive precise-resume arm the interpreter doors' gate
    // asks (`door_precise_resume_allowed`).
    cratonvm_jit::deopt_sink_resume_enabled()
}

/// [`resume_or_despeculate_stash`] without the compiled-lock bookkeeping.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn resume_or_despeculate_stash_inner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
    sink_label: &str,
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    dbg_deopt_sink(sink_label, rframe, "");
    // Wave 10 (L4): a point this body does not own was fired by another body,
    // possibly one with the same name (another loader's copy, a nested
    // activation). Its frame is never resumed as this activation; it is
    // charged as a foreign frame, and the door re-runs, as for a name mismatch.
    if !super::deopt_resume::deopt_stash_is_from_artifact(compiled, point_addr) {
        despeculate_stashed_frame_method_with_cause(shared, rframe, cause, point_addr);
        return Ok(None);
    }
    if ir_deopt_resume_enabled() {
        if let Some(r) = resume_from_ir_deopt(shared, thread, cached, rframe) {
            // Round 15 wave 4 (lane chain5, chain4's patch page): a forced
            // chain exit, resumed here (census); left uncharged under
            // `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED`.
            let forced = super::deopt_resume::note_forced_chain_trip_outcome(
                shared,
                compiled,
                point_addr,
                !rframe.caller_frames.is_empty(),
            );
            // Charged like every other arm (interpreter round i1 wave 11, L4):
            // this resume used to return uncharged, so under
            // `CRATONVM_IR_DEOPT_RESUME` a failing guard was never recorded,
            // evicted or de-spec'd and trapped again on every call.
            if !super::deopt_resume::leave_forced_trip_uncharged(shared, forced) {
                charge_own_stashed_trap(shared, compiled, cached, rframe, cause);
            }
            return Ok(Some(r));
        }
    }
    // real-frame-deopt Step 4 / x64-backport Step 5: RESUME the deopt at the
    // trapping bci — build the interpreter frame (GC-rooting the oops across
    // the pool refill AND the push handoff), push it, and resume there instead
    // of re-running the whole method from entry — when the artifact's
    // per-method coverage flag `can_deopt_resume` allows it (deopt snapshots
    // present AND no scalar replacement). jit-invokedynamic-groovy-regression:
    // a frame-stashing deopt (e.g. the unconditional invokedynamic reason-8
    // trap) otherwise double-executes every side effect committed before it.
    //
    // ADDITIVE second arm: `can_deopt_resume` is false on every
    // optimizing-tier artifact in a production build, so without it this sink
    // fell through to the whole-method re-run and ran any side effect the
    // compiled body had ALREADY committed a second time, silently. See
    // `sink_precise_resume_allowed`.
    //
    // deopt-osr Step 9: `real_frame_deopt_resume_and_despeculate` owns the
    // identity/epoch checks and the de-speculation (record, evict, escalate to
    // not-entrant / not-compilable, advance the live epoch); any refusal falls
    // through to the safe re-run.
    // Interpreter round i1 wave 46, lane L2: the exit of a method-entry body
    // forced to leave as its class's obsolete activation is resumed in the
    // bytecode it was compiled from, so the gate asks that bytecode when the
    // door's own template refuses (the door may hold the class's NEW body).
    if (cratonvm_jit::deopt_real_enabled() && compiled.can_deopt_resume)
        || door_precise_resume_allowed(cached, rframe)
        || granted_obsolete_activation_source(
            shared,
            cause,
            point_addr,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
        .is_some_and(|(source, _)| door_precise_resume_allowed(&source, rframe))
    {
        if let Some(r) = super::deopt_resume::real_frame_deopt_resume_or_throw_and_despeculate(
            shared, thread, compiled, cached, rframe, cause, point_addr,
        )? {
            return Ok(Some(r));
        }
    } else if !rframe.method_key.is_empty()
        && !deopt_frame_matches_method(
            // An inlined chain is `cached`'s own when its OUTERMOST scope
            // names it (wave 26, lane L2).
            super::deopt_resume::stash_identity_scope(rframe),
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        )
    {
        // Resume unavailable and the stash names a nested callee: de-speculate
        // the frame's real owner so it stops re-trapping; `cached` is innocent.
        despeculate_stashed_frame_method_with_cause(shared, rframe, cause, point_addr);
    } else {
        // Round 15 wave 4 (lane chain5, chain4's patch page): a forced chain
        // exit this door re-runs is claimed (forced, not resumed) and left
        // uncharged under `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED`. (The
        // foreign-frame branch above claims inside its despeculation.)
        let forced = super::deopt_resume::note_forced_chain_trip_outcome(
            shared, compiled, point_addr, false,
        );
        // Resume unavailable (e.g. a synchronized method, `can_deopt_resume`
        // false, gate off): still charge the trap, so an `UnreachedCode`
        // (reason 8) trap reached through this call site gets blacklisted
        // instead of re-triggering on every call.
        if !super::deopt_resume::leave_forced_trip_uncharged(shared, forced) {
            charge_own_stashed_trap(shared, compiled, cached, rframe, cause);
        }
    }
    Ok(None)
}

/// The additive resume arm as the doors ask it (round 12 wave 7, lane
/// replay4): [`sink_precise_resume_allowed_for`] without its
/// `ACC_SYNCHRONIZED` refusal. That refusal says "a resumed frame would not
/// hold the method monitor", which is false at every caller of
/// [`resume_or_despeculate_stash`]: the two frame-pushing doors and the
/// door-locked static door hand their hold to the resumed frame
/// (`hand_monitor_to_resumed_frame`, pinned by
/// `a_synchronized_body_is_entered_only_by_doors_that_hand_over_its_monitor`),
/// and the one-shot door runs the frame to completion under a hold that
/// outlives it (it declines synchronized methods anyway). So a synchronized
/// body the backend did not vouch for (`can_deopt_resume` false, or
/// `CRATONVM_DEOPT_REAL=0`) resumes here instead of re-running from entry,
/// which since wave 7 is refused with `InternalError` when the replay would
/// commit again. It also makes the single-pass post-check exact: its
/// `sink_resumes` is this gate "minus its `ACC_SYNCHRONIZED` term, which this
/// backend does not see" (`x64/driver.rs`). Kill switch
/// `CRATONVM_JIT_DOOR_SYNC_ADDITIVE_RESUME=0`.
pub(in crate::runtime::interpreter::jit_bridge) fn door_precise_resume_allowed(
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
) -> bool {
    if cached.is_synchronized && door_sync_additive_resume() {
        return super::deopt_resume::sink_precise_resume_allowed(
            &cached.code,
            cached.code.len(),
            false,
            super::deopt_resume::stash_identity_scope(rframe).bci,
        );
    }
    sink_precise_resume_allowed_for(cached, rframe)
}

/// `CRATONVM_JIT_DOOR_SYNC_ADDITIVE_RESUME`, **default ON** (round 12 wave 7,
/// lane replay4); see [`door_precise_resume_allowed`]. Not memoised: read
/// only for a synchronized body's stash the first arm did not resume.
pub(in crate::runtime::interpreter) fn door_sync_additive_resume() -> bool {
    true
}

/// Charge a trap `compiled` itself fired (the stash is `cached`'s own) to
/// `cached`, unless a newer body is already published: charging then would
/// evict that body by name and count the retired body's speculation failure a
/// second time. The reason comes from the deopt point matching this bci
/// (falling back to `UnreachedCode`) — the same attribution the frame-resuming
/// arm charges under (it used to be a different helper here than there, so
/// one trap had two reasons depending on the arm).
pub(in crate::runtime::interpreter::jit_bridge) fn charge_own_stashed_trap(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) {
    if super::deopt_resume::trapped_artifact_is_superseded(shared, cached, compiled) {
        return;
    }
    // Interpreter round i1 wave 15, lane L3: an exit the interpreter-only
    // mode asked for is not a failed speculation
    // (`jvmti_events::exit_left_for_the_interpreter`).
    if cause.is_some_and(|c| {
        super::exit_left_for_the_interpreter(
            shared,
            c.reason,
            cached.declaring_class_id,
            &cached.method_name,
            &cached.method_descriptor,
        )
    }) {
        return;
    }
    // JIT round 16 (lane indy, IB-3): the artifact's own IR SITE TRAP goes
    // through the site-trap policy (memo, IR ban, recompile), not through a
    // charge under the plain resume point's `TransferToInterpreter`. See
    // `deopt_resume::stash_is_own_ir_site_trap`.
    if super::deopt_resume::stash_is_own_ir_site_trap(compiled, rframe) {
        crate::jit::helpers::despeculate_trapped_method(
            shared,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
            cratonvm_jit::deopt::DeoptReason::UncommonTrap,
            rframe.bci,
        );
        return;
    }
    // At the site in `cached`'s own bytecode: an inlined chain's trap is
    // charged at its call site (wave 26, lane L2; `stash_charge_site`). The
    // attribution is the resuming arm's own (nio21 wave 7, lane L2:
    // `stash_trap_attribution`, whose reason and site equal
    // `stash_charge_site`'s), so the per-bci de-spec below withdraws what
    // that arm would have withdrawn.
    let attribution =
        super::deopt_resume::stash_trap_attribution(compiled, &cached.code, rframe, cause);
    let _ = crate::jit::helpers::DeoptimizationController::deoptimize_in(
        shared,
        cached.declaring_class_id,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        attribution.charge_reason(),
        attribution.charge_bci,
    );
    // Round nio21 wave 7 (lane L2,
    // `r21-charat-trap-despec-skips-the-rerun-sinks-20261008`): the per-bci
    // de-spec the resuming arm records after its charge. Without it a
    // speculation failing on this arm (a door that re-runs, or the opt-in
    // `CRATONVM_IR_DEOPT_RESUME` arm) was planted again by every recompile
    // until the aggregate count made the whole method not compilable. Kill
    // switch `CRATONVM_JIT_RERUN_SINK_PER_BCI_DESPEC=0`.
    if super::deopt_resume::rerun_sink_per_bci_despec_enabled() {
        let method_key = format!(
            "{}.{}:{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
        let _ = super::deopt_resume::despec_own_stash_site_after_charge(
            shared,
            &method_key,
            rframe,
            &attribution,
        );
    }
}

/// A method-entry door is about to re-run `cached` from entry because its
/// compiled body stopped short of a return and could not be resumed: count it
/// by cause and by the replay rule's verdict, name it under
/// `CRATONVM_DBG_DEOPT`, and when the re-run commits again what the attempt
/// had committed, retire the method from compilation (round 12 wave 4, lane
/// replay2; `r12w3-replay-jit-bridge-doors-rerun-without-the-replay-rule`).
///
/// At this point there is no correct answer left: the frame cannot be rebuilt
/// and the attempt ran past bci 0. The compile-time checks keep such bodies
/// from being installed (`deopt::first_point_needing_unsound_replay`, and its
/// single-pass twin); this is the census that proves it and the backstop that
/// makes the double commit happen at most once per method instead of on every
/// call until the trap ledger escalates. `stash` is the body's stashed frame
/// with its point address, `None` for the frameless sentinel.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn note_door_rerun_from_entry(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    cached: &CachedBytecodeMethod,
    stash: Option<(&cratonvm_jit::deopt::ReconstructedFrame, usize)>,
    door: &str,
) -> (super::deopt_resume::DoorRerunCause, bool) {
    note_door_rerun_of_method(
        shared,
        compiled,
        &cached.code,
        DoorMethod {
            class_id: cached.declaring_class_id,
            class_name: &cached.class_name,
            method_name: &cached.method_name,
            descriptor: &cached.method_descriptor,
        },
        stash,
        door,
    )
}

/// A method-entry door's stashed arm, once the stash could not be resumed:
/// count the re-run ([`note_door_rerun_from_entry`]) and decide whether the
/// door may take it (round 12 wave 7, lane replay4;
/// `r12w3-replay-jit-bridge-doors-rerun-without-the-replay-rule`).
///
/// `Err` is the interpreter tier-up sink's answer to the same verdict
/// (`execute-first-call-tierup`, "refusing side-effecting replay"), as a Java
/// `InternalError`: the stash is the body's OWN frame, the replay rule says a
/// re-run from entry commits again what this attempt already committed, and
/// no resume arm could rebuild the frame. The door must return it instead of
/// re-running. The method is retired first, as before, so the answer is given
/// at most once per method. Until wave 7 the doors re-ran silently, the one
/// sink family that did.
///
/// Still re-run (`Ok`), as the tier-up sink re-runs them:
///
/// * a sound verdict (nothing would commit twice);
/// * a body of a REDEFINED class, whose frame the resume declined because it
///   is not the class's current code
///   (`r12w5-replay3-bytecode-identity-on-the-artifact-patch`, W4-2): a
///   hot-swapped program keeps the re-run it always had;
/// * `CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES=0` (own stash) /
///   `CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES=0` (foreign stash).
///
/// Round 13 wave 8 (lane replay4): a FOREIGN stash whose re-run is unsound is
/// refused too (`deopt_resume::unsound_rerun_refused`). It used to re-run on
/// the ground that its verdict is the whole-body rule; but the attempt it
/// abandons called the compiled callee that trapped, so the re-run calls it
/// again, and "the whole body commits something" is then exactly the double
/// commit.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn door_rerun_or_refuse(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &CachedBytecodeMethod,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    point: usize,
    door: &str,
) -> Result<(), MethodCallFailed> {
    let (cause, sound) =
        note_door_rerun_from_entry(shared, compiled, cached, Some((rframe, point)), door);
    // Round 13 wave 5 (lane resume; R13-2): the one "own stash, no resume,
    // replay unsound" predicate, shared with the lambda direct arm. An
    // inlined chain whose spliced callee's class was redefined since the
    // compile was not resumed for that reason, and re-runs as a redefined
    // class's frame does. Round 13 wave 8 (lane replay4): every cause, so a
    // FOREIGN stash whose re-run is unsound is refused too
    // (`CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES`).
    if !super::deopt_resume::unsound_rerun_refused(cause, sound, || {
        crate::runtime::redefine_state::class_was_redefined(shared, cached.declaring_class_id)
            || super::deopt_resume::chain_inner_scope_redefined_since_compile(
                shared,
                compiled,
                cached.declaring_class_id,
                rframe,
            )
    }) {
        return Ok(());
    }
    Err(refuse_door_rerun(
        shared,
        thread,
        cached,
        cause,
        &format!("at bci {}", rframe.bci),
        door,
    ))
}

/// A method-entry door's frameless arm (the bare sentinel, no stashed frame):
/// count the re-run ([`note_door_rerun_from_entry`]) and decide whether the
/// door may take it (round 13 wave 8, lane replay4). `Err` is
/// [`door_rerun_or_refuse`]'s answer: the replay rule, asked of the trap site
/// the thread recorded (`deopt_resume::frameless_rerun_is_exact`), says a
/// re-run commits again what the attempt committed. Until this wave the four
/// frameless arms re-ran unconditionally, counted.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn door_frameless_rerun_or_refuse(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &CachedBytecodeMethod,
    door: &str,
) -> Result<(), MethodCallFailed> {
    let (cause, sound) = note_door_rerun_from_entry(shared, compiled, cached, None, door);
    if !super::deopt_resume::unsound_rerun_refused(cause, sound, || {
        crate::runtime::redefine_state::class_was_redefined(shared, cached.declaring_class_id)
    }) {
        return Ok(());
    }
    let at = match crate::jit::helpers::last_frameless_trap_site() {
        crate::jit::helpers::FramelessTrapSite::Stub { bci, .. } => {
            format!("after a frameless trap at bci {bci}")
        }
        site => format!("after a trap with no frame ({site:?})"),
    };
    Err(refuse_door_rerun(shared, thread, cached, cause, &at, door))
}

/// The refusal [`door_rerun_or_refuse`] and [`door_frameless_rerun_or_refuse`]
/// raise: the census moves the re-run it counted to the `refused` row, and
/// the caller gets a Java `InternalError` to propagate instead of re-running.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn refuse_door_rerun(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: &CachedBytecodeMethod,
    cause: super::deopt_resume::DoorRerunCause,
    at: &str,
    door: &str,
) -> MethodCallFailed {
    super::deopt_resume::note_door_rerun_refused(cause);
    let message = format!(
        "precise deoptimization unavailable for {}.{}{} {at} (door {door}, {cause:?}: the \
         frame could not be rebuilt); refusing side-effecting replay",
        cached.class_name, cached.method_name, cached.method_descriptor,
    );
    crate::runtime::exceptions::throw_runtime_error(
        shared,
        thread,
        RuntimeError::InternalError { message },
    )
}

/// The method a door is about to re-run, for [`note_door_rerun_of_method`].
pub(in crate::runtime::interpreter) struct DoorMethod<'a> {
    pub(in crate::runtime::interpreter) class_id: ClassId,
    pub(in crate::runtime::interpreter) class_name: &'a str,
    pub(in crate::runtime::interpreter) method_name: &'a str,
    pub(in crate::runtime::interpreter) descriptor: &'a str,
}

/// [`note_door_rerun_from_entry`] for a door that holds the method's padded
/// bytecode and identity but no `CachedBytecodeMethod` (`execute`'s first-call
/// door in `interpreter.rs`).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn note_door_rerun_of_method(
    shared: &SharedVm,
    compiled: &crate::jit::CompiledMethod,
    code: &[u8],
    method: DoorMethod<'_>,
    stash: Option<(&cratonvm_jit::deopt::ReconstructedFrame, usize)>,
    door: &str,
) -> (super::deopt_resume::DoorRerunCause, bool) {
    let (cause, sound) = super::deopt_resume::door_rerun_verdict(
        code,
        compiled,
        method.class_name,
        method.method_name,
        method.descriptor,
        stash,
    );
    super::deopt_resume::note_door_rerun(cause, sound);
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] door {door} re-runs {}.{}{} from entry: cause={cause:?} \
             replay={} (stash {:?}); census {:?}",
            method.class_name,
            method.method_name,
            method.descriptor,
            if sound { "exact" } else { "UNSOUND" },
            stash.map(|(rframe, _)| (rframe.method_key.as_str(), rframe.bci)),
            super::deopt_resume::door_rerun_census(),
        );
    }
    if !sound {
        retire_method_after_unsound_rerun(shared, &method);
    }
    (cause, sound)
}



/// Evict `cached`'s compiled body and keep the method interpreted, the
/// `DeoptAction::MakeNotCompilable` outcome of
/// `DeoptimizationController::deoptimize_in` taken without waiting for the
/// trap ledger: the body just made a door commit a side effect twice, and a
/// recompile of the same bytecode can do it again.
pub(in crate::runtime::interpreter::jit_bridge) fn retire_method_after_unsound_rerun(shared: &SharedVm, method: &DoorMethod<'_>) {
    let _ = crate::jit::helpers::DeoptimizationController::deoptimize_in(
        shared,
        method.class_id,
        method.class_name,
        method.method_name,
        method.descriptor,
        cratonvm_jit::deopt::DeoptReason::UncommonTrap,
        u32::MAX,
    );
    shared.jit.jit_skip_set.write().insert((
        Arc::from(method.class_name),
        Arc::from(method.method_name),
        Arc::from(method.descriptor),
    ));
    shared.jit.runtime_despec.mark(
        method.class_id,
        method.class_name,
        method.method_name,
        method.descriptor,
    );
}

/// The lambda direct arm's twin of [`note_door_rerun_of_method`]'s retire
/// (round 13 wave 5, lane resume): an impl whose re-run from entry the
/// replay rule calls unsound is retired from compilation, whatever the
/// cause, as at the doors (`CRATONVM_JIT_DOOR_UNSOUND_RERUN_RETIRE`).
pub(crate) fn retire_lambda_impl_after_rerun(
    shared: &SharedVm,
    impl_method: &crate::classloading::resolution::CachedBytecodeMethod,
    sound: bool,
) {
    if sound {
        return;
    }
    retire_method_after_unsound_rerun(
        shared,
        &DoorMethod {
            class_id: impl_method.declaring_class_id,
            class_name: &impl_method.class_name,
            method_name: &impl_method.method_name,
            descriptor: &impl_method.method_descriptor,
        },
    );
}

/// The call-site services' twin of [`retire_lambda_impl_after_rerun`]
/// (round 13 wave 8, lane replay4): a callee whose unsound re-run a service
/// REFUSED is retired from compilation, as a door retires the method whose
/// re-run it refused, so the next exception or trap through the same callee
/// runs interpreted instead of raising again
/// (`CRATONVM_JIT_DOOR_UNSOUND_RERUN_RETIRE`).
pub(crate) fn retire_callee_after_refused_rerun(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    retire_method_after_unsound_rerun(
        shared,
        &DoorMethod {
            class_id,
            class_name,
            method_name,
            descriptor,
        },
    );
}

/// Push back the arguments [`execute_jit_call`] popped, so the caller's slow
/// path — which pops them AGAIN to re-run the call — finds the operand stack
/// exactly as it expects.
///
/// CRIT (BC SPHINCS-256 / SHA-512 underflow): without the restore the next
/// bytecode (`lastore`, etc.) popped a now-missing slot and panicked with a
/// value_stack underflow. The mark is restored as well as the bits: a
/// `KIND_DOUBLE` slot re-pushed as `KIND_UNKNOWN` would be re-read by its
/// NaN-box sub-tag on the slow path, which is how a double carrying a NaN
/// payload lost it across a deopt. References are re-read through their pins:
/// the compiled call may have moved them.
pub(in crate::runtime::interpreter::jit_bridge) fn restore_popped_jit_args(
    thread: &mut JvmThread,
    frame_idx: usize,
    saved_args: &[(CompactValue, u8)],
    arg_pins: &[usize],
) {
    for i in 0..saved_args.len() {
        let (cv, kind) = pinned_saved_arg(thread, saved_args, arg_pins, i);
        thread.frames[frame_idx]
            .stack
            .push_with_kind_unchecked(cv, kind);
    }
}

/// Must an interpreter->compiled door decline `compiled` because its entry was
/// made not entrant (`CompiledMethod::is_not_entrant`; JIT round 16 wave 17,
/// lane trapfix)? A door's body came out of a cache that can outlive the
/// body's publication -- the thread's invoke cache (`CachedInvokeTarget::Jit`,
/// pruned of withdrawn bodies only at tier-up strides and safepoints,
/// [`release_withdrawn_code_owners`]), a lambda or virtual memo -- and since
/// `CRATONVM_JIT_TRAP_EVICT_NOT_ENTRANT` (wave 15) every deoptimization that
/// evicts a body patches it. Entering such a body runs its not-entrant stub,
/// which FORWARDS to the method's current body when one is published: that
/// body's trap then stashes a point the door's `compiled` does not own, the
/// door judges the stash foreign (`deopt_resume::door_rerun_verdict`) and,
/// with no resume and an unsound whole-body replay, raises `InternalError:
/// precise deoptimization unavailable ... ForeignStash` (`R16CallbindUnbox`
/// `npe`, 3/3 on w15a). Declined before anything runs, the call takes the
/// door's ordinary interpreted fallback, which resolves the method afresh
/// (the current body through the door that owns it, or the interpreter).
///
/// Not switched: a not-entrant body is never published, so no door is meant
/// to enter it, whichever pass patched it (a redefinition, the VarHandle mint,
/// a late-bind retire or a deoptimization's eviction). One acquire load on the
/// door's path. The thread's invoke cache is pruned of withdrawn bodies on the
/// way out, so the site's next call re-resolves once instead of declining
/// every time until the next stride.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn door_declines_not_entrant_body(
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    door: &str,
) -> bool {
    if !compiled.is_not_entrant() {
        return false;
    }
    note_door_declined_not_entrant_body(thread, compiled, door);
    true
}

/// The cold half of [`door_declines_not_entrant_body`]: prune this thread's
/// withdrawn compiled targets, and name the decline under `CRATONVM_DBG_DEOPT`.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn note_door_declined_not_entrant_body(
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    door: &str,
) {
    release_withdrawn_code_owners(thread);
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] door {door} declined a not-entrant body: {} id={}",
            compiled.method_label, compiled.compile_id
        );
    }
}


// gci i13/oj (2026-10-13) -- oomjit10 item 1, the door promise
// (`docs/feature-designs/concurrent-gc-maturation.md`,
// opt-in `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE`, read once per thread).
//
// With the weak door arguments on (`WeakDoorArgs`), the last holder of a
// dropped argument was the outermost compiled frame's callee-saved image of a
// register the Rust door kept the argument in (`relay ... region=callee-saved-gpr-image
// reg=r13 caller=native claim=Keep`). `caller_claim_step` must keep such a
// word: rustc may hold an object there across the call. The shim below makes
// the door's promise explicit for one call:
//
//   1. It publishes, for the call, the argument words it handed over and the
//      chain index its `JitEntryGuard` is pushed at (`JvmThread::door_promise`);
//      the GC side drops a callee-saved image word at exactly that segment's
//      boundary iff it EQUALS one of them
//      (`conservative_roots::door_promise_claim`). Every other word of the
//      image stays `Keep`, and the remap (`remap_register_image_words`) still
//      rewrites a dropped word whose object moved: the claim takes away only
//      that word's contribution to liveness.
//   2. After the call the shim's extent (`run_jit_body` -> `run_jit_body_raw`
//      -> `try_call*`) reads the body's result and the thread's signal block,
//      never an argument: the argument values are dead in Rust once the call
//      is made. The doors (`execute_jit_call`, `execute_jit_call_decoded`,
//      `interpreter::execute`'s first-call door) read their arguments after
//      the call only through the re-pinned weak handles
//      (`WeakDoorArgs::repin`, `WeakExecuteArgs::restore`) and only off the
//      `Returned` arm -- the promise `WeakDoorArgs` already made.
//   3. A frame ABOVE a door that holds an unrooted copy of a handed argument
//      and relies on it to keep the object alive is exactly what
//      `WeakDoorArgs` itself already excludes (it releases the door's only
//      strong root of that argument): the claim is sound iff the weak door is,
//      with no assumption about how rustc allocates any register.
//
// gci i14/od: i13/oj ALSO zeroed RSI/RDI/R12-R15 in an `#[inline(never)]`
// shim (an `asm!` naming them as outputs), so rustc saved its callers' values
// of them in the shim's prologue, where no scan reads and no remap rewrites
// them. That made every NON-argument reference a Rust frame above kept in one
// of those registers unrooted for the call, which nothing proves safe (it is a
// property of rustc's register allocation in every frame above every door,
// up to the whole VM above `execute`). The zeroing is gone: the callers'
// values stay where rustc puts them, read and remapped exactly as on the
// default path, and the claim needs no register to be zeroed (point 1 decides
// by the word, not by where it came from), so it covers RBX too.

/// `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE` for `thread`, read once per thread
/// (gci i13/oj; default OFF). Allocates the thread's `door_promise` box on the
/// first read; reached only on a weak-args door (gci i14/od: the three doors,
/// `interpreter::execute`'s first-call door included).
pub(in crate::runtime::interpreter) fn door_promise_enabled(thread: &mut JvmThread) -> bool {
    thread
        .door_promise
        .get_or_insert_with(|| {
            Box::new(crate::threading::jvm_thread::DoorPromiseState {
                enabled: false,
                record: crate::threading::jvm_thread::DoorHandedArgWords::default(),
            })
        })
        .enabled
}

/// The record a promising door publishes: the address bits of every pinned
/// (reference, non-null) argument it hands over, in argument order, and the
/// chain index its entry guard will take (gci i13/oj).
pub(in crate::runtime::interpreter::jit_bridge) fn door_handed_record(
    args_jit: &[i64],
    arg_pins: &[usize],
    chain_depth: usize,
    weak_pin_slots: &[usize],
) -> crate::threading::jvm_thread::DoorHandedArgWords {
    let mut rec = crate::threading::jvm_thread::DoorHandedArgWords {
        active: true,
        chain_depth,
        ..Default::default()
    };
    for (&bits, &pin) in args_jit.iter().zip(arg_pins.iter()) {
        if pin == usize::MAX || bits == 0 {
            continue;
        }
        if rec.len == rec.words.len() {
            break;
        }
        // Cast: a reference argument's JIT-ABI word is its address bits.
        rec.words[rec.len] = bits as usize;
        rec.len += 1;
    }
    rec.weak_pin_len = weak_pin_slots.len().min(rec.weak_pin_slots.len());
    rec.weak_pin_slots[..rec.weak_pin_len].copy_from_slice(&weak_pin_slots[..rec.weak_pin_len]);
    rec
}

/// Publishes a promising door's record on its thread and puts the enclosing
/// door's record back on drop, on every path (gci i13/oj).
pub(in crate::runtime::interpreter::jit_bridge) struct DoorPromisePublication {
    pub(in crate::runtime::interpreter::jit_bridge) thread: *mut JvmThread,
    pub(in crate::runtime::interpreter::jit_bridge) prev: crate::threading::jvm_thread::DoorHandedArgWords,
}

impl DoorPromisePublication {
    pub(in crate::runtime::interpreter::jit_bridge) fn publish(
        thread: &mut JvmThread,
        record: crate::threading::jvm_thread::DoorHandedArgWords,
    ) -> Self {
        let state = thread.door_promise.get_or_insert_with(|| {
            Box::new(crate::threading::jvm_thread::DoorPromiseState {
                enabled: true,
                record: crate::threading::jvm_thread::DoorHandedArgWords::default(),
            })
        });
        let prev = std::mem::replace(&mut state.record, record);
        Self {
            thread: thread as *mut JvmThread,
            prev,
        }
    }
}

impl Drop for DoorPromisePublication {
    fn drop(&mut self) {
        // SAFETY: `thread` came from the exclusive `&mut JvmThread` of the
        // enclosing activation, which outlives this guard -- the same aliasing
        // discipline `JitArgPinGuard` relies on.
        let thread = unsafe { &mut *self.thread };
        if let Some(state) = thread.door_promise.as_deref_mut() {
            state.record = self.prev;
        }
    }
}

/// Publish the promising door's record for the compiled call it is about to
/// make: the handed-over reference words of `args_jit` (`arg_pins[i] !=
/// usize::MAX`) and the chain index the body's `JitEntryGuard` will take
/// (the current depth: `run_jit_body_raw` pushes exactly one entry). The
/// returned guard puts the enclosing record back (gci i13/oj; i14/od: one
/// helper for the three doors).
pub(in crate::runtime::interpreter::jit_bridge) fn publish_door_promise(
    thread: &mut JvmThread,
    args_jit: &[i64],
    arg_pins: &[usize],
) -> DoorPromisePublication {
    publish_door_promise_with_weak_pins(thread, args_jit, arg_pins, &[])
}

pub(in crate::runtime::interpreter::jit_bridge) fn publish_door_promise_with_weak_pins(
    thread: &mut JvmThread,
    args_jit: &[i64],
    arg_pins: &[usize],
    weak_pin_slots: &[usize],
) -> DoorPromisePublication {
    let record = door_handed_record(
        args_jit,
        arg_pins,
        crate::jit::conservative_roots::current_thread_jit_depth(),
        weak_pin_slots,
    );
    DoorPromisePublication::publish(thread, record)
}

/// The promising door's shim around [`run_jit_body`] (gci i13/oj; see the
/// section comment above), for `execute_jit_call` and (gci i14/od)
/// `execute_jit_call_decoded`. `arg_pins[i] != usize::MAX` marks a reference
/// argument the door handed over (its weak handle is `WeakDoorArgs`'s).
/// Registers are left to rustc (gci i14/od: no zeroing).
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn run_jit_body_promised(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &CachedBytecodeMethod,
    needs_heap: bool,
    args_jit: &[i64],
    arg_pins: &[usize],
    weak_pin_slots: &[usize],
) -> Result<JitBodyOutcome, MethodCallFailed> {
    let _published = publish_door_promise_with_weak_pins(
        thread,
        args_jit,
        arg_pins,
        weak_pin_slots,
    );
    run_jit_body(shared, thread, compiled, cached, needs_heap, args_jit)
}

/// The promising shim around [`run_jit_body_raw`] for `interpreter::execute`'s
/// first-call door (gci i14/od), whose weak arguments are a
/// [`WeakExecuteArgs`]: its `arg_pins` mark the handed-over references in
/// argument order, which is `args_jit`'s order.
pub(in crate::runtime::interpreter) fn run_jit_body_raw_promised(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    name: JitBodyName<'_>,
    needs_heap: bool,
    args_jit: &[i64],
    weak: &WeakExecuteArgs<'_>,
) -> Result<RawJitBodyOutcome, MethodCallFailed> {
    let _published = publish_door_promise_with_weak_pins(
        thread,
        args_jit,
        weak.handed_arg_pins(),
        weak.weak_dispatch_pin_slots(),
    );
    run_jit_body_raw(shared, thread, compiled, name, needs_heap, args_jit)
}



// A compiled continuation must transfer before a call or monitor can suspend it.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn vthread_body_admitted(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &CachedBytecodeMethod,
    can_push_frames: bool,
) -> bool {
    !thread.jdk_continuation.jdk()
        || (can_push_frames
            && compiled.vthread_boundary_guards
            && crate::jit::conservative_roots::current_thread_jit_depth() == 0)
        || crate::vm::jdk_continuation::vthread_body_cannot_yield(shared, thread, cached)
}

/// Execute a JIT-compiled method call: pop args, call native code, push result.
///
/// The stack-popping door, used by the static invoke caches. Returns
/// `CacheMiss` (with the operand stack as the caller left it) when the call
/// must run interpreted instead.
#[inline]
pub(in crate::runtime::interpreter) fn execute_jit_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    compiled: &crate::jit::CompiledMethod,
    num_params: u16,
    return_type: u8,
    needs_heap: bool,
    cached: &Arc<CachedBytecodeMethod>,
) -> Result<CachedCallResult, MethodCallFailed> {
    let needs_heap = jit_entry_needs_heap(compiled, needs_heap, cached, "execute_jit_call");
    let np = num_params as usize; // Widening: parameter count conversion
                                  // Both declines happen before a single argument is popped.
    if np > jit_abi_max_java_params(needs_heap) || jvmti_requires_interpreter_for(shared, cached) {
        return Ok(CachedCallResult::CacheMiss);
    }
    // Round 16 wave 17 (lane trapfix): and a body made not entrant.
    if door_declines_not_entrant_body(thread, compiled, "jit-callsite-a") {
        return Ok(CachedCallResult::CacheMiss);
    }
    if !vthread_body_admitted(shared, thread, compiled, cached, true) {
        return Ok(CachedCallResult::CacheMiss);
    }
    // Interpreter round i2 wave 56 (lane L6): a contended synchronized callee
    // declines here, before the pop, so the general path enters it frame
    // first. The receiver of an instance callee is the deepest argument slot.
    // A hold taken here is adopted below; nothing in between can return.
    let frame_first_hold = {
        let receiver = if cached.is_synchronized && !cached.is_static && np > 0 {
            let (cv, kind) = thread.frames[frame_idx].stack.peek_with_kind_at(np - 1);
            match decode_arg_kind_aware(cv, kind, b'L') {
                Value::Object(Some(obj)) => Some(obj),
                _ => None,
            }
        } else {
            None
        };
        match frame_first_compiled_door(shared, thread, compiled, cached, receiver) {
            Ok(hold) => hold,
            Err(()) => return Ok(CachedCallResult::CacheMiss),
        }
    };
    let mut jit_args = [0i64; JIT_ABI_MAX_JAVA_ARGS];
    // Pop raw slots directly and decode each by its *parameter descriptor*
    // (receiver slot = 'L' for instance methods):
    //
    //   * NaN-box leak: an `Int(11)` slot is encoded 0xFFFC_0000_0000_000B;
    //     `pop_raw().as_i64` would pass those tag bits as the value, so a JIT'd
    //     `int n` arrived as 0xFFFC_..._000B instead of 11 — forwarded to
    //     `alloc_array` it produced "young gen exhausted" (fannkuch n=11,
    //     FullStackBench `new boolean[100000]`).
    //   * Collision long: a `long` arg whose bit pattern collides with the
    //     NaN-tag int space (BC safegcd 0xFFFC_… accumulator) was decoded by an
    //     unconditional `to_value()` as `Value::Int`, truncating to the low 32
    //     bits. See bc-ec-mod-mododdinverse-investigation.md.
    let is_static = cached.is_static;
    // The raw popped slots (bit-exact + kind mark), restored by the
    // re-run arms below before the slow path re-pops the args.
    let mut saved_args: [(CompactValue, u8); JIT_ABI_MAX_JAVA_ARGS] =
        [(CompactValue::zero(), 0u8); JIT_ABI_MAX_JAVA_ARGS];
    // ONE forward scan, hoisted out of this per-argument loop.
    let param_tags = ParamTags::for_method(
        &cached,
        shared.config.param_tag_scan,
        shared.config.descriptor_facts,
    );
    // Reference arguments are rooted as they leave the operand stack; see
    // `JitArgPinGuard`, armed immediately after this loop.
    let args_pin_base = thread.native_pin_roots.len();
    let mut arg_pins = [usize::MAX; JIT_ABI_MAX_JAVA_ARGS];
    for i in (0..np).rev() {
        let (cv, kind) = thread.frames[frame_idx].stack.pop_with_kind_unchecked();
        saved_args[i] = (cv, kind);
        let desc_byte = if is_static {
            param_tags.get(&cached.method_descriptor, i)
        } else {
            param_tags.get_with_receiver(&cached.method_descriptor, i)
        };
        let v = decode_arg_kind_aware(cv, kind, desc_byte);
        // Interpreter round i7 wave 136, lane L2: no address in a primitive
        // argument ([`jit_abi_arg_kind_agrees`]; debug builds only).
        debug_assert!(
            jit_abi_arg_kind_agrees(&v, desc_byte),
            "interpreter-to-compiled door: argument {i} of {}.{}{} is {v:?} for tag {}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            desc_byte as char,
        );
        if let Value::Object(Some(obj)) = &v {
            arg_pins[i] = thread.native_pin_roots.len();
            thread.native_pin_roots.push(*obj);
        }
        jit_args[i] = jit_abi_arg_bits(&v);
    }

    let _arg_pin_guard = JitArgPinGuard {
        thread: thread as *mut JvmThread,
        base: args_pin_base,
    };
    let mut synchronized_args = door_holds_method_monitor(compiled, cached)
        .then(|| jit_saved_args_to_values(cached, &saved_args, np));
    let mut synchronized_monitor = if let Some(held) = frame_first_hold {
        Some(JitSynchronizedMonitorGuard::adopt_frame_first_hold(
            shared, thread, cached, held,
        ))
    } else if let Some(args) = synchronized_args.as_mut() {
        Some(JitSynchronizedMonitorGuard::acquire(
            shared, thread, cached, args,
        )?)
    } else {
        None
    };
    if synchronized_args.is_some() {
        // From the PINS, not from `synchronized_args`: those were decoded
        // before `acquire`, which can move them.
        reencode_pinned_jit_args(thread, &arg_pins[..np], &mut jit_args[..np]);
    }
    // gch h2/fj (d9d P3, opt-in `CRATONVM_JIT_INTERP_DOOR_ARGS_WEAK`): an
    // owning body's arguments are held weakly across the run (`WeakDoorArgs`);
    // never with a monitor held by this door.
    let weak_args = if synchronized_monitor.is_none()
        && synchronized_args.is_none()
        && shared.jit.jit_cache.interp_door_args_weak()
        && cratonvm_jit::callee_body_owns_its_arguments(compiled)
    {
        WeakDoorArgs::take(shared, thread, args_pin_base, &arg_pins[..np])
    } else {
        None
    };

    if crate::runtime::env_cache::dbg_asserteq()
        && cached.class_name.as_ref() == "junit/framework/Assert"
        && cached.method_name.as_ref() == "assertEquals"
    {
        let mut buf = String::new();
        for (i, a) in jit_args[..np].iter().enumerate() {
            buf.push_str(&format!(" a{}=0x{:x}", i, a));
        }
        eprintln!(
            "[ASSERTEQ-ENTER] {}{} np={} needs_heap={}{}",
            cached.method_name, cached.method_descriptor, np, needs_heap, buf
        );
    }

    // gci i13/oj (oomjit10 item 1, opt-in `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE`):
    // a door that holds its arguments weakly promises, through its shim, not
    // to keep them in a register the compiled body's image could save
    // (`run_jit_body_promised`).
    let outcome = if weak_args.is_some() && door_promise_enabled(thread) {
        run_jit_body_promised(
            shared,
            thread,
            compiled,
            cached,
            needs_heap,
            &jit_args[..np],
            &arg_pins[..np],
            weak_args
                .as_ref()
                .map(|weak| weak.weak_dispatch_pin_slots.as_slice())
                .unwrap_or(&[]),
        )?
    } else {
        run_jit_body(
            shared,
            thread,
            compiled,
            cached,
            needs_heap,
            &jit_args[..np],
        )?
    };
    // gch h2/fj: the weak arguments back into the pin window for the readers
    // below (none after a return), a collected one nulled in `saved_args`.
    let collected_arg = match &weak_args {
        Some(weak) if !matches!(&outcome, JitBodyOutcome::Returned(_)) => {
            let cleared = weak.repin(thread, &mut arg_pins[..np]);
            for &i in &cleared {
                if let Some(slot) = saved_args.get_mut(i) {
                    slot.0 = CompactValue::null();
                }
            }
            !cleared.is_empty()
        }
        _ => false,
    };
    drop(weak_args);
    match outcome {
        JitBodyOutcome::Returned(result) => {
            if let Some(v) = jit_return_value(return_type, result) {
                thread.frames[frame_idx].stack.push_unchecked(v);
            }
            Ok(CachedCallResult::Handled)
        }
        JitBodyOutcome::Throw { exc, throw_pc } => {
            // Decoded through the argument pins: the compiled call, and the
            // exception allocation, may have moved a reference argument.
            let exc_locals =
                jit_saved_args_to_values_pinned(cached, &saved_args, np, thread, &arg_pins);
            route_jit_signal_exception_noting_ir_catch(
                shared,
                thread,
                frame_idx,
                compiled,
                cached,
                throw_pc,
                exc,
                &exc_locals,
            )
        }
        JitBodyOutcome::Stashed(rframe, cause, point) => {
            // HANDLER11-2: the body a not-entrant stub forwarded this call to,
            // when that is the body that trapped (`forwarded_trap_body`).
            let forwarded = forwarded_trap_body(compiled, point);
            let compiled: &crate::jit::CompiledMethod = forwarded.as_deref().unwrap_or(compiled);
            // A resume pushes this method's frame (the outermost, for an
            // inlined chain) at the current depth; that frame inherits the
            // `ACC_SYNCHRONIZED` monitor.
            let resume_depth = thread.frames.len();
            // Round 13 wave 6 (lane sync4): a synchronized frame resumes only
            // with a hold to hand it. Round 13 wave 9 (lane chain4): or with
            // the hold its self-locking body handed over in the stash, which
            // the rebuilt frame owns (`frame_hands_over_method_monitor`).
            // Otherwise the method re-runs below and locks for itself, after
            // the stash's compiled locks are released.
            if !(cached.is_synchronized && synchronized_monitor.is_none())
                || super::deopt_resume::frame_hands_over_method_monitor(
                    shared,
                    cached.declaring_class_id,
                    cached.is_synchronized,
                    cached.is_static,
                    &cached.code,
                    &rframe,
                )
            {
                if let Some(r) = resume_or_despeculate_stash(
                    shared,
                    thread,
                    compiled,
                    cached,
                    &rframe,
                    cause,
                    point,
                    "jit-callsite-a",
                )? {
                    hand_monitor_to_resumed_frame(&mut synchronized_monitor, thread, resume_depth);
                    return Ok(r);
                }
            } else {
                super::deopt_resume::release_locks_of_an_unresumed_stash(shared, thread, &rframe);
            }
            // Round 12 wave 7 (lane replay4): an unsound own-stash re-run is
            // refused (the method monitor guard releases on the way out).
            door_rerun_or_refuse(
                shared,
                thread,
                compiled,
                cached,
                &rframe,
                point,
                "jit-callsite-a",
            )?;
            if collected_arg {
                return Err(refuse_rerun_with_a_collected_owned_arg(
                    shared,
                    thread,
                    cached,
                    "jit-callsite-a",
                ));
            }
            restore_popped_jit_args(thread, frame_idx, &saved_args[..np], &arg_pins[..np]);
            Ok(CachedCallResult::CacheMiss)
        }
        JitBodyOutcome::Deopt => {
            // Round 13 wave 8 (lane replay4): refused when unsound.
            door_frameless_rerun_or_refuse(shared, thread, compiled, cached, "jit-callsite-a")?;
            if collected_arg {
                return Err(refuse_rerun_with_a_collected_owned_arg(
                    shared,
                    thread,
                    cached,
                    "jit-callsite-a",
                ));
            }
            restore_popped_jit_args(thread, frame_idx, &saved_args[..np], &arg_pins[..np]);
            Ok(CachedCallResult::CacheMiss)
        }
    }
}

/// Dispatch an already-resolved compiled method using **pre-decoded** args
/// (`caller_args`), instead of popping them off the operand stack like
/// [`execute_jit_call`]. Used by the instance-method invocation tier-up path
/// (bug-03 layer B, default-ON; off-switch `CRATONVM_JIT_VIRTUAL_TIERUP=0`), which
/// reaches the JIT *after* the interception checks have already popped+decoded the args
/// into `caller_args`.
///
/// Returns:
///   * `Ok(Some(ccr))` — handled; `ccr` is the call result to return (normally
///     `Handled` with the return value pushed, or whatever
///     `route_jit_exception_through_method` decided when the JIT body threw).
///   * `Ok(None)` — the call could not be JIT-dispatched (too many args for the
///     JIT register ABI, a JVMTI agent needs interpreted execution, or the
///     method deoptimized via the `i64::MIN` sentinel). NOTHING was pushed and
///     the operand stack is untouched, so the caller must fall through to the
///     interpreted frame push from `caller_args` (a post-call decline first
///     writes the relocated references back into it).
///   * `Err(_)` — a Java exception was raised (NPE / AIOOBE / in-method throw).
///
/// `num_params` includes the receiver for a non-static `cached` (so
/// `caller_args[0]` is the receiver, matching the compiled instance prologue).
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn execute_jit_call_decoded(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    compiled: &crate::jit::CompiledMethod,
    num_params: u16,
    return_type: u8,
    needs_heap: bool,
    cached: &Arc<CachedBytecodeMethod>,
    caller_args: &mut [Value],
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    // r9w2-vm2: `caller_args` is `&mut` only so the post-call declines can
    // write the pinned (relocated) references back before the caller re-runs
    // the method interpreted from this buffer; everything else reads it.
    let needs_heap = jit_entry_needs_heap(compiled, needs_heap, cached, "execute_jit_call_decoded");
    // Round 16 wave 17 (lane trapfix): a body made not entrant is declined
    // before anything runs (`Ok(None)`: the caller interprets from `caller_args`).
    if door_declines_not_entrant_body(thread, compiled, "jit-callsite-b") {
        return Ok(None);
    }
    if !vthread_body_admitted(shared, thread, compiled, cached, true) {
        return Ok(None);
    }
    let np = num_params as usize; // Widening: parameter count conversion
                                  // Too many args for the register-only JIT ABI, or a mismatch between the
                                  // decoded args and the declared count → interpreter fallback (Ok(None)).
                                  //
                                  // This is the LAST place a site that passed every tier-up condition and
                                  // found a compiled body can still be interpreted, and until the second
                                  // census (`CRATONVM_DBG_PROMOTE_REFUSE`) nothing named it: the site falls
                                  // through to the interpreted frame push with the operand stack untouched,
                                  // indistinguishable from never having been admitted at all. See
                                  // `interp_census::promote_refuse_enabled`.
    let max_java_params = jit_abi_max_java_params(needs_heap);
    if np > max_java_params || caller_args.len() != np {
        if crate::runtime::interp_census::promote_refuse_enabled() {
            crate::runtime::interp_census::record_decoded_call_refusal(
                if np > max_java_params {
                    "decoded_call_abi_too_many_args"
                } else {
                    "decoded_call_arg_count_mismatch"
                },
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
            );
        }
        return Ok(None);
    }
    if jvmti_requires_interpreter_for(shared, cached) {
        return Ok(None);
    }
    // Interpreter round i2 wave 56 (lane L6): a contended synchronized callee
    // declines to the interpreted push (frame first); a hold taken here is
    // adopted below, and nothing in between can return.
    let receiver = match caller_args.first() {
        Some(Value::Object(Some(obj))) if !cached.is_static => Some(*obj),
        _ => None,
    };
    let frame_first_hold =
        match frame_first_compiled_door(shared, thread, compiled, cached, receiver) {
            Ok(hold) => hold,
            Err(()) => return Ok(None),
        };
    debug_assert_jit_args_agree(cached, caller_args);
    let mut jit_args = [0i64; JIT_ABI_MAX_JAVA_ARGS];
    for (slot, v) in jit_args.iter_mut().zip(caller_args.iter()) {
        *slot = jit_abi_arg_bits(v);
    }
    // r9-vmside: root the reference arguments for the whole compiled
    // activation. `caller_args` is a Rust-side copy the collector cannot see,
    // and it is read AFTER the compiled call as the handler frame's locals, so
    // a moving collection inside the body handed those locals pre-move
    // addresses.
    //
    // Pinned BEFORE the synchronized-method monitor is acquired: a static
    // method's `get_or_create_class_mirror` can allocate on its slow path, and
    // the arguments were otherwise unrooted across it. Declared before
    // `synchronized_monitor`, whose pin then sits above this window, so that
    // guard drops first and the two windows unwind in stack order.
    let mut arg_pins = [usize::MAX; JIT_ABI_MAX_JAVA_ARGS];
    let _arg_pin_guard = pin_decoded_jit_args(thread, &caller_args[..np], &mut arg_pins);
    let mut synchronized_args =
        door_holds_method_monitor(compiled, cached).then(|| caller_args.to_vec());
    let mut synchronized_monitor = if let Some(held) = frame_first_hold {
        Some(JitSynchronizedMonitorGuard::adopt_frame_first_hold(
            shared, thread, cached, held,
        ))
    } else if let Some(args) = synchronized_args.as_mut() {
        Some(JitSynchronizedMonitorGuard::acquire(
            shared, thread, cached, args,
        )?)
    } else {
        None
    };
    if synchronized_args.is_some() {
        // References re-read through their pins: `acquire` may have moved them.
        reencode_pinned_jit_args(thread, &arg_pins[..np], &mut jit_args[..np]);
    }
    // gch h2/fj (d9d P3, opt-in): as in `execute_jit_call`. The pin window
    // starts where `pin_decoded_jit_args` began it, at the first pinned index.
    let weak_args = if synchronized_monitor.is_none()
        && synchronized_args.is_none()
        && shared.jit.jit_cache.interp_door_args_weak()
        && cratonvm_jit::callee_body_owns_its_arguments(compiled)
    {
        let base = arg_pins[..np]
            .iter()
            .copied()
            .filter(|&pin| pin != usize::MAX)
            .min()
            .unwrap_or(thread.native_pin_roots.len());
        WeakDoorArgs::take(shared, thread, base, &arg_pins[..np])
    } else {
        None
    };

    // gci i14/od (oomjit10 item 1, opt-in `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE`):
    // the promise of `execute_jit_call`, on this door too. Its post-call
    // readers of `caller_args` go through the re-pinned handles below
    // (`pinned_decoded_args`, `write_back_pinned_decoded_args`) and only off
    // the `Returned` arm; every caller returns at once on `Some(_)`.
    let outcome = if weak_args.is_some() && door_promise_enabled(thread) {
        run_jit_body_promised(
            shared,
            thread,
            compiled,
            cached,
            needs_heap,
            &jit_args[..np],
            &arg_pins[..np],
            weak_args
                .as_ref()
                .map(|weak| weak.weak_dispatch_pin_slots.as_slice())
                .unwrap_or(&[]),
        )?
    } else {
        run_jit_body(
            shared,
            thread,
            compiled,
            cached,
            needs_heap,
            &jit_args[..np],
        )?
    };
    // gch h2/fj: survivors back into the pin window, a collected argument
    // nulled in `caller_args` (the readers below and the caller's re-run read it).
    let collected_arg = match &weak_args {
        Some(weak) if !matches!(&outcome, JitBodyOutcome::Returned(_)) => {
            let cleared = weak.repin(thread, &mut arg_pins[..np]);
            for &i in &cleared {
                if let Some(slot) = caller_args.get_mut(i) {
                    *slot = Value::Object(None);
                }
            }
            !cleared.is_empty()
        }
        _ => false,
    };
    drop(weak_args);
    match outcome {
        JitBodyOutcome::Returned(result) => {
            if let Some(v) = jit_return_value(return_type, result) {
                thread.frames[frame_idx].stack.push_unchecked(v);
            }
            Ok(Some(CachedCallResult::Handled))
        }
        JitBodyOutcome::Throw { exc, throw_pc } => {
            // Through the argument pins (r9-vmside): the compiled call may
            // have moved a reference argument.
            let exc_locals = pinned_decoded_args(thread, &caller_args[..np], &arg_pins);
            route_jit_signal_exception_noting_ir_catch(
                shared,
                thread,
                frame_idx,
                compiled,
                cached,
                throw_pc,
                exc,
                &exc_locals,
            )
            .map(Some)
        }
        JitBodyOutcome::Stashed(rframe, cause, point) => {
            // HANDLER11-2: the body a not-entrant stub forwarded this call to,
            // when that is the body that trapped (`forwarded_trap_body`).
            let forwarded = forwarded_trap_body(compiled, point);
            let compiled: &crate::jit::CompiledMethod = forwarded.as_deref().unwrap_or(compiled);
            // The instance-method tier-up path, the dominant path for Groovy's
            // `IndyInterface`-dispatched `doCall` methods. `resume_from_ir_deopt`
            // bails side-effect-free before any frame mutation, so re-running
            // after a `None` is safe. The resumed frame inherits the
            // `ACC_SYNCHRONIZED` monitor, as in `execute_jit_call`.
            let resume_depth = thread.frames.len();
            // Round 13 wave 6 (lane sync4): as in `execute_jit_call`, a
            // synchronized frame resumes only with a hold to hand it (round
            // 13 wave 9, lane chain4: the door's, or the one the stash
            // carries).
            if !(cached.is_synchronized && synchronized_monitor.is_none())
                || super::deopt_resume::frame_hands_over_method_monitor(
                    shared,
                    cached.declaring_class_id,
                    cached.is_synchronized,
                    cached.is_static,
                    &cached.code,
                    &rframe,
                )
            {
                if let Some(r) = resume_or_despeculate_stash(
                    shared,
                    thread,
                    compiled,
                    cached,
                    &rframe,
                    cause,
                    point,
                    "jit-callsite-b",
                )? {
                    hand_monitor_to_resumed_frame(&mut synchronized_monitor, thread, resume_depth);
                    return Ok(Some(r));
                }
            } else {
                super::deopt_resume::release_locks_of_an_unresumed_stash(shared, thread, &rframe);
            }
            door_rerun_or_refuse(
                shared,
                thread,
                compiled,
                cached,
                &rframe,
                point,
                "jit-callsite-b",
            )?;
            if collected_arg {
                return Err(refuse_rerun_with_a_collected_owned_arg(
                    shared,
                    thread,
                    cached,
                    "jit-callsite-b",
                ));
            }
            // r9w2-vm2: the caller re-runs from `caller_args`; hand it the
            // references at their current addresses.
            write_back_pinned_decoded_args(thread, &mut caller_args[..np], &arg_pins);
            Ok(None)
        }
        JitBodyOutcome::Deopt => {
            // Round 13 wave 8 (lane replay4): refused when unsound.
            door_frameless_rerun_or_refuse(shared, thread, compiled, cached, "jit-callsite-b")?;
            if collected_arg {
                return Err(refuse_rerun_with_a_collected_owned_arg(
                    shared,
                    thread,
                    cached,
                    "jit-callsite-b",
                ));
            }
            if crate::runtime::interp_census::promote_refuse_enabled() {
                crate::runtime::interp_census::record_decoded_call_refusal(
                    "decoded_call_deopt",
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                );
            }
            // r9w2-vm2: see the stashed-frame arm above.
            write_back_pinned_decoded_args(thread, &mut caller_args[..np], &arg_pins);
            Ok(None)
        }
    }
}

/// A ONE-SHOT sibling of [`execute_jit_call_decoded`] — enter an already
/// compiled method, run it to completion, and hand its return value back as a
/// `Value` instead of pushing it onto a caller's operand stack.
///
/// # Why this exists rather than another caller of `execute_jit_call_decoded`
///
/// `execute_jit_call_decoded` is written for ONE shape of caller: the
/// interpreter's own per-instruction dispatch loop. Its contract is
/// loop-integrated in two ways that a plain Rust subroutine cannot honour:
///
///  * the normal return value is PUSHED onto `thread.frames[frame_idx].stack`,
///    where the loop expects to find it; and
///  * on an exception routed into the callee's own handler, or on a
///    precise-resume deopt, it PUSHES an interpreter frame and returns
///    `CachedCallResult::FramePushed`, meaning "I have set up a frame; the
///    stepping loop will run it as part of normal control flow".
///
/// A dispatch helper such as `try_invoke_cached_lambda_impl` is not that loop.
/// It must return a complete `Value` synchronously, and it has no way to run a
/// frame somebody else pushed. Handing it `FramePushed` and reading the caller
/// frame's stack anyway leaves an ORPHANED frame behind, whose later return
/// corrupts the frame/stack accounting of a thread that has long since moved
/// on — observed as a `usize::MAX` operand-stack index underflow inside a
/// LATER, interpreted execution of the very same lambda body, thousands of
/// calls after the deopt that actually caused it. See
/// known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md
/// section 5.3 for that crash and 5.4(b) for this function being the
/// prescribed fix.
///
/// So this function shares `execute_jit_call_decoded`'s body run, signal drain
/// and sentinel classification ([`run_jit_body`]) and differs in exactly the
/// two places above:
///
///  * a normal return is CONVERTED to a `Value` and returned, never pushed;
///  * a sink that materialises a frame has that frame RUN TO COMPLETION here
///    (`run_pushed_frame_to_completion`), so the handler / resumed body
///    finishes as part of this call and its result — value or exception —
///    becomes this call's result. Nothing is left on `thread.frames`.
///
/// The caller's frames are therefore never read and never written. That is why
/// this needs no trusted `frame_idx` and is safe from every dispatch context,
/// including the ones that hold no interpreter frame at all.
///
/// Returns:
///   * `Ok(Some(value))` — ran to completion; `value` is `None` for a `void`
///     descriptor and `Some(v)` otherwise.
///   * `Ok(None)` — DECLINED, side-effect-free: the ABI could not carry the
///     args, a JVMTI agent needs interpreted execution, or a deopt landed with
///     no resumable frame. Nothing was executed that the caller must not
///     repeat, so the caller falls back to the interpreted path with the same
///     `args`.
///   * `Err(_)` — a Java exception escaped the callee (or an internal error).
///     Propagates synchronously, exactly like the interpreted path's own `?`.
pub(in crate::runtime::interpreter) fn execute_jit_call_oneshot(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    caller_args: &mut [Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    // r9w2-vm2: `&mut` only for the post-call write-back of the pinned
    // references (see `write_back_pinned_decoded_args`).
    let needs_heap = compiled.needs_heap();
    // Widening: parameter count conversion
    let np = cached.num_params as usize + usize::from(!cached.is_static);
    // Too many args for the register-only JIT ABI, or a mismatch between the
    // decoded args and the declared count → interpreter fallback (Ok(None)).
    if np > jit_abi_max_java_params(needs_heap) || caller_args.len() != np {
        return Ok(None);
    }
    // `is_synchronized` needs the monitor enter/exit this path does not do.
    // Every producer of a lambda-impl `CachedBytecodeMethod` already refuses
    // one, so this is a guard against a future producer, not a live arm.
    if cached.is_synchronized || jvmti_requires_interpreter_for(shared, cached) {
        return Ok(None);
    }
    // Round 16 wave 17 (lane trapfix): and a body made not entrant.
    if door_declines_not_entrant_body(thread, compiled, "jit-oneshot") {
        return Ok(None);
    }
    if !vthread_body_admitted(shared, thread, compiled, cached, false) {
        return Ok(None);
    }
    debug_assert_jit_args_agree(cached, caller_args);
    let mut jit_args = [0i64; JIT_ABI_MAX_JAVA_ARGS];
    for (slot, v) in jit_args.iter_mut().zip(caller_args.iter()) {
        *slot = jit_abi_arg_bits(v);
    }
    let return_type = cached.return_tag();
    // `run_pushed_frame_to_completion` needs the depth the thread had BEFORE
    // any sink below materialised a frame.
    let frames_depth_on_entry = thread.frames.len();
    // r9-vmside: root the reference arguments across the compiled call, for
    // the same reason as `execute_jit_call_decoded` — `caller_args` is read
    // again afterwards as the handler frame's locals.
    let mut arg_pins = [usize::MAX; JIT_ABI_MAX_JAVA_ARGS];
    let _arg_pin_guard = pin_decoded_jit_args(thread, &caller_args[..np], &mut arg_pins);
    // Interpreter round i7 (orchestrator, L1W58JvmtiInNative round 2): this
    // door runs a compiled body from contexts that never pass through
    // `execute` (the lambda one-shot a JNI upcall's `Runnable.run` takes once
    // its impl is compiled), so it lowers the native's marks itself, as
    // `execute` and `run_pushed_frame_to_completion` do: Java a native calls
    // is not "in native" (HotSpot MEASURED `0x5`, was `0x400005`). One load
    // of a thread-owned field otherwise; nested under `execute` it is inert.
    let _nested_java_ack = NestedJavaAck::enter(thread);

    match run_jit_body(
        shared,
        thread,
        compiled,
        cached,
        needs_heap,
        &jit_args[..np],
    )? {
        // Normal return — convert, never push.
        JitBodyOutcome::Returned(result) => Ok(Some(jit_return_value(return_type, result))),
        JitBodyOutcome::Throw { exc, throw_pc } => {
            // Through the argument pins (r9-vmside).
            let exc_locals = pinned_decoded_args(thread, &caller_args[..np], &arg_pins);
            oneshot_route_exception(
                shared,
                thread,
                compiled,
                cached,
                throw_pc,
                exc,
                &exc_locals,
                frames_depth_on_entry,
            )
        }
        // The stashed reconstructed frame is the only way this call can still
        // produce the right answer: an `Ok(None)` decline re-runs the whole
        // body from entry in the interpreter, which double-executes every side
        // effect the compiled body already committed before it trapped. So
        // prefer resuming — and, unlike the loop-integrated sink, RUN the
        // resumed frame here instead of leaving it for a stepping loop that
        // does not exist. (This is the arm that produced section 5.3's crash
        // in the earlier `execute_jit_call_decoded`-reusing attempt.)
        JitBodyOutcome::Stashed(rframe, cause, point) => {
            // HANDLER11-2: the body a not-entrant stub forwarded this call to,
            // when that is the body that trapped (`forwarded_trap_body`).
            let forwarded = forwarded_trap_body(compiled, point);
            let compiled: &crate::jit::CompiledMethod = forwarded.as_deref().unwrap_or(compiled);
            let resumed = resume_deopted_body_at_point(
                shared,
                thread,
                compiled,
                cached,
                &rframe,
                cause,
                point,
                frames_depth_on_entry,
                "lambda-oneshot",
            );
            if matches!(resumed, Ok(None)) {
                door_rerun_or_refuse(
                    shared,
                    thread,
                    compiled,
                    cached,
                    &rframe,
                    point,
                    "lambda-oneshot",
                )?;
                // r9w2-vm2: declined — the caller re-runs from `caller_args`,
                // so hand it the references at their current addresses.
                write_back_pinned_decoded_args(thread, &mut caller_args[..np], &arg_pins);
            }
            resumed
        }
        JitBodyOutcome::Deopt => {
            // Round 13 wave 8 (lane replay4): refused when unsound.
            door_frameless_rerun_or_refuse(shared, thread, compiled, cached, "lambda-oneshot")?;
            write_back_pinned_decoded_args(thread, &mut caller_args[..np], &arg_pins);
            Ok(None)
        }
    }
}

/// How [`execute_wrapped_body_door_locked`] ended.
pub(in crate::runtime::interpreter) enum DoorLockedOutcome {
    /// The call is complete, with this result or exception. The door's monitor
    /// hold was released, or was owned by a frame this call ran to completion
    /// (whose removal released it).
    Finished(Result<Option<Value>, MethodCallFailed>),
    /// The compiled body could not finish the call: a deopt with no resumable
    /// frame, or a routing answer the one-shot sinks do not take. The caller
    /// STILL OWNS the monitor hold (at `monitor`, its current address) and must
    /// run the method interpreted from `args` (the arguments re-read through
    /// their pins), installing `monitor` as that frame's `monitor_on_exit`
    /// before anything can collect. This is the re-run from entry every other
    /// door makes on these outcomes, with one difference: the lock is never
    /// dropped between the compiled attempt and the re-run.
    Interpret {
        monitor: ObjectRef,
        args: Vec<Value>,
    },
}

/// A `synchronized` template's published wrapped-entry body (static since
/// round 11 wave 6, instance since round 12 wave 8 under
/// `CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY`) (`requires_wrapped_entry`), when the door that already holds its monitor
/// may enter it instead of interpreting the template. `None`: interpret.
///
/// Refused, so the template runs as before: a virtual thread (compiled code
/// carries none of the interpreter's unmount points, the lambda twin's rule),
/// a JVMTI agent that needs interpreted execution, an argument count the
/// register ABI cannot carry or that disagrees with the template, and a body
/// that is not stamped `requires_wrapped_entry` (only a stamped body is one the
/// door's own monitor is the missing half of).
///
/// Memoized per thread by `JitCache::generation()`, which every publish and
/// every withdrawal advances, so the steady state is one acquire load and a
/// four-slot scan instead of `JitCache::get`'s three-string hash. The memo
/// holds `Weak`s only: it keeps no retired body's code mapped, and a `Weak`
/// keeps its allocation's address from being reused, so a pointer compare on
/// the template is an identity test.
pub(in crate::runtime::interpreter) fn door_locked_wrapped_body(
    shared: &SharedVm,
    thread: &JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
    nargs: usize,
) -> Option<Arc<crate::jit::CompiledMethod>> {
    if !door_may_enter_wrapped_body(shared, thread, cached) {
        return None;
    }
    let compiled = wrapped_body_memoized(shared, cached)?;
    if door_admits_wrapped_body(cached, nargs, &compiled) {
        return Some(compiled);
    }
    // j27 wave 7 (lane T1LAM3): the memo's `Weak` upgrade may be the last
    // owner; a refused body is released through the retirement queue.
    drop(cratonvm_jit::RetainedCode::new(compiled));
    None
}

/// The template-level refusals of [`door_locked_wrapped_body`]: everything it
/// asks before it looks for a body.
pub(in crate::runtime::interpreter::jit_bridge) fn door_may_enter_wrapped_body(
    shared: &SharedVm,
    thread: &JvmThread,
    cached: &CachedBytecodeMethod,
) -> bool {
    cached.is_synchronized
        && (cached.is_static || door_sync_instance_body_enabled())
        && !matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && !crate::runtime::env_cache::disable_jit()
        && !jvmti_requires_interpreter(shared)
}

/// `CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY` (round 12 wave 8 orchestrator,
/// default on): a compiled caller's `synchronized` INSTANCE callee enters its
/// published wrapped-entry body under the door's uncontended hold, as a
/// `static synchronized` one has since round 11 wave 6. It was interpreted on
/// every call: `R12MonitorContended` `sync-method-1t`, a four-field
/// `synchronized int step()`, ran ~520 ns a call against HotSpot's ~5
/// (`r12w8-orch-probe-findings-on-w8` item 4). `0` interprets the template
/// again.
pub(in crate::runtime::interpreter::jit_bridge) fn door_sync_instance_body_enabled() -> bool {
    crate::runtime::env_cache::jit_door_sync_instance_body()
}

/// The body-level refusals of [`door_locked_wrapped_body`], for a body found.
pub(in crate::runtime::interpreter::jit_bridge) fn door_admits_wrapped_body(
    cached: &CachedBytecodeMethod,
    nargs: usize,
    compiled: &crate::jit::CompiledMethod,
) -> bool {
    let calling_bodies = crate::runtime::env_cache::jit_door_sync_calling_body();
    // `num_params` excludes an instance method's receiver, which `args` carries.
    nargs == cached.num_params as usize + usize::from(!cached.is_static)
        && nargs <= jit_abi_max_java_params(compiled.needs_heap())
        && (door_wrapped_body_attempt_admitted(compiled, calling_bodies)
            || (calling_bodies && wrapped_body_own_exit_rerun_is_exact(compiled)))
        && optimizing_exits_route_exactly(cached, compiled)
}

/// The rerun half of [`door_admits_wrapped_body`]: may the door ENTER this
/// wrapped body instead of interpreting the template?
///
/// Always for a body that never re-runs ([`wrapped_body_attempt_never_reruns`]).
/// JIT round 16 wave 8 (lane sync), under `CRATONVM_JIT_DOOR_SYNC_CALLING_BODY`
/// (`calling_bodies`, default ON since the round-16 w8b A/B, `0` refuses):
/// also for a body whose only way to stop
/// short of a return is a callee's -- it makes Java calls, but its OWN exits
/// still never re-run ([`wrapped_body_own_exits_never_rerun`]).
///
/// Why that is the interpreter doors' contract and no wider: an interpreted
/// caller's call of the same `synchronized` method enters the same wrapped
/// body through `execute_jit_call` / `execute_jit_call_decoded`, which hold
/// the monitor in a `JitSynchronizedMonitorGuard` exactly as
/// [`execute_wrapped_body_door_locked`] does and ask nothing about calls. A
/// callee's trap that its call-site service declined reaches both doors as a
/// FOREIGN stash; both try `resume_or_despeculate_stash` (which releases the
/// stash's compiled locks when it declines), then [`door_rerun_or_refuse`]:
/// a re-run the replay rule calls exact runs the template under the SAME hold
/// ([`DoorLockedOutcome::Interpret`] here, `CacheMiss` / `Ok(None)` there),
/// and an unsound one is refused with the doors' `InternalError`, never re-run.
/// So this door gains the interpreter doors' outcomes for a body they already
/// enter, not a new one. What it cost before: a compiled caller's call of
/// such a method INTERPRETED the template on every call, for the life of the
/// process (`R16SyncDoorCalls` `tv`, ~5-6 us a call on the Windows box against
/// ~100 ns for the unsynchronized twin; the population is every synchronized
/// method the self-locking route refuses -- an exception table, a nested
/// `synchronized` block, a store to local 0 -- that calls anything).
pub(in crate::runtime::interpreter::jit_bridge) fn door_wrapped_body_attempt_admitted(
    compiled: &crate::jit::CompiledMethod,
    calling_bodies: bool,
) -> bool {
    wrapped_body_attempt_never_reruns(compiled)
        || (calling_bodies && wrapped_body_own_exits_never_rerun(compiled))
}

/// A synchronized wrapped-entry body may also run when its own optimizing
/// exits can safely fall back to the interpreter under the monitor already
/// held by the door. Optimizing bodies pass the same per-point replay proof
/// before publication; the door keeps its runtime replay check and monitor
/// hand-off. Reject single-pass, frameless, invokedynamic, and OSR map exits
/// because that proof does not cover those replay paths.
pub(in crate::runtime::interpreter::jit_bridge) fn wrapped_body_own_exit_rerun_is_exact(compiled: &crate::jit::CompiledMethod) -> bool {
    if !compiled.used_ir_backend
        || compiled.has_indy_trap
        || compiled.has_frameless_trap_stub
        || !compiled.osr_exit_points.is_empty()
    {
        return false;
    }
    // IR publication has already checked every point in `_deopt_point_boxes`
    // with this method's exact bytecode, splice-purity fact, resume policy,
    // and synchronized monitor. Rechecking that whole proof on every virtual
    // call would put a bytecode scan on the hot door path.
    compiled.ir_osr_sentinel_free || compiled.ir_osr_exception_exit_bcis.is_some()
}

/// Round 11 wave 16 (lane tier): the door routes an exception out of the body
/// through `route_jit_signal_exception`, which falls back to the
/// PARAMETERS-ONLY handler frame when the body published no precise frame.
/// An optimizing body's call-exception exits (`emit_call_exc_stub`) publish
/// none, so one inside a handler range would enter the method's handler with
/// the parameters as its locals. `wrapped_body_has_no_deopt_exit` now admits
/// optimizing bodies here, so the OSR door's own rule for the same exits is
/// asked too: every such exit bci lies outside every exception-table row
/// (`osr_exception_exit_bcis_uncovered`), or the body has no exit at all.
/// (Its reason-9 rethrow pads, which DO publish a precise frame, carry a
/// deopt box and are refused before this.) Single-pass bodies are unchanged.
pub(in crate::runtime::interpreter::jit_bridge) fn optimizing_exits_route_exactly(
    cached: &CachedBytecodeMethod,
    compiled: &crate::jit::CompiledMethod,
) -> bool {
    // A method with no exception table has no handler an exit could be
    // routed into with the wrong locals: every exception leaves it (round 12
    // wave 8 orchestrator; `synchronized int step()`'s IR body was refused
    // here only because it records no exit bcis, and ran interpreted).
    !compiled.used_ir_backend
        || compiled.ir_osr_sentinel_free
        || cached.exception_table.is_empty()
        || osr_exception_exit_bcis_uncovered(
            compiled.ir_osr_exception_exit_bcis.as_deref(),
            &cached.exception_table,
        )
}

/// Will [`door_locked_wrapped_body`] ENTER a compiled body for `cached`, as far
/// as this thread's [`WRAPPED_BODY_MEMO`] already knows? A peek: a memo miss
/// (or a stale generation) answers `false` and looks nothing up, so the caller
/// only ever skips work, never adds a `JitCache::get`.
///
/// For `lambda::bytecode_callee_compiled_or_nominate` (round 11 wave 7,
/// proposal 23): a call the door enters is counted by the body's own entry
/// counter (`cratonvm_jit::entry_counter`), so counting it again in the shared
/// `ProfileStore` is a sharded lock and a hash lookup per call that feeds the
/// tiered manager nothing the drain does not. The door's own refusals are
/// re-asked here, so a template the door will INTERPRET keeps being counted
/// (its C1 body may still become a call-free C2 body the door can enter).
pub(in crate::runtime::interpreter) fn door_will_enter_memoized_wrapped_body(
    shared: &SharedVm,
    thread: &JvmThread,
    cached: &Arc<CachedBytecodeMethod>,
) -> bool {
    if !door_may_enter_wrapped_body(shared, thread, cached) {
        return false;
    }
    let generation = shared.jit.jit_cache.generation();
    let template = Arc::as_ptr(cached);
    let body = WRAPPED_BODY_MEMO.with(|memo| {
        memo.borrow()
            .iter()
            .find(|slot| slot.template.as_ptr() == template && slot.generation == generation)
            .and_then(|slot| slot.body.upgrade())
            // j27 wave 7 (lane T1LAM3): the upgrade may be the last owner.
            .map(cratonvm_jit::RetainedCode::new)
    });
    // Round 16 (lane sync): the argument count the door itself is handed --
    // the receiver included for an instance template (`num_params` excludes
    // it). Passing `num_params` alone made `door_admits_wrapped_body` refuse
    // every INSTANCE template, so since round 12 wave 8 admitted those to the
    // door (`CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY`, which stays the kill
    // switch: off, `door_may_enter_wrapped_body` refuses above) each call to a
    // door-entered instance body was still counted in the shared
    // `ProfileStore` on top of the body's own entry counter.
    let nargs = cached.num_params as usize + usize::from(!cached.is_static);
    body.is_some_and(|compiled| door_admits_wrapped_body(cached, nargs, &compiled))
}

/// Can every way `compiled` stops short of a return be finished from where it
/// stopped, so the door never re-runs a partly executed body?
///
/// The template this door replaces cannot re-run anything. A compiled attempt
/// can: a stash that `resume_or_despeculate_stash` declines, and the frameless
/// sentinel, are re-run from entry by every door (the method's `putstatic`s
/// then happen twice). So only bodies that cannot produce either are entered:
///
/// * no Java call of any shape (dispatch infos, baked direct callees, cycle
///   cells, MIC/PIC slots): a nested callee's declined trap reaches the sink
///   as a FOREIGN stash, which nothing here can resume;
/// * no invokedynamic trap and no frameless trap stub (both end in the bare
///   sentinel);
/// * and either no trap point at all, or a single-pass-style precise resume
///   (`can_deopt_resume` with `CRATONVM_DEOPT_REAL`), which hands the monitor
///   to the resumed frame. An optimizing body of an `ACC_SYNCHRONIZED` method
///   with traps has neither: `sink_precise_resume_allowed` refuses a
///   synchronized method, and the IR finalizer sets `can_deopt_resume` only for
///   bodies with monitor BLOCKS. See
///   `r11w6-tier-optimizing-synchronized-body-trap-reruns-from-entry`.
///
/// A refusal only keeps the template, so a wrong `false` costs speed, never
/// correctness. The precise resume is not total, though:
/// `real_frame_deopt_resume_and_despeculate` still declines a superseded body
/// of a REDEFINED class and a frame `build_deopt_frame_inner` cannot map, and
/// the door then re-runs the template under its hold, the whole-method re-run
/// every other door makes on those two outcomes (round 11 wave 7 audit).
pub(in crate::runtime::interpreter::jit_bridge) fn wrapped_body_attempt_never_reruns(compiled: &crate::jit::CompiledMethod) -> bool {
    // JIT round 16 wave 8 (lane sync): split in two halves, unchanged in
    // meaning, so the door's calling-body arm can ask the second alone.
    wrapped_body_calls_nothing(compiled) && wrapped_body_own_exits_never_rerun(compiled)
}

/// The first bullet of [`wrapped_body_attempt_never_reruns`]: no Java call of
/// any shape.
pub(in crate::runtime::interpreter::jit_bridge) fn wrapped_body_calls_nothing(compiled: &crate::jit::CompiledMethod) -> bool {
    compiled._jit_invoke_infos.is_empty()
        && compiled._direct_callee_entries.is_empty()
        && compiled._jit_cycle_cells.is_empty()
        && compiled._jit_mic_slots.is_empty()
        && compiled._jit_pic_slots.is_empty()
}

/// The last two bullets of [`wrapped_body_attempt_never_reruns`]: no exit of
/// the body's OWN ends in the bare sentinel or in a stash only a re-run from
/// entry can finish.
pub(in crate::runtime::interpreter::jit_bridge) fn wrapped_body_own_exits_never_rerun(compiled: &crate::jit::CompiledMethod) -> bool {
    if compiled.has_indy_trap || compiled.has_frameless_trap_stub {
        return false;
    }
    // Tier-aware since round 11 wave 16: an optimizing body publishes resume
    // points it can never reach. See `wrapped_body_has_no_deopt_exit`.
    wrapped_body_has_no_deopt_exit(compiled)
        || (compiled.can_deopt_resume && cratonvm_jit::deopt_real_enabled())
}

/// One slot of [`WRAPPED_BODY_MEMO`]: the answer `JitCache::get` gave for
/// `template` at `generation` (an empty `Weak` for "no wrapped body").
pub(in crate::runtime::interpreter::jit_bridge) struct WrappedBodyMemo {
    pub(in crate::runtime::interpreter::jit_bridge) template: std::sync::Weak<CachedBytecodeMethod>,
    pub(in crate::runtime::interpreter::jit_bridge) generation: u64,
    pub(in crate::runtime::interpreter::jit_bridge) body: std::sync::Weak<crate::jit::CompiledMethod>,
}

/// Slots in [`WRAPPED_BODY_MEMO`]. A hot loop calls one or two
/// `static synchronized` methods; more than this and the oldest is replaced.
pub(in crate::runtime::interpreter::jit_bridge) const WRAPPED_BODY_MEMO_SLOTS: usize = 4;

thread_local! {
    /// Per-thread memo for [`door_locked_wrapped_body`]. Thread-local, not a
    /// process global: it only saves a lookup, holds no strong reference, and
    /// its answers are validated against the owning VM's cache generation.
    pub(in crate::runtime::interpreter::jit_bridge) static WRAPPED_BODY_MEMO: std::cell::RefCell<Vec<WrappedBodyMemo>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(in crate::runtime::interpreter::jit_bridge) fn wrapped_body_memoized(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
) -> Option<Arc<crate::jit::CompiledMethod>> {
    let generation = shared.jit.jit_cache.generation();
    let template = Arc::as_ptr(cached);
    let hit = WRAPPED_BODY_MEMO.with(|memo| {
        memo.borrow()
            .iter()
            .find(|slot| slot.template.as_ptr() == template && slot.generation == generation)
            .map(|slot| slot.body.upgrade())
    });
    if let Some(body) = hit {
        return body;
    }
    let found = shared
        .jit
        .jit_cache
        .read()
        .get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        )
        .filter(|compiled| compiled.requires_wrapped_entry);
    // Round 15 wave 4 (lane sync3, SY2-2): a wrapped body published since this
    // thread last looked may be a compile that began before the mirror slot
    // was minted (the mint's own request was then refused: that compile held
    // the method's queue slot). Nothing here can collect.
    if found.is_some() && cached.is_static {
        request_static_self_lock_recompile(shared, cached);
    }
    let entry = WrappedBodyMemo {
        template: Arc::downgrade(cached),
        generation,
        body: found.as_ref().map(Arc::downgrade).unwrap_or_default(),
    };
    WRAPPED_BODY_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some(slot) = memo
            .iter_mut()
            .find(|slot| slot.template.as_ptr() == template)
        {
            *slot = entry;
        } else {
            if memo.len() >= WRAPPED_BODY_MEMO_SLOTS {
                memo.remove(0);
            }
            memo.push(entry);
        }
    });
    found
}

/// Enter a `static synchronized` method's wrapped-entry compiled body from a
/// door that ALREADY HOLDS its monitor (the uncontended acquire of
/// `install_and_run_cached_frame_monitored`), run it to completion, and return
/// its result — the one-shot door of [`execute_jit_call_oneshot`] plus the
/// monitor half that door refuses.
///
/// # Why
///
/// A compiled caller's `static synchronized` callee used to be served by
/// INTERPRETING its cached frame template (round 11 wave 5). For a body made of
/// static-field traffic that is the dominant cost: every `getstatic` /
/// `putstatic` the template runs re-resolves the field ref, re-checks
/// staticness and initialisation, and reads the static slot through the
/// class store (`opcodes::op_getstatic` / `op_putstatic`; a pair measured
/// 86 ns against HotSpot's 3.5 before the System latch). `R11X64genSyncCost`'s
/// `incSync` runs seven of them per call, and its `sync-method-1t` line stayed
/// at ~1.07 s against `plain-method-1t`'s ~50 ms. The background tier has
/// already published a compiled body for the method (a wrapped entry, because
/// the body has no monitor prologue); this door supplies the missing monitor.
///
/// The 2026-09-23 measurement that kept the INSTANCE twin interpreting
/// (`Throwable.fillInStackTrace`, ~5 % slower compiled) priced a blocking
/// acquire, the JIT-entry guard and a stack walk over one more compiled frame.
/// Here the acquire is the door's uncontended one, and the body is not a stack
/// walker. Round 12 wave 8 admits the instance door here too
/// ([`door_may_enter_wrapped_body`]).
///
/// # The monitor on every exit
///
/// The hold is adopted by a [`JitSynchronizedMonitorGuard`] pinned above the
/// argument window, so a moving collection inside the body updates it:
///
/// * normal return, an exception with no handler here, an internal error:
///   the guard's `Drop` releases it (`monitor_exit_and_retract_jmx`, the
///   release `pop_and_recycle_frame_with_reason` makes for a door frame);
/// * an exception routed to a handler in this method:
///   `route_jit_exception_through_method` builds the handler frame with its
///   OWN recursive hold (as for `execute_jit_call`); that frame runs to
///   completion here and releases its hold, and the guard then releases ours;
/// * a precise deopt resume: the hold is handed to the resumed frame
///   ([`hand_monitor_to_resumed_frame`], the `execute_jit_call_decoded` rule),
///   which runs to completion here and releases it on removal;
/// * a deopt with no resumable frame: [`DoorLockedOutcome::Interpret`]; the
///   hold is surrendered to the caller, which re-runs the method interpreted
///   under it.
///
/// Callers check [`door_locked_wrapped_body`] first; `args` has exactly the
/// template's parameter count and fits the register ABI.
pub(in crate::runtime::interpreter) fn execute_wrapped_body_door_locked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    args: &[Value],
    monitor: ObjectRef,
) -> DoorLockedOutcome {
    // Round 15 wave 4 (lane sync3, SY2-2): the class's mirror slot, minted as
    // `JitSynchronizedMonitorGuard::acquire` mints it and on the same terms
    // (a compiled body of the method exists; `monitor` is the mirror the
    // door just locked, read with no safepoint since). This is the door every
    // COMPILED caller's `static synchronized` callee takes, and it never
    // minted: a method called only from compiled code got every compile with
    // no slot, so a wrapped body for good. Its kill switch is read only on
    // this thread's first entry (`mint_static_sync_mirror_slot_if`).
    let minted = cached.is_static
        && crate::runtime::env_cache::jit_self_locking_sync_static()
        && crate::jit::helpers::mint_static_sync_mirror_slot_if(
            shared,
            cached.declaring_class_id,
            monitor,
            static_self_lock_recompile_enabled,
        );
    let needs_heap = compiled.needs_heap();
    let np = args.len().min(JIT_ABI_MAX_JAVA_ARGS);
    debug_assert_jit_args_agree(cached, args);
    let mut jit_args = [0i64; JIT_ABI_MAX_JAVA_ARGS];
    for (slot, v) in jit_args.iter_mut().zip(args.iter()) {
        *slot = jit_abi_arg_bits(v);
    }
    let return_type = cached.return_tag();
    // `run_pushed_frame_to_completion` needs the depth before any sink pushed.
    let frames_depth_on_entry = thread.frames.len();
    // Declared before the monitor guard, whose pin then sits above this
    // window, so that guard drops first (the `execute_jit_call_decoded` order).
    let mut arg_pins = [usize::MAX; JIT_ABI_MAX_JAVA_ARGS];
    let _arg_pin_guard = pin_decoded_jit_args(thread, &args[..np], &mut arg_pins);
    let mut monitor_guard = Some(JitSynchronizedMonitorGuard::adopt(shared, thread, monitor));
    if minted {
        request_static_self_lock_recompile(shared, cached);
    }

    let outcome = match run_jit_body(
        shared,
        thread,
        compiled,
        cached,
        needs_heap,
        &jit_args[..np],
    ) {
        Ok(outcome) => outcome,
        // Propagates out of the synchronized method: the guard releases.
        Err(error) => return DoorLockedOutcome::Finished(Err(error)),
    };
    match outcome {
        JitBodyOutcome::Returned(result) => {
            DoorLockedOutcome::Finished(Ok(jit_return_value(return_type, result)))
        }
        JitBodyOutcome::Throw { exc, throw_pc } => {
            // Through the argument pins: the body may have moved a reference.
            let exc_locals = pinned_decoded_args(thread, &args[..np], &arg_pins);
            let idx_for_signature = thread.frames.len().saturating_sub(1);
            match route_jit_signal_exception_noting_ir_catch(
                shared,
                thread,
                idx_for_signature,
                compiled,
                cached,
                throw_pc,
                exc,
                &exc_locals,
            ) {
                Ok(CachedCallResult::FramePushed) => DoorLockedOutcome::Finished(
                    run_pushed_frame_to_completion(shared, thread, frames_depth_on_entry),
                ),
                // `route_jit_exception_through_method` answers only
                // `FramePushed` or `Err`; decline as `execute_jit_call_oneshot`
                // does rather than invent a return value.
                Ok(_) => door_locked_interpret(monitor_guard.take(), thread, args, &arg_pins),
                Err(error) => DoorLockedOutcome::Finished(Err(error)),
            }
        }
        JitBodyOutcome::Stashed(rframe, cause, point) => {
            // HANDLER11-2: the body a not-entrant stub forwarded this call to,
            // when that is the body that trapped (`forwarded_trap_body`).
            let forwarded = forwarded_trap_body(compiled, point);
            let compiled: &crate::jit::CompiledMethod = forwarded.as_deref().unwrap_or(compiled);
            let resume_depth = thread.frames.len();
            let resumed = match resume_or_despeculate_stash(
                shared,
                thread,
                compiled,
                cached,
                &rframe,
                cause,
                point,
                "sync-door-oneshot",
            ) {
                Ok(r) => r.is_some(),
                // Propagates out of the synchronized method: the guard releases.
                Err(error) => return DoorLockedOutcome::Finished(Err(error)),
            };
            if resumed {
                hand_monitor_to_resumed_frame(&mut monitor_guard, thread, resume_depth);
                return DoorLockedOutcome::Finished(run_pushed_frame_to_completion(
                    shared,
                    thread,
                    frames_depth_on_entry,
                ));
            }
            // Propagates out of the synchronized method: the guard releases.
            if let Err(error) = door_rerun_or_refuse(
                shared,
                thread,
                compiled,
                cached,
                &rframe,
                point,
                "sync-door-oneshot",
            ) {
                return DoorLockedOutcome::Finished(Err(error));
            }
            door_locked_interpret(monitor_guard.take(), thread, args, &arg_pins)
        }
        JitBodyOutcome::Deopt => {
            // Round 13 wave 8 (lane replay4): refused when unsound; the
            // error propagates out of the synchronized method and the guard
            // releases.
            if let Err(error) = door_frameless_rerun_or_refuse(
                shared,
                thread,
                compiled,
                cached,
                "sync-door-oneshot",
            ) {
                return DoorLockedOutcome::Finished(Err(error));
            }
            door_locked_interpret(monitor_guard.take(), thread, args, &arg_pins)
        }
    }
}

/// The [`DoorLockedOutcome::Interpret`] exit: surrender the hold and re-read
/// the arguments through their pins (both may have moved during the body).
pub(in crate::runtime::interpreter::jit_bridge) fn door_locked_interpret(
    guard: Option<JitSynchronizedMonitorGuard>,
    thread: &mut JvmThread,
    args: &[Value],
    arg_pins: &[usize],
) -> DoorLockedOutcome {
    let relocated = pinned_decoded_args(thread, args, arg_pins);
    match guard.and_then(|guard| guard.surrender(thread)) {
        Some(monitor) => DoorLockedOutcome::Interpret {
            monitor,
            args: relocated,
        },
        // Unreachable: `surrender` answers `None` only for a guard that was
        // already disarmed (transferred or released), and every arm takes the
        // guard exactly once. Nothing is held for the caller to run under, so
        // the call must not run at all.
        None => {
            DoorLockedOutcome::Finished(Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "synchronized door lost its monitor pin".to_string(),
            })))
        }
    }
}

/// Resume a compiled body that trapped, and run the resumed frame to
/// completion here.
///
/// **This is the only correct answer for a body that has already committed a
/// side effect.** A deopt sentinel does NOT mean "nothing happened": it means
/// the compiled body ran up to `rframe.bci` and stopped. Declining — returning
/// `Ok(None)` so the caller re-enters the body from entry — therefore
/// re-executes everything before that bci a second time. `rframe` is the
/// reconstructed state that makes resuming from the trap point possible, which
/// is why it must be spent rather than dropped.
///
/// Shared by both one-shot doors into a compiled lambda impl: the interpreter's
/// [`execute_jit_call_oneshot`] and the compiled caller's
/// `jit::helpers::try_lambda_site_direct_call`. It was inlined in the first of
/// those and simply absent from the second, which dropped the frame and let its
/// caller re-run the body — see
/// `docs/feature-designs/concurrent-gc-maturation.md`
/// section 8 for the hibernate-reactive `reactiveRemove`-fires-twice defect
/// that came out of exactly that asymmetry.
///
/// Returns:
///   * `Ok(Some(v))` — the frame was resumed and ran to completion; `v` is this
///     call's result.
///   * `Ok(None)` — no resume was possible; the owning method has been
///     de-speculated instead. The caller may re-run the body, and by doing so
///     accepts that any side effect committed before the trap happens twice.
///   * `Err(_)` — a Java exception escaped the resumed frame, or (round 12
///     wave 5) the frame's scalar-replaced objects could not be re-allocated
///     in a full heap and the trapped body ends in `OutOfMemoryError`.
///
/// The taker hands over the stash's [`cratonvm_jit::deopt::DeoptCause`] and
/// the address of the point that stashed `rframe`
/// (`take_last_deopt_with_point`), so the trap is charged under the guard that
/// fired rather than a reason re-derived from the bci, and a foreign frame's
/// charge can tell a superseded body from the published one (round 11 wave
/// 10). `0` means "unknown point" and keeps the old charge. Both one-shot
/// doors pass the address.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_deopted_body_at_point(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
    frames_depth_on_entry: usize,
    sink_label: &str,
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    // `?`: a heap that could not re-materialise the frame is the body's
    // outcome (round 12 wave 5), never a re-run.
    if resume_or_despeculate_stash(
        shared, thread, compiled, cached, rframe, cause, point_addr, sink_label,
    )?
    .is_some()
    {
        return run_pushed_frame_to_completion(shared, thread, frames_depth_on_entry).map(Some);
    }
    Ok(None)
}

/// [`route_jit_signal_exception`] at the three interpreter doors and the
/// one-shot door, noting a catch an OPTIMIZING body had to leave compiled code
/// for (round 12 wave 3, lane exc).
///
/// `FramePushed` is exactly that: the body's own handler matched, and it is
/// about to run in an interpreter frame rebuilt from the reason-9 snapshot,
/// followed by the rest of the invocation. The optimizing tier compiles no
/// handler blocks, so a body that catches in a loop pays this on every catch;
/// `retier_hot_optimizing_catch` hands such a method to the single-pass tier,
/// whose `catch` runs in compiled code. A single-pass body is not counted: it
/// reaches here only for a catch its own local-handler lookup declined, which
/// re-tiering cannot improve. The routing itself is unchanged.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn route_jit_signal_exception_noting_ir_catch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller_frame_idx: usize,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    fallback_kind: JitThrowPc,
    exc: ObjectRef,
    fallback_locals: &[Value],
) -> Result<CachedCallResult, MethodCallFailed> {
    let routed = route_jit_signal_exception(
        shared,
        thread,
        caller_frame_idx,
        compiled,
        cached,
        fallback_kind,
        exc,
        fallback_locals,
    );
    if compiled.used_ir_backend && matches!(routed, Ok(CachedCallResult::FramePushed)) {
        crate::jit::helpers::retier_hot_optimizing_catch(
            shared,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        );
    }
    routed
}

/// The one-shot arm of `route_jit_signal_exception`: route an exception raised
/// inside a compiled body through that body's OWN exception table, and — when
/// a handler matched and a frame was materialised for it — run that frame to
/// completion here so the handler's result is this call's result.
///
/// `route_jit_exception_through_method` returns `FramePushed` after pushing the
/// handler frame; for the interpreter's stepping loop that IS the answer, but a
/// one-shot subroutine must finish the frame itself. `Err(ExceptionThrown)` (no
/// matching handler) needs nothing: it already propagates synchronously, which
/// is exactly what the interpreted lambda path does.
///
/// The `caller_frame_idx` argument is passed as the current top-of-stack index
/// purely to satisfy the signature — `route_jit_exception_through_method` does
/// not read it (`let _ = caller_frame_idx`), because a routed handler frame is
/// built from the CALLEE's own incoming args, never from the caller's stack.
/// The `debug_assert` below pins the only property this path depends on: that
/// the sink pushed exactly one frame, the one we are about to run.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn oneshot_route_exception(
    shared: &SharedVm,
    thread: &mut JvmThread,
    compiled: &crate::jit::CompiledMethod,
    cached: &Arc<CachedBytecodeMethod>,
    throw_pc: JitThrowPc,
    exc: ObjectRef,
    fallback_locals: &[Value],
    frames_depth_on_entry: usize,
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    let idx_for_signature = thread.frames.len().saturating_sub(1);
    match route_jit_signal_exception_noting_ir_catch(
        shared,
        thread,
        idx_for_signature,
        compiled,
        cached,
        throw_pc,
        exc,
        fallback_locals,
    )? {
        CachedCallResult::FramePushed => {
            debug_assert_eq!(thread.frames.len(), frames_depth_on_entry + 1);
            run_pushed_frame_to_completion(shared, thread, frames_depth_on_entry).map(Some)
        }
        // `Handled` / `CacheMiss` are unreachable from
        // `route_jit_exception_through_method`, which only ever returns
        // `FramePushed` or `Err`. Decline rather than invent a return value.
        _ => Ok(None),
    }
}


/// Re-offer held deferred-`new` retries in every live VM, called immediately
/// after a class-manager write guard releases its lock.
///
/// That is the moment a `new` site can have become resolvable, and it is the
/// only moment: a method holding a retry already has a body, so nothing
/// compiles it again and no compile-door sweep will ever look at it. Placed
/// beside `drain_pending_class_hooks` for the same reason that call is there —
/// the write lock is gone, so taking a fresh read lock here is safe.
///
/// `owner_domain` is the released class manager's layout domain
/// (`JitRealm::class_layout_domain`): only the VM that owns it is swept
/// (interpreter round i1 wave 18). Another VM's held sites cannot have become
/// resolvable through this class manager, and every fruitless look spends one
/// of that memo's `MAX_DEFERRED_NEW_LOOKS`, so sweeping it let one VM's
/// class-loading churn retire another VM's retries. A domain no live VM owns
/// (a VM not in the hook registry) sweeps nothing, for the same reason;
/// `None` sweeps every VM, the old behaviour. The owner is found by one keyed
/// registry probe (`vm_init::hook_vm_for_domain`, interpreter round i1 wave
/// 21), not by copying every live VM.
pub fn resweep_deferred_new_after_class_definition(owner_domain: Option<u32>) {
    if cratonvm_jit::held_deferred_new_count() == 0 {
        return;
    }
    // `CRATONVM_JIT_DEFERRED_NEW_RELEASE_STAMP` (interpreter round i4 wave 92,
    // lane L7): a release whose section settled nothing (most of them: one
    // define + link + init takes five or more guards) skips the sweep's list
    // clone and by-name re-resolution (`deferred_new_release_sweep_due`).
    match owner_domain {
        Some(domain) => {
            if let Some(shared) = crate::vm::vm_init::hook_vm_for_domain(domain) {
                if deferred_new_release_sweep_due(&shared) {
                    resweep_held_deferred_new_retries(&shared, true);
                }
            }
        }
        None => {
            for shared in crate::vm::vm_init::live_hook_vms_for_jit() {
                if deferred_new_release_sweep_due(&shared) {
                    resweep_held_deferred_new_retries(&shared, true);
                }
            }
        }
    }
}

/// Seed profile-replay records whose class has just been defined.
///
/// Called from the class manager's write-guard release
/// (`ClassManagerWriteGuard::drop`, beside
/// [`resweep_deferred_new_after_class_definition`]), after the write lock is
/// gone, so the resolver may take a read lock. The eager load in
/// `SharedVm::new` runs before any application class exists and parks the
/// records it cannot resolve (`profile_store::load_into_with_policy`); this is
/// the door that seeds them when the program defines their class. It loads
/// nothing: it only reacts to definitions the program made. See
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// Cost: one relaxed load when no replay is waiting (every default run); with
/// one, one keyed registry probe and one epoch swap per write-guard release
/// and, per class definition epoch of the releasing VM, one read-locked walk
/// of its newly allocated class slots plus one pending-map probe per new
/// class.
///
/// `owner_domain` is the released class manager's layout domain, as for
/// [`resweep_deferred_new_after_class_definition`]: only the VM that owns it
/// can have new classes to seed, so only it is swept, and its latch is its
/// own store's epoch (`JitBridgeState::last_profile_replay_sweep_epoch`).
/// Before interpreter round i1 wave 21 (lane L5) one process latch on the
/// process-wide epoch re-walked every live VM whenever any VM defined a
/// class. A domain no live VM owns sweeps nothing; `None` sweeps every VM.
pub fn seed_profile_replay_after_class_definition(owner_domain: Option<u32>) {
    if !cratonvm_jit::profile_store::any_replay_pending() {
        return;
    }
    match owner_domain {
        Some(domain) => {
            if let Some(shared) = crate::vm::vm_init::hook_vm_for_domain(domain) {
                seed_profile_replay_if_definitions_moved(&shared);
            }
        }
        None => {
            for shared in crate::vm::vm_init::live_hook_vms_for_jit() {
                seed_profile_replay_if_definitions_moved(&shared);
            }
        }
    }
}

/// One VM's latched step of [`seed_profile_replay_after_class_definition`]:
/// walk the VM's new class slots unless its store's definition epoch is the
/// one it last swept at. Returns whether it walked.
pub(in crate::runtime::interpreter::jit_bridge) fn seed_profile_replay_if_definitions_moved(shared: &Arc<SharedVm>) -> bool {
    let latch = &shared.jit.bridge.last_profile_replay_sweep_epoch;
    // Stored plus one, so the default 0 reads as "never swept".
    let stamp = cratonvm_classloading::store_epochs(shared.jit.class_layout_domain)
        .class_definition_epoch()
        .wrapping_add(1);
    if latch.swap(stamp, std::sync::atomic::Ordering::AcqRel) == stamp {
        return false;
    }
    if !seed_profile_replay_for_new_classes(shared) {
        // The class manager was busy: forget the latch so the next
        // write-guard release retries. The watermark did not move, so
        // nothing defined in between is skipped.
        latch.store(0, std::sync::atomic::Ordering::Release);
    }
    true
}

/// The live bytecode length of `class_id`'s own `name`+`descriptor`, the
/// second resolver of both profile-replay seeding doors (format version 2,
/// r11-tier-profile-replay-residuals item 4). Declared methods only, like the
/// profile key: a replayed record names the method the interpreter ran, and
/// that is the declaring class's. Code attributes are decoded at definition,
/// so `code()` answers for every method that has bytecode.
///
/// Format version 3 (round 11 wave 14): the answer carries the bytecode's
/// `profile_store::bytecode_hash` too, and the same function is the save's
/// third resolver (`impl Drop for Vm`), so a record is stamped with the hash
/// of the method the saving run had and dropped at seed when the live bytes
/// differ, even at the same length.
pub(crate) fn live_method_code_len(
    cm: &crate::classloading::ClassManager,
    class_id: u32,
    name: &str,
    descriptor: &str,
) -> cratonvm_jit::profile_store::LiveCode {
    use cratonvm_jit::profile_store::LiveCode;
    let Some(class) = cm.get_class(crate::classloading::ClassId::new(class_id)) else {
        return LiveCode::Absent;
    };
    match class.find_method(name, descriptor).and_then(|m| m.code()) {
        Some(code) => LiveCode::Code {
            len: code.code.len(),
            hash: cratonvm_jit::profile_store::bytecode_hash(code.code.as_bytes()),
        },
        None => LiveCode::Absent,
    }
}

/// One VM's half of [`seed_profile_replay_after_class_definition`]. Returns
/// `false` when the class manager could not be read without blocking (the
/// watermark is then left where it was).
pub(in crate::runtime::interpreter::jit_bridge) fn seed_profile_replay_for_new_classes(shared: &Arc<SharedVm>) -> bool {
    seed_profile_replay_for_new_classes_with(shared, live_method_code_len)
}

/// [`seed_profile_replay_for_new_classes`] with the live-bytecode lookup as a
/// parameter, so a test can stand in a class with bytecode for the synthetic
/// one it can define.
pub(in crate::runtime::interpreter::jit_bridge) fn seed_profile_replay_for_new_classes_with(
    shared: &Arc<SharedVm>,
    live_code: fn(
        &crate::classloading::ClassManager,
        u32,
        &str,
        &str,
    ) -> cratonvm_jit::profile_store::LiveCode,
) -> bool {
    let store = &shared.jit.profile_store;
    let from = shared
        .jit
        .bridge
        .profile_replay_watermark
        .load(std::sync::atomic::Ordering::Acquire);
    // Names of the classes allocated since the last visit, collected under
    // one non-blocking read lock and released before any seeding: the
    // resolver below takes the class manager again, and `ReplayPending`'s
    // mutex must never be taken while this lock is held.
    let (names, upto) = {
        // `try_read_untracked`: this runs from a write-guard `Drop`, on
        // whatever thread defined a class, and a non-blocking acquisition
        // adds no wait-for edge (the lock's own doc gives the argument).
        let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
            return false;
        };
        let class_store = cm.class_store();
        // Cast: `ClassId`s are `u32` by construction (`ClassStore::next_id`).
        let upto = class_store.slot_count() as u32;
        let names: Vec<Arc<str>> = (from..upto)
            .filter_map(|slot| class_store.get(crate::classloading::ClassId::new(slot)))
            .map(|class| Arc::clone(&class.name))
            .collect();
        (names, upto)
    };
    // A resolver that could not take the lock answers `None`, which
    // `seed_pending_replay_for_class` reads as "leave the records waiting" --
    // correct, but the watermark must then not move past that class, or its
    // records would wait forever. `busy` records it.
    let busy = std::cell::Cell::new(false);
    let resolver = |name: &str| -> Option<u32> {
        let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
            busy.set(true);
            return None;
        };
        cm.find_unique_class_by_name(name).map(|cid| cid.as_u32())
    };
    // Same discipline: `Busy` puts the class's records back to wait, and
    // `busy` keeps the watermark so this class is visited again.
    let code_len = |class_id: u32, name: &str, descriptor: &str| {
        let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
            busy.set(true);
            return cratonvm_jit::profile_store::LiveCode::Busy;
        };
        live_code(&cm, class_id, name, descriptor)
    };
    for name in &names {
        let _ = cratonvm_jit::profile_store::seed_pending_replay_for_class(
            store, name, &resolver, &code_len,
        );
    }
    if busy.get() {
        return false;
    }
    // `fetch_max`: a racing visit that walked further must not be rolled back.
    shared
        .jit
        .bridge
        .profile_replay_watermark
        .fetch_max(upto, std::sync::atomic::Ordering::AcqRel);
    true
}


/// j27 wave 2 (lane NATMISC, REFLECT2-21-2 of
/// `jr21-native-shadow-refusals-on-intrinsified-calls-are-not-the-reflect-cost-20261014.md`):
/// does the single-pass splice emitter serve this call inline
/// (`cratonvm_jit::x64::spliced_static_minmax_entry`, the int/long
/// `Math`/`StrictMath` `min`/`max` CMOV, under
/// `CRATONVM_JIT_SPLICE_STATIC_MINMAX`)? Such a row is neither refused as
/// `static-intrinsic-keeps-its-own-fast-path` nor counted as unbound by the
/// "neither spliced nor direct-bound" rule, and keeps its `invoke_targets`
/// entry, which the emitter reads the triple from. Optimizing-tier splices
/// (`ir_mode`) have their own scalar-row lowering and are not asked.
pub(in crate::runtime::interpreter::jit_bridge) fn sp_splice_emits_minmax_inline(
    ir_mode: bool,
    invoke_kind: u8,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    !ir_mode
        && invoke_kind == 3
        && cratonvm_jit::x64::spliced_static_minmax_entry(class, name, descriptor).is_some()
}

pub(in crate::runtime::interpreter::jit_bridge) fn sp_splice_emits_getclass_inline(
    ir_mode: bool,
    invoke_kind: u8,
    name: &str,
    descriptor: &str,
) -> bool {
    !ir_mode && cratonvm_jit::x64::spliced_getclass_site(invoke_kind, name, descriptor)
}

pub(in crate::runtime::interpreter::jit_bridge) fn sp_splice_emits_value_of_inline(
    ir_mode: bool,
    invoke_kind: u8,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    !ir_mode && cratonvm_jit::x64::spliced_value_of_site(invoke_kind, class, name, descriptor)
}

pub(in crate::runtime::interpreter::jit_bridge) fn sp_splice_emits_string_accessor_inline(
    ir_mode: bool,
    invoke_kind: u8,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    !ir_mode
        && (invoke_kind == 0 || invoke_kind == 1 || invoke_kind == 2)
        && cratonvm_jit::x64::spliced_string_accessor_entry(class, name, descriptor).is_some()
}

pub(in crate::runtime::interpreter::jit_bridge) fn sp_splice_emits_char_scalar_inline(
    ir_mode: bool,
    invoke_kind: u8,
    class: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    !ir_mode && cratonvm_jit::x64::spliced_char_scalar_site(invoke_kind, class, name, descriptor)
}

