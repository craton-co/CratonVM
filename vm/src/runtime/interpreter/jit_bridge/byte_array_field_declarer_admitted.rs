#[allow(unused_imports)]
use super::*;

/// The second half of [`byte_array_field_row`]: the resolved field is declared
/// by the bootstrap `java/nio/ByteBuffer`, which declares exactly one field
/// named `hb`, `final byte[] hb` (JDK 25). A `java.*` class is defined by the
/// bootstrap loader only, so no other loader can supply a different
/// `java/nio/ByteBuffer`; the loader is checked all the same. The JVM's own
/// type safety then makes every value of the field a `byte[]` or null: a
/// `putfield` of it must verify against `[B`, which no other array type is
/// assignable to. Takes the class manager for reading, with nothing else of
/// this subsystem held (the phase-2 rule).
pub(in crate::runtime::interpreter::jit_bridge) fn byte_array_field_declarer_admitted(
    shared: &SharedVm,
    field: &ResolvedField,
) -> bool {
    let cm = shared.classes.class_manager.read();
    let decl = field.declaring_class_id;
    cm.get_loader_id(decl).is_some_and(|l| l.is_builtin())
        && cm
            .class_store()
            .get(decl)
            .is_some_and(|c| &*c.name == "java/nio/ByteBuffer")
}

pub(in crate::runtime::interpreter) fn resolve_inline_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    direct_bind: Option<InlineDirectBind<'_>>,
    substituted: Option<&SubstitutedOwners>,
) -> Option<cratonvm_jit::InlineSite> {
    resolve_inline_site_from(
        shared,
        requesting_class_id,
        None,
        callee_class,
        callee_method,
        callee_desc,
        0,
        direct_bind,
        false,
        substituted,
        false,
    )
}

/// Resolve a body for the **optimizing (IR) tier** to splice — a different
/// admission set from the single-pass one [`resolve_inline_site`] serves, in
/// both directions.
///
/// WIDER, because the refusals the single-pass path carries are its EMITTER's
/// limits, not resolution's: `x64::try_emit_inline_body` has no arm for `new`,
/// for an array load or store, or for `arraylength`, so the resolver refuses
/// those shapes to keep planning and emission consistent. `IrBuilder` lowers
/// all of them natively, and `new` is the entire point — an accessor that
/// allocates is exactly the body escape analysis has to see inside its caller.
///
/// NARROWER, because the IR splice re-executes the whole `invoke` on a deopt
/// (see `ir::IrInlineSite`) and because `IrBuilder` walks a relocated body with
/// no merge bookkeeping of its own. So v1 additionally refuses:
///
///  * any branch — a relocated body's merges and loop headers are computed
///    over the CALLER's code alone, and there is no second walker;
///  * `idiv`/`irem`/`ldiv`/`lrem` — the one guard `IrBuilder` emits is the
///    div-zero guard, and a guard inside a spliced region would deopt to a
///    re-execution point rather than to itself;
///  * `ldc`/`ldc2_w` — `InlineSite` records a raw `i64` where the builder needs
///    the value plus its float/double discriminator, and inventing that bit is
///    how a `long` constant becomes a `double`;
///  * more than one return, or a return that is not the last instruction;
///  * any target that is not provably monomorphic (see the `ir_mode` check on
///    the selected method) — the IR tier has no class-id guard node, so a
///    speculative splice has nothing to fall back to.
///
/// No [`SubstitutedOwners`]: the optimizing tier's inline planner asks with
/// the CONSTANT-POOL name of every site (`ir_tier`'s `ir_inline_resolver`
/// loop), never a substituted owner, so a recorded substitution could only
/// match another site's name here. The `final`-class proof it needs is asked
/// of that constant-pool class directly (the `ir_mode` check).
pub(in crate::runtime::interpreter::jit_bridge) fn ensure_ir_collection_layouts(shared: &SharedVm) {
    let vm_id = shared.vm_identity;
    if cratonvm_jit::ir_collection_layouts_cached(vm_id) {
        return;
    }
    let prev = cratonvm_jit::ir_collection_layouts_for_vm(vm_id);
    if prev.is_none_or(|l| l.object_array_class_id == 0) {
        let _ = shared.load_class_concurrent("[Ljava/lang/Object;");
    }
    if prev.is_none_or(|l| l.object_class_id == 0) {
        let _ = shared.load_class_concurrent("java/lang/Object");
    }
    let domain = shared.jit.class_layout_domain;
    let jdk_only = crate::vm::dispatch_policy(shared).is_jdk_only();
    let use_jdk_al = jdk_only
        || shared
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkArrayList);
    let use_jdk_hm = jdk_only
        || shared
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkHashCollections);

    let cm = shared.classes.class_manager.read();
    let store = cm.class_store();
    let al_uses_jdk = use_jdk_al
        || (!inline_screen_native_shadows(
            shared,
            Some(&*cm),
            "java/util/ArrayList",
            "get",
            "(I)Ljava/lang/Object;",
        ) && !inline_screen_native_shadows(
            shared,
            Some(&*cm),
            "java/util/ArrayList",
            "size",
            "()I",
        ));
    let hm_uses_jdk = use_jdk_hm
        || !inline_screen_native_shadows(
            shared,
            Some(&*cm),
            "java/util/HashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        );
    let object_array_class_id = prev
        .map(|l| l.object_array_class_id)
        .filter(|&id| id != 0)
        .unwrap_or_else(|| {
            cm.loaded_class_under_exact_key(
                "[Ljava/lang/Object;",
                cratonvm_types::ClassLoaderId::Bootstrap,
            )
            .or_else(|| cm.find_bootstrap_class_by_name("[Ljava/lang/Object;"))
            .map_or(0, |id| id.as_u32())
        });
    let object_class_id = prev
        .map(|l| l.object_class_id)
        .filter(|&id| id != 0)
        .unwrap_or_else(|| {
            cm.loaded_class_under_exact_key(
                "java/lang/Object",
                cratonvm_types::ClassLoaderId::Bootstrap,
            )
            .or_else(|| cm.find_bootstrap_class_by_name("java/lang/Object"))
            .map_or(0, |id| id.as_u32())
        });

    let array_list = prev.and_then(|l| l.array_list).or_else(|| {
        if al_uses_jdk {
            (|| {
                let al_id = cm.find_bootstrap_class_by_name("java/util/ArrayList")?;
                let (ed_idx, _, ed_decl) = crate::classloading::find_field_recursive_by_descriptor(
                    al_id,
                    "elementData",
                    "[Ljava/lang/Object;",
                    store,
                )?;
                let (sz_idx, _, sz_decl) = crate::classloading::find_field_recursive_by_descriptor(
                    al_id, "size", "I", store,
                )?;
                let ed_off =
                    cratonvm_types::compact_field_disp_storage_in(domain, ed_decl.as_u32(), ed_idx)
                        .filter(|(_, s)| s.is_reference())
                        .and_then(|(o, _)| u32::try_from(o).ok())
                        .unwrap_or(u32::MAX);
                let sz_off =
                    cratonvm_types::compact_field_disp_storage_in(domain, sz_decl.as_u32(), sz_idx)
                        .filter(|(_, s)| matches!(s, cratonvm_types::FieldStorageKind::Int))
                        .and_then(|(o, _)| u32::try_from(o).ok())
                        .unwrap_or(u32::MAX);
                Some(cratonvm_jit::IrArrayListLayout {
                    class_id: al_id.as_u32(),
                    element_data_field_index: u32::try_from(ed_idx).ok()?,
                    element_data_compact_offset: ed_off,
                    size_field_index: u32::try_from(sz_idx).ok()?,
                    size_compact_offset: sz_off,
                })
            })()
        } else {
            None
        }
    });

    let hash_map = prev.and_then(|l| l.hash_map).or_else(|| {
        if hm_uses_jdk && cratonvm_types::compact_ref_fields_enabled() {
            (|| {
                let hm_id = cm.find_bootstrap_class_by_name("java/util/HashMap")?;
                let node_id = cm.find_bootstrap_class_by_name("java/util/HashMap$Node")?;
                let int_id = cm.find_bootstrap_class_by_name("java/lang/Integer")?;
                let (tab_idx, _, tab_decl) =
                    crate::classloading::find_field_recursive_by_descriptor(
                        hm_id,
                        "table",
                        "[Ljava/util/HashMap$Node;",
                        store,
                    )?;
                let (nh_idx, _, nh_decl) = crate::classloading::find_field_recursive_by_descriptor(
                    node_id, "hash", "I", store,
                )?;
                let (nk_idx, _, nk_decl) = crate::classloading::find_field_recursive_by_descriptor(
                    node_id,
                    "key",
                    "Ljava/lang/Object;",
                    store,
                )?;
                let (nv_idx, _, nv_decl) = crate::classloading::find_field_recursive_by_descriptor(
                    node_id,
                    "value",
                    "Ljava/lang/Object;",
                    store,
                )?;
                let (iv_idx, _, iv_decl) = crate::classloading::find_field_recursive_by_descriptor(
                    int_id, "value", "I", store,
                )?;
                if tab_decl != hm_id
                    || nh_decl != node_id
                    || nk_decl != node_id
                    || nv_decl != node_id
                    || iv_decl != int_id
                {
                    return None;
                }
                let ref_off = |cid: ClassId, idx: usize| {
                    cratonvm_types::compact_field_disp_storage_in(domain, cid.as_u32(), idx)
                        .filter(|(_, s)| s.is_reference())
                        .and_then(|(o, _)| i32::try_from(o).ok())
                };
                let int_off = |cid: ClassId, idx: usize| {
                    cratonvm_types::compact_field_disp_storage_in(domain, cid.as_u32(), idx)
                        .filter(|(_, s)| matches!(s, cratonvm_types::FieldStorageKind::Int))
                        .and_then(|(o, _)| i32::try_from(o).ok())
                };
                Some(cratonvm_jit::IrHashMapLayout {
                    hashmap_class_id: hm_id.as_u32(),
                    table_field_index: u32::try_from(tab_idx).ok()?,
                    table_compact_offset: ref_off(hm_id, tab_idx)?,
                    node_class_id: node_id.as_u32(),
                    node_hash_field_index: u32::try_from(nh_idx).ok()?,
                    node_hash_compact_offset: int_off(node_id, nh_idx)?,
                    node_key_field_index: u32::try_from(nk_idx).ok()?,
                    node_key_compact_offset: ref_off(node_id, nk_idx)?,
                    node_value_field_index: u32::try_from(nv_idx).ok()?,
                    node_value_compact_offset: ref_off(node_id, nv_idx)?,
                    integer_class_id: int_id.as_u32(),
                    integer_value_field_index: u32::try_from(iv_idx).ok()?,
                    integer_value_compact_offset: int_off(int_id, iv_idx)?,
                })
            })()
        } else {
            None
        }
    });

    cratonvm_jit::register_ir_collection_layouts_with_objects(
        vm_id,
        array_list,
        hash_map,
        object_array_class_id,
        object_class_id,
    );
}

fn ir_collection_site_kept_as_call_or_intrinsic(
    shared: &SharedVm,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> bool {
    if (callee_class == "java/lang/StringBuilder"
        || callee_class == "java/lang/AbstractStringBuilder")
        && cratonvm_jit::ir_sb_prefix_enabled()
        && ((callee_method == "setLength" && callee_desc == "(I)V")
            || (callee_method == "length" && callee_desc == "()I")
            || (callee_method == "append" && callee_desc == "(C)Ljava/lang/StringBuilder;"))
    {
        return true;
    }
    if (callee_class == "java/util/HashMap" || callee_class == "java/util/Map")
        && callee_method == "get"
        && callee_desc == "(Ljava/lang/Object;)Ljava/lang/Object;"
        && cratonvm_jit::ir_hashmap_get_inline_enabled()
    {
        ensure_ir_collection_layouts(shared);
        return true;
    }
    if (callee_class == "java/util/ArrayList" || callee_class == "java/util/List")
        && ((callee_method == "get" && callee_desc == "(I)Ljava/lang/Object;")
            || (callee_method == "size" && callee_desc == "()I"))
    {
        ensure_ir_collection_layouts(shared);
        if cratonvm_jit::ir_collection_layouts_current()
            .and_then(|l| l.array_list)
            .is_some()
        {
            return true;
        }
    }
    false
}

pub(in crate::runtime::interpreter) fn resolve_ir_inline_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    direct_bind: Option<InlineDirectBind<'_>>,
) -> Option<cratonvm_jit::InlineSite> {
    if ir_collection_site_kept_as_call_or_intrinsic(
        shared,
        callee_class,
        callee_method,
        callee_desc,
    ) {
        return None;
    }
    resolve_inline_site_from(
        shared,
        requesting_class_id,
        None,
        callee_class,
        callee_method,
        callee_desc,
        0,
        direct_bind,
        true,
        None,
        false,
    )
}

/// Round 12 wave 4 (lane iropt3, W3-1): the body a receiver of EXACTLY the
/// constant-pool class `callee_class` dispatches to at an `invokevirtual`
/// site, for the optimizing tier to splice with no receiver guard, and that
/// class's id.
///
/// The IR has no receiver guard, so this answer is only sound together with
/// its consumer: `IrBuilder::exact_receiver_proof` splices the body only when
/// the receiver is provably an object of exactly this class (a `new` of it,
/// directly or through a local the method writes once), and builds the
/// ordinary call otherwise. Resolution starts the JVMS selection walk at the
/// class itself (`receiver_class_id = Some`), so every receiver-dispatch
/// faithfulness check applies.
///
/// The class is marked as copied under the guard it was looked up under, so
/// that a redefinition of it takes the full flush. That is a second line of
/// defence. The first is that a redefinition cannot change its method set.
pub(in crate::runtime::interpreter) fn resolve_ir_exact_receiver_inline_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    direct_bind: Option<InlineDirectBind<'_>>,
) -> Option<(cratonvm_jit::InlineSite, u32)> {
    if ir_collection_site_kept_as_call_or_intrinsic(
        shared,
        callee_class,
        callee_method,
        callee_desc,
    ) {
        return None;
    }
    let receiver = {
        let cm = shared.classes.class_manager.read();
        let id = jit_known_class(shared, &cm, callee_class, requesting_class_id)?;
        shared.jit.jit_cache.note_bytecode_copied(id);
        id
        // `cm` drops here: `resolve_inline_site_from` takes the lock itself.
    };
    let site = resolve_inline_site_from(
        shared,
        requesting_class_id,
        Some(receiver),
        callee_class,
        callee_method,
        callee_desc,
        0,
        direct_bind,
        true,
        None,
        false,
    )?;
    // JIT round 23 wave 6 (`CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE`, opt-in):
    // when every loaded subtype of the constant-pool class selects the very
    // body just resolved, hand the planner the class-id watermark it may guard
    // the splice with (`ClassRealm::cha_unique_target_watermark`). The body's
    // declaring class must be that unique target; `0` (an unknown declaring
    // id) notes nothing.
    if cratonvm_jit::ir::ir_cha_monomorphic_splice_enabled() && site.class_id != 0 {
        if let Some((target, watermark)) =
            shared
                .classes
                .cha_unique_target_watermark(receiver, callee_method, callee_desc)
        {
            if target.as_u32() == site.class_id {
                cratonvm_jit::ir::note_cha_watermark(watermark);
            }
        }
    }
    Some((site, receiver.as_u32()))
}

/// Round 13 wave 12 (lane guardsplice, W7-1 of the round-12 iropt book): the
/// body a receiver of EXACTLY `receiver_class_id` -- the class the call site's
/// receiver profile says dominates it -- dispatches to at an `invokevirtual` /
/// `invokeinterface` declared `(cp_class, callee_method, callee_desc)`, for the
/// optimizing tier to splice behind an exact class test whose miss edge is the
/// site's own call (`IrBuilder::begin_guarded_receiver_splice`).
///
/// [`resolve_receiver_inline_site`]'s contract (the JVMS selection walk from
/// the receiver, every dispatch-faithfulness check, the constant-pool class
/// kept as the constant-pool class) under the optimizing tier's admission set
/// (`ir_mode`). The receiver class is marked as copied into the open compile
/// here, and the body's declaring class by the core: those marks are the
/// dependency a redefinition, and an unload
/// (`CRATONVM_JIT_UNLOAD_WITHDRAWS_COPIED`), withdraws the artifact by. A
/// profile class may live in a loader that dies before the caller's.
pub(in crate::runtime::interpreter) fn resolve_ir_profile_receiver_inline_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    receiver_class_id: u32,
    cp_class: &str,
    callee_method: &str,
    callee_desc: &str,
    direct_bind: Option<InlineDirectBind<'_>>,
) -> Option<cratonvm_jit::InlineSite> {
    if ir_collection_site_kept_as_call_or_intrinsic(shared, cp_class, callee_method, callee_desc) {
        return None;
    }
    if receiver_class_id == 0 {
        return None;
    }
    let receiver = ClassId::new(receiver_class_id);
    {
        // Under the guard, as the exact-receiver resolvers mark theirs.
        let _cm = shared.classes.class_manager.read();
        shared.jit.jit_cache.note_bytecode_copied(receiver);
        // `_cm` drops here: `resolve_inline_site_from` takes the lock itself.
    }
    // JIT round 23 wave 1, lane L2 (INLINE22-4): a lambda proxy receiver's
    // SAM call is answered by its forwarding body, not refused
    // (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE`). Asked before the class-manager
    // guard, as the refusal it replaces is (`lambda_proxies` lock order).
    if cratonvm_jit::ir::ir_lambda_sam_splice_enabled()
        && shared.classes.is_lambda_proxy_class(receiver)
    {
        return resolve_ir_lambda_sam_site(
            shared,
            requesting_class_id,
            receiver,
            cp_class,
            callee_method,
            callee_desc,
            direct_bind,
            0,
            false,
            cratonvm_jit::ir::splice_site_objects()
                .first()
                .copied()
                .flatten(),
        );
    }
    let mut site = resolve_inline_site_from(
        shared,
        requesting_class_id,
        Some(receiver),
        cp_class,
        callee_method,
        callee_desc,
        0,
        direct_bind,
        true,
        None,
        false,
    );
    let mut site = site?;
    // j27 wave 5, lane PROXY (`CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`,
    // opt-in): an interface site's other exact classes selecting this body,
    // carried on the answer (`InlineSite::cha_class_set`).
    site.cha_class_set = profile_class_set::profile_interface_class_set(
        shared,
        requesting_class_id,
        receiver,
        cp_class,
        callee_method,
        callee_desc,
        site.class_id,
    );
    Some(site)
}

/// JIT round 23 wave 1, lane L2 (INLINE22-4's census, RUNTIME22-3): one
/// `[cratonvm-jitc] lambda-proxy-census` line per optimizing / single-pass
/// resolution whose receiver is lambda proxy `proxy`, naming the CALLER's
/// class (the class whose method holds the site), the proxy, the site, the
/// tier and nesting depth, and what became of it. `grep lambda-proxy-census |
/// sort | uniq -c` is the per-caller count the proposal asks for. Only under
/// `CRATONVM_DBG_JITC`; never with the class manager held by the caller.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn note_lambda_proxy_census(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    proxy: ClassId,
    site_class: &str,
    site_method: &str,
    site_desc: &str,
    tier: &str,
    depth: usize,
    verdict: &str,
) {
    let caller = shared
        .classes
        .class_manager
        .read()
        .get_class(requesting_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("<class {}>", requesting_class_id.as_u32()));
    let implementation = shared
        .classes
        .lambda_call_site_for(proxy)
        .map(|s| {
            format!(
                "{}.{}{}",
                s.impl_handle.class_name, s.impl_handle.member_name, s.impl_handle.descriptor
            )
        })
        .unwrap_or_else(|| "?".to_string());
    eprintln!(
        "[cratonvm-jitc] lambda-proxy-census caller={caller} site={site_class}.{site_method}{site_desc} \
         proxy={} impl={implementation} tier={tier} depth={depth}: {verdict}",
        proxy.as_u32(),
    );
}

/// The nested-body size cap of a lambda SAM splice
/// ([`resolve_ir_lambda_sam_site`]): the implementation is spliced only when
/// its own bytecode is at most this many bytes. The hot cap
/// (`cratonvm_jit::MAX_INLINE_BYTECODE_SIZE`): since JIT round 23 wave 2 (lane
/// L2, L2W1-2) the planner charges the forwarder AND its implementation against
/// the site's own cap -- cold, or hot for a profile-hot site
/// (`cratonvm_jit` `ir_guarded_body_bytes`) -- which this resolver cannot see.
pub(in crate::runtime::interpreter::jit_bridge) const LAMBDA_SAM_IMPL_MAX_BYTES: usize =
    cratonvm_jit::MAX_INLINE_BYTECODE_SIZE;

/// One call a lambda proxy's spun forwarding code makes
/// ([`lambda_forwarder_rows`]).
#[derive(Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) struct LambdaForwarderCall {
    pub(in crate::runtime::interpreter::jit_bridge) pc: usize,
    pub(in crate::runtime::interpreter::jit_bridge) op: u8,
    pub(in crate::runtime::interpreter::jit_bridge) owner: String,
    pub(in crate::runtime::interpreter::jit_bridge) name: String,
    pub(in crate::runtime::interpreter::jit_bridge) desc: String,
    /// For a conversion's `invokevirtual` (`Number.intValue` & co.): the final
    /// box class the `checkcast` right before it proved the receiver to be;
    /// `None` for an unboxing through `Number` (wave 3: kept as a virtual
    /// call row).
    pub(in crate::runtime::interpreter::jit_bridge) exact_receiver: Option<String>,
    /// The instruction's length: 3, or 5 for an `invokeinterface`.
    pub(in crate::runtime::interpreter::jit_bridge) len: usize,
}

/// What [`lambda_forwarder_rows`] reads off a lambda proxy's spun forwarding
/// code (JIT round 23 wave 1, lane L2, INLINE22-4; conversions since wave 2).
#[derive(Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) struct LambdaForwarderRows {
    /// `(pc, capture index, descriptor byte)` per `getfield arg$N`.
    pub(in crate::runtime::interpreter::jit_bridge) field_info: Vec<(usize, usize, u8)>,
    /// `(pc, class internal name)` per `checkcast`.
    pub(in crate::runtime::interpreter::jit_bridge) casts: Vec<(usize, String)>,
    /// The implementation's call: its pc, opcode, owner, name and descriptor.
    pub(in crate::runtime::interpreter::jit_bridge) call_pc: usize,
    pub(in crate::runtime::interpreter::jit_bridge) call_op: u8,
    pub(in crate::runtime::interpreter::jit_bridge) owner: String,
    pub(in crate::runtime::interpreter::jit_bridge) impl_name: String,
    pub(in crate::runtime::interpreter::jit_bridge) impl_desc: String,
    /// LambdaMetafactory's boxing / unboxing calls around it
    /// (`TypeConvertingMethodAdapter.box` / `unbox`), in code order. Non-empty
    /// makes the forwarder OPAQUE to the deopt chain
    /// (`cratonvm_jit::ir` `SpliceFrame::opaque`).
    pub(in crate::runtime::interpreter::jit_bridge) conversions: Vec<LambdaForwarderCall>,
    /// The implementation's call is followed directly by the return: nothing
    /// converts its result.
    pub(in crate::runtime::interpreter::jit_bridge) returns_call_result: bool,
    /// JIT round 23 wave 3: a constructor reference's `new` (`(pc, class
    /// internal name)`), followed by its `dup`; the call is the `<init>`.
    pub(in crate::runtime::interpreter::jit_bridge) news: Vec<(usize, String)>,
    /// JIT round 23 wave 4: a `MethodHandle.invokeExact` forwarder's `ldc` of
    /// the class data (`CONSTANT_Dynamic`, `MethodHandles.classData`), as
    /// `(pc, length)`; the call at `call_pc` is then `invokeExact` of that
    /// handle, which is the implementation itself (a direct handle).
    pub(in crate::runtime::interpreter::jit_bridge) handle_ldc: Option<(usize, usize)>,
}

/// The box classes LambdaMetafactory converts through: the final classes
/// whose `valueOf` / `xxxValue` a forwarder may call.
pub(in crate::runtime::interpreter::jit_bridge) fn lambda_box_class(name: &str) -> bool {
    matches!(
        name,
        "java/lang/Integer"
            | "java/lang/Long"
            | "java/lang/Boolean"
            | "java/lang/Short"
            | "java/lang/Byte"
            | "java/lang/Character"
            | "java/lang/Float"
            | "java/lang/Double"
    )
}

/// Is `(op, owner, name, desc)` one of `TypeConvertingMethodAdapter`'s
/// conversion calls: a box class's `valueOf(p)` (`invokestatic`) or an
/// `xxxValue()` of `Number` / `Boolean` / `Character` (`invokevirtual`)?
pub(in crate::runtime::interpreter::jit_bridge) fn lambda_conversion_call(
    op: u8,
    owner: &str,
    name: &str,
    desc: &str,
) -> bool {
    match op {
        0xb8 => {
            name == "valueOf"
                && lambda_box_class(owner)
                && desc.len() == 5 + owner.len()
                && desc.starts_with('(')
                && desc.ends_with(&format!(")L{owner};"))
        }
        0xb6 => {
            matches!(
                owner,
                "java/lang/Number" | "java/lang/Boolean" | "java/lang/Character"
            ) && name.ends_with("Value")
                && desc.len() == 3
                && desc.starts_with("()")
        }
        _ => false,
    }
}

/// The rows of the forwarding method `code` of a lambda proxy's spun class
/// (`pool`, entry 1 first; `invoke_at`, the implementation call's pc, from
/// `lambda_proxy::SpunClass::invoke_at`; `num_captures` capture fields), or
/// why the lambda SAM splice ([`resolve_ir_lambda_sam_site`]) refuses it.
///
/// Admitted: loads, `aload_0; getfield arg$N`, primitive widenings and
/// non-array `checkcast`s, LambdaMetafactory's conversion calls
/// ([`lambda_conversion_call`]; an unboxing `invokevirtual` only on a receiver
/// a `checkcast` right before it proved to be a final box class), exactly one
/// implementation call (`invokestatic` / `invokevirtual` of a `Methodref`) at
/// `invoke_at`, and the closing return. A forwarder with no conversion call
/// must return the implementation's result untouched (wave 1's transparent
/// shape: the deopt chain skips its frame); one with conversions may convert
/// the result too (it is opaque). Anything else -- `new` / `dup` (a
/// constructor reference), `ldc` (a `MethodHandle.invokeExact` forwarder),
/// `invokeinterface`, `wide`, an unboxing through `Number` -- is refused by
/// name.
///
/// The default screen (no array cast): the resolver itself calls
/// [`lambda_forwarder_rows_with`] with its switch, so outside the unit tests
/// nothing calls this one.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::runtime::interpreter::jit_bridge) fn lambda_forwarder_rows(
    code: &[u8],
    pool: &[crate::runtime::lambda_proxy::PoolEntry],
    invoke_at: usize,
    num_captures: usize,
) -> Result<LambdaForwarderRows, String> {
    lambda_forwarder_rows_with(code, pool, invoke_at, num_captures, false)
}

/// [`lambda_forwarder_rows`], with `admit_array_casts`
/// (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_ARRAY_CASTS`; t1 wave 11, lane JLAM,
/// LAMBDA221-2's remainder): an ARRAY `checkcast` (`Function<String[], R>`'s
/// bridge cast) becomes a typecheck row like a class cast, instead of
/// refusing the forwarder. The resolver resolves its class in the host's
/// loader exactly as the general splice's typecheck rows do (an array class
/// not created yet refuses, `cast class ... not loaded`), and the jit interns
/// it with the method's own array-cast rows' identity, so a failing cast
/// traps and re-executes the `invokeinterface` and the interpreter raises
/// HotSpot's `ClassCastException`. An array cast is never a box class, so it
/// never proves an unboxing receiver.
pub(in crate::runtime::interpreter::jit_bridge) fn lambda_forwarder_rows_with(
    code: &[u8],
    pool: &[crate::runtime::lambda_proxy::PoolEntry],
    invoke_at: usize,
    num_captures: usize,
    admit_array_casts: bool,
) -> Result<LambdaForwarderRows, String> {
    use crate::runtime::lambda_proxy::PoolEntry;
    let entry = |i: u16| pool.get(usize::from(i).checked_sub(1)?);
    let utf8 = |i: u16| match entry(i) {
        Some(PoolEntry::Utf8(s)) => Some(s.as_str()),
        _ => None,
    };
    let class_of = |i: u16| match entry(i) {
        Some(PoolEntry::Class(n)) => utf8(*n),
        _ => None,
    };
    let name_and_type = |i: u16| match entry(i) {
        Some(PoolEntry::NameAndType(n, t)) => Some((utf8(*n)?, utf8(*t)?)),
        _ => None,
    };
    let operand = |pc: usize| -> Option<u16> {
        Some(u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]))
    };
    let mut field_info: Vec<(usize, usize, u8)> = Vec::new();
    let mut casts: Vec<(usize, String)> = Vec::new();
    let mut calls: Vec<LambdaForwarderCall> = Vec::new();
    let mut news: Vec<(usize, String)> = Vec::new();
    let mut handle_ldc: Option<(usize, usize)> = None;
    // The class the instruction just before `pc` cast to (`checkcast` only).
    let mut last_cast: Option<(usize, String)> = None;
    let mut return_pc: Option<usize> = None;
    let mut pc = 0usize;
    while pc < code.len() {
        if return_pc.is_some() {
            return Err("code after the return".to_string());
        }
        let op = code[pc];
        let cast_before = last_cast
            .take()
            .filter(|(end, _)| *end == pc)
            .map(|(_, c)| c);
        let len = match op {
            // `iload_0` .. `aload_3`.
            0x1a..=0x2d => 1,
            // `iload` .. `aload` with a one-byte index.
            0x15..=0x19 => 2,
            // `i2l` .. `d2f`: a widening.
            0x85..=0x90 => 1,
            // `getfield arg$N`: capture N - 1, a final slot of the proxy.
            0xb4 => {
                let Some((name, desc)) = operand(pc)
                    .and_then(|i| match entry(i) {
                        Some(PoolEntry::Fieldref(_, nt)) => Some(*nt),
                        _ => None,
                    })
                    .and_then(name_and_type)
                else {
                    return Err("an unmodelled field read".to_string());
                };
                let Some(n) = name
                    .strip_prefix("arg$")
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| (1..=num_captures).contains(n))
                else {
                    return Err("a field read that is no capture".to_string());
                };
                let Some(&tag) = desc.as_bytes().first() else {
                    return Err("a capture with no descriptor".to_string());
                };
                field_info.push((pc, n - 1, tag));
                3
            }
            0xc0 => {
                let Some(name) = operand(pc).and_then(class_of) else {
                    return Err("an unmodelled cast".to_string());
                };
                if name.starts_with('[') && !admit_array_casts {
                    return Err("an array cast".to_string());
                }
                casts.push((pc, name.to_string()));
                last_cast = Some((pc + 3, name.to_string()));
                3
            }
            // JIT round 23 wave 4: the class-data `ldc` of a
            // `MethodHandle.invokeExact` forwarder (`useImplMethodHandle`: a
            // protected implementation in another package, a `super::`
            // reference) -- the forwarder's first instruction, a
            // `CONSTANT_Dynamic`, nothing else.
            0x12 | 0x13 => {
                let len = if op == 0x12 { 2 } else { 3 };
                let index = if op == 0x12 {
                    code.get(pc + 1).map(|&b| u16::from(b))
                } else {
                    operand(pc)
                };
                let dynamic =
                    index.is_some_and(|i| matches!(entry(i), Some(PoolEntry::Dynamic(..))));
                if pc != 0 || !dynamic || handle_ldc.is_some() {
                    return Err("an `ldc` that is no class-data handle".to_string());
                }
                handle_ldc = Some((pc, len));
                len
            }
            // JIT round 23 wave 3: a constructor reference's `new C; dup`,
            // before the call only.
            0xbb => {
                if !calls.is_empty() || !news.is_empty() {
                    return Err("a `new` after a call, or a second one".to_string());
                }
                let Some(name) = operand(pc).and_then(class_of) else {
                    return Err("an unmodelled `new`".to_string());
                };
                news.push((pc, name.to_string()));
                3
            }
            0x59 => {
                if news.last().is_none_or(|&(at, _)| at + 3 != pc) {
                    return Err("a `dup` that is not the `new`'s".to_string());
                }
                1
            }
            0xb8 | 0xb6 | 0xb7 | 0xb9 => {
                // `invokespecial` and `invokeinterface` only as the
                // implementation's call (a constructor; an interface method
                // reference), never as a conversion.
                if matches!(op, 0xb7 | 0xb9) && pc != invoke_at {
                    return Err(format!("opcode 0x{op:02x} that is no implementation call"));
                }
                let Some((owner, name, desc)) = operand(pc).and_then(|i| match entry(i) {
                    Some(PoolEntry::Methodref(c, nt)) if op != 0xb9 => {
                        let (name, desc) = name_and_type(*nt)?;
                        Some((class_of(*c)?, name, desc))
                    }
                    Some(PoolEntry::InterfaceMethodref(c, nt)) if op == 0xb9 => {
                        let (name, desc) = name_and_type(*nt)?;
                        Some((class_of(*c)?, name, desc))
                    }
                    _ => None,
                }) else {
                    return Err("an unmodelled call".to_string());
                };
                let exact_receiver = if pc == invoke_at {
                    None
                } else if !lambda_conversion_call(op, owner, name, desc) {
                    return Err(format!(
                        "a call that is no conversion ({owner}.{name}{desc})"
                    ));
                } else if op == 0xb6 {
                    // Wave 3: an unboxing whose receiver no box-class cast
                    // proves (an erased `Object` through `Number`) is kept as
                    // a virtual call row by the resolver.
                    cast_before.filter(|c| lambda_box_class(c))
                } else {
                    None
                };
                let len = if op == 0xb9 { 5 } else { 3 };
                calls.push(LambdaForwarderCall {
                    pc,
                    op,
                    owner: owner.to_string(),
                    name: name.to_string(),
                    desc: desc.to_string(),
                    exact_receiver,
                    len,
                });
                len
            }
            0xac..=0xb1 => {
                if pc + 1 != code.len() {
                    return Err("a return that is not the last instruction".to_string());
                }
                return_pc = Some(pc);
                1
            }
            other => return Err(format!("opcode 0x{other:02x}")),
        };
        pc += len;
    }
    let Some(return_pc) = return_pc else {
        return Err("no return".to_string());
    };
    let Some(at) = calls.iter().position(|c| c.pc == invoke_at) else {
        return Err("no implementation call".to_string());
    };
    let implementation = calls.remove(at);
    let returns_call_result = implementation.pc + implementation.len == return_pc;
    if calls.is_empty() && news.is_empty() && !returns_call_result {
        return Err("a conversion of the result with no conversion call".to_string());
    }
    if !news.is_empty() && (implementation.op != 0xb7 || implementation.name != "<init>") {
        return Err("a `new` whose call is no constructor".to_string());
    }
    // Wave 4: a class-data handle is spent only by its `invokeExact`.
    let handle_call = implementation.op == 0xb6
        && implementation.owner == "java/lang/invoke/MethodHandle"
        && implementation.name == "invokeExact";
    if handle_ldc.is_some() != handle_call {
        return Err("a class-data handle without its `invokeExact`, or the reverse".to_string());
    }
    Ok(LambdaForwarderRows {
        field_info,
        casts,
        call_pc: implementation.pc,
        call_op: implementation.op,
        owner: implementation.owner,
        impl_name: implementation.name,
        impl_desc: implementation.desc,
        conversions: calls,
        returns_call_result,
        news,
        handle_ldc,
    })
}

/// JIT round 23 wave 1, lane L2 (INLINE22-4 = RUNTIME22-3,
/// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE`): the body the optimizing tier splices
/// for an `invokeinterface` / `invokevirtual` whose profiled receiver is
/// lambda proxy `proxy`, behind the guarded splice's `ExactClassIs(proxy)`
/// (proxy ids are per VM and never reused while the proxy lives; the miss edge
/// is the site's own call).
///
/// The body is the proxy's SAM forwarding method EXACTLY as HotSpot's
/// `InnerClassLambdaMetafactory` spins it (`lambda_proxy::
/// lambda_proxy_spun_class_of`, the model JVMTI's `GetBytecodes` already
/// answers from): `aload_0; getfield arg$N` per capture, the SAM's argument
/// loads with their `checkcast`s and primitive widenings, the implementation's
/// call, the `return`. Its rows are the resolver's own: a capture read is a
/// `final` field of slot `N - 1` (a proxy is always a uniform legacy-cell
/// object, `cratonvm_jit::lambda_adapter`), a cast's class is resolved in the
/// host's loader, and the call is a nested splice of the implementation
/// resolved as the host's own constant-pool call would be.
///
/// Stage 1 admits only the shapes whose frame the deopt chain may skip
/// (`ir::IrBuilder::splice_scope_chain`, `SpliceFrame::frameless`):
///
///  * the implementation is a `REF_invokeStatic` method, or a private
///    instance method of the host (`REF_invokeSpecial`, which the spun class
///    calls with `invokevirtual` on its captured `this`) -- no virtual or
///    interface dispatch, no constructor, no `MethodHandle.invokeExact`
///    (a protected implementation in another package);
///  * nothing after the call but the `return` of its value: no boxing,
///    unboxing, widening or cast of the result, and the implementation's
///    return descriptor equals the SAM's (both references counts as equal:
///    no conversion is spun between two reference types it does not cast);
///  * the implementation splices (no call is left behind in a frameless body)
///    and is at most [`LAMBDA_SAM_IMPL_MAX_BYTES`] bytes;
///  * the call names the functional method or one of its bridges (any other
///    method of a proxy -- `equals`, a default method -- stays refused);
///  * no C JVMTI env lists proxy frames (`lambda_proxy_frames_armed`).
///
/// Every refusal prints a `lambda-proxy-census` line under
/// `CRATONVM_DBG_JITC` ([`note_lambda_proxy_census`]).
/// The implementation's argument slots a lambda's captures fill, as `(exact
/// class, frozen object)` per slot: `target(i)` is capture `i`'s, from the
/// snapshot; a `long` / `double` capture takes two slots.
pub(in crate::runtime::interpreter::jit_bridge) fn lambda_capture_slots(
    capture_types: &[char],
    target: impl Fn(usize) -> Option<(u32, usize)>,
) -> (Vec<Option<u32>>, Vec<Option<usize>>) {
    let (mut classes, mut objects) = (Vec::new(), Vec::new());
    for (i, &kind) in capture_types.iter().enumerate() {
        let known = if kind == 'L' { target(i) } else { None };
        classes.push(known.map(|(class, _)| class));
        objects.push(known.map(|(_, addr)| addr));
        if matches!(kind, 'J' | 'D') {
            classes.push(None);
            objects.push(None);
        }
    }
    (classes, objects)
}

pub(in crate::runtime::interpreter::jit_bridge) fn resolve_ir_lambda_sam_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    proxy: ClassId,
    cp_class: &str,
    callee_method: &str,
    callee_desc: &str,
    direct_bind: Option<InlineDirectBind<'_>>,
    // 0 for the compiling method's own guarded site; wave 2d (L2W1-3): the
    // depth of a site inside a spliced body (`resolve_inline_site_from`'s
    // nested routes), whose receiver the builder proves the proxy.
    nest_depth: usize,
    planning_only: bool,
    // `CRATONVM_FROZEN_PAGES`: the frozen proxy object the call is expected
    // on, whose captures the implementation's arguments then are.
    receiver_object: Option<usize>,
) -> Option<cratonvm_jit::InlineSite> {
    use crate::classloading::resolution::MethodHandleKind as K;
    // The planner's operand classes for this site are the constant-pool
    // route's; this body reads none of them (and must not leave them for the
    // nested resolution, whose operands are the forwarder's).
    let _ = cratonvm_jit::ir::take_splice_arg_classes();
    let _ = cratonvm_jit::ir::take_splice_arg_objects();
    let census = |verdict: &str| {
        if crate::runtime::env_cache::dbg_jitc() {
            note_lambda_proxy_census(
                shared,
                requesting_class_id,
                proxy,
                cp_class,
                callee_method,
                callee_desc,
                "ir",
                nest_depth,
                verdict,
            );
        }
    };
    macro_rules! refuse {
        ($why:expr) => {{
            census(&format!("refused ({})", $why));
            return None;
        }};
    }
    if super::lambda::lambda_proxy_frames_armed(shared) {
        refuse!("lambda-sam: a C JVMTI env lists proxy frames");
    }
    let Some(site) = shared.classes.lambda_call_site_for(proxy) else {
        refuse!("lambda-sam: no call site for the proxy");
    };
    if *site.sam_method_name != *callee_method {
        refuse!("lambda-sam: not the functional method");
    }
    let Some(facts) = crate::runtime::lambda_proxy::lambda_proxy_facts(shared, proxy) else {
        refuse!("lambda-sam: no proxy facts");
    };
    let Some(host_id) = facts.host_id else {
        refuse!("lambda-sam: host class unknown");
    };
    // The forwarding method this call enters: the functional method (index 1)
    // or an `altMetafactory` bridge, by name AND descriptor.
    let Some(index) =
        facts.methods.iter().enumerate().position(|(i, m)| {
            i >= 1 && *m.name == *callee_method && *m.descriptor == *callee_desc
        })
    else {
        refuse!("lambda-sam: no forwarding method of this descriptor");
    };
    let Some(method) = facts.methods.get(index) else {
        refuse!("lambda-sam: no forwarding method of this descriptor");
    };
    // `writeReplace` / `readObject` / `writeObject` are private; the
    // functional method and its bridges public.
    if method.access_flags & 0x0001 == 0 {
        refuse!("lambda-sam: not a public forwarding method");
    }
    let Some(spun) = crate::runtime::lambda_proxy::lambda_proxy_spun_class_of(shared, proxy) else {
        refuse!("lambda-sam: the spun class cannot be modelled");
    };
    let (Some(code), Some(Some(invoke_at))) = (spun.codes.get(index), spun.invoke_at.get(index))
    else {
        refuse!("lambda-sam: no forwarding code");
    };
    let code: &[u8] = code;
    let invoke_at = *invoke_at;
    let rows = match lambda_forwarder_rows_with(
        code,
        &spun.pool,
        invoke_at,
        site.capture_types.len(),
        cratonvm_jit::ir::ir_lambda_sam_splice_array_casts_enabled(),
    ) {
        Ok(rows) => rows,
        Err(why) => refuse!(format!("lambda-sam: {why}")),
    };
    let LambdaForwarderRows {
        field_info,
        casts,
        call_pc,
        call_op,
        owner,
        impl_name,
        impl_desc,
        conversions,
        returns_call_result,
        news,
        handle_ldc,
    } = rows;
    let handle = &site.impl_handle;
    // JIT round 23 wave 4 (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_HANDLE_FORWARDERS`):
    // an `invokeExact` forwarder (`useImplMethodHandle`: a protected
    // implementation in another package, a `super::` reference) calls the
    // implementation's DIRECT handle, which is the implementation itself:
    // `invokespecial` for `REF_invokeSpecial`, `invokevirtual` for a
    // `REF_invokeVirtual`. The spliced body is the spun code with the handle's
    // `ldc` made `nop`s and the `invokeExact` made that call (same length;
    // its rows are keyed by pc). HotSpot's LambdaForm frames between the proxy
    // and the implementation are hidden, as the proxy's is, so traces match.
    let (call_op, owner, impl_name, impl_desc, body_code): (u8, String, String, String, Vec<u8>) =
        match handle_ldc {
            None => (call_op, owner, impl_name, impl_desc, code.to_vec()),
            Some((ldc_pc, ldc_len)) => {
                if !cratonvm_jit::ir::ir_lambda_sam_splice_handle_forwarders_enabled() {
                    refuse!("lambda-sam: a handle forwarder (CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_HANDLE_FORWARDERS=0)");
                }
                let direct_op = match handle.kind {
                    K::InvokeSpecial => 0xb7u8,
                    K::InvokeVirtual => 0xb6,
                    K::InvokeStatic => 0xb8,
                    _ => refuse!("lambda-sam: a handle forwarder of an unmodelled kind"),
                };
                // r23 w4 review (probe `C:\craton\n23p\w4R\W4RHandleRecvCast`,
                // HotSpot 25): a `REF_invokeSpecial` handle's receiver is
                // narrowed to the caller, but the JDK's forwarder types its
                // `invokeExact` with the member's DECLARING class
                // (`AbstractValidatingLambdaMetafactory.implClass`), so for a
                // member of another class HotSpot throws
                // `WrongMethodTypeException` on every call. Splicing that as a
                // direct `invokespecial` would CALL the method; only a member
                // of the host itself, where the two types agree, is spliced.
                if matches!(handle.kind, K::InvokeSpecial) && *handle.class_name != *facts.host_name
                {
                    refuse!("lambda-sam: an invokespecial handle of another class (WrongMethodTypeException on HotSpot)");
                }
                // r23 w4 review: the `invokeExact` must be EXACTLY the
                // handle's type -- its own parameters, preceded by the
                // receiver for an instance kind -- or the patched call would
                // pop a different operand count than the forwarder pushed
                // (HotSpot: `WrongMethodTypeException`, never a call).
                let exact_tail = if direct_op == 0xb8 {
                    impl_desc.strip_prefix('(')
                } else {
                    impl_desc
                        .strip_prefix("(L")
                        .and_then(|rest| rest.split_once(';'))
                        .map(|(_, tail)| tail)
                };
                if exact_tail.is_none() || exact_tail != handle.descriptor.strip_prefix('(') {
                    refuse!("lambda-sam: the `invokeExact` type is not the handle's");
                }
                let mut patched = code.to_vec();
                for b in patched.iter_mut().skip(ldc_pc).take(ldc_len) {
                    *b = 0x00;
                }
                if let Some(b) = patched.get_mut(call_pc) {
                    *b = direct_op;
                }
                (
                    direct_op,
                    handle.class_name.to_string(),
                    handle.member_name.to_string(),
                    handle.descriptor.to_string(),
                    patched,
                )
            }
        };
    let code: &[u8] = &body_code;
    if *handle.class_name != *owner
        || *handle.member_name != *impl_name
        || *handle.descriptor != *impl_desc
    {
        refuse!("lambda-sam: the call is not the implementation");
    }
    // `REF_invokeStatic` -> `invokestatic`; a host-private `REF_invokeSpecial`
    // -> `invokevirtual` on the captured `this` (statically bound: private);
    // wave 2 (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_VIRTUAL_REFS`): a
    // `REF_invokeVirtual` / class `REF_invokeInterface` method reference
    // (`String::length`), whose target the nested resolution below admits only
    // when the language makes it monomorphic (a final class or method, a
    // private one: `ir-splice-target-not-provably-monomorphic` otherwise).
    let instance = match (call_op, handle.kind) {
        (0xb8, K::InvokeStatic) => false,
        (0xb6, K::InvokeSpecial) if owner == facts.host_name => true,
        (0xb6, K::InvokeVirtual | K::InvokeInterface)
            if cratonvm_jit::ir::ir_lambda_sam_splice_virtual_refs_enabled() =>
        {
            true
        }
        // Wave 3: an interface method reference (`List::size`), and a
        // constructor reference (`Foo::new`: `new; dup; invokespecial <init>`).
        (0xb9, K::InvokeInterface)
            if cratonvm_jit::ir::ir_lambda_sam_splice_virtual_refs_enabled() =>
        {
            true
        }
        // Wave 4: a `super::` / protected `REF_invokeSpecial` reached through
        // its direct handle (`handle_ldc` above): statically bound.
        (0xb7, K::InvokeSpecial) if handle_ldc.is_some() => true,
        (0xb7, K::NewInvokeSpecial)
            if cratonvm_jit::ir::ir_lambda_sam_splice_ctor_refs_enabled() && news.len() == 1 =>
        {
            true
        }
        _ => refuse!("lambda-sam: a handle (`invokeExact`) or unmodelled implementation"),
    };
    if !news.is_empty() && nest_depth > 0 {
        // A constructor reference's forwarder is opaque (its result is the
        // allocation), and an opaque forwarder is spliced at depth 0 only.
        refuse!("lambda-sam: a constructor reference in a nested forwarder");
    }
    if conversions.is_empty() && news.is_empty() {
        // Transparent (wave 1): the deopt chain skips the forwarder's frame,
        // so the implementation's result must be the SAM's unconverted.
        let ret_of = |d: &str| d.rsplit_once(')').map_or("V", |(_, r)| r).to_string();
        let (impl_ret, sam_ret) = (ret_of(impl_desc.as_str()), ret_of(callee_desc));
        let is_ref = |d: &str| d.starts_with('L') || d.starts_with('[');
        if !returns_call_result
            || (impl_ret != sam_ret && !(is_ref(impl_ret.as_str()) && is_ref(sam_ret.as_str())))
        {
            refuse!("lambda-sam: the result needs a conversion");
        }
    } else if !conversions.is_empty()
        && !cratonvm_jit::ir::ir_lambda_sam_splice_conversions_enabled()
    {
        refuse!("lambda-sam: boxing / unboxing (CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_CONVERSIONS=0)");
    } else if nest_depth > 0 {
        // An opaque forwarder resumes its subtree flat, which is sound only
        // at depth 0 (the builder refuses one nested too).
        refuse!("lambda-sam: boxing / unboxing in a nested forwarder");
    }
    // A nested forwarder's implementation must still fit the resolver's
    // nesting budget (the builder refuses a chain deeper than it opens).
    if nest_depth > 0
        && nest_depth + 1 >= cratonvm_jit::ir_inline_max_nest_depth_in(chain_tree_flag())
    {
        refuse!("lambda-sam: no nesting depth left for the implementation");
    }
    // The casts' classes and the unboxing receivers' box classes, in the
    // host's loader (the proxy's).
    let mut typecheck: Vec<(usize, u32, String, bool)> = Vec::with_capacity(casts.len());
    let mut conversion_receivers: Vec<Option<ClassId>> = Vec::with_capacity(conversions.len());
    let mut new_rows: Vec<(usize, u32, usize, bool)> = Vec::with_capacity(news.len());
    {
        let cm = shared.classes.class_manager.read();
        for (cast_pc, name) in casts {
            let Some(id) = jit_known_class(shared, &cm, &name, host_id) else {
                drop(cm);
                refuse!(format!("lambda-sam: cast class {name} not loaded"));
            };
            typecheck.push((cast_pc, id.as_u32(), name, true));
        }
        for c in &conversions {
            let receiver = match &c.exact_receiver {
                None => None,
                Some(name) => {
                    let Some(id) = jit_known_class(shared, &cm, name, host_id) else {
                        drop(cm);
                        refuse!(format!("lambda-sam: box class {name} not loaded"));
                    };
                    // Copied under the guard it was looked up under, as the
                    // exact-receiver resolvers mark theirs.
                    shared.jit.jit_cache.note_bytecode_copied(id);
                    Some(id)
                }
            };
            conversion_receivers.push(receiver);
        }
        // Wave 3: a constructor reference's `new`, resolved in the host's
        // loader; only an initialised class with no finalizer (the
        // optimizing tier's spliced-`new` rules,
        // `ir-splice-new-site-class-not-initialized`; a finalizable class's
        // allocation keeps its call).
        for (new_pc, name) in &news {
            let class = jit_known_class(shared, &cm, name, host_id)
                .and_then(|id| cm.get_class(id).map(|c| (id, c)));
            let Some((id, class)) = class else {
                drop(cm);
                refuse!(format!("lambda-sam: constructed class {name} not loaded"));
            };
            // r23 w3 review: the caller's own `new` rows' flags
            // (`jit_new_site_flags`), so a `java.lang.ref` class
            // (`WeakReference::new`), which keeps the registering helper call,
            // is refused like a finalizer -- `class.has_finalizer` alone
            // answers `false` for it.
            let (num_fields, finalizable, _) = jit_new_site_flags(&cm, id);
            if !crate::vm::is_class_initialized_fast(class) || finalizable {
                drop(cm);
                refuse!(format!(
                    "lambda-sam: constructed class {name} not initialised or finalizable"
                ));
            }
            new_rows.push((*new_pc, id.as_u32(), num_fields, false));
        }
        // `cm` drops here: the nested resolution takes the lock itself.
    }
    // The implementation, as the host's own constant pool would resolve the
    // call (its loader, its access), one nesting level down.
    // A nested forwarder has no planner site cap over its implementation
    // (`ir_guarded_body_bytes` charges the compiling method's own sites):
    // HotSpot's `MaxInlineSize` there.
    let impl_cap = if nest_depth > 0 {
        cratonvm_jit::MAX_INLINE_SIZE_COLD
    } else {
        LAMBDA_SAM_IMPL_MAX_BYTES
    };
    // A frozen proxy's captures are the implementation's leading arguments:
    // the planner sees through them as through a field of the proxy.
    if let Some(addr) = receiver_object {
        if let Some(facts) = shared.classes.frozen.facts.read().clone() {
            let (classes, objects) =
                lambda_capture_slots(&site.capture_types, |i| facts.field_target(addr, i));
            cratonvm_jit::ir::set_splice_arg_classes(classes);
            cratonvm_jit::ir::set_splice_arg_objects(objects);
        }
    }
    let nested = resolve_inline_site_from(
        shared,
        host_id,
        None,
        &owner,
        &impl_name,
        &impl_desc,
        nest_depth + 1,
        direct_bind,
        true,
        None,
        planning_only,
    )
    .filter(|n| {
        n.callee_is_static != instance && !n.callee_is_synchronized && n.callee_code_len <= impl_cap
    });
    let mut invoke_targets: Vec<(usize, cratonvm_jit::InlineInvokeTarget)> = Vec::new();
    let mut nested_sites: Vec<cratonvm_jit::NestedInlineSite> = Vec::new();
    let impl_len = match nested {
        Some(nested) => {
            let len = nested.callee_code_len;
            nested_sites.push(cratonvm_jit::NestedInlineSite {
                callee_pc: call_pc,
                guard_class_id: 0,
                site: nested,
                bimorphic_second: None,
            });
            len
        }
        // Wave 3 (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_IMPL_CALL`): an
        // implementation that does not splice -- an overridable
        // virtual / interface target (`Object::toString`, `List::size`: the
        // forwarder's own call site has no profile to guard a splice on), a
        // body too large or refused -- stays the forwarder's CALL, with its
        // own inline cache (virtual / interface) or direct entry (static,
        // constructor, private). The guarded splice still removes the
        // interface call into the proxy and the lambda dispatch. Only at
        // depth 0: a nested forwarder's call would need a post-call chain
        // through a frameless scope.
        None if nest_depth == 0 && cratonvm_jit::ir::ir_lambda_sam_splice_impl_call_enabled() => {
            let (kind, owner_class_id) = match (call_op, handle.kind) {
                (0xb8, _) => (3u8, 0u32),
                (0xb7, _) => (1, 0),
                // A host-private method, statically bound like `invokespecial`.
                (0xb6, K::InvokeSpecial) => (1, host_id.as_u32()),
                (0xb6, _) => (0, 0),
                _ => (2, 0),
            };
            invoke_targets.push((
                call_pc,
                cratonvm_jit::InlineInvokeTarget {
                    class_name: owner.clone(),
                    method_name: impl_name.clone(),
                    descriptor: impl_desc.clone(),
                    num_jit_args: count_method_params(&impl_desc) + usize::from(instance),
                    return_type: cratonvm_jit::return_type(&impl_desc),
                    invoke_kind: kind,
                    declaring_class_id: host_id.as_u32(),
                    target_class_id: 0,
                    resolved_class_id: 0,
                    owner_class_id,
                    direct_entry: None,
                },
            ));
            0
        }
        None => refuse!("lambda-sam: the implementation does not splice"),
    };
    // Wave 2: each conversion call is spliced in turn (an unboxing on the box
    // class its `checkcast` proved, statically bound: the box classes are
    // final), or kept as a call row -- which only an OPAQUE forwarder (one
    // with conversions) may hold: no chain is recorded under it, so a call
    // there needs no post-call chain (`cratonvm_jit::ir` `SpliceFrame::opaque`).
    for (c, receiver) in conversions.iter().zip(conversion_receivers) {
        let spliced = resolve_inline_site_from(
            shared,
            host_id,
            receiver,
            &c.owner,
            &c.name,
            &c.desc,
            nest_depth + 1,
            direct_bind,
            true,
            None,
            planning_only,
        )
        .filter(|s| s.callee_is_static == (c.op == 0xb8) && !s.callee_is_synchronized);
        if let Some(site) = spliced {
            nested_sites.push(cratonvm_jit::NestedInlineSite {
                callee_pc: c.pc,
                guard_class_id: 0,
                site,
                bimorphic_second: None,
            });
            continue;
        }
        // Kept as a call: `invokestatic valueOf` statically bound (direct
        // entry, or the box-thin helper the planner binds), an unboxing
        // `invokevirtual` with its own inline cache.
        let is_static = c.op == 0xb8;
        // JIT r23 wave 5, lane L2 (`CRATONVM_JIT_IR_LAMBDA_SAM_EXACT_UNBOX_ROW`,
        // default ON): an unboxing `Number.intValue()` whose receiver the
        // forwarder's own `checkcast` proved to be exactly a (final) box class
        // is that class's own method, statically bound -- the row the
        // optimizing tier's spliced unbox intrinsic recognises
        // (`ir_spliced_unbox_row`: `Integer.intValue` / `Long.longValue`,
        // kind 0 or 1), so with `CRATONVM_JIT_IR_FORWARDER_FLAT_UNBOX` the
        // unboxing becomes a guarded field load instead of a call that raises
        // the replay fence. A null receiver still traps (the call's own null
        // check, or the intrinsic's guard) and re-executes the SAM call.
        //
        // Wave 6: the row stays a VIRTUAL one (kind 0) naming the box class,
        // with its own inline cache. Kind 1 had no `direct_entry` (the box
        // accessor is a registered Intrinsic, never a compiled body), so the
        // planner refused the whole forwarder splice as
        // `ir-splice-unbindable-surviving-call` (n23-w5, `BoxRef.boxed`).
        // `ir_spliced_unbox_row` admits kind 0 as well.
        let exact_unbox = (!is_static)
            .then_some(())
            .and(receiver.zip(c.exact_receiver.as_ref()));
        let (row_class, row_kind, row_owner) = match exact_unbox {
            Some((_, name)) => (name.clone(), 0u8, 0),
            None => (c.owner.clone(), if is_static { 3 } else { 0 }, 0),
        };
        invoke_targets.push((
            c.pc,
            cratonvm_jit::InlineInvokeTarget {
                class_name: row_class,
                method_name: c.name.clone(),
                descriptor: c.desc.clone(),
                num_jit_args: count_method_params(&c.desc) + usize::from(!is_static),
                return_type: cratonvm_jit::return_type(&c.desc),
                invoke_kind: row_kind,
                declaring_class_id: host_id.as_u32(),
                target_class_id: 0,
                resolved_class_id: 0,
                owner_class_id: row_owner,
                direct_entry: None,
            },
        ));
    }
    if let Some(bind) = direct_bind {
        bind_spliced_static_calls(&mut invoke_targets, &nested_sites, bind);
    }
    nested_sites.sort_by_key(|n| n.callee_pc);
    let opaque = nested_sites.len() + invoke_targets.len() > 1 || !new_rows.is_empty();
    let final_field_pcs: Vec<usize> = field_info.iter().map(|&(p, _, _)| p).collect();
    let padded = crate::runtime::frame::padded_bytecode(code);
    let site_out = cratonvm_jit::InlineSite {
        callee_code: padded.to_vec(),
        callee_code_len: code.len(),
        callee_max_locals: usize::from(method.args_size),
        callee_num_args: count_method_params(callee_desc) + 1,
        callee_is_static: false,
        return_type: cratonvm_jit::return_type(callee_desc),
        field_decl_info: field_info
            .iter()
            .map(|&(pc, index, _)| (pc, index, proxy.as_u32()))
            .collect(),
        field_info,
        final_field_pcs,
        needs_heap: true,
        // The forwarder's identity: a lambda proxy id, which is what makes
        // the builder treat it as frameless
        // (`cratonvm_jit::ir::ir_splice_body_is_lambda_forwarder`).
        class_name: format!("{}$$Lambda/0x{:016x}", facts.host_name, proxy.as_u32()),
        class_id: proxy.as_u32(),
        method_name: callee_method.to_string(),
        descriptor: callee_desc.to_string(),
        // A spliced implementation leaves no row for its call
        // (`append_ir_inline_site` appends a nested body with no surviving
        // call strictly). Kept as calls: a conversion of an opaque forwarder,
        // and (wave 3, depth 0) an implementation that does not splice.
        invoke_targets,
        nested_sites: nested_sites.into(),
        ir_typecheck_info: typecheck,
        // Wave 3: a constructor reference's `new`.
        ir_new_info: new_rows,
        ..Default::default()
    };
    census(&format!(
        "SPLICED ({} forwarder {} bytes, implementation {impl_len} bytes, {} body(ies) \
         spliced, {} kept as calls)",
        if opaque { "opaque" } else { "transparent" },
        site_out.callee_code_len,
        site_out.nested_sites.len(),
        site_out.invoke_targets.len(),
    ));
    Some(site_out)
}

/// Round 13 wave 10 (lane iropt8, HS-2 of `jit-r13-hashsplice-proposals-RETIRED-20260929.md`):
/// the body a receiver of EXACTLY the constant-pool class `callee_class`
/// dispatches to at an `invokevirtual` INSIDE a spliced body, and that class's
/// id -- [`resolve_ir_exact_receiver_inline_site`]'s contract one nesting level
/// down. The consumer (`cratonvm_jit`'s planner and `IrBuilder`) splices it
/// only where it proves the receiver node is exactly that class, and keeps the
/// call otherwise. `None` for a static or larger-than-`size_cap` body, which
/// the planner would not take: `size_cap` is
/// `cratonvm_jit::ir_splice_nested_exact_size_cap` of the enclosing body, the
/// planner's own answer (`MAX_INLINE_SIZE_COLD`, or the hot cap when the
/// enclosing body is a receiver forwarder such as a javac bridge; nio21 wave
/// 5, lane L4).
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn resolve_ir_nested_exact_receiver_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    nest_depth: usize,
    direct_bind: Option<InlineDirectBind<'_>>,
    size_cap: usize,
) -> Option<(cratonvm_jit::InlineSite, u32)> {
    if nest_depth == 0
        && ir_collection_site_kept_as_call_or_intrinsic(
            shared,
            callee_class,
            callee_method,
            callee_desc,
        )
    {
        return None;
    }
    let receiver = {
        let cm = shared.classes.class_manager.read();
        let id = jit_known_class(shared, &cm, callee_class, requesting_class_id)?;
        shared.jit.jit_cache.note_bytecode_copied(id);
        id
        // `cm` drops here: `resolve_inline_site_from` takes the lock itself.
    };
    // The filter below, asked of the body before its nested sites resolve.
    set_exact_row_size_cap(size_cap);
    let site = resolve_inline_site_from(
        shared,
        requesting_class_id,
        Some(receiver),
        callee_class,
        callee_method,
        callee_desc,
        nest_depth,
        direct_bind,
        true,
        None,
        false,
    )?;
    (!site.callee_is_static && site.callee_code_len <= size_cap)
        .then_some((site, receiver.as_u32()))
}

/// JIT round 20 wave 11 (lane pSplice, W5L4-5 of
/// `docs/feature-designs/jit-ir-optimizations.md`): the body
/// the exact class `receiver` of an enclosing body dispatches to at that
/// body's `invokeinterface` on `this`, when the enclosing body is a receiver
/// FORWARDER (`cratonvm_jit::ir_splice_body_is_receiver_forwarder`: `aload_0`,
/// operand loads and `checkcast`s, the call, a return -- a default method that
/// re-dispatches on `this`). `receiver` is the class the enclosing body was
/// resolved for (its guarded or exact receiver); the row is marked with it, so
/// the builder splices it unguarded only where it proves the receiver node is
/// exactly that class (a guarded body's own `this`), and otherwise takes the
/// nested trap plan of an `invokeinterface` row or keeps the call: sound for
/// every receiver whatever this guess. Its cap is the forwarder's (the hot
/// cap), the planner's own answer. `None` under
/// `CRATONVM_JIT_IR_PSPLICE_IFACE_FORWARDER=0`, for a non-forwarder, a static
/// or over-cap body.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn resolve_ir_nested_iface_forwarder_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    receiver: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    nest_depth: usize,
    direct_bind: Option<InlineDirectBind<'_>>,
    parent_code: &[u8],
    parent_code_len: usize,
    call_pc: usize,
    callee_arg_values: usize,
) -> Option<(cratonvm_jit::InlineSite, u32)> {
    if !cratonvm_jit::ir_psplice_iface_forwarder_enabled()
        || !cratonvm_jit::ir::ir_splice_chain_fences_enabled()
        || !cratonvm_jit::ir_splice_nested_exact_receiver_enabled()
        || !cratonvm_jit::ir_splice_body_is_receiver_forwarder(
            parent_code,
            parent_code_len,
            call_pc,
            callee_arg_values,
        )
    {
        return None;
    }
    let size_cap = cratonvm_jit::ir_splice_nested_exact_size_cap(
        parent_code,
        parent_code_len,
        call_pc,
        callee_arg_values,
    );
    {
        // Under the guard, as the other receiver resolvers mark theirs: a
        // redefinition of the receiver's class withdraws this compile.
        let _cm = shared.classes.class_manager.read();
        shared.jit.jit_cache.note_bytecode_copied(receiver);
    }
    // No size cap handed down here: the debug line below names a size refusal.
    let site = resolve_inline_site_from(
        shared,
        requesting_class_id,
        Some(receiver),
        callee_class,
        callee_method,
        callee_desc,
        nest_depth,
        direct_bind,
        true,
        None,
        false,
    )?;
    let admitted = !site.callee_is_static && site.callee_code_len <= size_cap;
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] nest-iface-forwarder {}.{}{} at callee_pc={} depth={} -> class {} len={} {}",
            callee_class,
            callee_method,
            callee_desc,
            call_pc,
            nest_depth,
            receiver.as_u32(),
            site.callee_code_len,
            if admitted { "(spliced where the receiver is proven)" } else { "refused (size)" },
        );
    }
    admitted.then_some((site, receiver.as_u32()))
}

/// Round nio20 wave 2, lane L3 (`utfencode`): the ONE receiver class the call
/// at `ipc` of the method `(callee_class, callee_method, callee_desc)` (declared
/// by `declaring_id`) has seen, for a nested trap-guarded splice
/// (`CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP`). The miss edge of that guard
/// is a deopt, so the bar is HotSpot's trap-on-miss rule, not the 80 % one of a
/// guard whose miss edge is the call:
///
/// * the method's own receiver profile at `ipc`, when it has at least
///   `MIN_GUARDED_RECEIVER_EVIDENCE` unsaturated samples, all of one class
///   (`ReceiverEvidence::only_class`); a profile that names two classes
///   answers `None` without asking the cache;
/// * else its compiled artifact's inline cache at `ipc`: one live class, no
///   miss, and at least `MIN_GUARDED_RECEIVER_EVIDENCE` hits
///   (`CompiledMethod::mic_slot_census`), the PIC publishing no other class
///   (`dominant_receiver_at_bci`).
pub(in crate::runtime::interpreter::jit_bridge) fn ir_nested_trap_receiver_class(
    shared: &SharedVm,
    callee_profile: Option<&cratonvm_jit::profile::MethodProfile>,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    declaring_id: ClassId,
    ipc: usize,
) -> Option<u32> {
    // A thin one-class profile does not answer alone, but the cache must then
    // name the same class.
    let mut profile_class: Option<u32> = None;
    if let Some(counts) = callee_profile.and_then(|p| p.receivers.get(&ipc)) {
        let evidence = cratonvm_jit::profile::ReceiverEvidence::of(counts);
        if let Some(class_id) = evidence.only_class(cratonvm_jit::MIN_GUARDED_RECEIVER_EVIDENCE) {
            return Some(class_id);
        }
        if evidence.summary().types > 1 {
            return None;
        }
        profile_class = evidence.summary().top.map(|(class_id, _)| class_id);
    }
    let artifact = shared.jit.jit_cache.read().get(
        &Arc::from(callee_class),
        &Arc::from(callee_method),
        &Arc::from(callee_desc),
        declaring_id,
    )?;
    let class_id = artifact.dominant_receiver_at_bci(ipc)?;
    let min_hits = u64::from(cratonvm_jit::MIN_GUARDED_RECEIVER_EVIDENCE);
    let clean = artifact
        .mic_slot_census()
        .iter()
        .filter(|&&(bci, _, _, _)| bci == ipc)
        .all(|&(_, cls, hits, misses)| cls == class_id && misses == 0 && hits >= min_hits);
    (class_id != 0 && clean && profile_class.is_none_or(|p| p == class_id)).then_some(class_id)
}

/// Round nio20 wave 3, lane L4 (proposal L3W2-3): does any evidence at `ipc`
/// of the method `(callee_class, callee_method, callee_desc)` name a receiver
/// class OTHER than `class_id`? The negative half of
/// [`ir_nested_trap_receiver_class`]'s bar, for a nest-exact row the builder
/// may splice behind a trap guard
/// (`cratonvm_jit::ir::ir_splice_nested_exact_trap_enabled`):
///
/// * the method's own receiver profile at `ipc` recorded two or more classes,
///   or one that is not `class_id`;
/// * its compiled artifact's inline cache at `ipc` holds another class
///   (`dominant_receiver_at_bci`), or a MIC slot there has taken a miss.
///
/// No evidence at all answers `false`: the row keeps its proven-receiver
/// splices, and an unproven arrival of another class is a trap, charged like
/// any chain trap.
#[allow(clippy::too_many_arguments)]
/// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: the exact class each operand of
/// the call at `ipc` of a spliced body carries into its callee, by the
/// callee's local slot (`None` = unknown; empty when nothing is known).
///
/// An operand is known when the straight-line run before the call
/// ([`cratonvm_jit::ir::splice_call_operand_sources`]) pushes it with
///
/// * a `getstatic` of a trusted `static final` reference (`static_rows`, the
///   body's resolved static sites) whose object the VM filed a class for
///   (`ClassRealm::static_final_ref_classes`): that field holds that one
///   object forever; or
/// * a load of a local the body never writes, whose class the body's own
///   caller proved (`arg_classes`): the slot holds that argument throughout.
///
/// Every fact is exact for the whole body, so a row keyed on it is spliced
/// only where the builder proves the same receiver
/// (`IrBuilder::exact_receiver_node_proof`), and is the call otherwise.
pub(in crate::runtime::interpreter::jit_bridge) fn nested_operand_classes(
    shared: &SharedVm,
    code: &[u8],
    code_len: usize,
    ipc: usize,
    callee_desc: &str,
    has_receiver: bool,
    handler_pcs: &[usize],
    arg_classes: &[Option<u32>],
    static_rows: &[(usize, u32, usize, u8, bool)],
    // `CRATONVM_FROZEN_PAGES`: the frozen objects (by address) the body's
    // arguments are expected to be, its resolved instance-field rows, and
    // where the objects this call's operands are expected to be are left.
    arg_objects: &[Option<usize>],
    field_rows: &[(usize, usize, u8)],
    call_effect: &dyn Fn(usize) -> Option<(usize, usize)>,
    operand_objects: &mut Vec<Option<usize>>,
) -> Vec<Option<u32>> {
    operand_objects.clear();
    if !cratonvm_jit::ir::ir_static_final_exact_class_enabled() {
        return Vec::new();
    }
    let Some(slots) = cratonvm_jit::ir::descriptor_operand_slots(callee_desc, has_receiver) else {
        return Vec::new();
    };
    let facts = if crate::runtime::frozen_pages::enabled() {
        shared.classes.frozen.facts.read().clone()
    } else {
        None
    };
    // A local the body never writes holds its argument throughout.
    let arg_object = |k: usize| {
        arg_objects
            .get(k)
            .copied()
            .flatten()
            .filter(|_| cratonvm_jit::ir::body_writes_local(code, code_len, k) == Some(false))
    };
    let static_row = |gpc: usize| {
        static_rows
            .iter()
            .find(|row| row.0 == gpc)
            .filter(|row| row.3 == b'L' && !row.4)
            .and_then(|row| Some((row.1, u32::try_from(row.2).ok()?)))
    };
    let root_object = |gpc: usize| {
        let facts = facts.as_ref()?;
        let id = *facts.roots.get(&static_row(gpc)?)?;
        facts.objects.get(id as usize).map(|o| o.addr)
    };
    // `(class, address)` of what a reference field of the object at `base`
    // holds, as the snapshot has it.
    let field_target = |base: Option<usize>, fpc: usize| {
        let row = field_rows
            .iter()
            .find(|row| row.0 == fpc)
            .filter(|row| row.2 == b'L')?;
        facts.as_ref()?.field_target(base?, row.1)
    };
    let Some(sources) = cratonvm_jit::ir::splice_call_operand_sources_with_calls(
        code,
        code_len,
        ipc,
        slots.len(),
        handler_pcs,
        call_effect,
    ) else {
        return Vec::new();
    };
    let mut out: Vec<Option<u32>> = Vec::new();
    for (&slot, source) in slots.iter().zip(sources) {
        let class = match source {
            cratonvm_jit::ir::SpliceOperandSource::GetStatic(gpc) => static_rows
                .iter()
                .find(|row| row.0 == gpc)
                .filter(|row| row.3 == b'L' && !row.4)
                .and_then(|row| {
                    let index = u32::try_from(row.2).ok()?;
                    shared
                        .classes
                        .static_final_ref_classes
                        .read()
                        .get(&(row.1, index))
                        .copied()
                        // `0`: a null static final, no receiver class.
                        .filter(|&c| c != 0 && c != cratonvm_jit::ir::STATIC_FINAL_OBJECT_FILED)
                }),
            cratonvm_jit::ir::SpliceOperandSource::Local(k) => {
                arg_classes.get(k).copied().flatten().filter(|_| {
                    cratonvm_jit::ir::body_writes_local(code, code_len, k) == Some(false)
                })
            }
            cratonvm_jit::ir::SpliceOperandSource::LocalField(k, fpc) => {
                field_target(arg_object(k), fpc).map(|(class, _)| class)
            }
            cratonvm_jit::ir::SpliceOperandSource::StaticField(gpc, fpc) => {
                field_target(root_object(gpc), fpc).map(|(class, _)| class)
            }
            cratonvm_jit::ir::SpliceOperandSource::Indy(_)
            | cratonvm_jit::ir::SpliceOperandSource::Other => None,
        };
        if crate::runtime::env_cache::dbg_jitc()
            && class.is_some()
            && matches!(
                source,
                cratonvm_jit::ir::SpliceOperandSource::LocalField(..)
                    | cratonvm_jit::ir::SpliceOperandSource::StaticField(..)
            )
        {
            eprintln!(
                "[frozen-pages] planner: call at callee_pc={ipc} operand {source:?} -> class {class:?} (argument objects {arg_objects:?})"
            );
        }
        let object = match source {
            cratonvm_jit::ir::SpliceOperandSource::GetStatic(gpc) => root_object(gpc),
            cratonvm_jit::ir::SpliceOperandSource::Local(k) => arg_object(k),
            cratonvm_jit::ir::SpliceOperandSource::LocalField(k, fpc) => {
                field_target(arg_object(k), fpc).map(|(_, addr)| addr)
            }
            cratonvm_jit::ir::SpliceOperandSource::StaticField(gpc, fpc) => {
                field_target(root_object(gpc), fpc).map(|(_, addr)| addr)
            }
            cratonvm_jit::ir::SpliceOperandSource::Indy(_)
            | cratonvm_jit::ir::SpliceOperandSource::Other => None,
        };
        if object.is_some() {
            if operand_objects.len() <= slot {
                operand_objects.resize(slot + 1, None);
            }
            operand_objects[slot] = object;
        }
        if let Some(class) = class {
            if out.len() <= slot {
                out.resize(slot + 1, None);
            }
            out[slot] = Some(class);
        }
    }
    out
}

thread_local! {
    /// The r23 L8 heap-buffer close ([`ir_super_special_splice_enabled`]):
    /// set by a nested resolution's caller around the one call whose site is an
    /// `invokespecial` of the requesting body's direct superclass; taken (and
    /// cleared) at the top of [`resolve_inline_site_from`], so its own nested
    /// resolutions never see it.
    pub(in crate::runtime::interpreter::jit_bridge) static IR_SUPER_SPECIAL_SITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The per-thread state of one inline-site resolution tree
    /// ([`InlineResolveState`]): the chain-tree flag, the exact-row size cap
    /// and the resolution memo. One declaration for the three (the statics
    /// ratchet counts declarations); it replaces the lone `IR_CHAIN_TREE` cell.
    pub(in crate::runtime::interpreter::jit_bridge) static INLINE_RESOLVE_STATE: std::cell::RefCell<InlineResolveState> =
        const { std::cell::RefCell::new(InlineResolveState::new()) };
}

/// See [`INLINE_RESOLVE_STATE`]. Borrowed only for a field read or write,
/// never across a resolution.
pub(in crate::runtime::interpreter::jit_bridge) struct InlineResolveState {
    /// The r25 W11-7 review fix (`CRATONVM_JIT_IR_NESTED_CHAIN_DEPTH_SCOPED`):
    /// set while a body's nested sites are resolved below a dispatch-chain
    /// body or a W10-4 lambda body ([`ChainTreeScope`]). The W10-3 depth raise
    /// and 24-byte leaf rule hold only there
    /// (`cratonvm_jit::ir_inline_max_nest_depth_in`).
    pub(in crate::runtime::interpreter::jit_bridge) chain_tree: bool,
    /// The body-size cap of the one resolution about to start, set by a caller
    /// whose result a longer (or static) body would only be filtered out of
    /// ([`set_exact_row_size_cap`]); taken at the top of the resolver so its
    /// own nested resolutions never see it.
    pub(in crate::runtime::interpreter::jit_bridge) exact_size_cap: Option<usize>,
    /// The memo of the outermost resolution on this thread ([`inline_memoized`]).
    pub(in crate::runtime::interpreter::jit_bridge) memo: Option<InlineMemo>,
    /// The methods selected along the inline path [`resolve_inline_site_from`]
    /// is resolving on this thread, outermost first ([`InlinePathEntry`]).
    pub(in crate::runtime::interpreter::jit_bridge) inline_path: Vec<(u32, Box<str>, Box<str>)>,
}

impl InlineResolveState {
    pub(in crate::runtime::interpreter::jit_bridge) const fn new() -> Self {
        Self {
            chain_tree: false,
            exact_size_cap: None,
            memo: None,
            inline_path: Vec::new(),
        }
    }
}

/// One selected method on `INLINE_RESOLVE_STATE.inline_path`, popped on drop.
///
/// Bounds recursion along one inline path, HotSpot's `MaxRecursiveInlineLevel`:
/// a method may appear on it at most `IR_RECURSIVE_INLINE_MAX_COPIES + 1` times,
/// the copies the bounded self-recursive splice uses plus the level whose
/// self-calls become the direct self-call, so that splice plans as before. The
/// static self-recursion rule in the nested loop sees only `invokestatic`
/// self-calls; a method that recurses through a virtual call on another
/// receiver (Spring's `ResolvableType.as`: `getSuperType().as(type)`) was
/// re-resolved through every nested route (static, static-final, profiled,
/// exact, chain) at every level of the raised chain depth, about x3 per level:
/// 800 000 resolutions in one C2 compile of `TypeMappedAnnotations.isPresent`
/// that held the single C2 worker for 86 s, while every callee a compiled call
/// site had deferred to that lane ran interpreted
/// (the Spring MVC C2-lane stall, 2026-11-14).
///
/// An entry at nest depth 0 starts an empty path and restores the enclosing one
/// on drop: a depth-0 resolution is a site of the compile being planned, or of
/// a compile nested in another's resolution, and neither inherits that path.
pub(in crate::runtime::interpreter::jit_bridge) struct InlinePathEntry(
    pub(in crate::runtime::interpreter::jit_bridge) Option<Vec<(u32, Box<str>, Box<str>)>>,
);

impl InlinePathEntry {
    /// `None` when `class.method desc` is already on the path the maximum
    /// number of times.
    pub(in crate::runtime::interpreter::jit_bridge) fn enter(
        nest_depth: usize,
        class: ClassId,
        method: &str,
        desc: &str,
    ) -> Option<Self> {
        INLINE_RESOLVE_STATE.with(|s| {
            let mut state = s.borrow_mut();
            let path = &mut state.inline_path;
            let saved = (nest_depth == 0).then(|| std::mem::take(path));
            let copies = path
                .iter()
                .filter(|(c, m, d)| *c == class.as_u32() && &**m == method && &**d == desc)
                .count();
            if copies > cratonvm_jit::ir::IR_RECURSIVE_INLINE_MAX_COPIES {
                if let Some(saved) = saved {
                    *path = saved;
                }
                return None;
            }
            path.push((class.as_u32(), method.into(), desc.into()));
            Some(Self(saved))
        })
    }
}

impl Drop for InlinePathEntry {
    fn drop(&mut self) {
        INLINE_RESOLVE_STATE.with(|s| {
            let mut state = s.borrow_mut();
            let path = &mut state.inline_path;
            path.pop();
            if let Some(saved) = self.0.take() {
                *path = saved;
            }
        });
    }
}

pub(in crate::runtime::interpreter::jit_bridge) fn chain_tree_flag() -> bool {
    INLINE_RESOLVE_STATE.with(|s| s.borrow().chain_tree)
}

/// Holds the chain-tree flag for one body's nested resolutions and restores
/// the enclosing value on drop, early returns included.
pub(in crate::runtime::interpreter::jit_bridge) struct ChainTreeScope(
    pub(in crate::runtime::interpreter::jit_bridge) bool,
);

impl ChainTreeScope {
    pub(in crate::runtime::interpreter::jit_bridge) fn enter(in_tree: bool) -> Self {
        Self(
            INLINE_RESOLVE_STATE
                .with(|s| std::mem::replace(&mut s.borrow_mut().chain_tree, in_tree)),
        )
    }
}

impl Drop for ChainTreeScope {
    fn drop(&mut self) {
        INLINE_RESOLVE_STATE.with(|s| s.borrow_mut().chain_tree = self.0);
    }
}

/// Hand the next resolution the cap its caller filters the answer by
/// (`!callee_is_static && callee_code_len <= cap`, the exact-receiver rows), so
/// the resolver refuses an over-cap or static body BEFORE it resolves that
/// body's own nested sites (the quadratic part of a compile's inline planning).
/// Taken, and so cleared, by the next [`resolve_inline_site_from`].
pub(in crate::runtime::interpreter::jit_bridge) fn set_exact_row_size_cap(cap: usize) {
    INLINE_RESOLVE_STATE.with(|s| s.borrow_mut().exact_size_cap = Some(cap));
}

/// Would the caller that set `cap` ([`set_exact_row_size_cap`]) discard a body
/// of this shape? The caller's own filter, negated.
pub(in crate::runtime::interpreter::jit_bridge) fn exceeds_exact_row_size_cap(
    cap: Option<usize>,
    is_static: bool,
    code_len: usize,
) -> bool {
    cap.is_some_and(|cap| is_static || code_len > cap)
}

/// Everything a resolution reads besides the VM's state: two resolutions with
/// equal keys, in one resolution tree, answer alike. `bind` is the data
/// address of the direct binder (one closure per compile; compares equal only
/// within the compile that made it), `arg_classes` / `super_special` /
/// `chain_tree` / `size_cap` the thread-local hand-offs.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::runtime::interpreter::jit_bridge) struct InlineMemoKey {
    pub(in crate::runtime::interpreter::jit_bridge) requesting: u32,
    pub(in crate::runtime::interpreter::jit_bridge) receiver: Option<u32>,
    pub(in crate::runtime::interpreter::jit_bridge) callee_class: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) callee_method: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) callee_desc: Box<str>,
    pub(in crate::runtime::interpreter::jit_bridge) nest_depth: usize,
    pub(in crate::runtime::interpreter::jit_bridge) ir_mode: bool,
    pub(in crate::runtime::interpreter::jit_bridge) planning_only: bool,
    pub(in crate::runtime::interpreter::jit_bridge) bind: usize,
    pub(in crate::runtime::interpreter::jit_bridge) arg_classes: Vec<Option<u32>>,
    pub(in crate::runtime::interpreter::jit_bridge) arg_objects: Vec<Option<usize>>,
    pub(in crate::runtime::interpreter::jit_bridge) super_special: bool,
    pub(in crate::runtime::interpreter::jit_bridge) chain_tree: bool,
    pub(in crate::runtime::interpreter::jit_bridge) size_cap: Option<usize>,
}

pub(in crate::runtime::interpreter::jit_bridge) struct InlineMemoLookup<'a> {
    pub(in crate::runtime::interpreter::jit_bridge) requesting: u32,
    pub(in crate::runtime::interpreter::jit_bridge) receiver: Option<u32>,
    pub(in crate::runtime::interpreter::jit_bridge) callee_class: &'a str,
    pub(in crate::runtime::interpreter::jit_bridge) callee_method: &'a str,
    pub(in crate::runtime::interpreter::jit_bridge) callee_desc: &'a str,
    pub(in crate::runtime::interpreter::jit_bridge) nest_depth: usize,
    pub(in crate::runtime::interpreter::jit_bridge) ir_mode: bool,
    pub(in crate::runtime::interpreter::jit_bridge) planning_only: bool,
    pub(in crate::runtime::interpreter::jit_bridge) bind: usize,
    pub(in crate::runtime::interpreter::jit_bridge) arg_classes: &'a [Option<u32>],
    pub(in crate::runtime::interpreter::jit_bridge) arg_objects: &'a [Option<usize>],
    pub(in crate::runtime::interpreter::jit_bridge) super_special: bool,
    pub(in crate::runtime::interpreter::jit_bridge) chain_tree: bool,
    pub(in crate::runtime::interpreter::jit_bridge) size_cap: Option<usize>,
}

impl InlineMemoLookup<'_> {
    fn hash_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = rustc_hash::FxHasher::default();
        self.requesting.hash(&mut h);
        self.receiver.hash(&mut h);
        self.callee_class.hash(&mut h);
        self.callee_method.hash(&mut h);
        self.callee_desc.hash(&mut h);
        self.nest_depth.hash(&mut h);
        self.ir_mode.hash(&mut h);
        self.planning_only.hash(&mut h);
        self.bind.hash(&mut h);
        self.arg_classes.hash(&mut h);
        self.arg_objects.hash(&mut h);
        self.super_special.hash(&mut h);
        self.chain_tree.hash(&mut h);
        self.size_cap.hash(&mut h);
        h.finish()
    }

    fn matches(&self, k: &InlineMemoKey) -> bool {
        self.requesting == k.requesting
            && self.receiver == k.receiver
            && self.nest_depth == k.nest_depth
            && self.ir_mode == k.ir_mode
            && self.planning_only == k.planning_only
            && self.bind == k.bind
            && self.super_special == k.super_special
            && self.chain_tree == k.chain_tree
            && self.size_cap == k.size_cap
            && self.callee_class == &*k.callee_class
            && self.callee_method == &*k.callee_method
            && self.callee_desc == &*k.callee_desc
            && self.arg_classes == k.arg_classes
            && self.arg_objects == k.arg_objects
    }

    fn to_owned(&self) -> InlineMemoKey {
        InlineMemoKey {
            requesting: self.requesting,
            receiver: self.receiver,
            callee_class: self.callee_class.into(),
            callee_method: self.callee_method.into(),
            callee_desc: self.callee_desc.into(),
            nest_depth: self.nest_depth,
            ir_mode: self.ir_mode,
            planning_only: self.planning_only,
            bind: self.bind,
            arg_classes: self.arg_classes.to_vec(),
            arg_objects: self.arg_objects.to_vec(),
            super_special: self.super_special,
            chain_tree: self.chain_tree,
            size_cap: self.size_cap,
        }
    }
}

/// The answers of one resolution tree, or (inside a compile) of the whole
/// compile: every root site the planner resolves shares them, until the compile
/// ends ([`end_inline_memo`]). `depth` is the compile-stack depth it was opened
/// at: a callee compile nested inside (the binder's eager chain) runs deeper,
/// and its answers belong to another compile (its own copied-classes record,
/// its own binder), so it neither reads nor writes.
pub(in crate::runtime::interpreter::jit_bridge) struct InlineMemo {
    pub(in crate::runtime::interpreter::jit_bridge) depth: usize,
    pub(in crate::runtime::interpreter::jit_bridge) answers:
        rustc_hash::FxHashMap<u64, Vec<(InlineMemoKey, Option<cratonvm_jit::InlineSite>)>>,
    pub(in crate::runtime::interpreter::jit_bridge) count: usize,
}

/// Entries kept per tree; past it answers are recomputed, as before.
pub(in crate::runtime::interpreter::jit_bridge) const INLINE_MEMO_MAX_ENTRIES: usize = 4096;

/// Closes the memo the outermost resolution opened, on every exit, unless a
/// compile is open: then the memo outlives the root site and the compile's end
/// closes it (a root site's tree repeats bodies its siblings resolved).
pub(in crate::runtime::interpreter::jit_bridge) struct InlineMemoScope(
    pub(in crate::runtime::interpreter::jit_bridge) bool,
);

impl Drop for InlineMemoScope {
    fn drop(&mut self) {
        if self.0 && !cratonvm_jit::compile_scope_open() {
            end_inline_memo();
        }
    }
}

/// Drops this thread's inline-resolution memo; the compile-end hook
/// (`cratonvm_jit::on_compile_end`) of a memo opened inside a compile.
pub(crate) fn end_inline_memo() {
    // Taken out first so the answers drop outside the borrow.
    let memo = INLINE_RESOLVE_STATE
        .try_with(|s| s.borrow_mut().memo.take())
        .ok()
        .flatten();
    drop(memo);
}

/// Run `resolve`, memoized within the resolution tree this thread is in.
/// Returns the answer and whether it came from the memo.
///
/// The OUTERMOST call opens the memo; inside a compile it stays open for the
/// compile's other root sites, so a later root site's calls (same thread, same
/// compile-stack depth) hit the bodies an earlier one resolved. Every call
/// under an open memo is looked up first: the tree
/// resolves the same `(body, depth, hand-offs)` again from several routes
/// (static, exact-receiver, profile-guarded) and from sibling bodies, and each
/// repeat used to redo the body's whole nested subtree. `cacheable` is false
/// for a call whose answer depends on state outside the key (a substituted
/// owner list, the `CRATONVM_DBG_JITC` refusal census).
///
/// Sound because the answer is a function of the key and of VM state that does
/// not change inside one compile-time resolution tree in any way the repeat
/// could observe differently: the copy records (`note_bytecode_copied`) land on
/// the same open compilation as the first resolution's, and the binder's pins
/// outlive the compile.
///
/// One order effect is accepted: `direct_bind` can compile or pin callees under
/// a fan-out budget, and a repeat replays the FIRST answer instead of asking the
/// (possibly exhausted) binder again. That is never a worse answer than the
/// first, and the budget only bounds compile work, not correctness.
pub(in crate::runtime::interpreter::jit_bridge) fn inline_memoized<'a>(
    cacheable: bool,
    lookup: &InlineMemoLookup<'a>,
    before_miss: impl FnOnce(),
    resolve: impl FnOnce() -> Option<cratonvm_jit::InlineSite>,
) -> (Option<cratonvm_jit::InlineSite>, bool) {
    let depth = cratonvm_jit::jit_active_compile_depth();
    let hash = if cacheable { lookup.hash_key() } else { 0 };
    let (opened, usable, hit) = INLINE_RESOLVE_STATE.with(|s| {
        let mut s = s.borrow_mut();
        let (opened, usable) = match &s.memo {
            None => {
                s.memo = Some(InlineMemo {
                    depth,
                    answers: rustc_hash::FxHashMap::default(),
                    count: 0,
                });
                (true, true)
            }
            Some(memo) if memo.depth > depth => {
                s.memo = Some(InlineMemo {
                    depth,
                    answers: rustc_hash::FxHashMap::default(),
                    count: 0,
                });
                (true, true)
            }
            Some(memo) => (false, memo.depth == depth),
        };
        let hit = if cacheable && usable && !opened {
            s.memo.as_ref().and_then(|memo| {
                memo.answers
                    .get(&hash)
                    .and_then(|bucket| bucket.iter().find(|(k, _)| lookup.matches(k)).map(|(_, v)| v.clone()))
            })
        } else {
            None
        };
        (opened, usable, hit)
    });
    let _scope = InlineMemoScope(opened);
    if opened {
        cratonvm_jit::on_compile_end(end_inline_memo);
    }
    if let Some(answer) = hit {
        return (answer, true);
    }
    before_miss();
    if !(cacheable && usable) {
        return (resolve(), false);
    }
    let answer = resolve();
    let owned_key = lookup.to_owned();
    INLINE_RESOLVE_STATE.with(|s| {
        if let Some(memo) = s.borrow_mut().memo.as_mut() {
            if memo.count < INLINE_MEMO_MAX_ENTRIES {
                memo.count += 1;
                memo.answers.entry(hash).or_default().push((owned_key, answer.clone()));
            }
        }
    });
    (answer, false)
}

/// The callee's receiver profile, fetched on first use. The snapshot clones
/// every map of the method's profile, and most bodies' sites never read it.
/// Offers the `Option` methods the resolver's uses call.
pub(in crate::runtime::interpreter::jit_bridge) struct LazyCalleeProfile<'a> {
    pub(in crate::runtime::interpreter::jit_bridge) shared: &'a SharedVm,
    pub(in crate::runtime::interpreter::jit_bridge) wanted: Option<(ClassId, &'a str, &'a str)>,
    pub(in crate::runtime::interpreter::jit_bridge) cell:
        std::cell::OnceCell<Option<cratonvm_jit::profile::MethodProfile>>,
}

impl LazyCalleeProfile<'_> {
    pub(in crate::runtime::interpreter::jit_bridge) fn as_ref(
        &self,
    ) -> Option<&cratonvm_jit::profile::MethodProfile> {
        self.cell
            .get_or_init(|| {
                let (class_id, name, desc) = self.wanted?;
                self.shared
                    .jit
                    .profile_store
                    .get_profile(&crate::jit::profile::MethodKey {
                        class_id: class_id.as_u32(),
                        method_name: Arc::from(name),
                        descriptor: Arc::from(desc),
                    })
            })
            .as_ref()
    }

    pub(in crate::runtime::interpreter::jit_bridge) fn is_some(&self) -> bool {
        self.as_ref().is_some()
    }
}

/// `CRATONVM_JIT_IR_SUPER_SPECIAL_SPLICE` (the r23 L8 heap-buffer close; perf,
/// **default ON**, `=0` kills): a spliced body's `super.m()` -- an
/// `invokespecial` of its declaring class's direct superclass, not `<init>` --
/// is spliced too. JVMS 6.5 selects its target from that superclass whatever
/// the receiver's class is, so it is as monomorphic as a private or final
/// method; the optimizing tier's monomorphism gate did not ask the opcode.
///
/// Why: `ByteBuffer.clear()` / `flip()` are `{ super.clear(); return this; }`,
/// so `heapbb` spliced into its caller kept `Buffer.clear()` and
/// `Buffer.flip()` as calls in its loops' pre-headers, and a call there keeps
/// every loop-entry predicate out (`affine guard predicate: header N
/// pre-header may trap (.. Call ..)`). Read per nested `invokespecial`.
pub(in crate::runtime::interpreter::jit_bridge) fn ir_super_special_splice_enabled() -> bool {
    true
}

pub(in crate::runtime::interpreter::jit_bridge) fn ir_nested_receiver_evidence_contradicts(
    shared: &SharedVm,
    callee_profile: Option<&cratonvm_jit::profile::MethodProfile>,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    declaring_id: ClassId,
    ipc: usize,
    class_id: u32,
) -> bool {
    if let Some(counts) = callee_profile.and_then(|p| p.receivers.get(&ipc)) {
        let evidence = cratonvm_jit::profile::ReceiverEvidence::of(counts);
        let summary = evidence.summary();
        if summary.types > 1 || summary.top.is_some_and(|(c, _)| c != 0 && c != class_id) {
            return true;
        }
    }
    let Some(artifact) = shared.jit.jit_cache.read().get(
        &Arc::from(callee_class),
        &Arc::from(callee_method),
        &Arc::from(callee_desc),
        declaring_id,
    ) else {
        return false;
    };
    artifact
        .dominant_receiver_at_bci(ipc)
        .is_some_and(|c| c != 0 && c != class_id)
        || artifact
            .mic_slot_census()
            .iter()
            .any(|&(bci, _, _, misses)| bci == ipc && misses > 0)
        // Round rx27 wave 7 (lane chain, `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE`):
        // or the callee's own artifact guard-spliced another class there.
        // Such a site has no inline cache to read (`Matcher.match` pc 121,
        // spliced as the greedy node in its own compile), so the two reads
        // above say nothing, and the constant-pool row (`Pattern$Node`,
        // whose body no receiver ever runs) pre-empted the profiled route.
        || (cratonvm_jit::rx27_chain_entry::enabled()
            && artifact
                .guarded_splice_class_at_bci(ipc)
                .is_some_and(|c| c != class_id))
}

/// Resolve the body a receiver of EXACTLY `receiver_class_id` dispatches to at
/// a call site declared `(cp_class, callee_method, callee_desc)` — PGO-02's
/// guarded-inline resolver.
///
/// A guarded inline compares the receiver's class id against a class taken
/// from the receiver-type profile, then runs a spliced body. Those two only
/// agree if the body is the one that class actually dispatches to. Resolving
/// from `cp_class` instead — the receiver expression's STATIC type — splices
/// the superclass's method behind a guard that just certified the subclass,
/// which is silent wrong code at every overriding site. Starting the JVMS
/// selection walk at the runtime receiver is also what gives `invokeinterface`
/// any reach at all: an interface's own declaration has no `Code`.
///
/// Fail-closed on every shape where "the method found by walking up from the
/// receiver" might NOT be the method real dispatch selects:
///
/// * the receiver class must be a loaded, non-interface, non-array class;
/// * the selected method must not be `private` (a private method is never
///   inherited, so a walk that finds one from a subclass receiver found
///   something dispatch would not);
/// * and if the selected method is declared somewhere OTHER than where the
///   constant-pool reference resolves, it must be genuinely an override:
///   `public`/`protected`, or package-private within the same runtime package.
///   A package-private method in a different package does NOT override
///   (JVMS §5.4.5), and the walk cannot tell the difference on its own.
pub(in crate::runtime::interpreter) fn resolve_receiver_inline_site(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    receiver_class_id: u32,
    cp_class: &str,
    callee_method: &str,
    callee_desc: &str,
    direct_bind: Option<InlineDirectBind<'_>>,
) -> Option<cratonvm_jit::InlineSite> {
    resolve_inline_site_from(
        shared,
        requesting_class_id,
        Some(ClassId::new(receiver_class_id)),
        cp_class,
        callee_method,
        callee_desc,
        0,
        direct_bind,
        false,
        None,
        false,
    )
}

/// Class-hierarchy analysis for one call site — the VM half of
/// `NOTES-deopt2.md` M5.
///
/// Answers, for a site declared `(cp_class, name, descriptor)` in a method of
/// `requesting_class_id`, the single loaded CONCRETE class below `cp_class`
/// that provides that method. `plan_inline` turns a `Some` into an
/// [`cratonvm_jit::InlineVerdict::UniqueConcreteMethod`] — a body bound behind
/// the exact receiver class-id guard — which is the ONLY way a virtual site
/// gets inlined on a compile with no receiver profile at all. Without it
/// `ReceiverShape::Unprofiled` refuses every one of them.
///
/// Three properties are load-bearing, and each is established here rather than
/// assumed by the planner:
///
///  * **The class name is resolved in the REQUESTING class's loader**, through
///    `find_class_by_name_for_class`, exactly as every other resolver on this
///    path does. Two same-named classes in different loaders are the ByteBuddy
///    / servlet-container shape, and a hierarchy answer about the wrong one
///    would guard on a class id no receiver at this site can have.
///  * **The class-manager lock is released before the analysis runs.**
///    `ClassRealm::unique_implementor` takes the L10 read lock itself, and
///    `OrderedPlRwLock` is `parking_lot`-backed: a second `read()` on a thread
///    that already holds one deadlocks as soon as a writer queues between them.
///  * **The answer is a hint, never a proof.** The bind it enables is guarded
///    and the guard's miss edge is unchanged dispatch, so an answer made stale
///    by a later class define costs a compare that always fails and never a
///    wrong method. See `cratonvm_jit::InlineDependency::UniqueConcreteMethod`
///    for what would have to change before an unguarded bind could be sound.
///
/// No separate kill switch: the caller passes this resolver only when it also
/// passes `class_id_name_resolver`, so `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=0`
/// takes CHA away with the rest of the devirtualization it feeds.
pub(in crate::runtime::interpreter) fn resolve_unique_concrete_bind(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    cp_class: &str,
    name: &str,
    descriptor: &str,
) -> Option<cratonvm_jit::UniqueConcreteBind> {
    let static_type = {
        let cm = shared.classes.class_manager.read();
        let static_type = jit_known_class(shared, &cm, cp_class, requesting_class_id)?;
        // JVMS 11 §6.5 invokeinterface/invokevirtual: a PRIVATE resolved method is
        // the selected method, so no receiver class can "implement" it. See
        // cha-binds-a-private-interface-method-to-the-receivers-public-namesake-20260918.md.
        if crate::classloading::invokevirtual_private_declaring_class(
            static_type,
            name,
            descriptor,
            cm.class_store(),
        )
        .is_some()
        {
            return None;
        }
        static_type
    };
    let implementor = shared
        .classes
        .unique_implementor(static_type, name, descriptor)
        .or_else(|| {
            if name == "get"
                && descriptor == "()Ljava/lang/Object;"
                && crate::vm::class_is_java_lang_ref_reference(shared, static_type)
            {
                Some(static_type)
            } else {
                None
            }
        })?;
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] cha unique-implementor {cp_class}.{name}{descriptor} -> class id {}",
            implementor.as_u32()
        );
    }
    Some(cratonvm_jit::UniqueConcreteBind {
        implementor_class_id: implementor.as_u32(),
        static_type_class_id: static_type.as_u32(),
        // The invalidation key. Deliberately the name the SITE carries: that is
        // the string the class-define listener reports through the supertype
        // closure when a second implementor arrives, and therefore the only one
        // `JitCache::invalidate_for_class_change` can match this dependency on.
        static_type_name: cp_class.to_string(),
    })
}

/// The inline resolver's native-shadow screens: does a registration on
/// `class_name` for this method stand in front of the body a splice would run?
///
/// JIT round 20 wave 19, lane yHttp (WALKCOMPAT20-6,
/// `jit-r20-walkcompat-proposals.md`). The three screens of
/// [`resolve_inline_site_from`] asked `find(..).is_some()`: any registration
/// refused the splice. Every other JIT gate asks whether the registration RUNS
/// -- the callee compile gates and the caller seal through
/// [`registered_native_will_run`], the direct-bind gate
/// (`direct_bind_name_refusal`) likewise -- because a `SyntheticStub` on a
/// [`real_protected_stub_class`] whose real body is loaded loses the
/// arbitration at every dispatch door ([`synthetic_stub_yields_with_cm`]): the
/// bytecode is what runs, so a compiled caller may BIND that body but could not
/// SPLICE it. Under `--compatible` that kept `ArrayList.get` / `size` / `add`,
/// `Objects.requireNonNull`, `ReentrantLock.lock` / `unlock`,
/// `AtomicBoolean.compareAndSet`, the `Instant` getters ... a call at every
/// compiled site, where `--real-jdk` (no stubs registered) splices them: one
/// of the `--compatible`-only micro-row gaps no alignment row closes (R16
/// `sublist` 1.48x, `rlock` 3.1x).
///
/// Only that one case changes, and it is the dispatch arbitration's own
/// predicate, monotone while the process runs (see
/// [`registered_native_will_run`]): every other kind, an allow-listed class
/// whose stub does not yield (its real body not loaded or not decoded, a
/// forced triple), and a registration found only through a descriptor quirk
/// (`find_with_kind` reports it a `Bridge`) still refuse.
/// `CRATONVM_JIT_INLINE_SCREEN_YIELDED_STUB=0` restores the bare probe.
///
/// `cm`: the class-manager guard when the caller holds it (screens 2 and 3);
/// `None` takes a read for the one question that needs it (screen 1, asked
/// before the resolver's own guard).
///
/// JIT round 21 (lane refs2): a `Bridge` the dispatch arbitration yields to
/// loaded bytecode is no shadow either ([`bridge_yields_to_loaded_bytecode_for_gates`]).
pub(in crate::runtime::interpreter::jit_bridge) fn inline_screen_native_shadows(
    shared: &SharedVm,
    cm: Option<&crate::classloading::ClassManager>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Some((_, kind)) =
        shared
            .natives
            .native_methods
            .find_with_kind(class_name, method_name, descriptor)
    else {
        return false;
    };
    // JIT round 21 (lane refs2, `CRATONVM_JIT_GATES_YIELDED_BRIDGE`): a
    // `Bridge` over the selected method's `Code` loses at every `--real-jdk`
    // dispatch door, so the body this splice runs is the one dispatch runs.
    if crate::runtime::interpreter::bridge_yields_to_loaded_bytecode_for_gates(
        shared,
        cm,
        class_name,
        method_name,
        descriptor,
        kind,
    ) {
        return false;
    }
    if kind != cratonvm_native_api::NativeKind::SyntheticStub
        || !real_protected_stub_class(class_name)
    {
        return true;
    }
    let yields = match cm {
        Some(cm) => synthetic_stub_yields_with_cm(cm, class_name, method_name, descriptor),
        None => {
            let cm = shared.classes.class_manager.read();
            synthetic_stub_yields_with_cm(&cm, class_name, method_name, descriptor)
        }
    };
    !yields
}

/// The `native-shadow` refusal text of [`resolve_inline_site_from`]'s
/// constant-pool screen (j27 wave 1, lane JNINAT; REFLECT2-21-3 of
/// `jr21-native-shadow-refusals-on-intrinsified-calls-are-not-the-reflect-cost-20261014.md`).
/// Built only under `CRATONVM_DBG_JITC` (the `no!` macro evaluates its reason
/// inside that test). It still starts with `native-shadow` followed by a space,
/// so `jit-native-shadow-rank.py`'s `native-shadow\S*` reads the
/// same reason as before. It names the registration's kind and, when either
/// tier's call-site intrinsic table answers the triple (single-pass
/// `try_resolve_intrinsic`, IR `try_ir_scalar_intrinsic`), that the call is
/// still served there: `Math.max(II)I` read as an inlining blocker in round
/// 21 although both tiers lower it in place.
pub(in crate::runtime::interpreter::jit_bridge) fn native_shadow_refusal_reason(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> String {
    let is_getclass = cratonvm_jit::is_object_get_class_site(method_name, descriptor);
    let is_value_of = (class_name == "java/lang/Integer"
        && method_name == "valueOf"
        && descriptor == "(I)Ljava/lang/Integer;")
        || (class_name == "java/lang/Long"
            && method_name == "valueOf"
            && descriptor == "(J)Ljava/lang/Long;");
    native_shadow_refusal_text(
        shared
            .natives
            .native_methods
            .find_with_kind(class_name, method_name, descriptor)
            .map(|(_, kind)| kind),
        cratonvm_jit::try_resolve_intrinsic(class_name, method_name, descriptor).is_some()
            || is_getclass
            || is_value_of,
        cratonvm_jit::ir::try_ir_scalar_intrinsic(class_name, method_name, descriptor).is_some()
            || is_getclass
            || is_value_of,
    )
}

/// [`native_shadow_refusal_reason`]'s text from its three facts (pure, for the
/// unit test).
pub(in crate::runtime::interpreter::jit_bridge) fn native_shadow_refusal_text(
    kind: Option<cratonvm_native_api::NativeKind>,
    sp_intrinsic: bool,
    ir_intrinsic: bool,
) -> String {
    let kind = match kind {
        Some(k) => format!("{k:?}"),
        None => "none".to_string(),
    };
    let served = match (sp_intrinsic, ir_intrinsic) {
        (true, true) => "; the call is intrinsified at the site (sp, ir)",
        (true, false) => "; the call is intrinsified at the site (sp)",
        (false, true) => "; the call is intrinsified at the site (ir)",
        (false, false) => "",
    };
    format!("native-shadow (kind={kind}{served})")
}

/// `CRATONVM_JIT_SPLICE_DECLARING_CLASS_FRAME` -- JIT round 21 wave 14, lane
/// trace (`docs/feature-designs/deopt-osr.md`);
/// trace correctness, both modes, **default ON**, `=0` is the kill word. A
/// constant-pool splice whose selected body is declared ABOVE the
/// constant-pool class (an inherited default or superclass method reached
/// through `invokevirtual K3.d1`) names the declaring class for its frame and
/// its dependency, as a receiver-resolved splice already does. Read once per
/// resolution that could differ (compile time, cold): no process static.
pub(in crate::runtime::interpreter::jit_bridge) fn splice_declaring_class_frame_enabled() -> bool {
    true
}

/// Does a spliced body name its DECLARING class (`true`) or the
/// constant-pool class (`false`) in the `InlineSite` it resolves to -- the
/// label and class id every stack trace and `StackWalker` frame of the
/// spliced level is reported as? A receiver resolution always names the
/// declaring class; a constant-pool resolution does when the selection left
/// the constant-pool class and `enabled` (asked only then) says so. Pure, for
/// the unit test.
pub(in crate::runtime::interpreter::jit_bridge) fn splice_names_declaring_class(
    receiver_resolution: bool,
    declaring_id: u32,
    cp_class_id: u32,
    enabled: impl FnOnce() -> bool,
) -> bool {
    receiver_resolution || (declaring_id != cp_class_id && enabled())
}

/// JIT round 21 (lane tl2, `CRATONVM_JIT_INLINE_SCREEN_RETIRED_MASK`, default
/// ON; `jr21-tl2-weakref-refersto-and-get-cost-FIXED-20261015.md`):
/// is the `Bridge` registered on the SELECTED method's declaring class one the
/// dispatch doors never run for this site, because the walk from the site's
/// class (or the receiver's) up to it passes a RETIRED row for the same method
/// ([`super::native_override::retired_row_masks_ancestor_bridge`], the mask
/// every ancestor walk applies under `--real-jdk`)?
///
/// The shape: `WeakReference.refersTo` is retired (the JDK body runs), but
/// `refersTo` is DECLARED on `Reference`, which keeps its `Bridge`
/// (`native_ref_refers_to`). Dispatch masks that bridge for every
/// `WeakReference` receiver and runs `Reference.refersTo`'s bytecode
/// (`refersToImpl` -> `refersTo0`); the inline resolver's screen asked only
/// "is anything registered on the declaring class" and refused the splice
/// (`native-shadow-on-selected-method`), so every compiled
/// `weakRef.refersTo(x)` was a call (1794 ns per 16 on `R21TlocalSplit`
/// `weakref-refersto`, HotSpot 4). With the mask the splice runs the same body
/// dispatch runs. `Compatible` answers `false` inside the mask (no change), an
/// `Intrinsic` is never masked, and `=0` restores the bare screen.
pub(in crate::runtime::interpreter::jit_bridge) fn selected_bridge_masked_by_retired_row(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    from: (
        crate::classloading::ClassId,
        Option<crate::classloading::ClassId>,
    ),
    declaring_id: crate::classloading::ClassId,
    declaring_class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if from.0 == declaring_id && from.1.is_none_or(|r| r == declaring_id) {
        return false;
    }
    let Some((_, kind)) =
        shared
            .natives
            .native_methods
            .find_with_kind(declaring_class_name, method_name, descriptor)
    else {
        return false;
    };
    super::native_override::retired_row_masks_ancestor_bridge(
        shared,
        cm,
        from,
        declaring_id,
        method_name,
        descriptor,
        kind,
    )
}

/// [`resolve_inline_site_from_body`], memoized within the resolution tree
/// ([`inline_memoized`]): a nested resolution whose key (callee, receiver,
/// depth, mode, binder and the thread-local hand-offs) repeats one already
/// answered in the same tree gets that answer instead of re-resolving the
/// body's whole nested subtree. A hit consumes the hand-offs exactly as the
/// body does.
pub(in crate::runtime::interpreter::jit_bridge) fn resolve_inline_site_from(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    receiver_class_id: Option<ClassId>,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    nest_depth: usize,
    direct_bind: Option<InlineDirectBind<'_>>,
    ir_mode: bool,
    substituted: Option<&SubstitutedOwners>,
    planning_only: bool,
) -> Option<cratonvm_jit::InlineSite> {
    // The substituted-owner list is mutable state outside the key, and the
    // refusal census prints per resolution.
    let nest_depth = if ir_mode && callee_class == "sun/misc/Unsafe" && nest_depth > 0 {
        nest_depth - 1
    } else {
        nest_depth
    };
    let cacheable = substituted.is_none() && !crate::runtime::env_cache::dbg_jitc();
    let arg_classes = cratonvm_jit::ir::take_splice_arg_classes();
    let arg_objects = if nest_depth == 0 {
        cratonvm_jit::ir::splice_site_objects()
    } else {
        cratonvm_jit::ir::take_splice_arg_objects()
    };
    let super_special = IR_SUPER_SPECIAL_SITE.with(std::cell::Cell::get);
    let size_cap = INLINE_RESOLVE_STATE.with(|s| s.borrow().exact_size_cap);
    let lookup = InlineMemoLookup {
        requesting: requesting_class_id.as_u32(),
        receiver: receiver_class_id.map(|r| r.as_u32()),
        callee_class,
        callee_method,
        callee_desc,
        nest_depth,
        ir_mode,
        planning_only,
        bind: direct_bind.map_or(0, |bind| std::ptr::from_ref(bind).cast::<()>() as usize),
        arg_classes: &arg_classes,
        arg_objects: &arg_objects,
        super_special,
        chain_tree: chain_tree_flag(),
        size_cap,
    };
    let (answer, hit) = inline_memoized(
        cacheable,
        &lookup,
        || {
            cratonvm_jit::ir::set_splice_arg_classes(arg_classes.clone());
            if nest_depth != 0 {
                cratonvm_jit::ir::set_splice_arg_objects(arg_objects.clone());
            }
        },
        || {
            resolve_inline_site_from_body(
                shared,
                requesting_class_id,
                receiver_class_id,
                callee_class,
                callee_method,
                callee_desc,
                nest_depth,
                direct_bind,
                ir_mode,
                substituted,
                planning_only,
            )
        },
    );
    if hit {
        IR_SUPER_SPECIAL_SITE.with(|c| c.set(false));
        INLINE_RESOLVE_STATE.with(|s| s.borrow_mut().exact_size_cap = None);
    }
    answer
}

inline_gates! {
    cold_blind_dispatch_weight_enabled = cratonvm_jit::ir_evidence::cold_blind_dispatch_weight_enabled,
    compact_ref_fields_enabled = cratonvm_types::compact_ref_fields_enabled,
    direct_jit_callee_calls_enabled = cratonvm_jit::direct_jit_callee_calls_enabled,
    invoke_site_classifier_enabled = invoke_site_classifier_enabled,
    ir_indy_splice_census_enabled = ir_indy_splice_census_enabled,
    ir_inline_cold_athrow_enabled = cratonvm_jit::ir::ir_inline_cold_athrow_enabled,
    ir_lambda_sam_splice_enabled = cratonvm_jit::ir::ir_lambda_sam_splice_enabled,
    ir_lambda_sam_splice_nested_enabled = cratonvm_jit::ir::ir_lambda_sam_splice_nested_enabled,
    ir_nested_bimorphic_exact_row_enabled = cratonvm_jit::ir::ir_nested_bimorphic_exact_row_enabled,
    ir_nested_chain_splice_enabled = cratonvm_jit::r25_w7b_chain_splice::ir_nested_chain_splice_enabled,
    ir_nested_guard_class_evidence_enabled = cratonvm_jit::ir::ir_nested_guard_class_evidence_enabled,
    ir_nested_guarded_hot_cap_enabled = cratonvm_jit::ir_nested_guarded_hot_cap_enabled,
    ir_nested_guarded_interface_enabled = cratonvm_jit::ir::ir_nested_guarded_interface_enabled,
    ir_nested_guarded_splice_enabled = cratonvm_jit::ir::ir_nested_guarded_splice_enabled,
    ir_on_spin_wait_barrier_enabled = cratonvm_jit::ir_lower::on_spin_wait::ir_on_spin_wait_barrier_enabled,
    ir_on_spin_wait_nested_barrier_enabled = cratonvm_jit::ir_lower::on_spin_wait::ir_on_spin_wait_nested_barrier_enabled,
    ir_recursive_inline_enabled = cratonvm_jit::ir::ir_recursive_inline_enabled,
    ir_self_copy_by_id_enabled = cratonvm_jit::ir_self_copy_by_id_enabled,
    ir_self_splice_base_arm_keeps_call_enabled = cratonvm_jit::ir::ir_self_splice_base_arm_keeps_call_enabled,
    ir_site_trap_enabled = cratonvm_jit::ir::ir_site_trap_enabled,
    ir_splice_anewarray_enabled = cratonvm_jit::ir::ir_splice_anewarray_enabled,
    ir_splice_branch_enabled = cratonvm_jit::ir::ir_splice_branch_enabled,
    ir_splice_callee_profile_seed_enabled = cratonvm_jit::ir_splice_callee_profile_seed_enabled,
    ir_splice_chain_fences_enabled = cratonvm_jit::ir::ir_splice_chain_fences_enabled,
    ir_splice_cold_handler_arm_enabled = cratonvm_jit::ir::ir_splice_cold_handler_arm_enabled,
    ir_splice_cold_nested_hot_path_enabled = cratonvm_jit::ir_splice_cold_nested_hot_path_enabled,
    ir_splice_cold_unloaded_typecheck_enabled = cratonvm_jit::ir::ir_splice_cold_unloaded_typecheck_enabled,
    ir_splice_const_division_enabled = cratonvm_jit::ir::ir_splice_const_division_enabled,
    ir_splice_fence_finally_enabled = cratonvm_jit::ir::ir_splice_fence_finally_enabled,
    ir_splice_ffm_element_enabled = cratonvm_jit::ir_splice_ffm_element_enabled,
    ir_splice_final_direct_enabled = ir_splice_final_direct_enabled,
    ir_splice_getstatic_enabled = cratonvm_jit::ir::ir_splice_getstatic_enabled,
    ir_splice_indy_capture_enabled = cratonvm_jit::ir::ir_splice_indy_capture_enabled,
    ir_splice_indy_enabled = cratonvm_jit::ir::ir_splice_indy_enabled,
    ir_splice_ldc_enabled = ir_splice_ldc_enabled,
    ir_splice_leaf_past_depth_enabled = cratonvm_jit::ir::ir_splice_leaf_past_depth_enabled,
    ir_splice_multi_return_enabled = cratonvm_jit::ir::ir_splice_multi_return_enabled,
    ir_splice_nested_exact_receiver_enabled = cratonvm_jit::ir_splice_nested_exact_receiver_enabled,
    ir_splice_nested_exact_trap_enabled = cratonvm_jit::ir::ir_splice_nested_exact_trap_enabled,
    ir_splice_nested_profile_trap_enabled = cratonvm_jit::ir::ir_splice_nested_profile_trap_enabled,
    ir_splice_prune_feed_enabled = cratonvm_jit::ir_splice_prune_feed_enabled,
    ir_splice_putstatic_enabled = cratonvm_jit::ir::ir_splice_putstatic_enabled,
    ir_splice_ref_ldc_enabled = cratonvm_jit::ir::ir_splice_ref_ldc_enabled,
    ir_splice_typecheck_enabled = cratonvm_jit::ir::ir_splice_typecheck_enabled,
    ir_splice_unprofiled_nonnull_typecheck_enabled = cratonvm_jit::ir::ir_splice_unprofiled_nonnull_typecheck_enabled,
    ir_splice_unrecorded_call_exit_enabled = cratonvm_jit::ir::ir_splice_unrecorded_call_exit_enabled,
    ir_splice_unresolved_ldc_exit_enabled = cratonvm_jit::ir::ir_splice_unresolved_ldc_exit_enabled,
    ir_splice_unresolved_new_exit_enabled = cratonvm_jit::ir::ir_splice_unresolved_new_exit_enabled,
    ir_super_special_splice_enabled = ir_super_special_splice_enabled,
    ir_sync_splice_enabled = cratonvm_jit::ir::ir_sync_splice_enabled,
    ir_sync_splice_multi_return_enabled = cratonvm_jit::ir::ir_sync_splice_multi_return_enabled,
    ir_sync_splice_nested_same_receiver_enabled = cratonvm_jit::ir::ir_sync_splice_nested_same_receiver_enabled,
    ir_sync_splice_self_class_holds_lock_enabled = cratonvm_jit::ir::ir_sync_splice_self_class_holds_lock_enabled,
    ir_volatile_fields_enabled = cratonvm_jit::ir::ir_volatile_fields_enabled,
    jit_cycle_direct_bind_enabled = cratonvm_jit::jit_cycle_direct_bind_enabled,
    jit_cycle_edge_cell_enabled = cratonvm_jit::jit_cycle_edge_cell_enabled,
    jit_field_site_faithful_enabled = jit_field_site_faithful_enabled,
    jit_ir_referent_splice_enabled = jit_ir_referent_splice_enabled,
    jit_late_static_cell_enabled = cratonvm_jit::jit_late_static_cell_enabled,
    keep_lambda_site_caller_enabled = keep_lambda_site_caller_enabled,
    keep_loop_rare_row_enabled = keep_loop_rare_row_enabled,
    keep_loop_splice_caller_enabled = keep_loop_splice_caller_enabled,
    nested_exact_yields_to_profile_enabled = nested_exact_yields_to_profile_enabled,
    nested_lambda_sam_guarded_enabled = nested_lambda_sam_guarded_enabled,
    prefix_kept_enabled = cratonvm_jit::r25_w10_3b::prefix_kept_enabled,
    sp_nested_guard_strict_share_enabled = cratonvm_jit::sp_nested_guard_strict_share_enabled,
    sp_nested_miss_share_refuse_enabled = cratonvm_jit::sp_nested_miss_share_refuse_enabled,
    sp_splice_indy_constant_enabled = cratonvm_jit::ir::sp_splice_indy_constant_enabled,
    sp_splice_newarray_enabled = cratonvm_jit::ir::sp_splice_newarray_enabled,
    sp_splice_typecheck_enabled = cratonvm_jit::ir::sp_splice_typecheck_enabled,
    splice_never_run_private_exit_enabled = splice_never_run_private_exit_enabled,
    splice_never_run_static_exit_enabled = splice_never_run_static_exit_enabled,
    splice_unbindable_nested_to_call_enabled = splice_unbindable_nested_to_call_enabled,
}

/// The VM's latched [`InlineGates`]; read afresh while a test override is
/// installed (the latch would hide it, or publish its values to the process).
pub(in crate::runtime::interpreter::jit_bridge) fn inline_gates(
    shared: &SharedVm,
) -> std::borrow::Cow<'_, InlineGates> {
    if cratonvm_types::flags::overrides_active() {
        std::borrow::Cow::Owned(InlineGates::read())
    } else {
        std::borrow::Cow::Borrowed(shared.jit.inline_gates.get_or_init(InlineGates::read))
    }
}
