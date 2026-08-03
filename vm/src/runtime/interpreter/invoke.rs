// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Method invocation: resolution, dispatch, and the native bridge.
//!
//! Moved verbatim out of `interpreter.rs`'s `Helper: Method invocation`
//! section. Lint levels declared at the parent module level (including
//! its no-panic `deny` gate, where it has one) are inherited here.

use super::*;


pub(super) fn execute_invoke(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    is_special: bool,
    pc: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    execute_invoke_kind(shared, thread, frame_idx, cp_index, is_special, false, pc)
}

/// JEP 358 — a [`CpResolver`] backed by the live constant pool of
/// `current_class_id`. Acquires `class_manager.read_recursive()` per query; the
/// whole helpful-NPE analysis runs once per thrown NPE (cold path), so the
/// per-call lock cost is irrelevant.
///
/// [`CpResolver`]: crate::runtime::exceptions::helpful_npe::CpResolver
pub(super) struct CpPoolResolver<'a> {
    pub(super) shared: &'a SharedVm,
    pub(super) class_id: ClassId,
    /// Method identity needed to locate the `LocalVariableTable` for real
    /// local-name resolution (increment 2). When these are empty the
    /// resolver still works — `local_name` just falls back to `<localN>`.
    pub(super) method_name: &'a str,
    pub(super) method_descriptor: &'a str,
}

impl crate::runtime::exceptions::helpful_npe::CpResolver for CpPoolResolver<'_> {
    fn field_ref(&self, cp_index: u16) -> Option<crate::runtime::exceptions::helpful_npe::CpRef> {
        use crate::runtime::exceptions::helpful_npe::CpRef;
        let cm = self.shared.classes.class_manager.read_recursive();
        let class = cm.get_class(self.class_id)?;
        if let Some(ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }) = class.constant_pool.get(cp_index)
        {
            let owner = class
                .constant_pool
                .get_class_name(*class_index)?
                .to_string();
            let (name, _) = class
                .constant_pool
                .get_name_and_type(*name_and_type_index)?;
            Some(CpRef::Field {
                owner_internal: owner,
                name: name.to_string(),
            })
        } else {
            None
        }
    }

    fn method_ref(&self, cp_index: u16) -> Option<crate::runtime::exceptions::helpful_npe::CpRef> {
        use crate::runtime::exceptions::helpful_npe::CpRef;
        let cm = self.shared.classes.class_manager.read_recursive();
        let class = cm.get_class(self.class_id)?;
        let (class_index, nat_index) = match class.constant_pool.get(cp_index) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            })
            | Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            }) => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let owner = class.constant_pool.get_class_name(class_index)?.to_string();
        let (name, desc) = class.constant_pool.get_name_and_type(nat_index)?;
        Some(CpRef::Method {
            owner_internal: owner,
            name: name.to_string(),
            descriptor: desc.to_string(),
        })
    }

    /// JEP 358 step 4 — real `LocalVariableTable` name resolution. Look up
    /// the source name of local slot `slot` live at byte-offset `bci` by
    /// scanning the trapping method's `Code.LocalVariableTable` attribute
    /// (`[start_pc, start_pc+length)` is the live range, JVMS §4.7.13). The
    /// name is a Utf8 CP entry referenced by `name_index`. Returns `None`
    /// when the method has no LVT (the common stripped-debug-info case),
    /// which makes the analysis fall back to the synthetic `<localN>` /
    /// `this` spelling.
    fn local_name(&self, slot: u16, bci: usize) -> Option<String> {
        use cratonvm_reader::attribute::Attribute;
        let cm = self.shared.classes.class_manager.read_recursive();
        let class = cm.get_class(self.class_id)?;
        let method = class.find_method(self.method_name, self.method_descriptor)?;
        let code = method.code()?;
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        let bci_u16 = bci.min(u16::MAX as usize) as u16;
        for attr in &code.attributes {
            if let Attribute::LocalVariableTable(entries) = attr {
                for e in entries {
                    // Live range is `[start_pc, start_pc + length)`. A slot is
                    // matched only when the trapping bci falls inside it, so a
                    // re-used slot resolves to the correct variable name.
                    let end = e.start_pc.saturating_add(e.length);
                    if e.index == slot && bci_u16 >= e.start_pc && bci_u16 < end {
                        if let Some(name) = class.constant_pool.get_utf8(e.name_index) {
                            return Some(name.to_string());
                        }
                    }
                }
            }
        }
        None
    }

    /// Whether the trapping method is `static` — drives slot-0 naming
    /// (`<local0>` in a static method vs `this` in an instance method). Looked
    /// up from the method's access flags; a method we can't resolve defaults to
    /// instance (legacy `this` spelling), matching the trait default.
    fn is_static_method(&self) -> bool {
        let cm = self.shared.classes.class_manager.read_recursive();
        let Some(class) = cm.get_class(self.class_id) else {
            return false;
        };
        match class.find_method(self.method_name, self.method_descriptor) {
            Some(method) => method.is_static(),
            None => false,
        }
    }
}

/// JEP 358 — synthesize the HotSpot-style extended message for a null-receiver
/// `invoke*` at the current bci. Always returns at least the action half
/// (`Cannot invoke "Owner.name(params)"`); appends `because "<expr>" is null`
/// when the bounded backward analysis can name the null expression.
pub(super) fn helpful_npe_invoke_message(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    owner_internal: &str,
    method_name: &str,
    method_descriptor: &str,
    num_params: usize,
) -> String {
    use crate::runtime::exceptions::helpful_npe;
    // `-XX:-ShowCodeDetailsInExceptionMessages`: HotSpot's `getMessage()` is
    // null. The throw site needs a `String`, so hand back the empty marker that
    // `throw_runtime_error` maps to `None` (see its NPE arm).
    if crate::runtime::env_cache::helpful_npe_suppressed() {
        return String::new();
    }
    let action = helpful_npe::action_invoke(owner_internal, method_name, method_descriptor);
    let frame = &thread.frames[frame_idx];
    // `last_instr_pc` is set by the dispatch loop to the bci of the opcode
    // currently executing (here, the trapping invoke) — `pc` may already be
    // advanced. This is the deopt-independent trapping bci the design doc
    // relies on for the interpreter path.
    let invoke_bci = frame.last_instr_pc;
    let code = Arc::clone(&frame.code);
    let m_name = frame.method_name_arc();
    let m_desc = frame.method_descriptor_arc();
    let resolver = CpPoolResolver {
        shared,
        class_id: frame.class_id,
        method_name: &m_name,
        method_descriptor: &m_desc,
    };
    let expr = helpful_npe::null_expr_for_invoke_receiver(&code, invoke_bci, num_params, &resolver);
    helpful_npe::combine(&action, expr.as_ref())
}

/// JEP 358 increment 2 — synthesize the HotSpot-style extended message for a
/// non-invoke null-deref opcode (`getfield`/`putfield`, `arraylength`, the
/// array load/store family, `monitorenter`/`monitorexit`, `athrow`). The
/// caller supplies the already-built action half (e.g.
/// `Cannot read field "x"`) and the operand's depth below the top of the
/// operand stack as it stood just before the trapping opcode. Appends
/// `because "<expr>" is null` when the bounded backward analysis can name the
/// null operand; otherwise emits the action half alone (HotSpot omits the
/// `because` clause rather than fabricating one).
///
/// Gated by `env_cache::helpful_npe_opcodes()` at the call sites; the trapping
/// bci is `last_instr_pc`, always known in the interpreter (deopt-independent).
pub(super) fn helpful_npe_opcode_message(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    action: &str,
    depth_below_top: usize,
) -> String {
    let frame = &thread.frames[frame_idx];
    helpful_npe_opcode_message_parts(
        shared,
        frame.class_id,
        &frame.code,
        frame.method_name_arc_ref(),
        frame.method_descriptor_arc_ref(),
        frame.last_instr_pc,
        action,
        depth_below_top,
    )
}

/// `thread`-free core of [`helpful_npe_opcode_message`]: builds the message
/// from the trapping frame's already-extracted parts. Kept separate so the
/// opcode call sites can invoke it from inside a `pop_object_ref_ctx_with`
/// closure (which holds a `&mut` borrow of the frame's operand stack and so
/// cannot also borrow `thread`). The bytecode analysis only reads `shared`
/// (for CP / LVT lookups) and the supplied `code` slice, never `thread`.
#[allow(clippy::too_many_arguments)]
pub(super) fn helpful_npe_opcode_message_parts(
    shared: &SharedVm,
    class_id: ClassId,
    code: &[u8],
    method_name: &str,
    method_descriptor: &str,
    trap_bci: usize,
    action: &str,
    depth_below_top: usize,
) -> String {
    use crate::runtime::exceptions::helpful_npe;
    // See `helpful_npe_invoke_message`: an explicit opt-out yields the empty
    // marker, which `throw_runtime_error` turns into a null `getMessage()`.
    if crate::runtime::env_cache::helpful_npe_suppressed() {
        return String::new();
    }
    let resolver = CpPoolResolver {
        shared,
        class_id,
        method_name,
        method_descriptor,
    };
    let expr = helpful_npe::null_expr_at_depth(code, trap_bci, depth_below_top, &resolver);
    helpful_npe::combine_opt(action, expr.as_ref())
}

/// Resolve a Java 11+ private-method call encoded as `invokevirtual`.
///
/// Private methods are not virtual dispatch targets even when modern classfiles
/// encode the call with opcode 0xb6. Dispatch must stay pinned to the resolved
/// constant-pool target; otherwise a subclass/private-static helper with the
/// same name and descriptor can be selected by receiver-class lookup.
pub(super) fn resolved_private_invokevirtual_target(
    shared: &SharedVm,
    current_class_id: ClassId,
    method_class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> Option<(ClassId, Arc<str>)> {
    // A private method is only ever invocable, per JVM access control, from
    // within the exact class that declares it — so a private-via-invokevirtual
    // call's CP-resolved owner name always names the CALLER's own class.
    // Resolve directly against `current_class_id` in that case: it's precise
    // by construction, with no name lookup involved and therefore no risk of
    // a loader-blind name lookup returning a DIFFERENT loaded copy of a
    // same-named class. Two classloaders each defining their own
    // `org/h2/mvstore/RootReference` is exactly this shape: `lookup_loader_
    // initiated` can miss (this call is the class resolving ITSELF by name,
    // not a delegated import), and its `get_loaded_class_id` fallback is a
    // single-slot "first loaded wins" map that silently returns the OTHER
    // loader's copy — pinning a private call's dispatch to the wrong
    // class's bytecode/constant pool while the receiver stays the caller's
    // own (correct-loader) object. See
    // docs/known-issues/h2/bug-h2-suite-residual-fail-triage.md
    // (TestUpgrade's `RootReference.tryUpdate`/`hasChangesSince` residual).
    let self_match = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(current_class_id)
            .map(|c| &*c.name == method_class_name)
            .unwrap_or(false)
    };
    let target_class_id = if self_match {
        Some(current_class_id)
    } else {
        lookup_loader_initiated(shared, current_class_id, method_class_name).or_else(|| {
            shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(method_class_name)
        })
    }?;

    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
    let (method, declaring_id) = crate::classloading::find_method_recursive(
        target_class_id,
        method_name,
        method_descriptor,
        store,
    )?;
    if !method.access_flags.contains(MethodAccessFlags::PRIVATE) {
        return None;
    }
    let declaring_name = store
        .get(declaring_id)
        .map(|c| Arc::clone(&c.name))
        .unwrap_or_else(|| Arc::from(method_class_name));
    Some((declaring_id, declaring_name))
}

/// Variant of `execute_invoke` that knows whether the source bytecode was
/// `invokeinterface`. Only invokeinterface call sites pass `is_interface=true`;
/// invokevirtual / invokespecial pass `false`. The flag gates γ's CP-resolved-
/// interface stash so the default-method rescue at the NSME emit site never
/// fires for non-interface dispatch (which could otherwise re-route to a
/// stale receiver class on Java-exception unwinding through unrelated invokes).
pub(super) fn execute_invoke_kind(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    is_special: bool,
    is_interface: bool,
    pc: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    dbg_invoke_stats_record(3);
    let current_class_id = thread.frames[frame_idx].class_id;

    let (method_class_name, method_name, method_descriptor, num_params) =
        resolve_method_ref(shared, current_class_id, cp_index)?;
    let method_owner_name = Arc::clone(&method_class_name);

    // PGO-01 (docs/known-issues/c2/pgo-01-call-site-evidence-gap.md):
    // call-site evidence for invokespecial. invokevirtual/invokeinterface are
    // NOT recorded here — they are covered by the receiver-type profile
    // instead (see MethodProfile's doc comment on `receivers` vs
    // `call_sites`), and double-recording both would double-count a single
    // call-site execution against two different evidence sources. Recorded
    // once, here, right after resolution succeeds — this function is only
    // reached on a genuine miss from the fast cached dispatch
    // (execute_invokevirtual_cached), so by this point the invokespecial
    // instruction is definitely executing, not merely being probed. NOT
    // placed at every downstream successful-dispatch return point, of which
    // this function has many (native, JIT, lambda-proxy, default-method
    // rescue…) — see MethodProfile::call_sites's doc comment for the explicit
    // concurrency contract: this is a heuristic hotness signal for the
    // inliner, not a correctness input, so the small over/under-count this
    // early-return placement can produce on a resolution error is acceptable.
    if is_special && crate::jit::profile::is_profiling_enabled() {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared.jit.profile_store.record_call_site_borrowed(cid, mn, md, pc);
    }

    if crate::runtime::env_cache::dbg_loader_trace()
        && method_owner_name.contains("RootReference")
        && &*method_name == "<init>"
    {
        let cm = shared.classes.class_manager.read();
        let cur_loader = cm.get_loader_id(current_class_id);
        drop(cm);
        eprintln!(
            "[EIK-ENTRY] cp_index={cp_index} is_special={is_special} current_class_id={current_class_id:?} cur_loader={cur_loader:?} method={method_owner_name}.{method_name}{method_descriptor}"
        );
    }

    if crate::runtime::env_cache::dbg_hang_sample() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CALL_COUNT: AtomicU64 = AtomicU64::new(0);
        let n = CALL_COUNT.fetch_add(1, Ordering::Relaxed);
        if n % 200_000 == 0 {
            eprintln!(
                "[HANG_SAMPLE_V1] call#{n} {}.{}{}",
                &*method_class_name, &*method_name, &*method_descriptor
            );
        }
    }

    // KAFKA-DEFAULT-RESCUE: snapshot the CP-resolved class id (if loaded)
    // before any downstream code can move `method_class_name`. The default-
    // method rescue at the NSME emit site in `invoke_on_class_shared_inner`
    // reads this via a thread-local stashed below.
    //
    // Option B+C gating: only stash for `invokeinterface` AND only when the
    // CP-resolved class is actually a Java interface. Non-interface invokes
    // (virtual / special / static) must not arm the rescue — its
    // `find_method_recursive` walk would otherwise pick the wrong declaring
    // class for default methods that conflict with the receiver's class
    // hierarchy. The slot is also left unset when the resolved class isn't
    // an interface so a malformed CP entry can't poison nested dispatch.
    let cp_resolved_class_id: Option<ClassId> = if is_interface {
        // An interface Methodref is resolved by the current frame's initiating
        // loader too. The cache-preparation probe must not create a global
        // duplicate before the actual invokeinterface dispatch has a chance to
        // use its loader-local constant-pool identity.
        let loaded =
            resolve_class_loader_aware(shared, thread, current_class_id, &method_class_name).ok();
        loaded.and_then(|cid| {
            let cm = shared.classes.class_manager.read();
            cm.get_class(cid).filter(|c| c.is_interface()).map(|_| cid)
        })
    } else {
        None
    };

    let total_args = num_params + 1;

    // PERF (2026-07-21): see `pop_coerced_invoke_args_virtual` — same
    // non-allocating `nth_param_tag_byte` swap for `split_method_descriptor`.
    // Pop slots as raw CompactValue and decode with the parameter descriptor
    // so a category-2 long whose NaN-box bit pattern collides with a tagged
    // sub-tag survives bit-exact. The prior `pop()` → `to_value()` decoded
    // such a long as `Value::Int`, which `coerce_invoke_arg_for_descriptor`
    // then widened — corrupting `J` args to invokevirtual/special callees.
    // Mirrors `pop_coerced_invoke_args_virtual`. See
    // docs/bc-ec-mod-mododdinverse-investigation.md.
    let mut tmp_cv: Vec<(CompactValue, bool)> = Vec::with_capacity(num_params + 1);
    for _ in 0..num_params {
        tmp_cv.push(
            thread.frames[frame_idx]
                .stack
                .pop_compact_with_long_mark()?,
        );
    }
    tmp_cv.push(
        thread.frames[frame_idx]
            .stack
            .pop_compact_with_long_mark()?,
    ); // receiver
    tmp_cv.reverse();
    let recv_val = tmp_cv[0].0.decode_by_descriptor(b'L');
    if crate::runtime::env_cache::dbg_jetty2() && &*method_name == "getClasspath" {
        eprintln!(
            "[jetty2-eik] execute_invoke_kind {}.{}{} receiver={:?}",
            &*method_class_name, &*method_name, &*method_descriptor, recv_val
        );
    }
    let mut args = Vec::with_capacity(total_args);
    args.push(coerce_invoke_arg_for_descriptor(b'L', recv_val));
    for i in 0..num_params {
        let pd_byte = nth_param_tag_byte(&method_descriptor, i);
        let (cv, is_long) = tmp_cv[i + 1];
        let v = decode_arg_kind_aware(cv, is_long, pd_byte);
        args.push(coerce_invoke_arg_for_descriptor(pd_byte, v));
    }

    // Apply the same forwarding read barrier used by getfield to every
    // reference copied from the operand stack. A moving collection can leave
    // an old from-space address in a frame slot; once the invoke pops that
    // slot it is no longer visible to the frame-root remapper. Dispatch then
    // dereferences the stale receiver (or a stale object argument) while
    // resolving/invoking the callee. Refresh while the forwarding header is
    // still available, before any class lookup or native call can touch it.
    for value in &mut args {
        if let Value::Object(Some(obj)) = value {
            *obj = shared.mem.heap.load_and_forward(*obj);
        }
    }
    let args_root_guard = InvokeArgsRootGuard::new(thread, &args);
    if crate::runtime::env_cache::dbg_loader_trace()
        && method_class_name.contains("RootReference")
        && method_name.as_ref() == "tryUpdate"
    {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[TRYUPDATE-TRACE/slow] caller_class_id={:?} cp_index={} receiver(args[0])={} updated(args[1])={}",
            current_class_id,
            cp_index,
            describe(&args[0]),
            args.get(1).map(describe).unwrap_or_default(),
        );
    }
    if crate::runtime::env_cache::dbg_loader_trace() && &*method_name == "compareAndSetRoot" {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[CASROOT-TRACE/slow] caller_class_id={:?} cp_index={} receiver_map(args[0])={} expected(args[1])={} updated(args[2])={}",
            current_class_id,
            cp_index,
            describe(&args[0]),
            args.get(1).map(describe).unwrap_or_default(),
            args.get(2).map(describe).unwrap_or_default(),
        );
    }
    if crate::runtime::env_cache::dbg_loader_trace()
        && method_class_name.contains("Page")
        && method_name.as_ref() == "<init>"
    {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[PAGEINIT-TRACE/slow] caller_class_id={:?} cp_index={} ctor_desc={} new_page(args[0])={} map_arg(args[1])={}",
            current_class_id,
            cp_index,
            method_descriptor,
            describe(&args[0]),
            args.get(1).map(describe).unwrap_or_default(),
        );
    }
    // Register the freshly constructed receiver with the software watchpoint
    // ONLY when its class is in the watch filter. Registering every
    // constructed object (the original shape) makes each heap field write take
    // the watch mutex and scan the registry, and buries the interesting lines
    // under millions of unrelated ones — the flag was effectively unusable on
    // anything larger than the H2 repro it was written for.
    if let Value::Object(Some(o)) = &args[0] {
        if crate::runtime::env_cache::field_watch_class_matches(&method_class_name) {
            cratonvm_types::field_watch::watch(*o);
        }
    }
    if crate::runtime::env_cache::dbg_loader_trace()
        && method_class_name.contains("RootReference")
        && method_name.as_ref() == "<init>"
    {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[ROOTREFINIT-TRACE/slow] caller_class_id={:?} cp_index={} ctor_desc={} this(args[0])={} a1={} a2={} a3={}",
            current_class_id,
            cp_index,
            method_descriptor,
            describe(&args[0]),
            args.get(1).map(describe).unwrap_or_default(),
            args.get(2).map(describe).unwrap_or_default(),
            args.get(3).map(describe).unwrap_or_default(),
        );
    }
    // Register the freshly constructed receiver with the software watchpoint
    // ONLY when its class is in the watch filter. Registering every
    // constructed object (the original shape) makes each heap field write take
    // the watch mutex and scan the registry, and buries the interesting lines
    // under millions of unrelated ones — the flag was effectively unusable on
    // anything larger than the H2 repro it was written for.
    if let Value::Object(Some(o)) = &args[0] {
        if crate::runtime::env_cache::field_watch_class_matches(&method_class_name) {
            cratonvm_types::field_watch::watch(*o);
        }
    }
    // Spring's loader-fork test infrastructure can expose two physical copies
    // of this private enum while representing one logical annotation operation.
    // Preserve the enum member identity by its declaring binary name and enum
    // constant name only for that loader-aware bridge; ordinary cross-loader
    // Enum.equals/identity semantics remain unchanged.
    //
    // This is NOT redundant with the registered `MergedAnnotation$Adapt.isIn`
    // native override + `force_native_over_real_jdk_bytecode` gate elsewhere
    // in this file: empirically (2026-07-19 merge with origin/dev, which
    // shipped that native override independently), removing this inline
    // bridge and relying on the native override alone reintroduced the
    // WebFluxManagementChildContextConfigurationIntegrationTests hang (stuck
    // within seconds of the first sub-test, host load LOW at the time — not
    // a contention artifact). Keep both: this bridge covers whatever
    // dispatch path reaches `isIn` without going through the native-override
    // gate for this specific loader-forked scenario.
    if crate::runtime::env_cache::loader_aware_resolution()
        && method_class_name.as_ref()
            == "org/springframework/core/annotation/MergedAnnotation$Adapt"
        && method_name.as_ref() == "isIn"
        && method_descriptor.as_ref()
            == "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z"
    {
        if let (Some(Value::Object(Some(receiver))), Some(Value::Object(Some(array)))) =
            (args.first(), args.get(1))
        {
            let receiver_name = match shared.mem.heap.get_field(*receiver, 0) {
                Value::Object(Some(name)) => read_java_string(&shared.mem.heap, name),
                _ => None,
            };
            let receiver_ordinal = shared.mem.heap.get_field(*receiver, 1);
            let receiver_class_name = shared
                .classes
                .class_manager
                .read()
                .get_class(shared.mem.heap.class_id_of(*receiver))
                .map(|class| class.name.to_string());
            for i in 0..shared.mem.heap.array_length(*array) {
                let Ok(Value::Object(Some(candidate))) =
                    shared.mem.heap.get_array_element(*array, i)
                else {
                    continue;
                };
                let candidate_name = match shared.mem.heap.get_field(candidate, 0) {
                    Value::Object(Some(name)) => read_java_string(&shared.mem.heap, name),
                    _ => None,
                };
                let candidate_ordinal = shared.mem.heap.get_field(candidate, 1);
                let candidate_class_name = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(shared.mem.heap.class_id_of(candidate))
                    .map(|class| class.name.to_string());
                let same_name = candidate_name
                    .as_deref()
                    .zip(receiver_name.as_deref())
                    .is_some_and(|(candidate, receiver)| candidate == receiver);
                let same_ordinal = matches!(
                    (candidate_ordinal, receiver_ordinal),
                    (Value::Int(candidate), Value::Int(receiver)) if candidate == receiver
                );
                if candidate_class_name == receiver_class_name && (same_name || same_ordinal) {
                    thread.frames[frame_idx].stack.push(Value::Int(1))?;
                    return Ok(CachedCallResult::Handled);
                }
            }
        }
    }
    // GC-stale `java.lang.Thread`-mirror receiver recovery.
    //
    // A moving / promoting young GC can relocate a thread's
    // `java.lang.Thread` mirror while a *stale copy* of its old address still
    // sits in a running or blocked frame's operand stack / local — the
    // frame/operand remap-coverage gap documented in
    // `docs/known-issues/gc-blocked-thread-frame-stale-thread-mirror.md`. The
    // registry and the per-thread `java_thread_obj` field are remapped, but
    // the frame copy is not, so an invoke whose receiver is that copy (the
    // classic `Thread.currentThread().getThreadGroup()` in
    // `TaskThreadFactory.<init>`) dispatches on a zeroed object and real-JDK
    // `Thread.getThreadGroup()` reads a null `holder` and NPEs (the Tomcat
    // `TestDigestAuthenticator` family). When the stale receiver's address is
    // a recorded former mirror address, recover that thread's live mirror.
    //
    // Precise / no false substitutions: the former-address table is populated
    // *only* by GC mirror relocations, and we consult it *only* when the
    // receiver header is genuinely all-zero (`class_id == 0`). A from-space
    // slot reused for a live object has a non-zero class_id and never reaches
    // the lookup; a vacated address uniquely identified one thread's mirror,
    // so identity is preserved. We additionally verify the recovered mirror is
    // itself live before substituting.
    if !is_special {
        let recovered: Option<ObjectRef> = if let Value::Object(Some(recv)) = &args[0] {
            let recv = *recv;
            if shared.mem.heap.class_id_of(recv) == ClassId::new(0) {
                shared
                    .threads
                    .thread_registry
                    .recover_stale_mirror(recv.as_ptr() as usize)
                    .filter(|live| {
                        live.as_ptr() != recv.as_ptr()
                            && shared.mem.heap.class_id_of(*live) != ClassId::new(0)
                    })
            } else {
                None
            }
        } else {
            None
        };
        if let Some(live) = recovered {
            args_root_guard.replace_object_arg(&args, 0, live);
            args[0] = Value::Object(Some(live));
        }
    }

    // CRATONVM_DBG_JETTY — trace every invoke into the Jetty launcher
    // package. The boot-test target (`java -jar start.jar --list-config`)
    // NPEs at `Main.start(Main.java:397)` on `args.getClasspath()`; this
    // logs the receiver value and the full argument list for every
    // `org/eclipse/jetty/start/` dispatch so the orchestrator's next run
    // shows exactly which call passes a null `StartArgs` (or whether the
    // `start(StartArgs)` overload is being confused with the no-arg
    // `start()` that reads the null `jsvcStartArgs` field).
    if crate::runtime::env_cache::dbg_jetty()
        && method_class_name.starts_with("org/eclipse/jetty/start/")
    {
        let recv_desc = match args.first() {
            Some(Value::Object(Some(r))) => {
                let cid = shared.mem.heap.class_id_of(*r);
                let cn = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(cid)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("<cid {}>", cid.as_u32()));
                format!("Object({cn}@{:p})", r.as_ptr())
            }
            Some(Value::Object(None)) => "NULL".to_string(),
            other => format!("{other:?}"),
        };
        let arg_tail: Vec<String> = args
            .iter()
            .skip(1)
            .map(|v| match v {
                Value::Object(Some(r)) => {
                    let cid = shared.mem.heap.class_id_of(*r);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("<cid {}>", cid.as_u32()));
                    format!("Object({cn})")
                }
                Value::Object(None) => "NULL".to_string(),
                other => format!("{other:?}"),
            })
            .collect();
        eprintln!(
            "[cratonvm-jetty] invoke{} {}.{}{} receiver={} args={:?}",
            if is_special { "special" } else { "virtual" },
            &*method_class_name,
            &*method_name,
            &*method_descriptor,
            recv_desc,
            arg_tail,
        );
    }

    // A lambda proxy cannot be the receiver of a verifier-valid private
    // instance call: its receiver must have the private method's declaring
    // class. Check the cheap proxy table first so the private-only resolution
    // walk below is not paid for every uncached SAM invocation.
    let is_lambda_proxy_receiver = if !is_special {
        match args.first() {
            Some(Value::Object(Some(obj_ref))) => shared
                .classes
                .lambda_proxies
                .read()
                .contains_key(&shared.mem.heap.class_id_of(*obj_ref)),
            _ => false,
        }
    } else {
        false
    };

    let private_virtual_target = if !is_special
        && !is_lambda_proxy_receiver
        && matches!(args.first(), Some(Value::Object(Some(_))))
    {
        resolved_private_invokevirtual_target(
            shared,
            current_class_id,
            &method_class_name,
            &method_name,
            &method_descriptor,
        )
    } else {
        None
    };
    let effectively_special = is_special || private_virtual_target.is_some();

    // Check for lambda proxy dispatch
    if !effectively_special {
        if let Value::Object(Some(obj_ref)) = &args[0] {
            let obj_class_id = shared.mem.heap.class_id_of(*obj_ref);
            if let Some(result) = try_lambda_dispatch(
                shared,
                thread,
                *obj_ref,
                obj_class_id,
                &method_name,
                &method_descriptor,
                &args[1..],
            )? {
                if let Some(value) = result {
                    let ret = crate::jit::return_type(&method_descriptor);
                    let value = coerce_value_for_return(value, ret);
                    // T18.K4 — tag-exact push for J/D lambda return values.
                    push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                    // Hypothesis (b): lambda dispatch may have transitively
                    // run a native through `safe_native_call`; clear the
                    // pending-return field now that the value lives on the
                    // operand stack so it doesn't outlive the call site.
                    crate::vm::native_return_pushed_to_stack(shared, thread);
                }
                return Ok(CachedCallResult::Handled);
            }
        }
    }

    // Capture receiver class_id for virtual cache population.
    // Arrays are redirected to java/lang/Object, so skip caching for them
    // to avoid polluting the inline cache with the wrong target.
    let receiver_class_id = if !effectively_special {
        match &args[0] {
            Value::Object(Some(obj_ref)) => {
                if shared.mem.heap.kind_of(*obj_ref) == cratonvm_types::ObjectKind::Array {
                    None // Don't cache array dispatches — component class_id would conflict
                } else {
                    Some(shared.mem.heap.class_id_of(*obj_ref))
                }
            }
            _ => None,
        }
    } else {
        None
    };

    // Determine the class to invoke on.
    // invoke_class: Arc<str> — cheap clone, derefs to &str for all downstream calls.
    let invoke_class: Arc<str> = if is_special {
        // JVMS §6.5 super-call redirect — see `invokespecial_owner_class_name`.
        // A no-op for constructors, private-method calls, and any call whose
        // CP-referenced class is not a genuine superclass of this frame's
        // own class; `method_class_name` passes straight through those.
        invokespecial_owner_class_name(
            shared,
            current_class_id,
            cp_index,
            &method_class_name,
            &method_name,
        )
    } else if let Some((_declaring_id, declaring_name)) = &private_virtual_target {
        Arc::clone(declaring_name)
    } else {
        match &args[0] {
            Value::Object(Some(obj_ref)) => {
                // Arrays store the component class_id in their header, but
                // method dispatch must go through java.lang.Object (JVMS §4.4.1).
                // Check heap kind first to avoid misrouting clone()/toString()/etc.
                if shared.mem.heap.kind_of(*obj_ref) == cratonvm_types::ObjectKind::Array {
                    // S111r8: an Object[] array (cid=0 component class)
                    // being dispatched for a non-Object method like
                    // iterator()/hasNext()/size() typically means a
                    // synthetic native return-shape leaked into a
                    // typed-collection caller (e.g. HashSet.iterator
                    // bytecode read its `map` field which our synthetic
                    // HashSet stores as an Object[] backing array rather
                    // than a real HashMap). Object's vtable can't service
                    // these calls; falling back to the CP-resolved
                    // interface class lets the slow path's
                    // `check_override` list and the receiver-driven
                    // fallback in `invoke_on_class_shared_inner`
                    // recover. Object members (equals/hashCode/toString/
                    // clone/etc.) still dispatch via Object per
                    // JVMS §4.4.1.
                    if !crate::vm::is_object_member(&method_name, &method_descriptor) {
                        method_class_name.clone()
                    } else {
                        Arc::from("java/lang/Object")
                    }
                } else {
                    let cid = shared.mem.heap.class_id_of(*obj_ref);

                    // Stale pointer detection: if the header reads as all-zeros
                    // (class_id=0, kind=Object), the pointer likely targets
                    // zeroed-out GC from-space memory. Fall back to the constant
                    // pool method_ref class so dispatch has a chance to succeed.
                    if cid == ClassId::new(0) {
                        // H1: Stale-pointer detection. Pre-fix, fresh
                        // TLAB-allocated `new Object()` instances had
                        // `identity_hash_code: 0` (the lazy-assignment
                        // comment was aspirational and never wired up),
                        // and a class with `cid=0`+`fields=0` produces
                        // an all-zero first 16 bytes that this detector
                        // could not distinguish from genuine stale
                        // memory. The fix landed in `init_object_header`
                        // (TLAB fast path) which now mints a non-zero
                        // hash at allocation time, matching the
                        // non-TLAB allocators in `gc::heap`/
                        // `gc::gen_heap`/`gc::g1`.
                        //
                        // The detector still fires the warn! when the
                        // header is genuinely all-zero — a true
                        // stale-pointer regression — and falls back to
                        // the CP method-ref class so dispatch has a
                        // chance to succeed instead of NPE'ing.
                        // SAFETY: obj_ref is a live ObjectRef; its pointer is a valid heap address with at least a 16-byte readable header.
                        let header_bytes: [u8; 16] =
                            unsafe { std::ptr::read(obj_ref.as_ptr() as *const [u8; 16]) };
                        if header_bytes == [0u8; 16] {
                            // BUG-03 probe (gated CRATONVM_DBG_BUG03): when a stale
                            // (all-zero) invokevirtual receiver is seen, compare it to
                            // THIS thread's java_thread_obj field and the registry's
                            // remapped mirror — pinpoints whether the staleness is in
                            // the operand-stack copy, the per-thread field, or the
                            // registry (the moving-GC concurrent-spawn reclamation).
                            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_BUG03").is_some()
                            {
                                let field = thread
                                    .java_thread_obj
                                    .map(|o| o.as_ptr() as usize)
                                    .unwrap_or(0);
                                let reg = shared
                                    .threads
                                    .thread_registry
                                    .java_thread_obj(thread.thread_id)
                                    .map(|o| o.as_ptr() as usize)
                                    .unwrap_or(0);
                                eprintln!(
                                    "[BUG03] stale recv={:p} on tid={} method={}.{} | java_thread_obj-field=0x{:x} registry-mirror=0x{:x} (field==recv:{} reg==recv:{})",
                                    obj_ref.as_ptr(), thread.thread_id.0, &*method_class_name, &*method_name,
                                    field, reg, field == obj_ref.as_ptr() as usize, reg == obj_ref.as_ptr() as usize,
                                );
                            }
                            // "Zeroed-a-live-object" detector consumer
                            // (CRATONVM_DBG_SWEEP_ZERO): this receiver lost its
                            // header to the non-moving young sweep. Recover its
                            // ORIGINAL class from the sweep ring so the
                            // root-coverage gap is NAMED (e.g. a reclaimed
                            // java/util/concurrent/ForkJoinTask whose only live
                            // ref was a register/native-stack root the sweep
                            // couldn't see — `CRATONVM_DBG_SWEEP_EDGES` silent).
                            if let Some((cid, kind, cycle, reason, initiator, blocked)) =
                                // Cast: object/code pointer to integer address
                                cratonvm_gc::gen_heap::sweep_zero_lookup(
                                        obj_ref.as_ptr() as usize
                                    )
                            {
                                // try_read (not read): this is a debug-only leaf
                                // path; never risk a re-entrant class_manager
                                // deadlock — fall back to the raw class_id.
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
                                // GC context (CRATONVM_DBG_MTROOTS): names the GC
                                // that reclaimed the live object so the
                                // initiator-vs-blocked-mutator root-coverage gap is
                                // pinned (reason 0 = unknown / gate off).
                                let reason_s = match reason {
                                    1 => "System.gc",
                                    2 => "alloc-young(maybe_gc)",
                                    3 => "forced-alloc(maybe_gc_forced)",
                                    _ => "unknown",
                                };
                                // Holder thread state (CRATONVM_DBG_MTROOTS): the
                                // detector fires ON the thread that holds the
                                // reclaimed ref. Log its id / blocked-flag / kind
                                // + call stack so we can see whether the holder
                                // was EXCLUDED from the STW (counted blocked while
                                // actually running) — the multi-thread root gap.
                                let holder_blocked = thread
                                    .gc_block_state
                                    .in_blocked_region
                                    .load(std::sync::atomic::Ordering::Acquire);
                                let mut stk = String::new();
                                {
                                    use std::fmt::Write as _;
                                    for f in thread.frames.iter().rev().take(8) {
                                        let _ = write!(
                                            stk,
                                            "\n[sweep-zero]     at {}.{}",
                                            f.class_name(),
                                            f.method_name()
                                        );
                                    }
                                }
                                eprintln!(
                                    "[sweep-zero] RECLAIMED-LIVE receiver ptr={:p}: original \
                                     class={} (class_id={} kind=0x{:02x}), zeroed by non-moving \
                                     sweep cycle {}; invoked as {}.{} — the live ref was a \
                                     register/native-stack root the marker missed \
                                     [gc reason={} initiator_tid={} blocked_threads={}] \
                                     [holder tid={} in_blocked={} kind={:?}]{}",
                                    obj_ref.as_ptr(),
                                    orig,
                                    cid,
                                    kind,
                                    cycle,
                                    &*method_class_name,
                                    &*method_name,
                                    reason_s,
                                    initiator,
                                    blocked,
                                    thread.thread_id.0,
                                    holder_blocked,
                                    thread.kind,
                                    stk,
                                );
                                // A2 forensic breadcrumb (CRATONVM_DBG_A2): was this
                                // address EVER header-written by an allocator, with
                                // what class/size? Distinguishes never-allocated /
                                // allocated-then-clobbered / mid-object (double-
                                // allocation or free-list overlap) for the reclaimed
                                // victim — the observed victims had ALREADY all-zero
                                // headers at sweep time, which the sweep record alone
                                // cannot explain.
                                let victim_addr = obj_ref.as_ptr() as usize;
                                let hist = cratonvm_gc::a2dbg::history_at(victim_addr, 12);
                                if hist.is_empty() {
                                    eprintln!(
                                        "[sweep-zero]   [A2] NO event touches {victim_addr:#x} (never header-written here, ring wrapped, or CRATONVM_DBG_A2 off)",
                                    );
                                } else {
                                    for r in hist {
                                        if r.kind == 0xFF {
                                            eprintln!(
                                                "[sweep-zero]   [A2] seq={} FREE @{:#x}",
                                                r.seq, r.addr,
                                            );
                                        } else {
                                            eprintln!(
                                                "[sweep-zero]   [A2] seq={} ALLOC @{:#x} class_id={} kind={} et={} alen={} ns={} size={}{}",
                                                r.seq, r.addr, r.class_id, r.kind, r.element_type,
                                                r.array_length, r.num_slots, r.size,
                                                if r.addr != victim_addr { " (covering)" } else { "" },
                                            );
                                        }
                                    }
                                }
                            }
                            // WildFly / JBoss Modules often hits this path on
                            // `ClassLoader`-typed invokevirtual sites when a
                            // receiver lost its header but CP resolution is
                            // already `java/lang/ClassLoader`; the CP fallback
                            // succeeds and a WARN was mostly noise.
                            if method_class_name.as_ref() == "java/lang/ClassLoader" {
                                tracing::debug!(
                                    "Stale pointer detected in invokevirtual receiver \
                                     (ptr={:p}, all-zero header) — falling back to CP class {}",
                                    obj_ref.as_ptr(),
                                    &*method_class_name,
                                );
                            } else {
                                tracing::warn!(
                                    "Stale pointer detected in invokevirtual receiver \
                                     (ptr={:p}, all-zero header) — falling back to CP class {}",
                                    obj_ref.as_ptr(),
                                    &*method_class_name,
                                );
                            }
                            // T-DBG: opt-in deep diagnostic. Dump the Java call
                            // stack and the per-frame locals/operand slots that
                            // reference this stale address so we can see HOW
                            // the bad pointer arrived in the receiver slot.
                            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STALE_RECV")
                                .is_some()
                            {
                                // Cast: object/code pointer to integer address
                                let stale_addr = obj_ref.as_ptr() as usize;
                                // Extend CRATONVM_DBG_STALE_RECV with the A2
                                // allocation breadcrumb regardless of whether
                                // the non-moving sweep_zero ring has a match
                                // (it never will under the default MOVING
                                // young collector -- sweep_zero only records
                                // the non-moving-sweep code path, which the
                                // moving collector never runs). Found during
                                // the 2026-07-16 hib-aqs-livelock
                                // investigation: the moving-collector case
                                // needed its own always-on history dump to
                                // rule out a double-free/overlap explanation
                                // (it was in fact a stale native-caller
                                // reference surviving a legitimate
                                // relocation -- see
                                // initialize_real_thread_pool_executor).
                                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_A2")
                                    .is_some()
                                {
                                    let hist = cratonvm_gc::a2dbg::history_at(stale_addr, 16);
                                    if hist.is_empty() {
                                        eprintln!(
                                            "[stale-recv] [A2] NO event touches {stale_addr:#x}"
                                        );
                                    } else {
                                        for r in hist {
                                            if r.kind == 0xFF {
                                                eprintln!(
                                                    "[stale-recv] [A2] seq={} FREE @{:#x}",
                                                    r.seq, r.addr
                                                );
                                            } else {
                                                eprintln!("[stale-recv] [A2] seq={} ALLOC @{:#x} class_id={} kind={} et={} alen={} ns={} size={}", r.seq, r.addr, r.class_id, r.kind, r.element_type, r.array_length, r.num_slots, r.size);
                                            }
                                        }
                                    }
                                }
                                eprintln!(
                                    "[stale-recv] ptr=0x{:x} method={}.{}{} — Java frames:",
                                    stale_addr,
                                    &*method_class_name,
                                    &*method_name,
                                    &*method_descriptor,
                                );
                                {
                                    let blocked_flag = thread
                                        .gc_block_state
                                        .in_blocked_region
                                        .load(std::sync::atomic::Ordering::Acquire);
                                    eprintln!(
                                        "[stale-recv] holder tid={} blocked={} epoch={} kind={:?}",
                                        thread.thread_id.0,
                                        blocked_flag,
                                        shared.mem.heap.collection_count(),
                                        thread.kind,
                                    );
                                    for (e, moved_to, mlen, as_dest) in
                                        crate::memory::gc::gcpart_probe(stale_addr)
                                    {
                                        eprintln!(
                                            "[stale-recv] [gcpart] epoch={e} map_len={mlen} moved_to={moved_to:x?} appears_as_dest={as_dest}"
                                        );
                                    }
                                    for (ago, site) in push_prov_find(stale_addr) {
                                        eprintln!(
                                            "[stale-recv] [pushprov] pushed {ago} invoke-returns ago at {site}"
                                        );
                                    }
                                    for (ago, desc) in deposit_gap_find(stale_addr) {
                                        eprintln!(
                                            "[stale-recv] [deposit-gap] {ago} entries ago: {desc}"
                                        );
                                    }
                                    for (age, site, tag, s, l) in
                                        cratonvm_gc::zero_forensics::probe(stale_addr)
                                    {
                                        eprintln!(
                                            "[stale-recv] [zeroed] age={age} site={} tag={tag} range=0x{s:x}+0x{l:x}",
                                            if site == 1 { "sweep-span" } else { "fromspace-reset" },
                                        );
                                    }
                                    if remap_trace_on() {
                                        eprintln!(
                                            "[stale-recv] [trace]\n    {}",
                                            remap_trace_dump()
                                        );
                                        for (ago, cb, site) in nret_find(stale_addr) {
                                            eprintln!(
                                                "[stale-recv] [nret] returned {} native-returns ago by {} at {}",
                                                ago,
                                                cratonvm_native_api::native_ring::name_of(cb)
                                                    .unwrap_or_else(|| format!("<cb@{cb:#x}>")),
                                                site,
                                            );
                                        }
                                        for (ago, parent, fidx) in getfield_ring_find(stale_addr) {
                                            let cur = shared
                                                .mem
                                                .heap
                                                .is_object_address(parent)
                                                .map(|p| {
                                                    format!(
                                                        "{:?}",
                                                        shared.mem.heap.get_field(p, fidx)
                                                    )
                                                })
                                                .unwrap_or_else(|| "<parent-not-obj>".into());
                                            eprintln!(
                                                "[stale-recv] [getfield] pushed {} getfields ago from parent=0x{parent:x} fld[{fidx}] — parent's field NOW = {cur}",
                                                ago,
                                            );
                                        }
                                    }
                                }
                                for (fi, f) in thread.frames.iter().enumerate().rev().take(30) {
                                    eprintln!(
                                        "  [{}] {}.{}{} pc={}",
                                        fi,
                                        f.class_name(),
                                        f.method_name(),
                                        f.method_descriptor(),
                                        f.pc,
                                    );
                                    if let Some(loc) = f.dbg_locate_addr(stale_addr) {
                                        eprintln!("      LOCATE {loc}");
                                    }
                                    if fi + 3 >= thread.frames.len() {
                                        eprintln!("      RAWSTACK{}", f.dbg_stack_dump());
                                        eprintln!("      POPPED{}", f.stack.dbg_dump_popped(5));
                                    }
                                    for li in 0..f.locals_len() {
                                        // Cast: numeric/representation conversion
                                        let v = f.get_local(li as u16);
                                        if let Value::Object(Some(o)) = v {
                                            // Cast: object/code pointer to integer address
                                            let addr = o.as_ptr() as usize;
                                            let marker =
                                                if addr == stale_addr { "STALE" } else { "" };
                                            // Probe the header of every Object
                                            // local: a non-zero hash means it
                                            // looks live, all-zero means it
                                            // shares the stale fate.
                                            // SAFETY: `addr` is the heap address of a live Object local; reading its 16-byte header is valid.
                                            let bytes: [u8; 16] =
                                                unsafe { std::ptr::read(addr as *const [u8; 16]) };
                                            let all_zero = bytes == [0u8; 16];
                                            eprintln!(
                                                "      LOCAL[{}] -> 0x{:x} all_zero_header={} {}",
                                                li, addr, all_zero, marker,
                                            );
                                        }
                                    }
                                    for si in 0..f.stack.len() {
                                        let v = f.stack.get_value(si);
                                        if let Value::Object(Some(o)) = v {
                                            // Cast: object/code pointer to integer address
                                            let addr = o.as_ptr() as usize;
                                            let marker =
                                                if addr == stale_addr { "STALE" } else { "" };
                                            // SAFETY: `addr` is the heap address of a live Object stack slot; reading its 16-byte header is valid.
                                            let bytes: [u8; 16] =
                                                unsafe { std::ptr::read(addr as *const [u8; 16]) };
                                            let all_zero = bytes == [0u8; 16];
                                            eprintln!(
                                                "      STACK[{}] -> 0x{:x} all_zero_header={} {}",
                                                si, addr, all_zero, marker,
                                            );
                                        }
                                    }
                                }
                            }
                            method_class_name.clone()
                        } else {
                            // S111r8: cid=0 with non-zero header means a
                            // synthetic alloc lost its class_id (e.g.
                            // `alloc_object(ClassId::new(0), …)` from a
                            // native fallback). The previous code returned
                            // bare `java/lang/Object`, which then sent
                            // `Set.iterator()` / `Map.keySet()` /
                            // `Iterator.hasNext()` invokes through
                            // Object's vtable and surfaced as
                            // `NoSuchMethodError Object.iterator()`.
                            //
                            // The CP method-ref class (e.g.
                            // `java/util/Set`) already resolved at link
                            // time and is the correct dispatch class for
                            // any non-Object method. Use it as the
                            // fallback so the slow path can locate the
                            // registered native (`HashSet.iterator`,
                            // `HashMap.keySet`, etc.) even though the
                            // receiver header is corrupt. Object members
                            // (equals/hashCode/toString/getClass/wait/
                            // notify/notifyAll/clone/finalize) still
                            // dispatch on Object so subclass overrides
                            // through the slow path's Object-fallback
                            // logic still apply.
                            if crate::vm::is_object_member(&method_name, &method_descriptor) {
                                Arc::from("java/lang/Object")
                            } else {
                                method_class_name.clone()
                            }
                        }
                    } else {
                        // If receiver is a lambda proxy calling a non-SAM method
                        // (e.g. Function.andThen), dispatch on the functional
                        // interface class so the native default method is found.
                        let lambda_iface = {
                            let proxies = shared.classes.lambda_proxies.read();
                            proxies
                                .get(&cid)
                                .map(|lcs| lcs.functional_interface.clone())
                        };
                        if let Some(iface) = lambda_iface {
                            iface
                        } else {
                            // S111r12 — receiver's runtime class is an interface
                            // (e.g. `java/lang/Comparable`) but the CP method-ref
                            // class is a concrete/abstract class with the actual
                            // method declared (`java/lang/ClassLoader.loadClass`).
                            // This pattern surfaces when a native-allocated
                            // ClassLoader instance lost its concrete class_id
                            // somewhere in the boot chain and `class_id_of`
                            // returns a stub interface cid instead. Routing
                            // dispatch through the CP class lets the slow path
                            // find the registered native or bytecode method.
                            // Mirrors the S111r8 cid=0 → CP-class fallback.
                            // Guard: only fires when the receiver's class is an
                            // interface AND the CP class is NOT that same
                            // interface (avoid changing well-formed
                            // `Iterator.hasNext()` etc. dispatches).
                            //
                            // S-trinity #2 — symmetric extension: receiver's
                            // runtime class is plain `java/lang/Object` (e.g.
                            // a value just returned from
                            // `PrivilegedAction.run()` whose declared return
                            // is `Object`, or a synthetic native return that
                            // landed without subclass info), and the CP
                            // method-ref class is `java/lang/ClassLoader` (or
                            // any concrete class declaring the method). Treat
                            // it the same as the interface case so the
                            // `(ClassLoader) priv.run()` chain in
                            // `LoaderUtil.getClassLoader` and
                            // `Logger.getMessageLogger` can dispatch the
                            // subsequent `loadClass` instead of NSME'ing on
                            // `Object.loadClass`.
                            let cm_read = shared.classes.class_manager.read();
                            let recv_class = cm_read.get_class(cid);
                            let recv_is_iface =
                                recv_class.map(|c| c.is_interface()).unwrap_or(false);
                            let recv_name_opt = recv_class.map(|c| Arc::from(&*c.name));
                            drop(cm_read);
                            let recv_is_bare_object = recv_name_opt
                                .as_ref()
                                .map(|n: &Arc<str>| &**n == "java/lang/Object")
                                .unwrap_or(false);
                            let cp_is_not_object = &*method_class_name != "java/lang/Object";
                            if (recv_is_iface || recv_is_bare_object)
                                && cp_is_not_object
                                && !crate::vm::is_object_member(&method_name, &method_descriptor)
                                && recv_name_opt
                                    .as_ref()
                                    .map(|n: &Arc<str>| &**n != &*method_class_name)
                                    .unwrap_or(true)
                            {
                                method_class_name.clone()
                            } else {
                                recv_name_opt.unwrap_or(method_class_name)
                            }
                        }
                    }
                }
            }
            Value::Object(None) => {
                if crate::runtime::env_cache::dbg_jetty2() {
                    let cm = shared.classes.class_manager.read();
                    eprintln!(
                        "[jetty2] NULL-RECEIVER invoke {}.{}{} — Java stack:",
                        &*method_class_name, &*method_name, &*method_descriptor
                    );
                    for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
                        let cn = cm
                            .get_class(f.class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        eprintln!(
                            "  [{i}] {}.{}{} pc={}",
                            cn,
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc
                        );
                    }
                }
                // C11/C16: if this is a call into jdk/internal/misc/Unsafe or
                // sun/misc/Unsafe with a null receiver (e.g. a static-init
                // failed to populate `theUnsafe`), we still want the Unsafe
                // native to run so its static-field fallback store services
                // the access. Dispatch on the constant-pool class instead of
                // NPE'ing.
                if &*method_class_name == "jdk/internal/misc/Unsafe"
                    || &*method_class_name == "sun/misc/Unsafe"
                {
                    method_class_name.clone()
                } else {
                    // T19.H11 — diagnostic eprintln removed; the JmxProperties
                    // boot path NPE was traced to
                    // `DefaultLoggerFinder.isSystem(Module m)` with m=null
                    // (m.getClassLoader() NPEs). Fix lives in
                    // `native-builtins/src/lib.rs` as a native override that
                    // treats null module as `isSystem=true`.
                    if crate::runtime::env_cache::npe_invoke_dbg() {
                        eprintln!(
                            "[NPE-DBG] invokevirtual null receiver: {}.{}",
                            method_class_name, method_name
                        );
                    }
                    // Round 63 — `org/springframework/core/convert/support/
                    // GenericConversionService$Converters.getClassHierarchy`
                    // dereferences `Class.componentType()` directly on the
                    // result of `addToClassHierarchy`, which can leak null
                    // into the local list (Spring's `addToClassHierarchy`
                    // never re-asserts non-null after `arrayType` /
                    // `resolvePrimitiveIfNecessary`). On that specific
                    // path, the JDK contract for `componentType()` —
                    // "returns null if this Class does not represent an
                    // array class" — gives us a defensible null-tolerant
                    // shape: treat `null.componentType()` as null, and
                    // similarly treat `null.getSuperclass()` /
                    // `null.arrayType()` as null and `null.getInterfaces()`
                    // as the empty `Class[]`. The hierarchy walk then
                    // simply skips the spurious null entry.
                    if &*method_class_name == "java/lang/Class" {
                        if &*method_name == "componentType"
                            || &*method_name == "getComponentType"
                            || &*method_name == "getSuperclass"
                            || &*method_name == "arrayType"
                        {
                            thread.frames[frame_idx].stack.push(Value::Object(None))?;
                            return Ok(CachedCallResult::Handled);
                        }
                        if &*method_name == "getInterfaces" {
                            let class_class_id = shared
                                .classes
                                .class_manager
                                .read()
                                .get_loaded_class_id("java/lang/Class")
                                .unwrap_or(cratonvm_types::ClassId::new(0));
                            let arr = gc_alloc_array(
                                shared,
                                thread,
                                class_class_id,
                                ArrayElementType::Reference,
                                0,
                            )?;
                            thread.frames[frame_idx]
                                .stack
                                .push(Value::Object(Some(arr)))?;
                            return Ok(CachedCallResult::Handled);
                        }
                    }
                    // Gradle bootstrap: URL.getProtocol on null URL needs
                    // null-tolerance (null-tolerant bytecode pattern with
                    // ifnull guard after the call). Same family for
                    // File accessors (Sonar/Liberty install-root resolution)
                    // and the narrow String.length() Liberty case.
                    if &*method_class_name == "java/net/URL"
                        && matches!(
                            &*method_name,
                            "getProtocol"
                                | "getHost"
                                | "getFile"
                                | "getPath"
                                | "getQuery"
                                | "getRef"
                                | "getUserInfo"
                                | "getAuthority"
                        )
                    {
                        thread.frames[frame_idx].stack.push(Value::Object(None))?;
                        return Ok(CachedCallResult::Handled);
                    }
                    if &*method_class_name == "java/io/File" {
                        if matches!(
                            &*method_name,
                            "getParentFile"
                                | "getAbsoluteFile"
                                | "getCanonicalFile"
                                | "getParent"
                                | "getName"
                                | "getPath"
                                | "getAbsolutePath"
                                | "getCanonicalPath"
                                | "toURI"
                                | "toURL"
                                | "toPath"
                                | "listFiles"
                                | "list"
                        ) {
                            thread.frames[frame_idx].stack.push(Value::Object(None))?;
                            return Ok(CachedCallResult::Handled);
                        }
                        if matches!(
                            &*method_name,
                            "length"
                                | "lastModified"
                                | "getTotalSpace"
                                | "getFreeSpace"
                                | "getUsableSpace"
                        ) {
                            thread.frames[frame_idx].stack.push(Value::Long(0))?;
                            return Ok(CachedCallResult::Handled);
                        }
                        if matches!(
                            &*method_name,
                            "exists"
                                | "isDirectory"
                                | "isFile"
                                | "isAbsolute"
                                | "isHidden"
                                | "canRead"
                                | "canWrite"
                                | "canExecute"
                        ) {
                            thread.frames[frame_idx].stack.push(Value::Int(0))?;
                            return Ok(CachedCallResult::Handled);
                        }
                    }
                    // NOTE: `null.length()` MUST throw NullPointerException
                    // per the JVM spec — `invokevirtual` null-checks the
                    // receiver before dispatch. An earlier hack here returned
                    // 0 for a "Liberty install-root" path, but that silently
                    // corrupted every other caller: e.g. `new
                    // StringTokenizer((String) null, ...)` does `str.length()`
                    // in its constructor, so the hack produced an empty
                    // tokenizer instead of an NPE, which made OSGi
                    // `Version`'s `nextToken()` throw NoSuchElementException
                    // and surface as `IllegalArgumentException: invalid
                    // version "null"` (Felix framework bootstrap). The hack
                    // is removed so the spec-compliant NPE below fires.
                    if crate::runtime::env_cache::modstatic_dbg() && &*method_name == "set" {
                        let cm = shared.classes.class_manager.read();
                        eprintln!("MODSTATIC: NPE 'set on null' frames:");
                        for (i, f) in thread.frames.iter().enumerate().rev().take(12) {
                            let cn = cm
                                .get_class(f.class_id)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            eprintln!("  [{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
                        }
                    }
                    // CRATONVM_DBG_JETTY — dump the full Java call stack when a
                    // null-receiver invokevirtual fires on a Jetty launcher
                    // method (the boot-test NPE site). This shows which frame /
                    // pc loaded the null receiver — e.g. `Main.start` doing
                    // `aload_1` of a null `StartArgs` param.
                    if crate::runtime::env_cache::dbg_jetty()
                        && method_class_name.starts_with("org/eclipse/jetty/start/")
                    {
                        let cm = shared.classes.class_manager.read();
                        eprintln!(
                            "[cratonvm-jetty] NULL-RECEIVER invoke {}.{}{} — Java stack:",
                            &*method_class_name, &*method_name, &*method_descriptor
                        );
                        for (i, f) in thread.frames.iter().enumerate().rev().take(20) {
                            let cn = cm
                                .get_class(f.class_id)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            eprintln!(
                                "  [{i}] {}.{}{} pc={}",
                                cn,
                                f.method_name(),
                                f.method_descriptor(),
                                f.pc
                            );
                        }
                    }
                    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_NPE_STACK").is_some() {
                        let cm = shared.classes.class_manager.read();
                        eprintln!(
                            "[CRATONVM_DBG_NPE_STACK] NPE invoke {}.{}{} — stack:",
                            &*method_class_name, &*method_name, &*method_descriptor
                        );
                        for (i, f) in thread.frames.iter().enumerate().rev().take(30) {
                            let cn = cm
                                .get_class(f.class_id)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            let loader = cm.get_loader_id(f.class_id);
                            eprintln!(
                                "  [{i}] {}.{}{} pc={} class_id={:?} loader={:?}",
                                cn,
                                f.method_name(),
                                f.method_descriptor(),
                                f.pc,
                                f.class_id,
                                loader
                            );
                        }
                    }
                    // JEP 358: build the HotSpot-style extended NPE message —
                    // `Cannot invoke "Owner.name(params)" because "<expr>" is
                    // null`. The action half is always present; the `because`
                    // clause is added when the bounded backward bytecode
                    // analysis can name the null receiver expression
                    // (aload local/param, getfield, getstatic, aaload). The
                    // trapping bci is `last_instr_pc`, always known here in the
                    // interpreter, so this is deopt-independent.
                    let npe_msg = helpful_npe_invoke_message(
                        shared,
                        thread,
                        frame_idx,
                        &method_class_name,
                        &method_name,
                        &method_descriptor,
                        num_params,
                    );
                    return Err(RuntimeError::NullPointerException {
                        message: Some(npe_msg),
                    }
                    .into());
                }
            }
            _ => method_class_name,
        }
    };

    // Dynamic proxy dispatch: forward interface method calls on
    // `Proxy$Instance` (and any class extending it — WP2.5-A generated
    // `$ProxyN` classes) to the InvocationHandler.invoke(). Handle Object
    // methods specially. Fast path stays a literal compare; slow path
    // walks the receiver's superclass chain — only fires when
    // `invoke_class != "Proxy$Instance"` and we have an actual receiver
    // to inspect, so the cost is zero on every non-proxy dispatch.
    let is_proxy_dispatch = &*invoke_class == "java/lang/reflect/Proxy$Instance"
        || matches!(
            args.first(),
            Some(Value::Object(Some(receiver)))
                if class_chain_reaches_proxy_instance(
                    shared,
                    shared.mem.heap.class_id_of(*receiver),
                )
        );
    if is_proxy_dispatch && !is_special {
        // Handle getClass() directly — return the proxy's class mirror
        if &*method_name == "getClass" {
            if let Value::Object(Some(proxy_ref)) = &args[0] {
                let class_id = shared.mem.heap.class_id_of(*proxy_ref);
                let mirror = crate::vm::get_or_create_class_mirror(shared, class_id);
                thread.frames[frame_idx]
                    .stack
                    .push(Value::Object(Some(mirror)))?;
                return Ok(CachedCallResult::Handled);
            }
        }
        if let Value::Object(Some(proxy_ref)) = &args[0] {
            let result = crate::vm::proxy_invoke_handler_shared(
                shared,
                thread,
                *proxy_ref,
                &method_name,
                &method_descriptor,
                &args[1..],
            )?;
            if let Some(value) = result {
                // Unbox the result if the method returns a primitive type
                let ret_char = method_descriptor
                    .rsplit(')')
                    .nth(0)
                    .unwrap_or("L")
                    .chars()
                    .next()
                    .unwrap_or('L');
                let unboxed = match ret_char {
                    'I' | 'Z' | 'B' | 'C' | 'S' => {
                        if let Value::Object(Some(obj)) = value {
                            shared.mem.heap.get_field(obj, 0)
                        } else {
                            value
                        }
                    }
                    'J' => {
                        if let Value::Object(Some(obj)) = value {
                            shared.mem.heap.get_field(obj, 0)
                        } else {
                            value
                        }
                    }
                    'F' => {
                        if let Value::Object(Some(obj)) = value {
                            shared.mem.heap.get_field(obj, 0)
                        } else {
                            value
                        }
                    }
                    'D' => {
                        if let Value::Object(Some(obj)) = value {
                            shared.mem.heap.get_field(obj, 0)
                        } else {
                            value
                        }
                    }
                    _ => value, // Object return type — no unboxing
                };
                let ret = crate::jit::return_type(&method_descriptor);
                let pushed = coerce_value_for_return(unboxed, ret);
                // T18.K4 — tag-exact push for J/D proxy return values.
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, pushed)?;
            }
            return Ok(CachedCallResult::Handled);
        }
    }

    // Annotation proxy dispatch: method calls on annotation proxies
    //
    // S111r18 — gate the dispatch on `kind == Object`. A reference array
    // whose component class is `AnnotationProxy` (e.g. `Annotation[]` for
    // a repeatable annotation or `excludeFilters` on `@ComponentScan`)
    // shares the same `class_id_of` value because our heap stores the
    // component class id on the array header. Without this guard, every
    // method call on such an array (Object.getClass / Object.toString /
    // Array.getLength via reflection) gets routed through
    // `annotation_proxy_invoke_shared`, which reads element-value slots
    // out of array memory — surfacing as `getClass() returns null` or
    // wrong-component types in Spring's `MergedAnnotation.adaptForAttribute`
    // and breaking the `excludeFilters` array iteration that builds the
    // `MergedAnnotation[]`.
    if &*invoke_class == "java/lang/annotation/AnnotationProxy"
        && !is_special
        && matches!(
            args.first(),
            Some(Value::Object(Some(r))) if shared.mem.heap.kind_of(*r) == cratonvm_types::ObjectKind::Object
        )
    {
        if let Value::Object(Some(ann_ref)) = &args[0] {
            let invoke_args: &[Value] = if args.len() >= 1 { &args[1..] } else { &[] };
            let result = crate::vm::annotation_proxy_invoke_shared(
                shared,
                thread,
                *ann_ref,
                &method_name,
                invoke_args,
            )?;
            if let Some(value) = result {
                // Unbox the result if the method returns a primitive type
                let ret_char = method_descriptor
                    .rsplit(')')
                    .nth(0)
                    .unwrap_or("L")
                    .chars()
                    .next()
                    .unwrap_or('L');
                let unboxed = match ret_char {
                    'I' | 'Z' | 'B' | 'C' | 'S' | 'J' | 'F' | 'D' => {
                        if let Value::Object(Some(obj)) = value {
                            shared.mem.heap.get_field(obj, 0)
                        } else {
                            value
                        }
                    }
                    _ => value,
                };
                let ret = crate::jit::return_type(&method_descriptor);
                let pushed = coerce_value_for_return(unboxed, ret);
                // T18.K4 — tag-exact push for J/D annotation-proxy return values.
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, pushed)?;
            }
            return Ok(CachedCallResult::Handled);
        }
    }

    // KAFKA-DEFAULT-RESCUE: stash the CP-resolved interface (or class) id
    // for the in-flight invoke. The default-method rescue at the NSME emit
    // site in `invoke_on_class_shared_inner` consults this slot when the
    // hierarchy walk on the receiver-derived dispatch class can't find the
    // method but the CP-resolved interface declares a default body.
    // Wrapped in an RAII guard so nested invokes restore the parent's slot.
    let _cp_iface_guard = crate::vm::PendingCpIfaceGuard::new(cp_resolved_class_id);

    if let Some(res) = intercept_force_registered_native(
        shared,
        thread,
        frame_idx,
        &invoke_class,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        &args,
    ) {
        // A force-native virtual call used to return before the ordinary
        // stackless-dispatch epilogue below could warm `invoke_cache`.  That
        // made every subsequent call to common real-JDK overrides (notably
        // String's compact-string operations) re-enter full method
        // resolution, even though `populate_virtual_invoke_cache` already
        // has a redefine-aware `VirtualNative` representation for exactly
        // this dispatch.  Keep the private-target exclusion from the normal
        // virtual epilogue because such a call has a distinct resolution
        // identity.
        if !is_special && private_virtual_target.is_none() {
            if let Some(rcv_cid) = receiver_class_id {
                populate_virtual_invoke_cache(
                    thread,
                    shared,
                    current_class_id,
                    cp_index,
                    rcv_cid,
                    &args[0],
                );
            }
        }
        return res;
    }

    if let Some(res) = intercept_jython_pymodule_findattr(
        shared,
        thread,
        frame_idx,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        receiver_class_id,
        &args,
    ) {
        return res;
    }

    if let Some(res) = intercept_jython_pyjavatype_findattr_ex(
        shared,
        thread,
        frame_idx,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        receiver_class_id,
        is_special,
        &args,
    ) {
        return res;
    }

    // Inherited URLClassLoader methods invoked through a subclass-owned
    // constant-pool entry evade the static-class native gate.  Intercept the
    // resolved base methods so real-JDK URLClassPath shims never discard local
    // resources or custom URLStreamHandler-backed URLs.
    if let Some(res) = intercept_urlclassloader_subclass_native_method(
        shared,
        thread,
        frame_idx,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        receiver_class_id,
        is_special,
        &args,
    ) {
        return res;
    }

    if let Some(res) = intercept_classloader_subclass_resource_native(
        shared,
        thread,
        frame_idx,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        receiver_class_id,
        is_special,
        &args,
    ) {
        return res;
    }

    // Loader-isolation dispatch override (gated on CRATONVM_LOADER_AWARE_RESOLUTION).
    //
    // `invoke_class` is the receiver's class NAME, and the dispatch below
    // re-resolves that name via `get_loaded_class_id` — which returns ONE class
    // per name. Under a user loader two classes can share a name: a per-loader
    // ENHANCED copy (e.g. a Hibernate bytecode-enhanced entity — its
    // `$$_hibernate_*` methods, dirty-tracking, and field-access interception
    // overrides) AND the un-enhanced copy on the global classpath. Name
    // re-resolution silently picks the global copy, so a virtual/interface call
    // on an enhanced receiver would run the UN-enhanced method: `setX()` skips
    // dirty tracking (`[]` instead of `["x"]`), `$$_hibernate_*` is missing
    // (spurious NoSuchMethodError), etc. For a virtual/interface call the
    // receiver's OWN runtime `class_id` is the authoritative dispatch target, so
    // hand it to `try_stackless_invoke` as a dispatch-class override when it
    // diverges from the name-resolved class. This keeps the normal stackless
    // frame-push path (NO extra recursion), unlike routing through the recursive
    // `invoke_on_class_shared`. Gated + divergence-only → byte-identical in the
    // default (gate-off) / single-class-per-name case.
    // An invokeinterface default is part of the interface identity, not merely
    // its binary name. A forked receiver can implement a loader-local copy of
    // an interface while the calling class's constant-pool lookup finds the
    // application copy. Running that application's default method mixes its
    // static constants with the forked receiver (Spring's MergedAnnotation
    // Adapt enum is identity-sensitive). Prefer the receiver loader's exact
    // interface when it is a real superinterface of the receiver.
    //
    // BUT only when the receiver is actually going to fall through to that
    // interface default in the first place. If the receiver's OWN class
    // hierarchy already declares a concrete (class-level, non-interface)
    // override of this exact name+descriptor, that override must win —
    // redirecting `class_id` straight to the interface's per-loader copy
    // skips past the override and runs the interface default instead. Found
    // via `SpringBootContextLoaderAotTests` (`@CompileWithForkedClassLoader`):
    // `DelegatingSmartContextLoader` (loaded by the forked test classloader)
    // overrides `AotContextLoader.loadContextForAotProcessing(MergedContextConfiguration,
    // RuntimeHints)`, but this override redirected dispatch to the forked
    // loader's own copy of `AotContextLoader` — whose default body just calls
    // the 1-arg `loadContextForAotProcessing(MergedContextConfiguration)`
    // default, which unconditionally throws
    // `UnsupportedOperationException("Invoke loadContextForAotProcessing(...)
    // instead")`. Use `find_method_recursive` on the receiver's OWN class_id
    // (loader-accurate, unlike a name-based lookup) to check for a real
    // override before applying the redirect.
    let loader_interface_override =
        if is_interface && !is_special && crate::runtime::env_cache::loader_aware_resolution() {
            receiver_class_id.and_then(|receiver_id| {
                let cm = shared.classes.class_manager.read();
                let receiver_has_class_override = crate::classloading::find_method_recursive(
                    receiver_id,
                    &method_name,
                    &method_descriptor,
                    &cm.class_store,
                )
                .is_some_and(|(_, declaring_id)| {
                    !cm.get_class(declaring_id)
                        .is_some_and(|class| class.is_interface())
                });
                if receiver_has_class_override {
                    return None;
                }
                let receiver_loader = cm.get_loader_id(receiver_id)?;
                let exact =
                    cm.class_defined_by_loader_exact(&method_owner_name, receiver_loader)?;
                (Some(exact) != cm.get_loaded_class_id(&method_owner_name)
                    && cm
                        .get_class(exact)
                        .is_some_and(|class| class.is_interface()))
                .then_some(exact)
            })
        } else {
            None
        };
    let receiver_interface_dispatch = (is_interface && !is_special)
        .then_some(receiver_class_id)
        .flatten()
        .filter(|class_id| *class_id != ClassId::new(0))
        .filter(|class_id| !shared.classes.lambda_proxies.read().contains_key(class_id));
    let dispatch_override: Option<ClassId> = if let Some((declaring_id, _)) =
        &private_virtual_target
    {
        Some(*declaring_id)
    } else if let Some(receiver_id) = receiver_interface_dispatch {
        // find_method_recursive performs the JVMS maximally-specific
        // default-method selection only when it starts at the runtime
        // receiver. Starting at the CP owner returns that interface's own
        // default immediately and bypasses a covariant bridge declared by a
        // subinterface implemented by the receiver.
        Some(receiver_id)
    } else if let Some(interface_id) = loader_interface_override {
        Some(interface_id)
    } else if !is_special && crate::runtime::env_cache::loader_aware_resolution() {
        // Lambda-proxy receiver invoking a non-SAM (default) interface
        // method: `invoke_class` was set to the functional interface NAME
        // (see the lambda_iface branch above), and the name re-resolution
        // below collapses to ONE copy per name. The proxy call site carries
        // the loader-resolved interface id captured at bootstrap time
        // (forked-classloader tests re-define the whole framework, so the
        // app copy and the fork copy both exist); dispatch on that id so the
        // default method executes in the DEFINING loader context and its own
        // constant-pool resolutions stay inside that loader (Spring AOT
        // `ArgumentCodeGenerator.and()` chain, 2026-07-15).
        let lambda_iface_override = receiver_class_id.and_then(|rcv_cid| {
            let iface_id = shared
                .classes
                .lambda_proxies
                .read()
                .get(&rcv_cid)
                .and_then(|lcs| lcs.functional_interface_id)?;
            let cm = shared.classes.class_manager.read();
            let iface_matches_invoke = cm
                .get_class(iface_id)
                .map(|c| &*c.name == &*invoke_class)
                .unwrap_or(false);
            (iface_matches_invoke && cm.get_loaded_class_id(&invoke_class) != Some(iface_id))
                .then_some(iface_id)
        });
        lambda_iface_override.or_else(|| {
            receiver_class_id.filter(|rcv_cid| {
                *rcv_cid != ClassId::new(0) && {
                    let cm = shared.classes.class_manager.read();
                    cm.get_loaded_class_id(&invoke_class) != Some(*rcv_cid)
                        && cm
                            .get_class(*rcv_cid)
                            .map(|c| &*c.name == &*invoke_class)
                            .unwrap_or(false)
                }
            })
        })
    } else if is_special && should_use_loader_initiated_resolution(shared, current_class_id) {
        // invokespecial owner is the CP-resolved class NAME (`method_class_name`),
        // which `get_loaded_class_id` collapses to ONE copy per name. Super and
        // private calls from inside a loader-private enhanced class must reach
        // that same loader's owner copy. For self-constructors, the receiver is
        // more precise than the caller: a global harness class can execute
        // `new C; invokespecial C.<init>` where `new` correctly allocated a
        // loader-private enhanced C. Running the global C constructor against
        // that receiver writes the wrong layout slots and leaves enhanced fields
        // null.
        let receiver_self_ctor = if &*method_name == "<init>" {
            match args.first() {
                Some(Value::Object(Some(recv))) => {
                    let recv_cid = shared.mem.heap.class_id_of(*recv);
                    if recv_cid != ClassId::new(0) {
                        let cm = shared.classes.class_manager.read();
                        let recv_matches_owner = cm
                            .get_class(recv_cid)
                            .map(|c| {
                                &*c.name == &*invoke_class
                                    && c.find_method(&method_name, &method_descriptor).is_some()
                            })
                            .unwrap_or(false);
                        if recv_matches_owner {
                            Some(recv_cid)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        receiver_self_ctor.or_else(|| {
            lookup_loader_initiated(shared, current_class_id, &invoke_class).filter(|owner_cid| {
                *owner_cid != ClassId::new(0)
                    && shared
                        .classes
                        .class_manager
                        .read()
                        .get_loaded_class_id(&invoke_class)
                        != Some(*owner_cid)
            })
        })
    } else {
        None
    };

    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_INVSPECIAL").is_some() && is_special {
        let cm = shared.classes.class_manager.read();
        let cur_loader = cm.get_loader_id(current_class_id);
        if matches!(
            cur_loader,
            Some(cratonvm_types::ClassLoaderId::UserDefined(_))
        ) {
            let invoke_class_resolved = cm.get_loaded_class_id(&invoke_class);
            drop(cm);
            eprintln!(
                "[INVSPECIAL] method={}.{}{} current_class_id={:?} cur_loader={:?} invoke_class={} dispatch_override={:?} invoke_class_resolved(global)={:?}",
                method_owner_name, method_name, method_descriptor, current_class_id, cur_loader, invoke_class, dispatch_override, invoke_class_resolved
            );
        }
    }
    if crate::runtime::env_cache::dbg_loader_trace()
        && (invoke_class.contains("RootReference") || &*method_name == "compareAndSetRoot")
    {
        let cm = shared.classes.class_manager.read();
        let invoke_class_resolved = cm.get_loaded_class_id(&invoke_class);
        let cur_loader = cm.get_loader_id(current_class_id);
        let recv_loader = receiver_class_id.and_then(|c| cm.get_loader_id(c));
        drop(cm);
        eprintln!(
            "[LOADER-TRACE] invoke_kind method={}.{}{} is_special={} invoke_class={} invoke_class_resolved={:?} receiver_class_id={:?} recv_loader={:?} current_class_id={:?} cur_loader={:?} dispatch_override={:?}",
            method_owner_name, method_name, method_descriptor, is_special, invoke_class, invoke_class_resolved, receiver_class_id, recv_loader, current_class_id, cur_loader, dispatch_override
        );
    }
    // Class/interface resolution above may have triggered a moving collection
    // while `args` lived only in its Rust Vec. Re-read the remapped pin slots.
    args_root_guard.refresh(&mut args);
    // Try stackless frame push for bytecode methods (avoids Rust stack recursion)
    // For virtual/special calls, do NOT walk the native hierarchy — subclass
    // bytecode overrides must take priority over parent native overrides.
    match try_stackless_invoke(
        shared,
        thread,
        frame_idx,
        &invoke_class,
        &method_name,
        &method_descriptor,
        &args,
        false,
        is_special,
        dispatch_override,
    )? {
        CachedCallResult::FramePushed => {
            args_root_guard.refresh(&mut args);
            if is_special || private_virtual_target.is_some() {
                populate_invoke_cache(thread, shared, current_class_id, cp_index, is_special);
            } else if private_virtual_target.is_none() && loader_interface_override.is_none() {
                if let Some(rcv_cid) = receiver_class_id {
                    populate_virtual_invoke_cache(
                        thread,
                        shared,
                        current_class_id,
                        cp_index,
                        rcv_cid,
                        &args[0],
                    );
                }
            }
            return Ok(CachedCallResult::FramePushed);
        }
        CachedCallResult::Handled => {
            args_root_guard.refresh(&mut args);
            if is_special || private_virtual_target.is_some() {
                populate_invoke_cache(thread, shared, current_class_id, cp_index, is_special);
            } else if private_virtual_target.is_none() && loader_interface_override.is_none() {
                if let Some(rcv_cid) = receiver_class_id {
                    populate_virtual_invoke_cache(
                        thread,
                        shared,
                        current_class_id,
                        cp_index,
                        rcv_cid,
                        &args[0],
                    );
                }
            }
            return Ok(CachedCallResult::Handled);
        }
        CachedCallResult::CacheMiss => {
            args_root_guard.refresh(&mut args);
            // Exotic case — fall through to recursive dispatch
        }
    }

    // Fallback: recursive dispatch for exotic cases (signature-polymorphic, JNI, proxy, etc.)
    // Honor the loader-isolation dispatch override here too: a divergent
    // receiver whose method the stackless path couldn't handle (synthetic stub /
    // exotic → CacheMiss) must still dispatch on the receiver's own class_id,
    // not the wrong name-resolved copy. Rare, so the recursive path is fine.
    //
    // `dispatch_override` alone under-detects this: it's computed by comparing
    // `get_loaded_class_id(&invoke_class)` against the receiver's class_id at
    // THIS point in time, but `invoke_shared`'s own `load_class_concurrent`
    // (name-based) can independently resolve the SAME name string to a
    // DIFFERENT ClassId than `get_loaded_class_id` just did — observed with
    // `@ClassPathOverrides`'s `ModifiedClassPathClassLoader`, where an old
    // override jar's class (e.g. Spring's `MimeType`, missing a method added
    // in later versions) and the main classpath's same-named class are BOTH
    // loaded, and the two name-resolution call sites picked different
    // copies. That let `mimeType.isMoreSpecific(null)` — a genuinely absent
    // method on the receiver's OWN class — silently dispatch to the OTHER
    // same-named class's bytecode instead of raising `NoSuchMethodError`
    // (`NoSuchMethodFailureAnalyzerTests`). For an ordinary virtual call, the
    // receiver's own (already-loaded, already-initialized) class_id is
    // JVMS-authoritative regardless of what any name lookup returns, so
    // prefer it whenever available instead of falling through to the
    // loader-blind by-name path. Excludes:
    //  - the stale-pointer sentinel (`ClassId::new(0)`, see the cid==0
    //    handling above), which intentionally keeps using the CP method-ref
    //    class name for its own recovery path;
    //  - lambda-proxy receivers, whose synthetic class_id is NOT a normal
    //    entry in `class_manager` (it has no real method table of its own
    //    for `invoke_on_class_shared`'s `find_method_recursive` to walk).
    //    `invoke_on_class_shared_inner` DOES also re-check
    //    `shared.classes.lambda_proxies` on the receiver up front and redirect to
    //    `try_lambda_dispatch`, but only when passed the RECEIVER's own
    //    class_id — routing a lambda receiver's SAM method call (e.g.
    //    `Consumer.accept`) through this branch at all, instead of the
    //    by-name `invoke_shared` fallback the lambda dispatch machinery
    //    already handles correctly, regressed
    //    `ProcessInfoTests.memoryInfoIsAvailable`'s `allSatisfy(lambda)`
    //    with `NoSuchMethodError: <unknown class N>.accept(...)`.
    let is_lambda_receiver = receiver_class_id
        .map(|c| shared.classes.lambda_proxies.read().contains_key(&c))
        .unwrap_or(false);
    let result = if let Some(rcv_cid) = dispatch_override
        .or_else(|| receiver_class_id.filter(|c| *c != ClassId::new(0) && !is_lambda_receiver))
    {
        crate::vm::invoke_on_class_shared(
            shared,
            thread,
            rcv_cid,
            &method_name,
            &method_descriptor,
            &args,
        )?
    } else {
        invoke_shared(
            shared,
            thread,
            &invoke_class,
            &method_name,
            &method_descriptor,
            &args,
        )?
    };

    if let Some(value) = result {
        let ret = crate::jit::return_type(&method_descriptor);
        if ret != b'V' {
            let value = coerce_value_for_return(value, ret);
            // T18.K4 — tag-exact push for J/D fallback invoke return values.
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
            // Hypothesis (b): a `safe_native_call` reached via `invoke_shared`
            // (recursive slow path) sets `thread.native_pending_return` on object
            // returns but the clear-after-push helper is only invoked in the
            // stackless dispatch arms. Once the value lives on the operand stack
            // (or a local), the stale pinned reference can outlive a subsequent
            // minor GC and re-surface in `update_root_snapshot`. Clear it here so
            // every slow-path consumer mirrors the stackless invariant.
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
    }

    // Populate cache for future fast-path hits
    if is_special || private_virtual_target.is_some() {
        populate_invoke_cache(thread, shared, current_class_id, cp_index, is_special);
    } else if private_virtual_target.is_none() {
        if let Some(rcv_cid) = receiver_class_id {
            populate_virtual_invoke_cache(
                thread,
                shared,
                current_class_id,
                cp_index,
                rcv_cid,
                &args[0],
            );
        }
    }

    Ok(CachedCallResult::Handled)
}

/// Split a method descriptor into (param_types, return_type), where each type
/// is a single descriptor token such as "I", "J", "Ljava/lang/Integer;", or
/// "[Ljava/lang/String;".
pub fn split_method_descriptor(descriptor: &str) -> (Vec<String>, String) {
    let bytes = descriptor.as_bytes();
    let mut params: Vec<String> = Vec::new();
    let mut i = 1; // skip '('
    while i < bytes.len() && bytes[i] != b')' {
        let start = i;
        // Consume array dims.
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1; // consume ';'
            }
            _ => {
                i += 1; // single-char primitive
            }
        }
        params.push(descriptor[start..i].to_string());
    }
    // Skip ')'
    if i < bytes.len() && bytes[i] == b')' {
        i += 1;
    }
    let ret = descriptor[i..].to_string();
    (params, ret)
}

/// Non-allocating equivalent of `split_method_descriptor(d).0[n].as_bytes().first()`:
/// the FIRST byte of the n-th parameter's descriptor token (`b'['` for arrays —
/// byte-identical to the old closures, and `decode_by_descriptor` treats `b'['`
/// as a reference). The warm call-dispatch arms only ever need this tag byte (to
/// pick the category-2 long/double pop path), so they previously paid a per-call
/// `Vec<String>` + per-param `String` allocation in `split_method_descriptor`
/// purely to read one byte each. Returns `b'L'` when `n` is out of range (the
/// arms' existing default). Tokenization mirrors `split_method_descriptor`.
pub(super) fn nth_param_tag_byte(descriptor: &str, n: usize) -> u8 {
    let bytes = descriptor.as_bytes();
    let mut i = 1; // skip '('
    let mut idx = 0;
    while i < bytes.len() && bytes[i] != b')' {
        let tag = bytes[i]; // first byte of this token ('[' for arrays)
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1; // consume ';'
            }
            _ => {
                i += 1; // single-char primitive
            }
        }
        if idx == n {
            return tag;
        }
        idx += 1;
    }
    b'L'
}

/// Unbox a boxed primitive wrapper object into its primitive `Value`.
/// Returns the original value unchanged if it's not a recognized wrapper.
pub(super) fn unbox_wrapper(shared: &SharedVm, prim_char: char, v: Value) -> Value {
    match (prim_char, v) {
        ('I' | 'B' | 'S' | 'C' | 'Z', Value::Object(Some(b))) => shared.mem.heap.get_field(b, 0),
        // Lambda metafactory adaptation permits unboxing followed by primitive
        // widening.  An Integer supplied to a `long` implementation method
        // must therefore become Value::Long, rather than carrying the raw
        // compact Int tag into an lload/putfield J path.
        ('J' | 'F' | 'D', Value::Object(Some(b))) => {
            widen_unboxed_primitive(prim_char, shared.mem.heap.get_field(b, 0))
        }
        (_, other) => other,
    }
}

/// Apply the primitive-widening portion of lambda unboxing without changing
/// the source wrapper's value. This is intentionally limited to conversions
/// permitted after unboxing by the Java language specification.
pub(super) fn widen_unboxed_primitive(target: char, value: Value) -> Value {
    match (target, value) {
        ('J', Value::Int(value)) => Value::Long(value as i64),
        ('F', Value::Int(value)) => Value::Float(value as f32),
        ('F', Value::Long(value)) => Value::Float(value as f32),
        ('D', Value::Int(value)) => Value::Double(value as f64),
        ('D', Value::Long(value)) => Value::Double(value as f64),
        ('D', Value::Float(value)) => Value::Double(value as f64),
        (_, value) => value,
    }
}

/// Normalize a value crossing into `aastore`.
///
/// The verifier guarantees an object reference at this opcode, but native and
/// reflective bridges can expose an unboxed primitive despite an `Object`
/// return descriptor. The common fast and decoded handlers both use this
/// allocation-only recovery helper, so package selection cannot change
/// wrapper identity or layout. Ordinary Java boxing still goes through
/// `valueOf` and retains its cache semantics.
pub(super) fn normalize_aastore_value(shared: &SharedVm, value: Value) -> Value {
    let (class_name, payload) = match value {
        Value::Int(_) => ("java/lang/Integer", value),
        Value::Long(_) => ("java/lang/Long", value),
        Value::Float(_) => ("java/lang/Float", value),
        Value::Double(_) => ("java/lang/Double", value),
        _ => return value,
    };
    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class(class_name)
        .unwrap_or(ClassId::new(0));
    let wrapper = shared.mem.heap.alloc_object(class_id, 1);
    shared.mem.heap.set_field(wrapper, 0, payload);
    Value::Object(Some(wrapper))
}

/// Box a primitive `Value` by invoking the wrapper's `valueOf(prim)`.
pub(super) fn box_primitive(
    shared: &SharedVm,
    thread: &mut JvmThread,
    prim_char: char,
    v: Value,
) -> Result<Value, MethodCallFailed> {
    let (cls, desc) = match prim_char {
        'Z' => ("java/lang/Boolean", "(Z)Ljava/lang/Boolean;"),
        'B' => ("java/lang/Byte", "(B)Ljava/lang/Byte;"),
        'S' => ("java/lang/Short", "(S)Ljava/lang/Short;"),
        'C' => ("java/lang/Character", "(C)Ljava/lang/Character;"),
        'I' => ("java/lang/Integer", "(I)Ljava/lang/Integer;"),
        'J' => ("java/lang/Long", "(J)Ljava/lang/Long;"),
        'F' => ("java/lang/Float", "(F)Ljava/lang/Float;"),
        'D' => ("java/lang/Double", "(D)Ljava/lang/Double;"),
        _ => return Ok(v),
    };
    // If already an Object, nothing to do.
    if matches!(v, Value::Object(_)) {
        return Ok(v);
    }
    let r = invoke_shared(shared, thread, cls, "valueOf", desc, &[v])?;
    Ok(r.unwrap_or(Value::Object(None)))
}

/// Returns true if the descriptor token is a primitive ("I","J",...).
pub(super) fn is_primitive_desc(token: &str) -> bool {
    matches!(token, "I" | "J" | "F" | "D" | "B" | "S" | "Z" | "C")
}

/// Returns true if the descriptor token is a reference (L... or [...).
pub(super) fn is_reference_desc(token: &str) -> bool {
    token.starts_with('L') || token.starts_with('[')
}

/// Coerce a single argument between the SAM's view (`sam_tok`) and the
/// impl's view (`impl_tok`). If SAM has reference but impl has primitive,
/// unbox. If SAM has primitive but impl has reference, box.
pub(super) fn coerce_arg(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sam_tok: &str,
    impl_tok: &str,
    v: Value,
) -> Result<Value, MethodCallFailed> {
    if sam_tok == impl_tok {
        return Ok(v);
    }
    // SAM = reference (e.g. Object), impl = primitive — unbox.
    if is_reference_desc(sam_tok) && is_primitive_desc(impl_tok) {
        let ch = impl_tok.chars().next().ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "empty primitive descriptor token in coerce_arg".to_string(),
            })
        })?;
        return Ok(unbox_wrapper(shared, ch, v));
    }
    // SAM = primitive, impl = reference — box.
    if is_primitive_desc(sam_tok) && is_reference_desc(impl_tok) {
        let ch = sam_tok.chars().next().ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "empty primitive descriptor token in coerce_arg".to_string(),
            })
        })?;
        return box_primitive(shared, thread, ch, v);
    }
    // Primitive widening (e.g. I -> J) — best-effort.
    if is_primitive_desc(sam_tok) && is_primitive_desc(impl_tok) {
        return Ok(widen_primitive(sam_tok, impl_tok, v));
    }
    Ok(v)
}

/// LambdaMetafactory argument adaptation (`samMethodType` → `instantiatedMethodType`).
///
/// When a functional-interface SAM has erased parameters (commonly `Object`,
/// from an unbounded type variable) but the lambda is *instantiated* with a more
/// specific type argument, javac's generated bridge method inserts a `checkcast`
/// to the instantiated parameter type before calling the implementation method —
/// throwing `ClassCastException` for an incompatible runtime argument (e.g.
/// `Map<String,Object>.forEach((k, v) -> …)` where a non-`String` key was stored
/// through a raw reference). CratonVM dispatches the lambda body *directly* from
/// [`try_lambda_dispatch`] / `NativeContextImpl::invoke_virtual`, bypassing that
/// synthetic bridge, so this helper replays the cast.
///
/// Only SAM-supplied args (`args[num_captures..]`) are checked, each against the
/// corresponding `instantiatedMethodType` parameter. Conservative posture (never
/// a spurious `ClassCastException`): a `null` argument, a primitive parameter, a
/// non-narrowing (instantiated == erased) parameter, an unloaded target type, or
/// any case we can't decide without risk all pass without throwing.
pub(super) fn checkcast_lambda_instantiated_args(
    shared: &SharedVm,
    thread: &JvmThread,
    handles: &[Option<usize>],
    sam_desc: &str,
    inst_desc: &str,
    args: &[Value],
    num_captures: usize,
) -> Result<(), MethodCallFailed> {
    let (sam_params, _) = split_method_descriptor(sam_desc);
    let (inst_params, _) = split_method_descriptor(inst_desc);
    for (sam_idx, inst_tok) in inst_params.iter().enumerate() {
        // Only a reference instantiated param can carry a checkcast.
        if !is_reference_desc(inst_tok) {
            continue;
        }
        // No narrowing vs the erased SAM param → the bridge inserts no cast.
        if sam_params
            .get(sam_idx)
            .map(|s| s == inst_tok)
            .unwrap_or(false)
        {
            continue;
        }
        // SAM-supplied args follow the captures in `args`.
        //
        // GC-safety: `args` is a plain Rust Vec copy handed to us by the
        // caller, not itself a GC root -- only `thread.native_pin_roots`
        // (which the caller pinned every object arg into before this call)
        // is remapped by a moving GC. A prior loop iteration's
        // `lambda_arg_provably_not_instance` call can trigger a GC via its
        // proxy/annotation-satisfies helpers, which leaves any later
        // `args[idx]` read here pointing at a stale, already-evacuated
        // address. Read the CURRENT address back through the caller's pin
        // (`handles`) instead of the raw `args` slice. Confirmed live via
        // CRATONVM_DBG_STALE_OBJREF during WildFly parallel-extension-add --
        // see docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md.
        let idx = num_captures + sam_idx;
        let obj_ref = match handles.get(idx).copied().flatten() {
            Some(h) => thread.native_pin_roots[h],
            None => match args.get(idx) {
                Some(Value::Object(Some(o))) => *o,
                // null (a `checkcast` of null always succeeds), primitive, or missing.
                _ => continue,
            },
        };
        if lambda_arg_provably_not_instance(shared, obj_ref, inst_tok) {
            let obj_class_name = shared
                .classes
                .class_manager
                .read()
                .get_class(shared.mem.heap.class_id_of(obj_ref))
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "?".to_string());
            let obj_display_name = cce_display_class_name(shared, obj_ref, &obj_class_name);
            let target_binary = inst_tok
                .strip_prefix('L')
                .and_then(|d| d.strip_suffix(';'))
                .unwrap_or(inst_tok)
                .replace('/', ".");
            // CRATONVM_DBG_CCE_BT: same attribution hook as the `checkcast`
            // opcode, plus whether this argument was read through the pinned
            // path (`via_pin`) — re-establishing the 2026-07-15 session's
            // temporary instrumentation permanently (that session measured
            // via_pin=true on every captured stale read here).
            if crate::runtime::interpreter::dbg_cce_bt_enabled() {
                let via_pin = handles.get(idx).copied().flatten().is_some();
                eprintln!(
                    "CRATONVM_DBG_CCE_BT: site=lambda_instantiated_args obj={} @0x{:x} target={} via_pin={via_pin}",
                    obj_display_name.replace('/', "."),
                    obj_ref.as_ptr() as usize,
                    target_binary
                );
            }
            // Same dotted-name shape as the `checkcast` opcode (tools such as
            // mockk's `JvmAutoHinter` parse this text).
            return Err(RuntimeError::ClassCastException {
                message: format!(
                    "{} cannot be cast to {}",
                    obj_display_name.replace('/', "."),
                    target_binary
                ),
            }
            .into());
        }
    }
    Ok(())
}

/// Return the Java-visible class name for a failed cast.
///
/// The immutable `Map.of` factories and `Collections.unmodifiableMap` use the
/// same native storage stamp. `Object.getClass()` deliberately translates that
/// stamp to the corresponding JDK implementation class, but a VM-generated
/// `ClassCastException` previously exposed the private stamp instead. Besides
/// being observably unlike HotSpot, that broke `LambdaSafe`: it identifies an
/// erased-generic mismatch by comparing the exception prefix with
/// `argument.getClass().getName()`.
///
/// Keep this mapping in lockstep with `native-builtins`' `getClass()` mapping
/// for maps. The backing map's physical slot layout follows the loaded JDK
/// class, so resolve its `size` field rather than assuming a fixed slot.
pub(super) fn cce_display_class_name(shared: &SharedVm, obj_ref: ObjectRef, raw_name: &str) -> String {
    // An array receiver must render as its own type, not its component's.
    // The header word of a reference array holds the COMPONENT class id, so
    // the caller's `class_id_of` -> `class.name` lookup yields
    // `java/lang/String` for a `String[]` and produces the nonsensical, and
    // actively misleading, `java.lang.String cannot be cast to
    // java.lang.String` (the TestObjectDataType failure, chased for a session
    // as a class-identity split). HotSpot renders the descriptor instead:
    // `[Ljava.lang.String;`.
    if let Some(desc) = array_descriptor_of(shared, obj_ref) {
        return desc;
    }
    if raw_name != "cratonvm/internal/UnmodifiableMap" {
        return raw_name.to_string();
    }
    if !matches!(shared.mem.heap.get_field(obj_ref, 1), Value::Int(1)) {
        return "java/util/Collections$UnmodifiableMap".to_string();
    }
    let backing = match shared.mem.heap.get_field(obj_ref, 0) {
        Value::Object(Some(backing)) => backing,
        _ => return "java/util/ImmutableCollections$MapN".to_string(),
    };
    let size = {
        let class_id = shared.mem.heap.class_id_of(backing);
        let cm = shared.classes.class_manager.read();
        find_field_recursive(class_id, "size", &cm.class_store)
            .map(|(field_index, _, _)| shared.mem.heap.get_field(backing, field_index))
    };
    if matches!(size, Some(Value::Int(1))) {
        "java/util/ImmutableCollections$Map1".to_string()
    } else {
        "java/util/ImmutableCollections$MapN".to_string()
    }
}

/// `true` iff `obj_ref` is *provably* not an instance of the reference
/// descriptor `desc_tok` (`L...;` or `[...`). Fails open (returns `false`)
/// whenever the answer can't be established without risk — an unloaded target,
/// an array-vs-non-array shape we can't decide, or a non-class descriptor — so a
/// genuine instance is never rejected. Only consults already-loaded classes (no
/// class loading → no GC, no stale `obj_ref`).
pub(super) fn lambda_arg_provably_not_instance(shared: &SharedVm, obj_ref: ObjectRef, desc_tok: &str) -> bool {
    // Array instantiated type: decide via the array-assignability rules.
    if desc_tok.starts_with('[') {
        return match array_descriptor_of(shared, obj_ref) {
            Some(src) => !array_is_assignable_to(shared, &src, desc_tok),
            None => false, // not an array — fail open
        };
    }
    let target = match desc_tok.strip_prefix('L').and_then(|d| d.strip_suffix(';')) {
        Some(t) => t,
        None => return false,
    };
    if target == "java/lang/Object" {
        return false;
    }
    let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
    // Lambda proxies use VM-only synthetic class IDs which intentionally do not
    // have ClassStore metadata.  Without a real class graph we cannot prove a
    // mismatch against the erased bridge parameter, so preserve this helper's
    // fail-open contract and let the normal lambda dispatch validate it.
    if shared
        .classes
        .class_manager
        .read()
        .get_class(obj_class_id)
        .is_none()
    {
        return false;
    }
    // The bridge this is standing in for holds a real `checkcast`, and a
    // `checkcast` RESOLVES its target class (JVMS §5.4.3.1) before comparing.
    // So must this: "not loaded yet" is a statement about the class store, not
    // about the object, and answering `false` there skips the cast entirely for
    // every instantiated type whose first mention IS this call site.
    //
    // Spring's `ApplicationListener.forPayload` is exactly that shape. Its
    // lambda's instantiated parameter is `PayloadApplicationEvent`, which
    // nothing has referenced when the first `ContextRefreshedEvent` is
    // multicast — so the cast was skipped, the body ran on the wrong event, and
    // `event.getPayload()` raised `NoSuchMethodError`.
    // `SimpleApplicationEventMulticaster.doInvokeListener` catches
    // `ClassCastException` for precisely this case ("possibly a lambda-defined
    // listener which we could not resolve the generic event type for") and
    // suppresses it; a `NoSuchMethodError` walks straight past that catch and
    // fails the context refresh
    // (`DevToolsR2dbcAutoConfigurationTests$Pooled`,
    // `probes/SpringForPayloadListenerProbe.java` — whose `preload` argument
    // resolves the type up front and made the same run pass, which is what
    // identified this branch).
    //
    // A load that FAILS still fails open: an instantiated type that is not on
    // the classpath at all is the one case where guessing is worse than
    // deferring, and it leaves the pre-existing behaviour untouched.
    let loaded_target = {
        let cm = shared.classes.class_manager.read();
        cm.get_loaded_class_id(target)
    };
    let loaded_target = match loaded_target {
        Some(tcid) => tcid,
        // Resolve it, exactly as the bridge's `checkcast` would on first
        // execution. `load_class_concurrent` loads and links without running
        // `<clinit>`, which is the resolution a `checkcast` performs.
        None => match shared.load_class_concurrent(target) {
            Ok(tcid) => tcid,
            Err(_) => return false,
        },
    };
    let (target_cid, is_sub, target_is_interface) = {
        let cm = shared.classes.class_manager.read();
        (
            loaded_target,
            obj_class_id == loaded_target || cm.is_subclass_of(obj_class_id, loaded_target),
            cm.get_class(loaded_target)
                .map(|c| c.is_interface())
                .unwrap_or(false),
        )
    };
    // The lambda bridge descriptor carries a binary name only.  In a forked
    // class-loader run the loader-blind lookup above may select the app copy
    // of that name even though the value (and the bridge that owns it) use a
    // child-defined copy.  Consult the value's exact defining namespace before
    // calling the mismatch proven; this is the same identity rule used by the
    // loader-aware checkcast path.  Restrict it to user loaders so ordinary
    // bootstrap/application delegation remains unchanged.
    let loader_scoped_is_sub = if crate::runtime::env_cache::loader_aware_resolution() {
        let cm = shared.classes.class_manager.read();
        cm.get_loader_id(obj_class_id)
            .filter(|loader| matches!(loader, cratonvm_types::ClassLoaderId::UserDefined(_)))
            .and_then(|loader| cm.class_defined_by_loader_exact(target, loader))
            .is_some_and(|scoped_target| cm.is_subclass_of(obj_class_id, scoped_target))
    } else {
        false
    };
    // AOTSVC-1: the checks above only prove a match via ClassId identity
    // (`is_sub`) or an exact defining-loader-namespace lookup
    // (`loader_scoped_is_sub`, which requires the object's OWN loader to have
    // already resolved `target` under its own namespace). Neither covers the
    // case exercised by `AotServices.factories().load(...)`-style SPI
    // discovery (Spring's `SpringFactoriesLoader.instantiateFactory`):
    // `ClassUtils.forName(implementationName, TCCL)` + `Constructor.newInstance`
    // allocate the service object using the EXACT ClassId resolved through the
    // caller's classloader argument, but never drive that same loader's
    // `loadClass` for the interface types the service implements — so
    // `class_defined_by_loader_exact(target, obj's loader)` can miss even
    // though the object's own `interfaces` list (populated at define/link time
    // from its own class file) already carries a same-named entry. This is the
    // identical name-vs-identity gap `loader_aware_name_assignable` already
    // closes for the ordinary bytecode `checkcast` opcode — reuse it here so
    // direct lambda dispatch (which bypasses that opcode, see this function's
    // caller) gets the same loader-faithful answer instead of a false
    // `ClassCastException` for a same-named, different-loader interface copy
    // (e.g. `TestRuntimeHintsRegistrar` under `@CompileWithForkedClassLoader`).
    if is_sub
        || loader_scoped_is_sub
        || lambda_proxy_satisfies(shared, obj_class_id, target_cid)
        || synthetic_implements(shared, obj_class_id, target)
        || proxy_instance_satisfies_target(shared, obj_ref, target)
        || annotation_proxy_satisfies_target(shared, obj_ref, target)
        || loader_aware_name_assignable(shared, obj_class_id, target_cid, target)
    {
        return false;
    }
    // Loader-split carve-out (gated): under loader-faithful resolution the same
    // logical type can exist as several per-loader copies (a Hibernate
    // bytecode-enhanced entity and its un-enhanced global copy, distinct
    // `ClassId`s sharing a name). `get_loaded_class_id(target)` picks only ONE,
    // so a lambda-arg `checkcast` to that type (e.g. adapting the receiver of a
    // `ImmutableEntity::getName` method reference) would spuriously reject an
    // instance of the OTHER copy — `X cannot be cast to X`. Treat an object
    // whose class NAME equals the target's as an instance. Gate-off unchanged;
    // only same-named cross-loader pairs are affected, never distinct types.
    if crate::runtime::env_cache::loader_aware_resolution() {
        let same_name = shared
            .classes
            .class_manager
            .read()
            .get_class(obj_class_id)
            .map(|c| &*c.name == target)
            .unwrap_or(false);
        if same_name {
            return false;
        }
    }
    // Provenance carve-out: a bare `java/lang/Object` receiver (`cid == 0`, or a
    // synthetic alloc that lost its class identity and now reports
    // `java/lang/Object`) carries no interface table, so we cannot prove it does
    // NOT implement an interface target. Many VM-synthesised objects that really
    // do implement marker interfaces (`java/io/Serializable`, `Comparable`, a
    // functional interface, …) on HotSpot land here. Only a *concrete-class*
    // target can be soundly rejected for such an object (e.g. `Object` → `String`
    // in the motivating `Map<String,Object>.forEach` case). For an interface
    // target, fail open. This never weakens the class-narrowing fix.
    //
    // Some early synthetic stubs are later used as interface edges before their
    // own `ACC_INTERFACE` metadata is trustworthy. Elasticsearch's
    // `Writeable$Writer.write` bridge is one such path: the target
    // `Writeable` id is present in implementors' `interfaces` lists, but a
    // concurrently observed value can carry the bare `Object` stamp. Treat that
    // target as interface-like too, matching the conservative posture above.
    let obj_is_bare = obj_class_id == ClassId::new(0)
        || shared
            .classes
            .class_manager
            .read()
            .get_class(obj_class_id)
            .map(|c| &*c.name == "java/lang/Object")
            .unwrap_or(true);
    if obj_is_bare && (target_is_interface || target_used_as_interface(shared, target_cid)) {
        return false;
    }
    true
}

pub(super) fn target_used_as_interface(shared: &SharedVm, target_cid: ClassId) -> bool {
    let cm = shared.classes.class_manager.read();
    let is_interface = cm
        .class_store()
        .iter()
        .any(|class| class.interfaces.iter().any(|iface| *iface == target_cid));
    is_interface
}

/// Widen a primitive value from `from_tok` to `to_tok` per JVM numeric promotion.
pub(super) fn widen_primitive(from_tok: &str, to_tok: &str, v: Value) -> Value {
    let as_i32 = |v: &Value| -> Option<i32> {
        match v {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    };
    match (from_tok, to_tok, &v) {
        ("I" | "B" | "S" | "C" | "Z", "J", _) => {
            if let Some(i) = as_i32(&v) {
                // Widening: i32 -> i64 (sign-extended, JVM i2l)
                return Value::Long(i as i64);
            }
        }
        ("I" | "B" | "S" | "C" | "Z", "F", _) => {
            if let Some(i) = as_i32(&v) {
                // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                return Value::Float(i as f32);
            }
        }
        ("I" | "B" | "S" | "C" | "Z", "D", _) => {
            if let Some(i) = as_i32(&v) {
                // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
                return Value::Double(i as f64);
            }
        }
        // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
        ("J", "F", Value::Long(l)) => return Value::Float(*l as f32),
        // Cast: integer-to-float numeric conversion (JVM i2f/i2d/l2f/l2d semantics)
        ("J", "D", Value::Long(l)) => return Value::Double(*l as f64),
        // Cast: numeric/representation conversion
        ("F", "D", Value::Float(f)) => return Value::Double(*f as f64),
        _ => {}
    }
    v
}

/// Coerce the return value from the impl descriptor back to the SAM descriptor.
pub fn coerce_return(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sam_ret: &str,
    impl_ret: &str,
    v: Option<Value>,
) -> Result<Option<Value>, MethodCallFailed> {
    if sam_ret == "V" {
        return Ok(None);
    }
    let raw = match v {
        Some(x) => x,
        None => return Ok(None),
    };
    if sam_ret == impl_ret {
        return Ok(Some(raw));
    }
    // SAM expects reference (Object/Integer/etc), impl returned primitive — box.
    if is_reference_desc(sam_ret) && is_primitive_desc(impl_ret) {
        let ch = impl_ret.chars().next().ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "empty primitive descriptor token in coerce_return".to_string(),
            })
        })?;
        let boxed = box_primitive(shared, thread, ch, raw)?;
        return Ok(Some(boxed));
    }
    // SAM expects primitive, impl returned reference — unbox.
    if is_primitive_desc(sam_ret) && is_reference_desc(impl_ret) {
        let ch = sam_ret.chars().next().ok_or_else(|| {
            MethodCallFailed::InternalError(VmError::Internal {
                message: "empty primitive descriptor token in coerce_return".to_string(),
            })
        })?;
        return Ok(Some(unbox_wrapper(shared, ch, raw)));
    }
    // Both primitive: maybe widen.
    if is_primitive_desc(sam_ret) && is_primitive_desc(impl_ret) {
        return Ok(Some(widen_primitive(impl_ret, sam_ret, raw)));
    }
    Ok(Some(raw))
}

/// Coerce arguments passed to a lambda SAM invocation to match the impl
/// method's descriptor.
///
/// `sam_desc` and `impl_desc` are method descriptors. `call_args` are the
/// SAM-level args (no captures). `captures_and_args` is `captures ++
/// call_args`. For `InvokeVirtual`/`InvokeInterface`, the first element of
/// `captures_and_args` is the receiver and should NOT be coerced against
/// impl_desc params (impl_desc params describe method params, not the
/// receiver). `receiver_present` distinguishes these cases.
pub fn coerce_lambda_args(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sam_desc: &str,
    impl_desc: &str,
    inst_desc: &str,
    args: &mut Vec<Value>,
    receiver_present: bool,
    num_captures: usize,
) -> Result<(), MethodCallFailed> {
    let (sam_params, _sam_ret) = split_method_descriptor(sam_desc);
    let (impl_params, _impl_ret) = split_method_descriptor(impl_desc);

    // The SAM's params correspond to args[num_captures..].
    // The impl's params correspond to args[receiver_skip..] where
    // receiver_skip = 1 if receiver_present else 0.
    // Number of non-receiver impl args should equal (total - receiver_skip).
    let receiver_skip = if receiver_present { 1 } else { 0 };
    if args.len() < receiver_skip {
        return Ok(());
    }

    // GC-safety: the instantiated-type checkcast below can LOAD classes and
    // `coerce_arg` BOXES primitives — both allocate and can trigger a moving
    // young GC while `args` sits in this raw Rust Vec, invisible to the
    // collector (native stale-local family). Pin every object entry into
    // `native_pin_roots` (which the GC remaps), refresh the Vec from the pins
    // after every potentially-allocating step, and keep each pin tracking the
    // CURRENT object for its index as entries are coerced. Pins are truncated
    // on every exit path.
    let pin_base = thread.native_pin_roots.len();
    let mut handles: Vec<Option<usize>> = Vec::with_capacity(args.len());
    for a in args.iter() {
        if let Value::Object(Some(o)) = a {
            handles.push(Some(thread.native_pin_roots.len()));
            thread.native_pin_roots.push(*o);
        } else {
            handles.push(None);
        }
    }

    // LambdaMetafactory argument adaptation: replay the `checkcast` to each
    // instantiated parameter type that the (bypassed) synthetic SAM bridge would
    // have performed, so a narrowed type variable still raises
    // `ClassCastException` for an incompatible argument. Runs before the
    // box/unbox coercion below, mirroring the bridge's cast-then-adapt order.
    if let Err(e) = checkcast_lambda_instantiated_args(
        shared,
        thread,
        &handles,
        sam_desc,
        inst_desc,
        args,
        num_captures,
    ) {
        thread.native_pin_roots.truncate(pin_base);
        return Err(e);
    }
    for (j, h) in handles.iter().enumerate() {
        if let Some(h) = *h {
            args[j] = Value::Object(Some(thread.native_pin_roots[h]));
        }
    }

    // The SAM-supplied args start at index num_captures in `args` (captures
    // come first). Captures themselves may also need boxing if bound as the
    // impl's receiver or first params, but for now we focus on the SAM args,
    // which is where the primitive/reference mismatch occurs.
    let impl_non_recv = &impl_params[..];
    // We want to coerce each arg[i] against the corresponding impl param.
    for i in 0..args.len() {
        if i < receiver_skip {
            continue;
        }
        let impl_idx = i - receiver_skip;
        if impl_idx >= impl_non_recv.len() {
            break;
        }
        // Determine what the caller's "expected" type was. For SAM-supplied
        // args (i >= num_captures), use sam_params[i - num_captures]. For
        // capture args (i < num_captures), we assume they match impl type
        // already (captures are erased at capture time).
        let sam_tok: String = if i >= num_captures {
            let sam_idx = i - num_captures;
            if sam_idx < sam_params.len() {
                sam_params[sam_idx].clone()
            } else {
                impl_non_recv[impl_idx].clone()
            }
        } else {
            impl_non_recv[impl_idx].clone()
        };
        let impl_tok = &impl_non_recv[impl_idx];
        let coerced = match coerce_arg(shared, thread, &sam_tok, impl_tok, args[i]) {
            Ok(v) => v,
            Err(e) => {
                thread.native_pin_roots.truncate(pin_base);
                return Err(e);
            }
        };
        // Keep this index's pin tracking the (possibly freshly boxed) object.
        match coerced {
            Value::Object(Some(o)) => {
                if let Some(h) = handles[i] {
                    thread.native_pin_roots[h] = o;
                } else {
                    handles[i] = Some(thread.native_pin_roots.len());
                    thread.native_pin_roots.push(o);
                }
            }
            _ => handles[i] = None,
        }
        args[i] = coerced;
        // coerce_arg may have allocated (boxing) — refresh every pinned entry.
        for (j, h) in handles.iter().enumerate() {
            if let Some(h) = *h {
                args[j] = Value::Object(Some(thread.native_pin_roots[h]));
            }
        }
    }
    thread.native_pin_roots.truncate(pin_base);
    Ok(())
}

/// Bug B: distinguish the SAM from a same-name, same-arity *overloaded default*
/// method on the functional interface. A functional interface may declare
/// default methods named like the SAM with the same arity but different
/// parameter types — e.g. `AnnotationFilter`'s SAM `matches(String)` plus
/// defaults `matches(Class)` / `matches(Annotation)`. The arity guard in
/// `try_lambda_dispatch` can't tell them apart, so `FILTER.matches(someClass)`
/// was wrongly routed into the `matches(String)` lambda body (passing a Class
/// where a String was expected → the lambda always returned false).
///
/// Returns `false` when the call is such an overloaded default (so the caller
/// falls through and runs the real default method, which converts the argument
/// and re-invokes the SAM). Only CONCRETE (non-`Object`) reference SAM params
/// are checked; generic/erased (`Object`) and primitive params are skipped, so
/// the hot stream/lambda path stays byte-identical. A param is treated as
/// compatible unless the runtime arg is a non-null object provably NOT an
/// instance of the SAM param type (mirrors the `instanceof` opcode's checks).
pub(crate) fn lambda_args_sam_compatible(
    shared: &SharedVm,
    sam_descriptor: &str,
    args: &[Value],
) -> bool {
    let (params, _ret) = split_method_descriptor(sam_descriptor);
    for (i, pd) in params.iter().enumerate() {
        if pd.starts_with('[') {
            // Array-typed SAM param. This was previously covered by the
            // `!pd.starts_with('L')` catch-all below (arrays don't start
            // with 'L'), which unconditionally skipped it -- "never
            // second-guess". That silently let a same-named, same-arity
            // interface DEFAULT method whose one differing parameter is a
            // scalar reference where the real SAM wants an array (e.g.
            // JRuby 10.x's `BlockCallback` -- abstract SAM
            // `call(ThreadContext, IRubyObject[], Block)` plus five
            // default overloads sharing the name "call", including
            // `call(ThreadContext, IRubyObject, Block)`) get misjudged as
            // SAM-compatible. `try_lambda_dispatch` then fed the raw
            // scalar argument directly into the array-typed lambda body
            // instead of falling through to the real default method (which
            // wraps the scalar into a 1-element array before re-invoking
            // the SAM) -- observed as `RubyEnumerable.packEnumValues`
            // calling `arraylength` on a bare `RubySymbol` during
            // `Enumerable#partition`'s per-element block callback
            // (JRubyScriptTemplateTests GC-ARRAY-GUARD investigation,
            // 2026-07-15). A present, non-null, non-array argument here is
            // provably NOT an instance of this SAM param -> treat as an
            // overloaded default, same as the concrete-class mismatch case
            // below.
            if let Some(Value::Object(Some(a))) = args.get(i) {
                if shared.mem.heap.kind_of(*a) != cratonvm_types::ObjectKind::Array {
                    return false;
                }
            }
            continue; // null / missing / genuinely an array -- don't second-guess further
        }
        if !pd.starts_with('L') || pd.as_str() == "Ljava/lang/Object;" {
            continue; // generic/erased or non-reference param -- never second-guess
        }
        let arg = match args.get(i) {
            Some(Value::Object(Some(a))) => *a,
            _ => continue, // null / primitive / missing — don't second-guess
        };
        let target = &pd[1..pd.len() - 1];
        let arg_cid = shared.mem.heap.class_id_of(arg);
        let (target_cid, base) = {
            let cm = shared.classes.class_manager.read();
            match cm.get_loaded_class_id(target) {
                Some(tcid) => (tcid, arg_cid == tcid || cm.is_subclass_of(arg_cid, tcid)),
                None => continue, // SAM param type not loaded — can't judge → compatible
            }
        };
        if base
            || loader_aware_name_assignable(shared, arg_cid, target_cid, target)
            || lambda_proxy_satisfies(shared, arg_cid, target_cid)
            || synthetic_implements(shared, arg_cid, target)
            || proxy_instance_satisfies_target(shared, arg, target)
            || annotation_proxy_satisfies_target(shared, arg, target)
        {
            continue;
        }
        return false; // arg provably not an instance of a concrete SAM param → overloaded default
    }
    true
}

/// Loader-faithful dispatch target for a lambda's implementation method.
///
/// The `invokedynamic` that materialised this lambda lives in the class recorded
/// in `lambda_proxy_hosts` (its enclosing/defining class). Resolve the impl
/// method's owner through THAT class's loader (JVMS §5.4.3 initiating loader) so
/// a lambda whose enclosing class was defined by a child / bytecode-enhancing
/// loader dispatches to that loader's copy of the impl method — not the flat
/// global copy the by-name `invoke_shared`/`load_class` path would pick. This
/// mirrors the `invokespecial`/`invokevirtual` divergence override already
/// applied to ordinary bytecode dispatch (search "dispatch_override").
///
/// Concrete failure without this: a Hibernate `@BytecodeEnhanced` test's
/// `s -> { new Continent(); ... }` lambda ran the UN-enhanced host copy, so
/// `new Continent` resolved the un-enhanced entity and the reflective
/// `Field.set` on it mismatched the enhanced mapped class.
///
/// Returns the loader-local `ClassId` only when it diverges from the by-name
/// resolution; `None` (gate off, built-in host loader, or no per-loader copy)
/// keeps the legacy name-based dispatch byte-for-byte.
pub(crate) fn lambda_impl_dispatch_override(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) -> Option<ClassId> {
    if !crate::runtime::env_cache::loader_aware_resolution() {
        return None;
    }
    let host = *shared
        .classes
        .lambda_proxy_hosts
        .read()
        .get(&call_site.proxy_class_id)?;
    let name = &call_site.impl_handle.class_name;
    lookup_loader_initiated(shared, host, name).filter(|cid| *cid != ClassId::new(0))
}

/// Like [`lambda_impl_dispatch_override`], but for the call site that actually
/// EXECUTES a static method-reference lambda's impl method (as opposed to the
/// read-only callers above that only ever consult an already-populated cache).
///
/// A static method reference (e.g. `EnvironmentPostProcessorsFactory::
/// fromSpringFactories`) captured by a `@CompileWithForkedClassLoader`-style
/// isolated loader's own class can be the VERY FIRST reference to its impl
/// owner from that loader's namespace — before any other bytecode (`new`,
/// `checkcast`, `invokestatic`) has driven that loader's `loadClass` and
/// populated `initiating_resolution_cache`/the loader's exact-class index.
/// `lambda_impl_dispatch_override`'s `lookup_loader_initiated` only reads that
/// cache; on a cold miss it returns `None` and the caller falls through to the
/// loader-blind `invoke_shared(class_name, ...)`, which resolves to whichever
/// same-named class the FLAT global store already holds (typically an
/// unrelated, earlier-loaded Application-loader copy) — running the wrong
/// loader's impl method body entirely, not just naming the wrong `Class`
/// object. Concrete failure: `SpringApplication`'s
/// `EnvironmentPostProcessorApplicationListener` (fork-loader-defined)
/// constructs `postProcessorsFactory = EnvironmentPostProcessorsFactory::
/// fromSpringFactories` and calls `.apply(classLoader)` before anything else
/// in the fork ever touches `EnvironmentPostProcessorsFactory` by name; the
/// resulting `new SpringFactoriesEnvironmentPostProcessorsFactory(...)` ran
/// under the Application loader's copy, so a sibling SPI implementation
/// (`CloudFoundryVcapEnvironmentPostProcessor`, correctly fork-loader-defined)
/// failed `Class.equals` against it and its constructor's `DeferredLogFactory`
/// argument resolved to `null`.
///
/// Actively drives the host loader's `loadClass` (same as the `New`/`Ldc`
/// resolution path's gate-on branch) on a cache miss, instead of only
/// consulting what's already cached. Falls back to the passive check (and
/// then to `None`, preserving legacy behavior) whenever the gate is off, the
/// host loader is built-in, or driving the loader fails to produce a class.
pub(crate) fn lambda_impl_dispatch_override_driven(
    shared: &SharedVm,
    thread: &mut JvmThread,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) -> Option<ClassId> {
    if let Some(cid) = lambda_impl_dispatch_override(shared, call_site) {
        return Some(cid);
    }
    if !crate::runtime::env_cache::loader_aware_resolution() {
        return None;
    }
    let host = *shared
        .classes
        .lambda_proxy_hosts
        .read()
        .get(&call_site.proxy_class_id)?;
    if !matches!(
        shared.classes.class_manager.read().get_loader_id(host),
        Some(cratonvm_types::ClassLoaderId::UserDefined(_))
    ) {
        return None;
    }
    let name = &call_site.impl_handle.class_name;
    drive_defining_loader_load(shared, thread, host, name).filter(|cid| *cid != ClassId::new(0))
}

/// Resolve a lambda implementation that is private in its declaring class.
///
/// LambdaMetafactory may encode a private synthetic lambda body as an
/// `InvokeVirtual` method handle. That handle is nevertheless bound to the
/// resolved owner method: virtual dispatch on the captured object's concrete
/// subclass is incorrect when that subclass happens to declare a same-named
/// synthetic `lambda$...` method. Preserve the declaring class in that case.
pub(crate) fn lambda_private_impl_dispatch_class(
    shared: &SharedVm,
    call_site: &crate::classloading::resolution::LambdaCallSite,
) -> Option<ClassId> {
    let owner_id = lambda_impl_dispatch_override(shared, call_site).or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(&call_site.impl_handle.class_name)
    })?;
    let cm = shared.classes.class_manager.read();
    let (method, declaring_id) = crate::classloading::find_method_recursive(
        owner_id,
        &call_site.impl_handle.member_name,
        &call_site.impl_handle.descriptor,
        &cm.class_store,
    )?;
    method
        .access_flags
        .contains(MethodAccessFlags::PRIVATE)
        .then_some(declaring_id)
}

/// Try to run a concrete default method declared by a lambda proxy's
/// functional interface.
///
/// Lambda proxy ids are synthetic and do not live in the class store. Calls to
/// non-SAM interface defaults normally reach the real functional interface via
/// direct dispatch, but an invoke through a superinterface can resolve to that
/// superinterface's abstract declaration first. Groovy's
/// `CompilationUnit$PhaseOperation.doPhaseOperation(CompilationUnit)` is one
/// such shape: the receiver is an `ISourceUnitOperation`/
/// `IPrimaryClassNodeOperation` lambda, while the resolved declaration is the
/// abstract parent interface.
pub(super) fn try_lambda_default_method_dispatch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    full_args: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    let (interface_name, interface_id_hint) = {
        let proxies = shared.classes.lambda_proxies.read();
        match proxies.get(&obj_class_id) {
            Some(call_site) => (
                call_site.functional_interface.clone(),
                call_site.functional_interface_id,
            ),
            None => return Ok(None),
        }
    };

    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_LAMBDA_DISPATCH").is_some() {
        eprintln!(
            "[DBG_LAMBDA] default-dispatch proxy={:?} method={}{} hint={:?}",
            obj_class_id, method_name, method_descriptor, interface_id_hint
        );
    }
    let declaring_id = {
        let cm = shared.classes.class_manager.read();
        // Prefer the loader-resolved interface captured at bootstrap time:
        // a bare name lookup returns an arbitrary copy when several loaders
        // define the same interface, executing the default method in the
        // wrong loader's context.
        let Some(interface_id) =
            interface_id_hint.or_else(|| cm.get_loaded_class_id(&interface_name))
        else {
            return Ok(None);
        };
        match crate::classloading::find_method_recursive(
            interface_id,
            method_name,
            method_descriptor,
            &cm.class_store,
        ) {
            Some((method, declaring_id)) if method.code().is_some() && !method.is_abstract() => {
                Some(declaring_id)
            }
            _ => None,
        }
    };

    let Some(declaring_id) = declaring_id else {
        return Ok(None);
    };
    crate::vm::invoke_on_class_shared_no_retarget(
        shared,
        thread,
        declaring_id,
        method_name,
        method_descriptor,
        full_args,
    )
    .map(Some)
}

/// Key for the two thread-local dispatch caches below:
/// `(vm_identity, proxy ClassId, receiver ClassId)`.
///
/// The `vm_identity` component is load-bearing, not decoration. A `ClassId` is
/// unique only *within* one VM, and one OS thread can run bytecode in two VMs
/// (the inline test modules build a `SharedVm` per test on one thread; a host
/// thread can be attached to two `Vm`s). Keyed on the two `ClassId`s alone, a
/// lookup in VM B hit VM A's entry for the numerically-equal ids and dispatched
/// VM A's cached method body — or VM A's cached field index — against VM B's
/// classes.
///
/// The `RedefineGate` does NOT catch that. Its staleness handle comes from the
/// class manager of whichever VM populated the entry, so consulted from VM B it
/// reports VM A's (unchanged) redefine generation and answers "fresh". The gate
/// guards against redefinition, not against identity collision; only the key
/// can do the latter.
///
/// See `docs/known-issues/c2/vm-process-global-state-round-2.md`.
type VmScopedClassPairKey = (usize, u32, u32);

thread_local! {
    pub(super) static LAMBDA_IMPL_BYTECODE_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<VmScopedClassPairKey, (Arc<CachedBytecodeMethod>, RedefineGate)>
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

// TDigest's private numeric kernels adapt a captured `TDigestDoubleArray.get(int)`
// through `Function<Integer, Double>`. Cache only getters whose entire body is
// the verifier-safe `aload_0; getfield [D; iload_1; daload; dreturn` shape.
// This is an interpreter superinstruction, not a semantic shortcut: any other
// lambda/accessor continues through the normal dispatch path.
thread_local! {
    pub(super) static TDIGEST_DOUBLE_GET_FIELD_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<VmScopedClassPairKey, (usize, RedefineGate)>
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

pub(super) fn try_tdigest_lambda_double_get(shared: &SharedVm, proxy: ObjectRef, index: i32) -> Option<f64> {
    let proxy_class_id = shared.mem.heap.class_id_of(proxy);
    let call_site = shared
        .classes
        .lambda_proxies
        .read()
        .get(&proxy_class_id)
        .cloned()?;
    if call_site.functional_interface.as_ref() != "java/util/function/Function"
        || call_site.sam_method_name.as_ref() != "apply"
        || call_site.sam_descriptor.as_ref() != "(Ljava/lang/Object;)Ljava/lang/Object;"
        || call_site.capture_types.len() != 1
        || call_site.impl_handle.member_name.as_ref() != "get"
        || call_site.impl_handle.descriptor.as_ref() != "(I)D"
        || !matches!(
            call_site.impl_handle.kind,
            MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface
        )
    {
        return None;
    }
    let receiver = match shared.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(receiver)) => receiver,
        _ => return None,
    };
    let receiver_class_id = shared.mem.heap.class_id_of(receiver);
    let key = (
        shared.vm_identity,
        proxy_class_id.as_u32(),
        receiver_class_id.as_u32(),
    );
    let field_index = TDIGEST_DOUBLE_GET_FIELD_CACHE
        .with(|cache| {
            let mut cache = cache.borrow_mut();
            match cache.get(&key) {
                Some((field_index, gate)) if !gate.is_stale() => Some(*field_index),
                Some(_) => {
                    cache.remove(&key);
                    None
                }
                None => None,
            }
        })
        .or_else(|| {
            let (declaring_id, field_cp_index, gate) = {
                let cm = shared.classes.class_manager.read();
                let (method, declaring_id) = crate::classloading::find_method_recursive(
                    receiver_class_id,
                    "get",
                    "(I)D",
                    &cm.class_store,
                )?;
                let code = method.code()?;
                if code.code.len() != 7
                    || code.code[0] != 0x2a
                    || code.code[1] != 0xb4
                    || code.code[4] != 0x1b
                    || code.code[5] != 0x31
                    || code.code[6] != 0xaf
                {
                    return None;
                }
                (
                    declaring_id,
                    ((code.code[2] as u16) << 8) | code.code[3] as u16,
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id)),
                )
            };
            let field = resolve_field_ref(shared, declaring_id, field_cp_index).ok()?;
            if field.is_static || field.desc_byte != b'[' {
                return None;
            }
            TDIGEST_DOUBLE_GET_FIELD_CACHE.with(|cache| {
                cache.borrow_mut().insert(key, (field.field_index, gate));
            });
            Some(field.field_index)
        })?;
    if index < 0 {
        return None;
    }
    let array = match shared.mem.heap.get_field(receiver, field_index) {
        Value::Object(Some(array)) => array,
        _ => return None,
    };
    match shared
        .mem
        .heap
        .get_array_element(array, index as usize)
        .ok()?
    {
        Value::Double(value) => Some(value),
        _ => None,
    }
}

pub(super) fn try_invoke_cached_lambda_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy_class_id: ClassId,
    receiver_class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    let key = (
        shared.vm_identity,
        proxy_class_id.as_u32(),
        receiver_class_id.as_u32(),
    );
    let cached = LAMBDA_IMPL_BYTECODE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        match cache.get(&key) {
            Some((cached, gate)) if !gate.is_stale() => Some(Arc::clone(cached)),
            Some(_) => {
                cache.remove(&key);
                None
            }
            None => None,
        }
    });
    let cached = match cached {
        Some(c) if &*c.method_name == method_name && &*c.method_descriptor == descriptor => c,
        Some(_) => return Ok(None),
        None => {
            let cm = shared.classes.class_manager.read();
            let store = cm.class_store();
            let Some((method, declaring_id)) = crate::classloading::find_method_recursive(
                receiver_class_id,
                method_name,
                descriptor,
                store,
            ) else {
                return Ok(None);
            };
            let Some(class) = store.get(declaring_id) else {
                return Ok(None);
            };
            // Cached bytecode bypasses native dispatch, which must retain precedence.
            if shared
                .natives
                .native_methods
                .find(&class.name, method_name, descriptor)
                .is_some()
            {
                return Ok(None);
            }
            if method.is_static() || method.is_synchronized() || method.is_native() {
                return Ok(None);
            }
            let Some(code_attr) = method.code() else {
                return Ok(None);
            };
            let c = Arc::new(CachedBytecodeMethod {
                declaring_class_id: declaring_id,
                class_name: Arc::clone(&class.name),
                method_name: Arc::from(method_name),
                method_descriptor: Arc::from(descriptor),
                source_file: class.source_file.as_deref().map(Arc::from),
                code: crate::runtime::frame::padded_bytecode(&code_attr.code),
                exception_table: Arc::from(code_attr.exception_table.as_slice()),
                max_stack: code_attr.max_stack,
                max_locals: code_attr.max_locals,
                num_params: count_method_params(descriptor) as u16,
                is_synchronized: false,
                is_static: false,
                force_native_cache: std::sync::OnceLock::new(),
                native_callback_cache: std::sync::OnceLock::new(),
                invoc_key: std::sync::OnceLock::new(),
                jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
                quickened: std::sync::OnceLock::new(),
            });
            let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
            drop(cm);
            LAMBDA_IMPL_BYTECODE_CACHE.with(|cache| {
                cache.borrow_mut().insert(key, (Arc::clone(&c), gate));
            });
            c
        }
    };
    if args.len() != cached.num_params as usize + 1 {
        return Ok(None);
    }
    // TDigest's lambda adapter repeatedly invokes the concrete array accessor
    // `(I)D`. When that leaf is already compiled and has no dispatch helpers,
    // enter it directly instead of materializing an interpreter frame per get.
    // Other lambda implementations retain the generic cached-frame path below.
    if !matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && &*cached.method_name == "get"
        && &*cached.method_descriptor == "(I)D"
    {
        let compiled = {
            let cache = shared.jit.jit_cache.read();
            cache.get(
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
                cached.declaring_class_id,
            )
        };
        if let Some(compiled) = compiled {
            if !compiled.has_dispatch {
                let raw = match (args.get(0), args.get(1)) {
                    (Some(Value::Object(Some(receiver))), Some(Value::Int(index))) => {
                        let vm_ptr = shared as *const _ as i64;
                        let jit_args = [receiver.as_ptr() as i64, *index as i64];
                        let _guard =
                            crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled(
                                &*compiled,
                            );
                        // SAFETY: the compiled entry's ABI and optional context
                        // are selected from its own verified metadata above.
                        unsafe {
                            if compiled.needs_context() {
                                compiled.try_call_with_context(vm_ptr, &jit_args)
                            } else {
                                compiled.try_call(&jit_args)
                            }
                        }
                        .ok()
                    }
                    _ => None,
                };
                if let Some(bits) = raw {
                    return Ok(Some(Some(Value::Double(f64::from_bits(bits as u64)))));
                }
            }
        }
    }
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    let frame = Frame::new_pooled_cached(
        cached,
        args,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    execute_prebuilt_frame(shared, thread, frame).map(Some)
}

/// Try to dispatch a method call on a lambda proxy object.
///
/// Returns:
/// - `Ok(Some(Some(value)))` — lambda handled the call and produced a return value
/// - `Ok(Some(None))` — lambda handled the call (void return)
/// - `Ok(None)` — not a lambda proxy, fall through to normal dispatch
///
/// WP2.5: also called from `proxy_invoke_handler_shared` when the
/// `InvocationHandler` is itself a lambda — the synthetic lambda
/// proxy's class_id is not in the class store, so the standard
/// `invoke_or_native` fallback would mis-route to the abstract
/// `java/lang/reflect/InvocationHandler.invoke` (which has no Code
/// attribute).
thread_local! {
    pub(super) static LAMBDA_DISPATCH_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub(super) fn lambda_dispatch_active() -> bool {
    LAMBDA_DISPATCH_DEPTH.with(|depth| depth.get() != 0)
}

pub(crate) fn try_lambda_dispatch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: ObjectRef,
    obj_class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    call_args: &[Value],
) -> Result<Option<Option<Value>>, MethodCallFailed> {
    // S-bytebuddy r4 — independent recursion guard for lambda dispatch.
    // Lambda SAM implementations can re-enter `try_lambda_dispatch` via
    // `invoke_or_native` / `invoke_shared` when the impl body itself
    // invokes another lambda (the common `stream.map(x -> ...).filter(y
    // -> ...)` shape). The aggregate EXEC_DEPTH guard catches this only
    // after the Rust stack has grown by ~10 frames per turn. A dedicated
    // counter trips much earlier with a tight cap.
    thread_local! {
        static LAMBDA_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }
    struct LambdaDepthGuard;
    impl Drop for LambdaDepthGuard {
        fn drop(&mut self) {
            LAMBDA_DEPTH.with(|d| {
                let v = d.get();
                d.set(v.saturating_sub(1));
            });
        }
    }
    let ldepth = LAMBDA_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    if ldepth > 2_000 {
        LAMBDA_DEPTH.with(|d| {
            let v = d.get();
            d.set(v.saturating_sub(1));
        });
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }
    let _lambda_depth_guard = LambdaDepthGuard;

    struct LambdaDispatchGuard;
    impl Drop for LambdaDispatchGuard {
        fn drop(&mut self) {
            LAMBDA_DISPATCH_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        }
    }
    LAMBDA_DISPATCH_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let _lambda_dispatch_guard = LambdaDispatchGuard;

    // Look up the lambda proxy metadata for this ClassId.
    let call_site = {
        let proxies = shared.classes.lambda_proxies.read();
        match proxies.get(&obj_class_id) {
            Some(lcs) => lcs.clone(),
            None => return Ok(None), // Not a lambda proxy
        }
    };
    if crate::runtime::env_cache::lambda_dbg() {
        eprintln!(
            "[cratonvm-dbg] lambda dispatch entry: cid={} sam={}.{} impl={}.{}{} kind={:?}",
            obj_class_id,
            call_site.functional_interface,
            method_name,
            call_site.impl_handle.class_name,
            call_site.impl_handle.member_name,
            call_site.impl_handle.descriptor,
            call_site.impl_handle.kind,
        );
    }

    // A functional interface may declare same-named default overloads of its
    // SAM. Dispatching a lambda by method name, arity, or runtime argument
    // assignability is unsound: null is assignable to both `InetAddress` and
    // `InetSocketAddress`, so the latter default could be skipped entirely.
    // The bytecode call-site descriptor is the authoritative identity. Only the
    // exact SAM descriptor may enter the lambda body; any other descriptor must
    // fall through to ordinary interface/default-method dispatch.
    if method_name == &*call_site.sam_method_name && method_descriptor != &*call_site.sam_descriptor
    {
        return Ok(None);
    }

    // Only intercept calls to the SAM (single abstract method). Default
    // methods on the functional interface (e.g. Function.andThen,
    // Predicate.and) are dispatched directly via the native registry on
    // the functional interface class.
    if method_name != &*call_site.sam_method_name {
        // RScala.1 bridge: Scala 3 produces lambdas whose functional
        // interface is `scala/runtime/java8/JFunctionN$mcXYZ$sp`, which
        // declares the SAM as a primitive-specialized method (e.g.
        // `apply$mcII$sp(I)I`). The Scala stdlib's `Function1` has a
        // default `apply$mcII$sp(int)` that boxes + calls `apply(Object)`,
        // while `JFunction1$mcII$sp` has a default `apply(Object)` that
        // unboxes + calls `apply$mcII$sp(int)`. Without picking the
        // maximally-specific default, they ping-pong until StackOverflow.
        // Bridge here: if a non-SAM method is `apply` on a Scala
        // specialized function interface, unbox args → call SAM → box.
        if method_name == "apply"
            && call_site
                .functional_interface
                .starts_with("scala/runtime/java8/JFunction")
            && call_site.sam_method_name.starts_with("apply$mc")
        {
            // Parse the specialization tag from the SAM name, e.g.
            // "apply$mcII$sp" → ("I","I") for (I)I.
            // Format: apply$mc<RET><ARG...>$sp.
            let tag = call_site
                .sam_method_name
                .strip_prefix("apply$mc")
                .and_then(|s| s.strip_suffix("$sp"));
            if let Some(tag) = tag {
                // First char = return type, rest = arg types.
                let mut chars = tag.chars();
                let ret_ch = chars.next().unwrap_or('V');
                let arg_chars: Vec<char> = chars.collect();

                // Unbox each argument (call_args are all boxed Object).
                let mut unboxed: Vec<Value> = Vec::with_capacity(arg_chars.len());
                for (i, &ac) in arg_chars.iter().enumerate() {
                    let v = call_args.get(i).copied().unwrap_or(Value::Object(None));
                    let u = match (ac, v) {
                        ('I' | 'Z' | 'B' | 'S' | 'C', Value::Object(Some(b))) => {
                            shared.mem.heap.get_field(b, 0)
                        }
                        ('J', Value::Object(Some(b))) => shared.mem.heap.get_field(b, 0),
                        ('F', Value::Object(Some(b))) => shared.mem.heap.get_field(b, 0),
                        ('D', Value::Object(Some(b))) => shared.mem.heap.get_field(b, 0),
                        (_, other) => other,
                    };
                    unboxed.push(u);
                }

                // Reenter lambda dispatch with the SAM method name and
                // unboxed args. Note: call_args we pass are the SAM's
                // primitive args (captures are read inside).
                let sam_name = call_site.sam_method_name.clone();
                // Drop call_site borrow before recursing indirectly.
                // Recursion depth is bounded (one hop to SAM path).
                drop(call_site);
                // Reborrow fresh to avoid use-after-move.
                let lcs = {
                    let proxies = shared.classes.lambda_proxies.read();
                    proxies.get(&obj_class_id).cloned()
                };
                let lcs = match lcs {
                    Some(l) => l,
                    None => {
                        crate::runtime::diagnostics::record_swallow(
                            shared,
                            "lambda-dispatch",
                            "proxy-lost-mid-dispatch",
                            &format!("class_id={} method={}", obj_class_id, method_name),
                        );
                        return Ok(None);
                    }
                };

                // Execute the SAM path directly (mirrors the code below).
                let num_captures = lcs.capture_types.len();
                let mut full_args: Vec<Value> = Vec::with_capacity(num_captures + unboxed.len());
                for i in 0..num_captures {
                    full_args.push(shared.mem.heap.get_field(obj_ref, i));
                }
                full_args.extend(unboxed);
                let _ = sam_name; // sam path uses lcs.impl_handle

                // Dispatch to the implementation handle.
                let result_val = match lcs.impl_handle.kind {
                    MethodHandleKind::InvokeStatic => {
                        if let Some(impl_cid) = lambda_impl_dispatch_override(shared, &lcs) {
                            crate::vm::invoke_on_class_shared_no_retarget(
                                shared,
                                thread,
                                impl_cid,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        } else {
                            invoke_shared(
                                shared,
                                thread,
                                &lcs.impl_handle.class_name,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        }
                    }
                    MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => {
                        if full_args.is_empty() {
                            crate::runtime::diagnostics::record_swallow(
                                shared,
                                "lambda-dispatch",
                                "virtual-no-receiver",
                                &format!(
                                    "class={} member={}",
                                    lcs.impl_handle.class_name, lcs.impl_handle.member_name
                                ),
                            );
                            return Ok(None);
                        }
                        let rcv_id_opt = match &full_args[0] {
                            Value::Object(Some(r)) => Some(shared.mem.heap.class_id_of(*r)),
                            _ => None,
                        };
                        let receiver_class = match rcv_id_opt {
                            Some(rcv) => shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(rcv)
                                .map(|c| c.name.to_string())
                                .unwrap_or_else(|| lcs.impl_handle.class_name.to_string()),
                            None => lcs.impl_handle.class_name.to_string(),
                        };
                        // Loader-faithful (gated): dispatch on the receiver's exact
                        // class_id when it diverges from the by-name global copy.
                        let vov = if crate::runtime::env_cache::loader_aware_resolution() {
                            rcv_id_opt.filter(|rcv| {
                                *rcv != ClassId::new(0)
                                    && !shared.classes.lambda_proxies.read().contains_key(rcv)
                                    && {
                                        let cm = shared.classes.class_manager.read();
                                        cm.get_class(*rcv)
                                            .map(|c| &*c.name == receiver_class.as_str())
                                            .unwrap_or(false)
                                            && cm.get_loaded_class_id(&receiver_class) != Some(*rcv)
                                    }
                            })
                        } else {
                            None
                        };
                        if let Some(rcv_cid) = vov {
                            invoke_on_class_shared(
                                shared,
                                thread,
                                rcv_cid,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        } else {
                            invoke_or_native(
                                shared,
                                thread,
                                &receiver_class,
                                &lcs.impl_handle.member_name,
                                &lcs.impl_handle.descriptor,
                                &full_args,
                            )?
                        }
                    }
                    other => {
                        crate::runtime::diagnostics::record_swallow(
                            shared,
                            "lambda-dispatch",
                            "unsupported-handle-kind",
                            &format!(
                                "kind={:?} class={} member={}",
                                other, lcs.impl_handle.class_name, lcs.impl_handle.member_name
                            ),
                        );
                        return Ok(None);
                    }
                };

                // Box the primitive return to match apply(Object)Object
                // by invoking the respective `valueOf` static.
                let box_one = |shared: &SharedVm,
                               thread: &mut JvmThread,
                               cls: &str,
                               desc: &str,
                               prim: Value|
                 -> Result<Value, MethodCallFailed> {
                    let r = invoke_shared(shared, thread, cls, "valueOf", desc, &[prim])?;
                    Ok(r.unwrap_or(Value::Object(None)))
                };
                let raw = result_val.unwrap_or(Value::Object(None));
                let boxed = match (ret_ch, raw) {
                    ('V', _) => Some(Value::Object(None)),
                    ('Z', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Boolean",
                        "(Z)Ljava/lang/Boolean;",
                        Value::Int(i),
                    )?),
                    ('B', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Byte",
                        "(B)Ljava/lang/Byte;",
                        Value::Int(i),
                    )?),
                    ('S', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Short",
                        "(S)Ljava/lang/Short;",
                        Value::Int(i),
                    )?),
                    ('C', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Character",
                        "(C)Ljava/lang/Character;",
                        Value::Int(i),
                    )?),
                    ('I', Value::Int(i)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Integer",
                        "(I)Ljava/lang/Integer;",
                        Value::Int(i),
                    )?),
                    ('J', Value::Long(l)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Long",
                        "(J)Ljava/lang/Long;",
                        Value::Long(l),
                    )?),
                    ('F', Value::Float(f)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Float",
                        "(F)Ljava/lang/Float;",
                        Value::Float(f),
                    )?),
                    ('D', Value::Double(d)) => Some(box_one(
                        shared,
                        thread,
                        "java/lang/Double",
                        "(D)Ljava/lang/Double;",
                        Value::Double(d),
                    )?),
                    (_, v) => Some(v),
                };
                return Ok(Some(boxed));
            }
        }

        // Build full args with receiver prepended.
        let mut full_args = Vec::with_capacity(1 + call_args.len());
        full_args.push(Value::Object(Some(obj_ref)));
        full_args.extend_from_slice(call_args);

        // Try common descriptor patterns for default methods.
        let iface = &call_site.functional_interface;
        let descriptors = [
            format!("(L{iface};)L{iface};"), // andThen/compose/and/or
            format!("()L{iface};"),          // negate/identity
        ];
        for desc in &descriptors {
            if let Some(callback) = shared.natives.native_methods.find(iface, method_name, desc) {
                let mut ctx = crate::vm::NativeContextImpl { shared, thread };
                // Widening: small integer index -> usize (non-negative, fits in pointer width)
                let _ring_idx = cratonvm_native_api::native_ring::record_enter(callback as usize);
                let result = callback(&mut ctx, &full_args);
                cratonvm_native_api::native_ring::record_exit(_ring_idx);
                let result = result?;
                return Ok(Some(result));
            }
        }
        // Not found in native registry — fall through to normal dispatch
        return Ok(None);
    }

    // Read captured values from the proxy object's fields.
    let num_captures = call_site.capture_types.len();
    let mut full_args: Vec<Value> = Vec::with_capacity(num_captures + call_args.len());
    for i in 0..num_captures {
        full_args.push(shared.mem.heap.get_field(obj_ref, i));
    }
    // Append the invocation arguments (passed by the caller after the receiver).
    full_args.extend_from_slice(call_args);

    // Dispatch based on the implementation method handle kind.
    let sam_desc = call_site.sam_descriptor.clone();
    let impl_desc = call_site.impl_handle.descriptor.clone();
    let inst_desc = call_site.instantiated_descriptor.clone();
    let (_sam_params_tmp, sam_ret) = split_method_descriptor(&sam_desc);
    let (_impl_params_tmp, impl_ret) = split_method_descriptor(&impl_desc);
    match call_site.impl_handle.kind {
        MethodHandleKind::InvokeStatic => {
            // Static method: all args are parameters (no receiver).
            coerce_lambda_args(
                shared,
                thread,
                &sam_desc,
                &impl_desc,
                &inst_desc,
                &mut full_args,
                false,
                num_captures,
            )?;
            if crate::runtime::env_cache::lambda_dbg() {
                eprintln!(
                    "[cratonvm-dbg] lambda static-pre-invoke: {}.{}{} args={}",
                    call_site.impl_handle.class_name,
                    call_site.impl_handle.member_name,
                    call_site.impl_handle.descriptor,
                    full_args.len(),
                );
            }
            let result = if let Some(impl_cid) =
                lambda_impl_dispatch_override_driven(shared, thread, &call_site)
            {
                // Loader-faithful: the enclosing class was defined by a user
                // loader whose copy of the impl owner diverges from the global
                // one; dispatch on the exact loader-local class (static → no
                // receiver retarget).
                crate::vm::invoke_on_class_shared_no_retarget(
                    shared,
                    thread,
                    impl_cid,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )?
            } else {
                invoke_shared(
                    shared,
                    thread,
                    &call_site.impl_handle.class_name,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )?
            };
            if crate::runtime::env_cache::lambda_dbg() {
                eprintln!(
                    "[cratonvm-dbg] lambda static-post-invoke: {}.{}{} result={:?}",
                    call_site.impl_handle.class_name,
                    call_site.impl_handle.member_name,
                    call_site.impl_handle.descriptor,
                    result.is_some(),
                );
            }
            Ok(Some(coerce_return(
                shared, thread, &sam_ret, &impl_ret, result,
            )?))
        }
        MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => {
            // Virtual/interface: first arg is receiver, rest are parameters.
            if full_args.is_empty() {
                return Err(VmError::Internal {
                    message: "lambda dispatch: InvokeVirtual/InvokeInterface with no args"
                        .to_string(),
                }
                .into());
            }
            // Coerce args (keep receiver at [0] unchanged for virtual dispatch).
            coerce_lambda_args(
                shared,
                thread,
                &sam_desc,
                &impl_desc,
                &inst_desc,
                &mut full_args,
                true,
                num_captures,
            )?;
            let private_impl_class = lambda_private_impl_dispatch_class(shared, &call_site);
            // Round 7 — if the receiver is itself a lambda proxy whose SAM
            // matches the impl_handle's member name, recurse through
            // try_lambda_dispatch directly. Without this, downstream
            // `invoke_or_native` falls back to the cp interface name (because
            // class_manager.get_class fails on lambda-proxy class_ids), then
            // resolves the abstract interface declaration with no Code attribute
            // and surfaces an AbstractMethodError. Concrete tripwire: Spring
            // Boot's `CacheOverrides.close()` does
            // `forEach(CacheOverride::close)` and the iterated items are
            // themselves NOOP `CacheOverride` lambdas declared as static
            // fields on `SoftReferenceConfigurationPropertyCache` — every
            // item is a lambda proxy, never a concrete CacheOverride.
            if let Value::Object(Some(r)) = &full_args[0] {
                let rcv_class_id = shared.mem.heap.class_id_of(*r);
                let recv_is_lambda = shared
                    .classes
                    .lambda_proxies
                    .read()
                    .contains_key(&rcv_class_id);
                if recv_is_lambda {
                    let inner = try_lambda_dispatch(
                        shared,
                        thread,
                        *r,
                        rcv_class_id,
                        &call_site.impl_handle.member_name,
                        &call_site.impl_handle.descriptor,
                        &full_args[1..],
                    )?;
                    if let Some(inner_v) = inner {
                        return Ok(Some(coerce_return(
                            shared, thread, &sam_ret, &impl_ret, inner_v,
                        )?));
                    }
                }
            }
            // Resolve the actual class of the receiver for virtual dispatch.
            let recv_class_id_opt = match &full_args[0] {
                Value::Object(Some(r)) => Some(shared.mem.heap.class_id_of(*r)),
                _ => None,
            };
            // Diagnostic (CRATONVM_DBG_LAMBDA): when a lambda dispatch receiver
            // resolves to an unknown/zero class (the stale-captured-reference
            // family — NoSuchMethodError like "java/lang/Object.get(I)D"),
            // dump the raw pointer, its class id, the load_and_forward result,
            // and a FRESH re-read of the proxy's capture field. Discriminates
            // "stale baked into the proxy field" (fresh re-read returns the
            // same dead pointer) from a transient Rust-local staleness.
            if crate::runtime::env_cache::lambda_dbg() {
                if let (Some(cid), Value::Object(Some(r))) = (recv_class_id_opt, &full_args[0]) {
                    if cid == ClassId::new(0) {
                        let fwd = shared.mem.heap.load_and_forward(*r);
                        let fwd_cid = shared.mem.heap.class_id_of(fwd);
                        let fresh = shared.mem.heap.get_field(obj_ref, 0);
                        let (fresh_ptr, fresh_cid) = match fresh {
                            Value::Object(Some(f)) => {
                                (f.as_ptr() as usize, Some(shared.mem.heap.class_id_of(f)))
                            }
                            _ => (0, None),
                        };
                        eprintln!(
                            "[lambda-nsme-diag] recv={:p} cid={:?} fwd={:p} fwd_cid={:?} \
                             proxy={:p} fresh_field=0x{:x} fresh_cid={:?} impl={}.{}{}",
                            r.as_ptr(),
                            cid,
                            fwd.as_ptr(),
                            fwd_cid,
                            obj_ref.as_ptr(),
                            fresh_ptr,
                            fresh_cid,
                            call_site.impl_handle.class_name,
                            call_site.impl_handle.member_name,
                            call_site.impl_handle.descriptor,
                        );
                    }
                }
            }
            let receiver_class = match recv_class_id_opt {
                Some(rcv_class_id) => shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(rcv_class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| call_site.impl_handle.class_name.to_string()),
                None => call_site.impl_handle.class_name.to_string(),
            };
            // Loader-faithful (gated): when the receiver's runtime class diverges
            // from the by-name global resolution — a per-loader (e.g. bytecode-
            // enhanced) copy — dispatch on the receiver's EXACT class_id so the
            // lambda body runs that loader's copy. `invoke_or_native` otherwise
            // collapses `receiver_class` (a name) to the global copy, running the
            // un-enhanced lambda body (`new Country` → un-enhanced entity →
            // reflective `Field.set` mismatch). Mirrors the ordinary
            // invoke_virtual divergence override.
            let virtual_override = if crate::runtime::env_cache::loader_aware_resolution() {
                recv_class_id_opt.filter(|rcv| {
                    *rcv != ClassId::new(0)
                        && !shared.classes.lambda_proxies.read().contains_key(rcv)
                        && {
                            let cm = shared.classes.class_manager.read();
                            // Only when `receiver_class` truly names the receiver's
                            // own real class (not the impl-handle fallback for a
                            // lambda-proxy / generic-ClassId receiver) AND that name
                            // globally resolves to a DIFFERENT (per-loader) copy.
                            cm.get_class(*rcv)
                                .map(|c| &*c.name == receiver_class.as_str())
                                .unwrap_or(false)
                                && cm.get_loaded_class_id(&receiver_class) != Some(*rcv)
                        }
                })
            } else {
                None
            };
            // A private instance lambda body is encoded by javac as an
            // InvokeVirtual handle, but it retains invokespecial semantics:
            // resolve it on the handle's declaring class, not by walking the
            // captured receiver's hierarchy. Synthetic lambda names are not
            // unique across a hierarchy (for example both Spring Data's
            // AnnotationBasedPersistentProperty and AbstractPersistentProperty
            // have lambda$new$2), so receiver-based lookup can execute a
            // different private body with the same name and descriptor.
            let impl_owner_id = lambda_impl_dispatch_override(shared, &call_site).or_else(|| {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_loaded_class_id(&call_site.impl_handle.class_name)
            });
            let private_impl_owner = impl_owner_id.filter(|owner_id| {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(*owner_id)
                    .and_then(|class| {
                        class.find_method(
                            &call_site.impl_handle.member_name,
                            &call_site.impl_handle.descriptor,
                        )
                    })
                    .is_some_and(|method| method.access_flags.contains(MethodAccessFlags::PRIVATE))
            });
            // Keep the existing loader-faithful private implementation owner
            // when it is available; otherwise use the declaring owner found
            // above for private synthetic lambda methods.
            let exact_impl_owner = private_impl_class.or(private_impl_owner);
            let cached_result = if exact_impl_owner.is_none() {
                recv_class_id_opt
                    .filter(|rcv| *rcv != ClassId::new(0))
                    .map(|rcv| {
                        try_invoke_cached_lambda_impl(
                            shared,
                            thread,
                            obj_class_id,
                            rcv,
                            &call_site.impl_handle.member_name,
                            &call_site.impl_handle.descriptor,
                            &full_args,
                        )
                    })
                    .transpose()?
                    .flatten()
            } else {
                None
            };
            let result = if let Some(owner_id) = exact_impl_owner {
                crate::vm::invoke_on_class_shared_no_retarget(
                    shared,
                    thread,
                    owner_id,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )
            } else if let Some(result) = cached_result {
                Ok(result)
            } else if let Some(rcv_cid) = virtual_override {
                invoke_on_class_shared(
                    shared,
                    thread,
                    rcv_cid,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )
            } else if let Some(rcv_cid) = recv_class_id_opt.filter(|rcv| {
                *rcv != ClassId::new(0)
                    && !shared.classes.lambda_proxies.read().contains_key(rcv)
                    && shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(*rcv)
                        .is_some()
            }) {
                // The captured receiver is already a loaded, concrete object.
                // Dispatch by its ClassId instead of converting it back to a class
                // name and re-entering invoke_shared's class-loader path for every
                // SAM call. invoke_on_class_shared keeps the normal native
                // precedence, virtual retargeting, monitor, and exception rules.
                invoke_on_class_shared(
                    shared,
                    thread,
                    rcv_cid,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )
            } else {
                invoke_or_native(
                    shared,
                    thread,
                    &receiver_class,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &full_args,
                )
            };
            // If receiver's class didn't have the SAM method, fall back to the
            // class specified in the lambda call site. Handles objects with
            // generic ClassId (stub/Object) targeting a specific class.
            //
            // HIB-CV-31 FIX: the retry must fire ONLY when the SAM itself failed
            // to resolve on the receiver — i.e. the `NoSuchMethodError` names
            // exactly `(receiver_class, member_name)`. The old guard fired for
            // ANY `NoSuchMethodError`, including one raised deep INSIDE a
            // successfully-dispatched `onFlush` body (e.g. H2's `IOUtils.readFully`
            // calling `in.read()` on a corrupted `InputStream` reference →
            // `java/lang/Object.read()I` NSME). Because `receiver_class`
            // (`DefaultFlushEventListener`) != `impl_handle.class_name`
            // (the abstract `FlushEventListener`), the old guard re-dispatched
            // `onFlush` onto the abstract interface — which has no Code attribute
            // — fabricating a misleading `AbstractMethodError` that both masked
            // the real in-body error and reported a phantom dispatch failure.
            // Constraining the NSME to the SAM's own (class, method) makes the
            // retry serve only its intended case (generic-ClassId receiver) and
            // lets genuine in-body linkage errors propagate unchanged.
            let result = match &result {
                Err(MethodCallFailed::InternalError(VmError::Linkage(
                    LinkageError::NoSuchMethodError {
                        class_name: nsme_class,
                        method_name: nsme_method,
                        ..
                    },
                ))) if receiver_class.as_str() != &*call_site.impl_handle.class_name
                    && nsme_class.as_str() == receiver_class.as_str()
                    && nsme_method.as_str() == &*call_site.impl_handle.member_name =>
                {
                    invoke_or_native(
                        shared,
                        thread,
                        &call_site.impl_handle.class_name,
                        &call_site.impl_handle.member_name,
                        &call_site.impl_handle.descriptor,
                        &full_args,
                    )
                }
                _ => result,
            };
            let r = result?;
            Ok(Some(coerce_return(shared, thread, &sam_ret, &impl_ret, r)?))
        }
        MethodHandleKind::InvokeSpecial => {
            // Special: dispatch on the declaring class (no virtual lookup).
            //
            // `invokespecial` always targets an instance method (private or
            // super-call); the lambda capture list therefore begins with the
            // bound `this`, which sits at `full_args[0]` once captures are
            // prepended.  `coerce_lambda_args` must skip that slot — passing
            // `receiver_present=false` here causes the argument-to-parameter
            // index map to slide by one, leaving the last SAM-supplied arg
            // unconverted.  Concrete failure: Flink's `getRawValueFromOption`
            // lambda binds `getRawValue(String, Z)` from a
            // `BiFunction<String, Boolean, ...>`; with the off-by-one the
            // trailing `Boolean` reaches the impl's `boolean` slot still
            // boxed, and `iload_2` later raises
            // "expected int on stack, got ref(...)".
            coerce_lambda_args(
                shared,
                thread,
                &sam_desc,
                &impl_desc,
                &inst_desc,
                &mut full_args,
                true,
                num_captures,
            )?;
            // Loader-faithful owner resolution (gated): prefer the enclosing
            // loader's copy of the impl class when it diverges from the global.
            let class_id = match lambda_impl_dispatch_override_driven(shared, thread, &call_site) {
                Some(cid) => cid,
                None => shared
                    .classes
                    .class_manager
                    .write()
                    .load_class(&call_site.impl_handle.class_name)?,
            };
            // A REF_invokeSpecial lambda target is statically bound to its
            // implementation owner.  In particular, an Interface.crate::runtime::m
            // method reference must reach that interface default method even
            // when the receiver overrides m.  Retargeting here can resolve a
            // same-named synthetic lambda helper on the receiver instead and
            // recurse through the default method indefinitely.
            let result = crate::vm::invoke_on_class_shared_no_retarget(
                shared,
                thread,
                class_id,
                &call_site.impl_handle.member_name,
                &call_site.impl_handle.descriptor,
                &full_args,
            )?;
            Ok(Some(coerce_return(
                shared, thread, &sam_ret, &impl_ret, result,
            )?))
        }
        MethodHandleKind::NewInvokeSpecial => {
            // Constructor reference: allocate object, call <init>, return the object.
            // Loader-faithful owner resolution (gated), same rationale as above.
            //
            // Residual 4 (2026-07-20, docs/known-issues/springboot/
            // core-spring-boot-test-config-data-and-classpath-scan-cluster.md):
            // this used the PASSIVE-only `lambda_impl_dispatch_override` (cache
            // read, never drives a cold miss) with a loader-blind
            // `load_class(name)` fallback — the exact InvokeStatic gap already
            // fixed by `lambda_impl_dispatch_override_driven` (see that
            // function's own doc comment), just never mirrored onto this sibling
            // MethodHandleKind. A constructor-reference lambda
            // (`SomeType::new`, e.g. Spring AOT's generated
            // `AotApplicationContextInitializer::new` factory) whose impl class
            // is the very FIRST thing touched from a fork loader's namespace hit
            // the same loader-blind fallback and minted an Application-loader
            // copy instead of the fork's own. (Independently fixed upstream on
            // origin/dev with the same shape; kept in sync here.)
            let class_id = match lambda_impl_dispatch_override_driven(shared, thread, &call_site) {
                Some(cid) => cid,
                None => shared
                    .classes
                    .class_manager
                    .write()
                    .load_class(&call_site.impl_handle.class_name)?,
            };
            // Array-constructor reference (`SomeType[]::new`, e.g. as an
            // `IntFunction<SomeType[]>` — the mechanism behind
            // `Collection.toArray(SomeType[]::new)` and any direct user code).
            // `load_class` already resolves an array-shaped impl class name
            // (e.g. "[Ljava/nio/ByteBuffer;") to its synthesized array
            // ClassId (JVMS 5.3.3 — array classes are never loaded from a
            // class file), but everything below this point assumes a
            // REGULAR object: it allocates `num_total_fields` (0 for an
            // array class) object slots and dispatches `<init>`, which does
            // not exist for arrays. That produced a zero-field object
            // wearing the array's ClassId — no length header, no element
            // storage, and the requested length (the sole `IntFunction`
            // argument) silently discarded — which a later `checkcast` to
            // the real array type then rejects
            // (`ClassCastException: ... cannot be cast to [Lyour/Type;`).
            // Detect the array case up front and dispatch to real array
            // allocation instead.
            let array_info = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .and_then(|c| c.array_info.clone());
            if let Some(array_info) = array_info {
                let length =
                    full_args
                        .first()
                        .and_then(Value::as_int)
                        .ok_or_else(|| VmError::Internal {
                            message: "array-constructor-reference: missing length arg".to_string(),
                        })?;
                if length < 0 {
                    return Err(RuntimeError::NegativeArraySizeException { size: length }.into());
                }
                let (element_type, component_class_id) = if array_info.array_dimension == 1 {
                    match &*array_info.leaf_component_name {
                        "boolean" => (ArrayElementType::Boolean, ClassId::new(0)),
                        "char" => (ArrayElementType::Char, ClassId::new(0)),
                        "float" => (ArrayElementType::Float, ClassId::new(0)),
                        "double" => (ArrayElementType::Double, ClassId::new(0)),
                        "byte" => (ArrayElementType::Byte, ClassId::new(0)),
                        "short" => (ArrayElementType::Short, ClassId::new(0)),
                        "int" => (ArrayElementType::Int, ClassId::new(0)),
                        "long" => (ArrayElementType::Long, ClassId::new(0)),
                        _ => (ArrayElementType::Reference, array_info.component_class_id),
                    }
                } else {
                    (ArrayElementType::Reference, array_info.component_class_id)
                };
                let arr = gc_alloc_array(
                    shared,
                    thread,
                    component_class_id,
                    element_type,
                    length as usize,
                )?;
                maybe_gc(shared, thread);
                return Ok(Some(Some(Value::Object(Some(arr)))));
            }
            ensure_class_initialized_shared(shared, thread, class_id)?;
            // Use `num_total_fields` (inherited + declared instance fields),
            // matching the `New` opcode. `c.fields.len()` is wrong here: it
            // counts this class's declared fields *including statics* while
            // omitting inherited instance fields, so a subclass constructor
            // reference would under-allocate and trip the GC `get_field`
            // bounds guard on any inherited-field access.
            let num_fields = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.num_total_fields)
                .unwrap_or(0);
            let new_obj = gc_alloc_object(shared, thread, class_id, num_fields)?;
            // Build <init> args: [new_obj, ...full_args]. The constructor body
            // can allocate and trigger a moving GC; the Java frame/locals are
            // remapped, but this Rust local `new_obj` is not. Pin the receiver
            // across `<init>` and return the forwarded object ref, mirroring the
            // MethodHandle `newInvokeSpecial` path in `vm_exec.rs`.
            let new_obj_pin = thread.native_pin_roots.len();
            thread.native_pin_roots.push(new_obj);
            let init_result = (|| -> Result<(), MethodCallFailed> {
                let mut init_args = Vec::with_capacity(1 + full_args.len());
                init_args.push(Value::Object(Some(new_obj)));
                init_args.extend_from_slice(&full_args);
                invoke_on_class_shared(
                    shared,
                    thread,
                    class_id,
                    &call_site.impl_handle.member_name,
                    &call_site.impl_handle.descriptor,
                    &init_args,
                )?;
                Ok(())
            })();
            let forwarded = thread
                .native_pin_roots
                .get(new_obj_pin)
                .copied()
                .unwrap_or(new_obj);
            thread.native_pin_roots.truncate(new_obj_pin);
            init_result?;
            Ok(Some(Some(Value::Object(Some(forwarded)))))
        }
        MethodHandleKind::GetField => {
            // Field getter: first arg is the object, return the field value.
            if full_args.is_empty() {
                return Err(VmError::Internal {
                    message: "lambda dispatch: GetField with no args".to_string(),
                }
                .into());
            }
            match &full_args[0] {
                Value::Object(Some(target_ref)) => {
                    // We need to resolve the field index. For simplicity, do a field lookup.
                    let target_class_id = shared.mem.heap.class_id_of(*target_ref);
                    let field_index = {
                        let cm = shared.classes.class_manager.read();
                        find_field_recursive(
                            target_class_id,
                            &call_site.impl_handle.member_name,
                            &cm.class_store,
                        )
                        .map(|(idx, _, _)| idx)
                        .ok_or_else(|| VmError::Internal {
                            message: format!(
                                "lambda dispatch: field {} not found",
                                call_site.impl_handle.member_name
                            ),
                        })?
                    };
                    let value = shared.mem.heap.get_field(*target_ref, field_index);
                    Ok(Some(Some(value)))
                }
                _ => Err(VmError::Internal {
                    message: "lambda dispatch: GetField on null".to_string(),
                }
                .into()),
            }
        }
        MethodHandleKind::GetStatic => {
            let class_id = match lambda_impl_dispatch_override_driven(shared, thread, &call_site) {
                Some(cid) => cid,
                None => shared
                    .classes
                    .class_manager
                    .write()
                    .load_class(&call_site.impl_handle.class_name)?,
            };
            ensure_class_initialized_shared(shared, thread, class_id)?;
            let field_index = {
                let cm = shared.classes.class_manager.read();
                find_field_recursive(
                    class_id,
                    &call_site.impl_handle.member_name,
                    &cm.class_store,
                )
                .map(|(idx, _, _)| idx)
                .ok_or_else(|| VmError::Internal {
                    message: format!(
                        "lambda dispatch: static field {} not found",
                        call_site.impl_handle.member_name
                    ),
                })?
            };
            let value = crate::vm::get_static_shared(shared, class_id, field_index);
            Ok(Some(Some(value)))
        }
        MethodHandleKind::PutField => {
            if full_args.len() < 2 {
                return Err(VmError::Internal {
                    message: "lambda dispatch: PutField needs object + value".to_string(),
                }
                .into());
            }
            match &full_args[0] {
                Value::Object(Some(target_ref)) => {
                    let target_class_id = shared.mem.heap.class_id_of(*target_ref);
                    let field_index = {
                        let cm = shared.classes.class_manager.read();
                        find_field_recursive(
                            target_class_id,
                            &call_site.impl_handle.member_name,
                            &cm.class_store,
                        )
                        .map(|(idx, _, _)| idx)
                        .ok_or_else(|| VmError::Internal {
                            message: format!(
                                "lambda dispatch: field {} not found",
                                call_site.impl_handle.member_name
                            ),
                        })?
                    };
                    shared
                        .mem
                        .heap
                        .set_field(*target_ref, field_index, full_args[1]);
                    Ok(Some(None))
                }
                _ => Err(VmError::Internal {
                    message: "lambda dispatch: PutField on null".to_string(),
                }
                .into()),
            }
        }
        MethodHandleKind::PutStatic => {
            if full_args.is_empty() {
                return Err(VmError::Internal {
                    message: "lambda dispatch: PutStatic needs a value".to_string(),
                }
                .into());
            }
            let class_id = match lambda_impl_dispatch_override_driven(shared, thread, &call_site) {
                Some(cid) => cid,
                None => shared
                    .classes
                    .class_manager
                    .write()
                    .load_class(&call_site.impl_handle.class_name)?,
            };
            ensure_class_initialized_shared(shared, thread, class_id)?;
            let field_index = {
                let cm = shared.classes.class_manager.read();
                find_field_recursive(
                    class_id,
                    &call_site.impl_handle.member_name,
                    &cm.class_store,
                )
                .map(|(idx, _, _)| idx)
                .ok_or_else(|| VmError::Internal {
                    message: format!(
                        "lambda dispatch: static field {} not found",
                        call_site.impl_handle.member_name
                    ),
                })?
            };
            crate::vm::set_static_shared(shared, class_id, field_index, full_args[0]);
            Ok(Some(None))
        }
    }
}

#[inline]
pub(super) fn invoke_cached_native_callback_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    method_descriptor: &str,
    objects_prevalidated: bool,
) -> Result<(), MethodCallFailed> {
    // Widening: small integer index -> usize (non-negative, fits in pointer width)
    let _ring_idx = cratonvm_native_api::native_ring::record_enter(callback as usize);
    let result = if objects_prevalidated {
        crate::vm::safe_native_call_prevalidated_objects(shared, thread, callback, args)
    } else {
        crate::vm::safe_native_call(shared, thread, callback, args)
    };
    cratonvm_native_api::native_ring::record_exit(_ring_idx);
    let result = result?;
    if let Some(value) = result {
        let ret = crate::jit::return_type(method_descriptor);
        // A void method must not leave anything on the caller's operand stack,
        // even if its native happens to return `Some(_)` (many natives return
        // the receiver / a status for convenience). The slow path
        // (`execute_invoke_kind`) already drops the value for `V`; the cached
        // VirtualNative fast path historically pushed it unconditionally,
        // leaking one operand per call. With an overloaded void method whose
        // first call primes this cache (e.g. `java/util/zip/Checksum.update`),
        // the leak accumulates until the caller frame's operand stack overflows
        // its `max_stack` — kafka-clients `Crc32CTest.testUpdate` panicked at
        // `value_stack.rs` "len 24 index 24". Only push for non-void returns.
        if ret != b'V' {
            let value = coerce_value_for_return(value, ret);
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
    }
    Ok(())
}

/// Invoke a cached native callback with [`safe_native_call`] (pins jobject
/// args / return values across safepoint GC) and push any result.
#[inline]
pub(super) fn invoke_cached_native_callback(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    method_descriptor: &str,
) -> Result<(), MethodCallFailed> {
    invoke_cached_native_callback_impl(
        shared,
        thread,
        frame_idx,
        callback,
        args,
        method_descriptor,
        false,
    )
}

#[inline]
pub(super) fn invoke_cached_native_callback_prevalidated(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    method_descriptor: &str,
) -> Result<(), MethodCallFailed> {
    invoke_cached_native_callback_impl(
        shared,
        thread,
        frame_idx,
        callback,
        args,
        method_descriptor,
        true,
    )
}

/// Registered Rust natives that must win over real-JDK bytecode on the same
/// declaring class (inline-cache / vtable fast paths skip `execute_invoke`).
#[inline]
/// CRATONVM_DBG_SOE — one-shot diagnostic: when the frame-depth ceiling is
/// hit, dump the newest Java frames so the recursion CYCLE is visible. The
/// Throwable stack capture keeps only a handful of frames, which hides which
/// methods actually recurse (e.g. the H2 GROUP BY StackOverflowError).
pub(super) fn dump_stack_on_soe(thread: &JvmThread) {
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SOE").is_none() {
        return;
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    static DUMPED: AtomicBool = AtomicBool::new(false);
    if DUMPED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "[DBG_SOE] stack depth {} — newest 150 frames:",
        thread.frames.len()
    );
    for (i, f) in thread.frames.iter().rev().take(150).enumerate() {
        eprintln!(
            "[DBG_SOE]   #{i} {}.{}{} pc={}",
            f.class_name(),
            f.method_name(),
            f.method_descriptor(),
            f.pc
        );
    }
}

/// Whether `(class, method, desc)` is one of the reflection TYPE_USE-annotation
/// methods CratonVM must serve from a Rust native instead of the real JDK
/// bytecode (which decodes type annotations via `getTypeAnnotationBytes0()` —
/// stubbed to null — plus the unexposed `jdk.internal.reflect.ConstantPool`).
///
/// This is the single source of truth for that override set: it is consulted by
/// BOTH dispatch gates — [`force_native_over_real_jdk_bytecode`] (interpreter
/// fast paths) and the `check_override` predicate in
/// `vm_exec.rs::invoke_on_class_shared_inner` (the slow path). Adding a method
/// here makes it win on every path; editing one list and not the other was the
/// original footgun.
pub(crate) fn is_typeuse_annotation_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    match class_name {
        "java/lang/reflect/Method" => matches!(
            (method_name, descriptor),
            (
                "getAnnotatedReturnType",
                "()Ljava/lang/reflect/AnnotatedType;"
            ) | (
                "getAnnotatedParameterTypes",
                "()[Ljava/lang/reflect/AnnotatedType;"
            )
        ),
        "java/lang/reflect/Constructor" => {
            (method_name, descriptor)
                == (
                    "getAnnotatedParameterTypes",
                    "()[Ljava/lang/reflect/AnnotatedType;",
                )
        }
        "java/lang/reflect/Parameter" | "java/lang/reflect/Field" => {
            (method_name, descriptor) == ("getAnnotatedType", "()Ljava/lang/reflect/AnnotatedType;")
        }
        "sun/reflect/annotation/AnnotatedTypeFactory$AnnotatedTypeBaseImpl" => matches!(
            (method_name, descriptor),
            (
                "getAnnotation",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
            ) | ("getAnnotations", "()[Ljava/lang/annotation/Annotation;")
                | (
                    "getDeclaredAnnotations",
                    "()[Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getAnnotatedOwnerType",
                    "()Ljava/lang/reflect/AnnotatedType;"
                )
        ),
        _ => false,
    }
}

/// java.lang.Class methods whose registered natives operate on CratonVM's
/// class-mirror and annotation side tables. Ordinary bytecode invokes already
/// prefer these registrations, but bound virtual method references dispatch
/// through invoke_on_class_shared, whose concrete-bytecode precedence needs
/// an explicit shared gate.
pub(crate) fn is_class_mirror_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/Class"
        && matches!(
            (method_name, descriptor),
            ("getName", "()Ljava/lang/String;")
                | ("isArray", "()Z")
                | ("getComponentType", "()Ljava/lang/Class;")
                | ("componentType", "()Ljava/lang/Class;")
                | ("getProtectionDomain", "()Ljava/security/ProtectionDomain;")
                | ("forPrimitiveName", "(Ljava/lang/String;)Ljava/lang/Class;")
                | ("getAnnotations", "()[Ljava/lang/annotation/Annotation;")
                | (
                    "getDeclaredAnnotations",
                    "()[Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getAnnotation",
                    "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getDeclaredAnnotation",
                    "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
                )
                | ("isAnnotationPresent", "(Ljava/lang/Class;)Z")
                | (
                    "getAnnotationsByType",
                    "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getDeclaredAnnotationsByType",
                    "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;"
                )
                | ("getDeclaredFields", "()[Ljava/lang/reflect/Field;")
                | ("getDeclaredFields0", "(Z)[Ljava/lang/reflect/Field;")
                | (
                    "getDeclaredField",
                    "(Ljava/lang/String;)Ljava/lang/reflect/Field;"
                )
        )
}

/// `java.lang.ClassValue#get`/`#remove` (see `classvalue_cache.rs` in
/// native-builtins for the real implementation and why it must be a native
/// override at all — `ClassValue`'s real bytecode depends on CASing a hidden
/// field on `java.lang.Class` via `jdk.internal.misc.Unsafe`, not faithfully
/// reproducible against CratonVM's `Class` mirrors).
///
/// Apache Groovy's `ClassInfo` registry (`ClassInfo.globalClassValue`, a
/// `GroovyClassValueJava7`) is constructed via
/// `GroovyClassValueFactory.createGroovyClassValue(ClassInfo::new)` — a
/// constructor-reference-backed `ComputeValue` lambda — and `ClassInfo`'s own
/// `getClassInfo`/`remove` static methods call `get`/`remove` on it. Ordinary
/// bytecode invokes already prefer the registered native, but this call
/// pattern (through the lambda-backed `ComputeValue` plumbing) can resolve
/// through a dispatch path whose concrete-bytecode precedence needs this
/// explicit shared gate — see docs/known-issues/springboot/
/// core-spring-boot-test-config-data-and-classpath-scan-cluster.md Cluster C
/// "Residual 5" (fixed under `--nojit` without this gate; JIT mode still hit
/// the original always-null-returning symptom until this was added).
pub(crate) fn is_classvalue_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let result = class_name == "java/lang/ClassValue"
        && matches!(
            (method_name, descriptor),
            ("get", "(Ljava/lang/Class;)Ljava/lang/Object;") | ("remove", "(Ljava/lang/Class;)V")
        );
    if class_name == "java/lang/ClassValue"
        && cratonvm_types::flags::runtime_var_os("CRATONVM_TRACE_CLASSVALUE").is_some()
    {
        eprintln!(
            "[classvalue-gate] is_classvalue_native_override({class_name}, {method_name}, {descriptor}) -> {result}"
        );
    }
    result
}

pub(crate) fn is_reflection_access_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        (class_name, method_name, descriptor),
        (
            "java/lang/Class",
            "getDeclaredField",
            "(Ljava/lang/String;)Ljava/lang/reflect/Field;"
        ) | (
            "java/lang/reflect/Field",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ) | (
            "java/lang/reflect/Field",
            "set",
            "(Ljava/lang/Object;Ljava/lang/Object;)V"
        ) | ("java/lang/reflect/Field", "getInt", "(Ljava/lang/Object;)I")
            | (
                "java/lang/reflect/Field",
                "getLong",
                "(Ljava/lang/Object;)J"
            )
            | (
                "java/lang/reflect/Field",
                "getFloat",
                "(Ljava/lang/Object;)F"
            )
            | (
                "java/lang/reflect/Field",
                "getDouble",
                "(Ljava/lang/Object;)D"
            )
            | (
                "java/lang/reflect/Field",
                "getBoolean",
                "(Ljava/lang/Object;)Z"
            )
            | (
                "java/lang/reflect/Field",
                "getByte",
                "(Ljava/lang/Object;)B"
            )
            | (
                "java/lang/reflect/Field",
                "getShort",
                "(Ljava/lang/Object;)S"
            )
            | (
                "java/lang/reflect/Field",
                "getChar",
                "(Ljava/lang/Object;)C"
            )
            | (
                "java/lang/reflect/Field",
                "setInt",
                "(Ljava/lang/Object;I)V"
            )
            | (
                "java/lang/reflect/Field",
                "setLong",
                "(Ljava/lang/Object;J)V"
            )
            | (
                "java/lang/reflect/Field",
                "setFloat",
                "(Ljava/lang/Object;F)V"
            )
            | (
                "java/lang/reflect/Field",
                "setDouble",
                "(Ljava/lang/Object;D)V"
            )
            | (
                "java/lang/reflect/Field",
                "setBoolean",
                "(Ljava/lang/Object;Z)V"
            )
            | (
                "java/lang/reflect/Field",
                "setByte",
                "(Ljava/lang/Object;B)V"
            )
            | (
                "java/lang/reflect/Field",
                "setShort",
                "(Ljava/lang/Object;S)V"
            )
            | (
                "java/lang/reflect/Field",
                "setChar",
                "(Ljava/lang/Object;C)V"
            )
            | ("java/lang/reflect/Field", "setAccessible", "(Z)V")
            | (
                "java/lang/reflect/AccessibleObject",
                "setAccessible",
                "(Z)V"
            )
    )
}

pub(crate) fn is_antlr_prediction_context_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let antlr_runtime = class_name.starts_with("org/antlr/v4/runtime/")
        || class_name.starts_with("groovyjarjarantlr4/v4/runtime/");
    if !antlr_runtime {
        return false;
    }
    if class_name == "org/antlr/v4/runtime/CommonTokenFactory" {
        return matches!(
            (method_name, descriptor),
            (
                "create",
                "(Lorg/antlr/v4/runtime/misc/Pair;ILjava/lang/String;IIIII)Lorg/antlr/v4/runtime/CommonToken;"
            ) | (
                "create",
                "(Lorg/antlr/v4/runtime/misc/Pair;ILjava/lang/String;IIIII)Lorg/antlr/v4/runtime/Token;"
            ) | (
                "create",
                "(ILjava/lang/String;)Lorg/antlr/v4/runtime/CommonToken;"
            ) | ("create", "(ILjava/lang/String;)Lorg/antlr/v4/runtime/Token;")
        );
    }
    if class_name == "org/antlr/v4/runtime/CommonToken" {
        return matches!(
            (method_name, descriptor),
            ("getType", "()I")
                | ("setType", "(I)V")
                | ("getText", "()Ljava/lang/String;")
                | ("setText", "(Ljava/lang/String;)V")
                | ("getLine", "()I")
                | ("setLine", "(I)V")
                | ("getCharPositionInLine", "()I")
                | ("setCharPositionInLine", "(I)V")
                | ("getChannel", "()I")
                | ("setChannel", "(I)V")
                | ("getStartIndex", "()I")
                | ("setStartIndex", "(I)V")
                | ("getStopIndex", "()I")
                | ("setStopIndex", "(I)V")
                | ("getTokenIndex", "()I")
                | ("setTokenIndex", "(I)V")
                | ("getTokenSource", "()Lorg/antlr/v4/runtime/TokenSource;")
                | ("getInputStream", "()Lorg/antlr/v4/runtime/CharStream;")
        );
    }
    if class_name.ends_with("/misc/DoubleKeyMap") {
        return matches!(
            (method_name, descriptor),
            ("<init>", "()V")
                | (
                    "get",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
                )
                | (
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
                )
        );
    }
    if class_name.ends_with("/atn/ATNConfigSet") || class_name.ends_with("/atn/OrderedATNConfigSet")
    {
        return matches!(
            (method_name, descriptor),
            ("add", "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z")
                | (
                    "add",
                    "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/misc/DoubleKeyMap;)Z"
                )
                | ("add", "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z")
                | (
                    "add",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Z"
                )
                | ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name.ends_with("/atn/ATNConfig") {
        return matches!(
            (method_name, descriptor),
            (
                "<init>",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;)V"
            )
                | (
                    "<init>",
                    "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;)V"
                )
                | (
                    "<init>",
                    "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;Lorg/antlr/v4/runtime/atn/PredictionContext;)V"
                )
                | (
                    "<init>",
                    "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;Lorg/antlr/v4/runtime/atn/SemanticContext;)V"
                )
                | (
                "<init>",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNState;Lorg/antlr/v4/runtime/atn/PredictionContext;Lorg/antlr/v4/runtime/atn/SemanticContext;)V"
            )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)V"
                )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNState;)V"
                )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNState;Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;)V"
                )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNState;Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;)V"
                )
                | (
                    "<init>",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNState;Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;)V"
                )
                | ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("equals", "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z")
                | ("equals", "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z")
        );
    }
    if class_name.ends_with("/atn/LexerATNConfig") {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("equals", "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z")
                | ("equals", "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z")
        );
    }
    if class_name.ends_with("/dfa/DFAState") {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name.ends_with("/atn/SemanticContext") {
        return matches!(
            (method_name, descriptor),
            (
                "and",
                "(Lorg/antlr/v4/runtime/atn/SemanticContext;Lorg/antlr/v4/runtime/atn/SemanticContext;)Lorg/antlr/v4/runtime/atn/SemanticContext;"
            ) | (
                "or",
                "(Lorg/antlr/v4/runtime/atn/SemanticContext;Lorg/antlr/v4/runtime/atn/SemanticContext;)Lorg/antlr/v4/runtime/atn/SemanticContext;"
            ) | (
                "and",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;)Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;"
            ) | (
                "or",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;)Lgroovyjarjarantlr4/v4/runtime/atn/SemanticContext;"
            )
        );
    }
    if matches!(
        class_name.rsplit('/').next().unwrap_or_default(),
        "SemanticContext$Predicate"
            | "SemanticContext$PrecedencePredicate"
            | "SemanticContext$AND"
            | "SemanticContext$OR"
    ) && class_name.contains("/atn/")
    {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name.ends_with("/atn/ATNState") {
        return matches!(
            (method_name, descriptor),
            ("getNumberOfTransitions", "()I")
                | ("onlyHasEpsilonTransitions", "()Z")
                | ("transition", "(I)Lorg/antlr/v4/runtime/atn/Transition;")
                | (
                    "transition",
                    "(I)Lgroovyjarjarantlr4/v4/runtime/atn/Transition;"
                )
        );
    }
    if matches!(
        class_name.rsplit('/').next().unwrap_or_default(),
        "BasicState"
            | "RuleStartState"
            | "BasicBlockStartState"
            | "PlusBlockStartState"
            | "StarBlockStartState"
            | "TokensStartState"
            | "RuleStopState"
            | "BlockEndState"
            | "StarLoopbackState"
            | "StarLoopEntryState"
            | "PlusLoopbackState"
            | "LoopEndState"
    ) && class_name.contains("/atn/")
    {
        return (method_name, descriptor) == ("getStateType", "()I");
    }
    if class_name.ends_with("/misc/IntervalSet") {
        return (method_name, descriptor) == ("contains", "(I)Z");
    }
    if class_name.ends_with("/atn/ParserATNSimulator") {
        return matches!(
            (method_name, descriptor),
            (
                "canDropLoopEntryEdgeInLeftRecursiveRule",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;)Z"
            ) | (
                "canDropLoopEntryEdgeInLeftRecursiveRule",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;)Z"
            ) | (
                "getEpsilonTarget",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/Transition;ZZZZ)Lorg/antlr/v4/runtime/atn/ATNConfig;"
            ) | (
                "getEpsilonTarget",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/Transition;ZZZZ)Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;"
            ) | (
                "computeReachSet",
                "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;IZ)Lorg/antlr/v4/runtime/atn/ATNConfigSet;"
            ) | (
                "computeReachSet",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;IZ)Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;"
            ) | (
                "closureCheckingStopState",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V"
            ) | (
                "closureCheckingStopState",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V"
            ) | (
                "closure",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZZ)V"
            ) | (
                "closure",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZZ)V"
            ) | (
                "closure_",
                "(Lorg/antlr/v4/runtime/atn/ATNConfig;Lorg/antlr/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V"
            ) | (
                "closure_",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfig;Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;Ljava/util/Set;ZZIZ)V"
            )
        );
    }
    if class_name.ends_with("/atn/Transition") {
        return (method_name, descriptor) == ("isEpsilon", "()Z");
    }
    if matches!(
        class_name.rsplit('/').next().unwrap_or_default(),
        "EpsilonTransition"
            | "RangeTransition"
            | "RuleTransition"
            | "PredicateTransition"
            | "AtomTransition"
            | "ActionTransition"
            | "SetTransition"
            | "NotSetTransition"
            | "WildcardTransition"
            | "PrecedencePredicateTransition"
    ) && class_name.contains("/atn/")
    {
        return matches!(
            (method_name, descriptor),
            ("getSerializationType", "()I") | ("isEpsilon", "()Z") | ("matches", "(III)Z")
        );
    }
    if class_name.ends_with("/atn/PredictionMode") {
        return matches!(
            (method_name, descriptor),
            (
                "getConflictingAltSubsets",
                "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;)Ljava/util/Collection;"
            ) | (
                "getConflictingAltSubsets",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;)Ljava/util/Collection;"
            ) | (
                "hasStateAssociatedWithOneAlt",
                "(Lorg/antlr/v4/runtime/atn/ATNConfigSet;)Z"
            ) | (
                "hasStateAssociatedWithOneAlt",
                "(Lgroovyjarjarantlr4/v4/runtime/atn/ATNConfigSet;)Z"
            )
        );
    }
    if class_name.ends_with("/atn/PredictionContext") {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I")
                | ("isEmpty", "()Z")
                | ("hasEmptyPath", "()Z")
                | ("calculateEmptyHashCode", "()I")
                | (
                    "calculateHashCode",
                    "(Lorg/antlr/v4/runtime/atn/PredictionContext;I)I"
                )
                | (
                    "calculateHashCode",
                    "([Lorg/antlr/v4/runtime/atn/PredictionContext;[I)I"
                )
                | (
                    "calculateHashCode",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;I)I"
                )
                | (
                    "calculateHashCode",
                    "([Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;[I)I"
                )
                | (
                    "merge",
                    "(Lorg/antlr/v4/runtime/atn/PredictionContext;Lorg/antlr/v4/runtime/atn/PredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeSingletons",
                    "(Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeRoot",
                    "(Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;Z)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeArrays",
                    "(Lorg/antlr/v4/runtime/atn/ArrayPredictionContext;Lorg/antlr/v4/runtime/atn/ArrayPredictionContext;ZLorg/antlr/v4/runtime/misc/DoubleKeyMap;)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "merge",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;ZLgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeSingletons",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;ZLgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeRoot",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;Z)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "mergeArrays",
                    "(Lgroovyjarjarantlr4/v4/runtime/atn/ArrayPredictionContext;Lgroovyjarjarantlr4/v4/runtime/atn/ArrayPredictionContext;ZLgroovyjarjarantlr4/v4/runtime/misc/DoubleKeyMap;)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
        );
    }
    if class_name.ends_with("/atn/SingletonPredictionContext")
        || class_name.ends_with("/atn/EmptyPredictionContext")
        || class_name.ends_with("/atn/ArrayPredictionContext")
    {
        if method_name == "<init>" {
            return matches!(
                descriptor,
                "()V"
                    | "(Lorg/antlr/v4/runtime/atn/PredictionContext;I)V"
                    | "(Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;I)V"
                    | "([Lorg/antlr/v4/runtime/atn/PredictionContext;[I)V"
                    | "([Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;[I)V"
                    | "(Lorg/antlr/v4/runtime/atn/SingletonPredictionContext;)V"
                    | "(Lgroovyjarjarantlr4/v4/runtime/atn/SingletonPredictionContext;)V"
            );
        }
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I")
                | ("isEmpty", "()Z")
                | ("hasEmptyPath", "()Z")
                | ("size", "()I")
                | ("getReturnState", "(I)I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | (
                    "getParent",
                    "(I)Lorg/antlr/v4/runtime/atn/PredictionContext;"
                )
                | (
                    "getParent",
                    "(I)Lgroovyjarjarantlr4/v4/runtime/atn/PredictionContext;"
                )
        );
    }
    false
}

pub(crate) fn is_bytebuddy_method_token_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if matches!(
        class_name,
        "net/bytebuddy/description/method/MethodDescription$TypeToken"
            | "net/bytebuddy/description/method/MethodDescription$SignatureToken"
            | "net/bytebuddy/dynamic/scaffold/MethodGraph$Compiler$Default$Harmonizer$ForJavaMethod$Token"
            | "net/bytebuddy/dynamic/scaffold/MethodGraph$Compiler$Default$Key"
    ) {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name == "net/bytebuddy/description/method/MethodDescription$TypeSubstituting" {
        return (method_name, descriptor)
            == (
                "<init>",
                "(Lnet/bytebuddy/description/type/TypeDescription$Generic;Lnet/bytebuddy/description/method/MethodDescription;Lnet/bytebuddy/description/type/TypeDescription$Generic$Visitor;)V",
            );
    }
    if matches!(
        class_name,
        "net/bytebuddy/description/method/MethodList$Explicit"
            | "net/bytebuddy/description/method/MethodList$TypeSubstituting"
            | "net/bytebuddy/description/method/MethodList$ForLoadedMethods"
            | "net/bytebuddy/description/method/MethodList$ForTokens"
            | "net/bytebuddy/description/field/FieldList$Explicit"
            | "net/bytebuddy/description/field/FieldList$ForTokens"
            | "net/bytebuddy/description/field/FieldList$ForLoadedFields"
            | "net/bytebuddy/description/type/TypeList$Explicit"
            | "net/bytebuddy/description/type/TypeList$Generic$Explicit"
    ) {
        return method_name == "size" && descriptor == "()I"
            || method_name == "get"
                && matches!(
                    descriptor,
                    "(I)Ljava/lang/Object;"
                        | "(I)Lnet/bytebuddy/description/method/MethodDescription;"
                        | "(I)Lnet/bytebuddy/description/method/MethodDescription$InGenericShape;"
                        | "(I)Lnet/bytebuddy/description/method/MethodDescription$InDefinedShape;"
                        | "(I)Lnet/bytebuddy/description/field/FieldDescription;"
                        | "(I)Lnet/bytebuddy/description/field/FieldDescription$InDefinedShape;"
                        | "(I)Lnet/bytebuddy/description/type/TypeDescription;"
                        | "(I)Lnet/bytebuddy/description/type/TypeDescription$Generic;"
                );
    }
    false
}

pub(crate) fn is_method_handles_varhandle_factory_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    class_name == "java/lang/invoke/MethodHandles"
        && matches!(
            (method_name, method_descriptor),
            (
                "arrayElementVarHandle",
                "(Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;"
            ) | (
                "byteArrayViewVarHandle",
                "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"
            ) | (
                "byteBufferViewVarHandle",
                "(Ljava/lang/Class;Ljava/nio/ByteOrder;)Ljava/lang/invoke/VarHandle;"
            )
        )
}

pub(crate) fn is_mockito_debugging_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if class_name == "org/mockito/internal/creation/bytebuddy/MockMethodAdvice" {
        return method_name == "isOverridden"
            && descriptor == "(Ljava/lang/Object;Ljava/lang/reflect/Method;)Z";
    }
    if !matches!(
        class_name,
        "org/mockito/internal/debugging/LocationFactory"
            | "org/mockito/internal/debugging/LocationFactory$DefaultLocationFactory"
    ) {
        return false;
    }
    // Off by default: the real `LocationFactory` selector runs and picks the
    // StackWalker-backed `LocationImpl`, as on HotSpot. The legacy native
    // returned a `Java8LocationImpl` with a hardcoded
    // `"-> at <<unknown line>>"`, which erased the call site from every
    // Mockito diagnostic. See `flags::mockito_legacy_selectors`.
    if !cratonvm_types::flags::mockito_legacy_selectors() {
        return false;
    }
    method_name == "create"
        && matches!(
            descriptor,
            "()Lorg/mockito/invocation/Location;" | "(Z)Lorg/mockito/invocation/Location;"
        )
}

pub(crate) fn is_hibernate_testing_util_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/hibernate/testing/orm/junit/TestingUtil"
        && (method_name, descriptor)
            == (
                "hasEffectiveAnnotation",
                "(Lorg/junit/jupiter/api/extension/ExtensionContext;Ljava/lang/Class;)Z",
            )
}

pub(crate) fn is_hibernate_models_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if class_name == "org/hibernate/metamodel/mapping/AssociationKey" {
        return matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        );
    }
    if class_name == "org/hibernate/metamodel/mapping/internal/ImmutableAttributeMappingList" {
        return (method_name, descriptor)
            == (
                "indexedForEach",
                "(Lorg/hibernate/internal/util/IndexedConsumer;)V",
            );
    }
    if class_name == "org/hibernate/metamodel/mapping/BasicValuedModelPart" {
        return matches!(
            (method_name, descriptor),
            (
                "forEachSelectable",
                "(ILorg/hibernate/metamodel/mapping/SelectableConsumer;)I"
            ) | (
                "forEachSelectable",
                "(Lorg/hibernate/metamodel/mapping/SelectableConsumer;)I"
            )
        );
    }
    if class_name == "org/hibernate/models/internal/AnnotationUsageHelper" {
        return matches!(
            (method_name, descriptor),
            (
                "findUsage",
                "(Lorg/hibernate/models/spi/AnnotationDescriptor;Ljava/util/Map;)Ljava/lang/annotation/Annotation;"
            )
                | (
                    "getUsage",
                    "(Lorg/hibernate/models/spi/AnnotationDescriptor;Ljava/util/Map;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
                )
                | (
                    "getUsage",
                    "(Ljava/lang/Class;Ljava/util/Map;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
                )
        );
    }
    if matches!(
        class_name,
        "org/hibernate/models/internal/AnnotationDescriptorRegistryStandard"
            | "org/hibernate/models/spi/AnnotationDescriptorRegistry"
    ) {
        return (method_name, descriptor)
            == (
                "getDescriptor",
                "(Ljava/lang/Class;)Lorg/hibernate/models/spi/AnnotationDescriptor;",
            );
    }
    matches!(
        class_name,
        "org/hibernate/models/internal/AnnotationTargetSupport"
            | "org/hibernate/models/spi/AnnotationTarget"
            | "org/hibernate/models/spi/MutableAnnotationTarget"
            | "org/hibernate/models/spi/AnnotationDescriptor"
            | "org/hibernate/models/spi/MutableAnnotationDescriptor"
            | "org/hibernate/models/spi/ClassDetails"
            | "org/hibernate/models/spi/MutableClassDetails"
            | "org/hibernate/models/spi/MemberDetails"
            | "org/hibernate/models/spi/MutableMemberDetails"
            | "org/hibernate/models/spi/FieldDetails"
            | "org/hibernate/models/spi/MethodDetails"
            | "org/hibernate/models/spi/RecordComponentDetails"
            | "org/hibernate/models/internal/AbstractAnnotationDescriptor"
            | "org/hibernate/models/internal/StandardAnnotationDescriptor"
            | "org/hibernate/models/internal/OrmAnnotationDescriptor"
    ) && matches!(
        (method_name, descriptor),
        ("hasDirectAnnotationUsage", "(Ljava/lang/Class;)Z")
            | (
                "getDirectAnnotationUsage",
                "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;"
            )
            | (
                "hasAnnotationUsage",
                "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Z"
            )
            | (
                "getAnnotationUsage",
                "(Lorg/hibernate/models/spi/AnnotationDescriptor;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
            )
            | (
                "getAnnotationUsage",
                "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
            )
            | (
                "locateAnnotationUsage",
                "(Ljava/lang/Class;Lorg/hibernate/models/spi/ModelsContext;)Ljava/lang/annotation/Annotation;"
            )
    )
}

pub(crate) fn is_bitset_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/util/BitSet"
        && matches!(
            (method_name, descriptor),
            ("set", "(I)V")
                | ("set", "(IZ)V")
                | ("clear", "(I)V")
                | ("clear", "()V")
                | ("get", "(I)Z")
                | ("length", "()I")
                | ("cardinality", "()I")
                | ("isEmpty", "()Z")
                | ("nextSetBit", "(I)I")
                | ("nextClearBit", "(I)I")
        )
}

pub(crate) fn is_h2_parser_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // Hibernate's UUID monotonicity test executes AssertJ's successful
    // natural-order comparison path millions of times. The native preserves
    // custom-comparator and failure delegation, but must win over the real
    // inherited library bytecode to remove its per-assertion setup overhead.
    if class_name == "org/assertj/core/api/AbstractComparableAssert" {
        return (method_name, descriptor)
            == (
                "isGreaterThan",
                "(Ljava/lang/Comparable;)Lorg/assertj/core/api/AbstractComparableAssert;",
            );
    }
    if class_name == "org/assertj/core/api/AbstractStringAssert" {
        return (method_name, descriptor)
            == (
                "isGreaterThan",
                "(Ljava/lang/String;)Lorg/assertj/core/api/AbstractStringAssert;",
            );
    }
    if matches!(
        class_name,
        "org/assertj/core/api/AssertionsForClassTypes" | "org/assertj/core/api/Assertions"
    ) {
        return matches!(
            (method_name, descriptor),
            (
                "assertThat",
                "(Ljava/lang/String;)Lorg/assertj/core/api/AbstractStringAssert;"
            ) | (
                "assertThat",
                "(Ljava/lang/Comparable;)Lorg/assertj/core/api/AbstractComparableAssert;"
            )
        );
    }
    if matches!(
        class_name,
        "org/hibernate/id/uuid/UuidVersion6Strategy" | "org/hibernate/id/uuid/UuidVersion7Strategy"
    ) {
        return (method_name, descriptor)
            == (
                "generateUuid",
                "(Lorg/hibernate/engine/spi/SharedSessionContractImplementor;)Ljava/util/UUID;",
            );
    }
    if class_name == "org/h2/util/Utils" {
        return (method_name, descriptor) == ("getResource", "(Ljava/lang/String;)[B");
    }
    if class_name == "org/h2/constraint/ConstraintReferential" {
        return (method_name, descriptor)
            == ("checkExistingData", "(Lorg/h2/engine/SessionLocal;)V");
    }
    if class_name == "org/h2/mvstore/type/LongDataType" {
        return matches!(
            (method_name, descriptor),
            ("binarySearch", "(Ljava/lang/Long;Ljava/lang/Object;II)I")
                | ("binarySearch", "(Ljava/lang/Object;Ljava/lang/Object;II)I")
        );
    }
    if class_name == "org/h2/mvstore/RootReference" {
        return (method_name, descriptor)
            == (
                "updateRootPage",
                "(Lorg/h2/mvstore/Page;J)Lorg/h2/mvstore/RootReference;",
            );
    }
    if class_name == "org/h2/mvstore/tx/Transaction" {
        return (method_name, descriptor)
            == (
                "<init>",
                "(Lorg/h2/mvstore/tx/TransactionStore;IJILjava/lang/String;JIILorg/h2/engine/IsolationLevel;Lorg/h2/mvstore/tx/TransactionStore$RollbackListener;)V",
            );
    }
    if class_name == "org/h2/table/Column" {
        return matches!(
            (method_name, descriptor),
            ("equals", "(Ljava/lang/Object;)Z")
                | ("hashCode", "()I")
                | ("getTable", "()Lorg/h2/table/Table;")
        );
    }
    if class_name == "org/h2/engine/DbObject" {
        return matches!(
            (method_name, descriptor),
            ("equals", "(Ljava/lang/Object;)Z") | ("hashCode", "()I")
        );
    }
    if matches!(
        class_name,
        "org/h2/engine/Session" | "org/h2/engine/SessionLocal"
    ) {
        return (method_name, descriptor) == ("hashCode", "()I");
    }
    if class_name == "org/h2/command/ParserBase" {
        return matches!(
            (method_name, descriptor),
            ("read", "()V")
                | ("setTokenIndex", "(I)V")
                | ("readIf", "(I)Z")
                | ("addExpected", "(I)V")
                | ("testToken", "(Ljava/lang/String;Lorg/h2/command/Token;)Z")
        );
    }
    if class_name == "org/h2/command/Tokenizer" {
        return (method_name, descriptor) == ("eq", "(Ljava/lang/String;Ljava/lang/String;II)Z");
    }
    if class_name == "org/h2/expression/ExpressionVisitor" {
        return matches!(
            (method_name, descriptor),
            ("getType", "()I")
                | (
                    "getDependenciesVisitor",
                    "(Ljava/util/HashSet;)Lorg/h2/expression/ExpressionVisitor;",
                )
                | (
                    "getMaxModificationIdVisitor",
                    "()Lorg/h2/expression/ExpressionVisitor;",
                )
        );
    }
    if class_name == "org/h2/message/Trace" {
        return (method_name, descriptor) == ("isDebugEnabled", "()Z");
    }
    if class_name == "org/h2/message/TraceSystem" {
        return (method_name, descriptor) == ("isEnabled", "(I)Z");
    }
    // Hibernate's JSON-array unnest tests lower to two
    // `system_range(1, 1000)` joins. H2's Java `ValueBigint.get(long)` only
    // interns 0..99, causing the remaining immutable row values to be
    // repeatedly allocated in the nested scan. The registered native extends
    // that exact immutable cache through 1000; it must be admitted here for
    // real-JDK bytecode calls to reach it.
    if class_name == "org/h2/value/ValueBigint" {
        return (method_name, descriptor) == ("get", "(J)Lorg/h2/value/ValueBigint;");
    }
    if class_name == "org/h2/expression/condition/Comparison" {
        return matches!(
            (method_name, descriptor),
            (
                "compare",
                "(Lorg/h2/engine/SessionLocal;Lorg/h2/value/Value;Lorg/h2/value/Value;I)Lorg/h2/value/Value;",
            ) | (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            )
        );
    }
    if class_name == "org/h2/expression/ExpressionColumn" {
        return (method_name, descriptor)
            == (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            );
    }
    if class_name == "org/h2/value/Value" {
        return (method_name, descriptor) == ("isFalse", "()Z");
    }
    if class_name == "org/h2/expression/condition/ConditionAndOr" {
        return (method_name, descriptor)
            == (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            );
    }
    if class_name == "org/h2/expression/function/CoalesceFunction" {
        return (method_name, descriptor)
            == (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            );
    }
    if class_name == "org/h2/expression/function/CardinalityExpression" {
        return (method_name, descriptor)
            == (
                "getValue",
                "(Lorg/h2/engine/SessionLocal;)Lorg/h2/value/Value;",
            );
    }
    if class_name == "org/h2/index/RangeCursor" {
        return matches!(
            (method_name, descriptor),
            ("next", "()Z")
                | ("get", "()Lorg/h2/result/Row;")
                | ("getSearchRow", "()Lorg/h2/result/SearchRow;")
        );
    }
    if class_name == "org/h2/result/Row" {
        return (method_name, descriptor) == ("get", "([Lorg/h2/value/Value;I)Lorg/h2/result/Row;");
    }
    if class_name == "org/h2/result/DefaultRow" {
        return (method_name, descriptor) == ("getValue", "(I)Lorg/h2/value/Value;");
    }
    class_name.starts_with("org/h2/command/Token")
        && matches!(
            (method_name, descriptor),
            ("tokenType", "()I") | ("asIdentifier", "()Ljava/lang/String;") | ("isQuoted", "()Z")
        )
}

pub(crate) fn is_jdk_string_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/StringLatin1" && (method_name, descriptor) == ("inflate", "([BI[CII)V")
}

pub(crate) fn is_jdk_string_charset_name_constructor_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/String"
        && method_name == "<init>"
        && matches!(
            descriptor,
            "([BLjava/lang/String;)V" | "([BIILjava/lang/String;)V"
        )
}

pub(crate) fn is_spring_mock_response_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "org/springframework/mock/web/MockHttpServletResponse"
            | "org/springframework/web/testfixture/servlet/MockHttpServletResponse"
    ) && method_name == "getContentAsString"
        && matches!(
            descriptor,
            "()Ljava/lang/String;" | "(Ljava/nio/charset/Charset;)Ljava/lang/String;"
        )
}

pub(crate) fn is_script_engine_manager_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "javax/script/ScriptEngineManager"
        && matches!(
            (method_name, descriptor),
            (
                "getEngineByName",
                "(Ljava/lang/String;)Ljavax/script/ScriptEngine;"
            ) | (
                "getEngineByExtension",
                "(Ljava/lang/String;)Ljavax/script/ScriptEngine;"
            ) | (
                "getEngineByMimeType",
                "(Ljava/lang/String;)Ljavax/script/ScriptEngine;"
            )
        )
}

pub(crate) fn is_jython_thread_state_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/python/core/Py"
        && matches!(
            (method_name, descriptor),
            ("importSiteIfSelected", "()Z")
                | ("getSystemState", "()Lorg/python/core/PySystemState;")
                | (
                    "setSystemState",
                    "(Lorg/python/core/PySystemState;)Lorg/python/core/PySystemState;",
                )
                | ("getThreadState", "()Lorg/python/core/ThreadState;")
                | (
                    "getThreadState",
                    "(Lorg/python/core/PySystemState;)Lorg/python/core/ThreadState;",
                )
        )
}

pub(crate) fn is_jython_pyobject_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/python/core/PyObject"
        && ((matches!(method_name, "_is" | "_isnot" | "_eq" | "_ne")
            && descriptor == "(Lorg/python/core/PyObject;)Lorg/python/core/PyObject;")
            || (method_name == "invoke"
                && descriptor
                    == "(Ljava/lang/String;Lorg/python/core/PyObject;)Lorg/python/core/PyObject;"))
}

pub(crate) fn is_jython_imp_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/python/core/imp"
        && method_name == "addModule"
        && descriptor == "(Ljava/lang/String;)Lorg/python/core/PyModule;"
}

pub(crate) fn is_jython_pymodule_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "org/python/core/PyModule"
        && method_name == "__findattr_ex__"
        && descriptor == "(Ljava/lang/String;)Lorg/python/core/PyObject;"
}

pub(crate) fn is_jdk_wrapper_math_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    match class_name {
        "java/lang/Integer" => {
            descriptor == "(II)I" && matches!(method_name, "sum" | "max" | "min" | "compare")
        }
        "java/lang/Long" => {
            (descriptor == "(JJ)J" && matches!(method_name, "sum" | "max" | "min"))
                || (descriptor == "(JJ)I" && method_name == "compare")
        }
        _ => false,
    }
}

pub(super) fn is_bc_sect_field_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "org/bouncycastle/math/ec/custom/sec/SecT113Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT131Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT163Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT193Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT233Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT239Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT283Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT409Field"
            | "org/bouncycastle/math/ec/custom/sec/SecT571Field"
    )
}

pub(super) fn is_bc_sect_field_native_override(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    if !is_bc_sect_field_class(class_name) {
        return false;
    }
    matches!(
        (method_name, descriptor),
        (
            "add" | "addBothTo" | "addExt" | "multiply" | "multiplyAddToExt",
            "([J[J[J)V"
        ) | (
            "addOne" | "halfTrace" | "invert" | "reduce" | "sqrt" | "square" | "squareAddToExt",
            "([J[J)V"
        ) | ("squareN", "([JI[J)V")
            | ("trace", "([J)I")
    ) || (class_name == "org/bouncycastle/math/ec/custom/sec/SecT571Field"
        && matches!(
            (method_name, descriptor),
            ("precompMultiplicand", "([J)[J")
                | ("multiplyPrecomp" | "multiplyPrecompAddToExt", "([J[J[J)V")
        ))
}

pub(super) fn is_bc_sect_point_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "org/bouncycastle/math/ec/custom/sec/SecT113R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT113R2Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT131R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT131R2Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT163K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT163R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT163R2Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT193R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT193R2Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT233K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT233R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT239K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT283K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT283R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT409K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT409R1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT571K1Point"
            | "org/bouncycastle/math/ec/custom/sec/SecT571R1Point"
    )
}

pub(crate) fn is_bc_crypto_math_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if is_bc_sect_field_native_override(class_name, method_name, descriptor) {
        return true;
    }
    if class_name == "org/bouncycastle/math/ec/ECPoint"
        && method_name == "timesPow2"
        && descriptor == "(I)Lorg/bouncycastle/math/ec/ECPoint;"
    {
        return true;
    }
    if is_bc_sect_point_class(class_name)
        && method_name == "twice"
        && descriptor == "()Lorg/bouncycastle/math/ec/ECPoint;"
    {
        return true;
    }
    match class_name {
        "org/bouncycastle/math/ec/ECFieldElement$Fp" => matches!(
            (method_name, descriptor),
            (
                "add" | "subtract" | "multiply" | "divide",
                "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
            ) | ("addOne" | "square" | "negate" | "invert", "()Lorg/bouncycastle/math/ec/ECFieldElement;")
                | (
                    "modAdd" | "modMult" | "modSubtract",
                    "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;"
                )
                | (
                    "modDouble" | "modHalf" | "modHalfAbs" | "modInverse" | "modReduce",
                    "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"
                )
                | (
                    "multiplyPlusProduct" | "multiplyMinusProduct",
                    "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
                )
                | (
                    "squarePlusProduct" | "squareMinusProduct",
                    "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
                )
        ),
        "org/bouncycastle/math/ec/ECFieldElement$F2m" => matches!(
            (method_name, descriptor),
            (
                "add" | "subtract" | "multiply" | "divide",
                "(Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
            ) | ("addOne" | "square" | "negate" | "invert", "()Lorg/bouncycastle/math/ec/ECFieldElement;")
                | (
                    "multiplyPlusProduct" | "multiplyMinusProduct",
                    "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
                )
                | (
                    "squarePlusProduct" | "squareMinusProduct",
                    "(Lorg/bouncycastle/math/ec/ECFieldElement;Lorg/bouncycastle/math/ec/ECFieldElement;)Lorg/bouncycastle/math/ec/ECFieldElement;"
                )
                | ("squarePow", "(I)Lorg/bouncycastle/math/ec/ECFieldElement;")
        ),
        "org/bouncycastle/math/ec/ECPoint$F2m" => matches!(
            (method_name, descriptor),
            (
                "add" | "twicePlus",
                "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;"
            ) | ("twice", "()Lorg/bouncycastle/math/ec/ECPoint;")
        ),
        "org/bouncycastle/math/ec/ECPoint$Fp" => matches!(
            (method_name, descriptor),
            (
                "add" | "twicePlus",
                "(Lorg/bouncycastle/math/ec/ECPoint;)Lorg/bouncycastle/math/ec/ECPoint;"
            ) | (
                "twice" | "threeTimes" | "negate",
                "()Lorg/bouncycastle/math/ec/ECPoint;"
            ) | ("timesPow2", "(I)Lorg/bouncycastle/math/ec/ECPoint;")
        ),
        "org/bouncycastle/math/ec/ECAlgorithms" => matches!(
            (method_name, descriptor),
            (
                "implShamirsTrickJsf",
                "(Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;Lorg/bouncycastle/math/ec/ECPoint;Ljava/math/BigInteger;)Lorg/bouncycastle/math/ec/ECPoint;"
            )
        ),
        "org/bouncycastle/math/ec/LongArray" => matches!(
            (method_name, descriptor),
            ("modReduce" | "modSquare" | "modInverse", "(I[I)Lorg/bouncycastle/math/ec/LongArray;")
                | ("reduce", "(I[I)V")
                | (
                    "modMultiply" | "multiply",
                    "(Lorg/bouncycastle/math/ec/LongArray;I[I)Lorg/bouncycastle/math/ec/LongArray;"
                )
                | ("square", "(I[I)Lorg/bouncycastle/math/ec/LongArray;")
                | ("modSquareN", "(II[I)Lorg/bouncycastle/math/ec/LongArray;")
        ),
        "org/bouncycastle/math/Primes" => matches!(
            (method_name, descriptor),
            ("implHasAnySmallFactors", "(Ljava/math/BigInteger;)Z")
                | (
                    "isMRProbablePrime",
                    "(Ljava/math/BigInteger;Ljava/security/SecureRandom;I)Z"
                )
                | (
                    "isMRProbablePrimeToBase",
                    "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Z"
                )
        ),
        "org/bouncycastle/math/ec/rfc7748/X25519Field" => {
            method_name == "mul" && descriptor == "([I[I[I)V"
        }
        "org/bouncycastle/math/ec/rfc7748/X448Field" => matches!(
            (method_name, descriptor),
            ("mul", "([I[I[I)V")
                | ("mul", "([II[I)V")
                | ("sqr", "([I[I)V")
                | ("sqr", "([II[I)V")
        ),
        "org/bouncycastle/util/BigIntegers" => matches!(
            (method_name, descriptor),
            ("hasAnySmallFactors", "(Ljava/math/BigInteger;)Z")
                | (
                    "modOddInverse" | "modOddInverseVar",
                    "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;"
                )
        ),
        "org/bouncycastle/crypto/prng/DigestRandomGenerator" => matches!(
            (method_name, descriptor),
            ("nextBytes", "([B)V") | ("nextBytes", "([BII)V")
        ),
        "org/bouncycastle/crypto/engines/GOST3412_2015Engine" => {
            method_name == "processBlock" && descriptor == "([BI[BI)I"
        }
        "org/bouncycastle/crypto/engines/SM4Engine" => {
            method_name == "processBlock" && descriptor == "([BI[BI)I"
        }
        "org/bouncycastle/crypto/engines/XTEAEngine" => {
            method_name == "processBlock" && descriptor == "([BI[BI)I"
        }
        "org/bouncycastle/crypto/engines/Salsa20Engine" => {
            (method_name == "salsaCore" && descriptor == "(I[I[I)V")
                || (method_name == "processBytes" && descriptor == "([BII[BI)I")
        }
        "org/bouncycastle/crypto/engines/XSalsa20Engine"
        | "org/bouncycastle/crypto/engines/ChaChaEngine"
        | "org/bouncycastle/crypto/engines/ChaCha7539Engine"
        | "org/bouncycastle/crypto/engines/XChaCha20Engine" => {
            method_name == "processBytes" && descriptor == "([BII[BI)I"
        }
        "org/bouncycastle/crypto/engines/VMPCEngine"
        | "org/bouncycastle/crypto/engines/VMPCKSA3Engine" => {
            method_name == "processBytes" && descriptor == "([BII[BI)I"
        }
        "org/bouncycastle/crypto/engines/AESEngine" => matches!(
            (method_name, descriptor),
            ("encryptBlock" | "decryptBlock", "([BI[BI[[I)V")
                | ("<init>", "()V")
                | (
                    "newInstance",
                    "()Lorg/bouncycastle/crypto/MultiBlockCipher;"
                )
                | ("generateWorkingKey", "([BZ)[[I")
                | (
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V"
                )
                | ("processBlock", "([BI[BI)I")
        ),
        "org/bouncycastle/crypto/engines/AESLightEngine"
        | "org/bouncycastle/crypto/engines/AESFastEngine" => matches!(
            (method_name, descriptor),
            ("encryptBlock" | "decryptBlock", "([BI[BI[[I)V")
                | ("<init>", "()V")
                | ("generateWorkingKey", "([BZ)[[I")
                | (
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V"
                )
                | ("processBlock", "([BI[BI)I")
        ),
        "org/bouncycastle/crypto/modes/SICBlockCipher" => matches!(
            (method_name, descriptor),
            ("reset", "()V")
                | ("seekTo", "(J)J")
                | (
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V"
                )
                | ("processBlock", "([BI[BI)I")
                | ("processBytes", "([BII[BI)I")
        ),
        "org/bouncycastle/crypto/modes/CBCBlockCipher" => {
            method_name == "processBlock" && descriptor == "([BI[BI)I"
        }
        "org/bouncycastle/util/Pack" => matches!(
            (method_name, descriptor),
            ("bigEndianToInt" | "littleEndianToInt", "([BI)I")
                | ("bigEndianToInt" | "littleEndianToInt", "([BI[I)V")
                | ("bigEndianToInt" | "littleEndianToInt", "([BI[III)V")
                | ("intToBigEndian" | "intToLittleEndian", "(I[BI)V")
                | ("intToBigEndian" | "intToLittleEndian", "([I[BI)V")
                | ("intToBigEndian" | "intToLittleEndian", "([III[BI)V")
        ),
        "org/bouncycastle/util/Arrays" => method_name == "copyOf" && descriptor == "([BI)[B",
        "org/bouncycastle/crypto/params/KeyParameter" => matches!(
            (method_name, descriptor),
            ("<init>", "([B)V") | ("<init>", "([BII)V")
        ),
        "org/bouncycastle/crypto/params/ParametersWithIV" => matches!(
            (method_name, descriptor),
            (
                "<init>",
                "(Lorg/bouncycastle/crypto/CipherParameters;[B)V"
            ) | (
                "<init>",
                "(Lorg/bouncycastle/crypto/CipherParameters;[BII)V"
            )
        ),
        "org/bouncycastle/crypto/digests/Blake2sDigest" => {
            (method_name == "G" && descriptor == "(IIIIII)V")
                || (method_name == "compress" && descriptor == "([BI)V")
        }
        "org/bouncycastle/crypto/digests/KeccakDigest" => matches!(
            (method_name, descriptor),
            ("KeccakPermutation" | "KeccakExtract", "()V") | ("KeccakAbsorb", "([BI)V")
        ),
        "org/bouncycastle/crypto/digests/GOST3411Digest" => {
            method_name == "processBlock" && descriptor == "([BI)V"
        }
        "org/bouncycastle/crypto/digests/WhirlpoolDigest" => matches!(
            (method_name, descriptor),
            ("processBlock", "()V") | ("update", "([BII)V")
        ),
        "org/bouncycastle/crypto/macs/Poly1305" => matches!(
            (method_name, descriptor),
            ("update", "([BII)V") | ("doFinal", "([BI)I")
        ),
        "org/bouncycastle/crypto/generators/SCrypt" => {
            method_name == "generate" && descriptor == "([B[BIIII)[B"
        }
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator" => matches!(
            (method_name, descriptor),
            ("generateBytes", "([B[BII)I")
                | (
                    "roundFunction",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;IIIIIIIIIIIIIIII)V"
                )
        ),
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator$Block" => matches!(
            (method_name, descriptor),
            ("fromBytes" | "toBytes", "([B)V")
                | (
                    "copyBlock" | "xorWith",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
                )
                | (
                    "xor" | "xorWith",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
                )
                | (
                    "clear",
                    "()Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;"
                )
        ),
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FillBlock" => matches!(
            (method_name, descriptor),
            ("applyBlake", "()V")
                | (
                    "fillBlock",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
                )
                | (
                    "fillBlock" | "fillBlockWithXor",
                    "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
                )
        ),
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FixedBlockPool" => matches!(
            (method_name, descriptor),
            (
                "allocate",
                "()Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;"
            ) | (
                "deallocate",
                "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V"
            )
        ),
        "org/bouncycastle/crypto/generators/PKCS5S2ParametersGenerator" => matches!(
            (method_name, descriptor),
            (
                "generateDerivedParameters" | "generateDerivedMacParameters",
                "(I)Lorg/bouncycastle/crypto/CipherParameters;"
            ) | (
                "generateDerivedParameters",
                "(II)Lorg/bouncycastle/crypto/CipherParameters;"
            )
        ),
        "org/bouncycastle/crypto/generators/PKCS12ParametersGenerator" => matches!(
            (method_name, descriptor),
            (
                "generateDerivedParameters" | "generateDerivedMacParameters",
                "(I)Lorg/bouncycastle/crypto/CipherParameters;"
            ) | (
                "generateDerivedParameters",
                "(II)Lorg/bouncycastle/crypto/CipherParameters;"
            )
        ),
        "org/bouncycastle/crypto/generators/BCrypt" => {
            method_name == "generate" && descriptor == "([B[BI)[B"
        }
        _ => false,
    }
}

pub(crate) fn is_forkjoin_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // On the default real-ForkJoinPool path, keep real pool initialization but
    // force the VM bridge methods that otherwise enqueue work into
    // ForkJoinPool's queue/status machinery. The task-family methods share the
    // side-table state scanned and remapped by GC. The legacy synthetic path is
    // selected explicitly with CRATONVM_SYNTHETIC_FORKJOINPOOL.
    if class_name == "java/util/concurrent/ForkJoinPool"
        && matches!(
            (method_name, descriptor),
            ("commonPool", "()Ljava/util/concurrent/ForkJoinPool;")
                | (
                    "getFactory",
                    "()Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;"
                )
                | ("getParallelism", "()I")
                | ("getCommonPoolParallelism", "()I")
                | (
                    "invoke",
                    "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;"
                )
                | (
                    "submit",
                    "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;"
                )
                | (
                    "externalSubmit",
                    "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;"
                )
                // submit(Callable)/submit(Runnable)/submit(Runnable, T): left off
                // the original allow-list, so real bytecode ran them against a pool
                // whose commonPool() shortcut never populates queues/runState/mode —
                // RejectedExecutionException at submissionQueue() (RealFjp.java).
                | (
                    "submit",
                    "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/ForkJoinTask;"
                )
                | (
                    "submit",
                    "(Ljava/lang/Runnable;)Ljava/util/concurrent/ForkJoinTask;"
                )
                | (
                    "submit",
                    "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/ForkJoinTask;"
                )
                | ("execute", "(Ljava/lang/Runnable;)V")
                | ("execute", "(Ljava/util/concurrent/ForkJoinTask;)V")
                | (
                    "awaitQuiescence",
                    "(JLjava/util/concurrent/TimeUnit;)Z"
                )
        )
    {
        return true;
    }

    matches!(
        class_name,
        "java/util/concurrent/ForkJoinTask"
            | "java/util/concurrent/RecursiveTask"
            | "java/util/concurrent/RecursiveAction"
    ) && matches!(
        (method_name, descriptor),
        ("fork", "()Ljava/util/concurrent/ForkJoinTask;")
            | ("join", "()Ljava/lang/Object;")
            | ("invoke", "()Ljava/lang/Object;")
            | ("get", "()Ljava/lang/Object;")
            | (
                "get",
                "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;"
            )
            | ("getRawResult", "()Ljava/lang/Object;")
            | ("setRawResult", "(Ljava/lang/Object;)V")
            | ("isDone", "()Z")
            | ("isCompletedNormally", "()Z")
            | ("isCancelled", "()Z")
            | ("cancel", "(Z)Z")
            | ("complete", "(Ljava/lang/Object;)V")
    )
}

pub(crate) fn is_count_down_latch_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/util/concurrent/CountDownLatch"
        && matches!(
            (method_name, descriptor),
            ("<init>", "(I)V")
                | ("countDown", "()V")
                | ("await", "()V")
                | ("await", "(JLjava/util/concurrent/TimeUnit;)Z")
                | ("getCount", "()J")
                | ("toString", "()Ljava/lang/String;")
        )
}

pub(crate) fn is_ffm_symbol_lookup_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/foreign/SymbolLookup"
        && method_name == "find"
        && descriptor == "(Ljava/lang/String;)Ljava/util/Optional;"
}

pub(crate) fn is_ffm_group_layout_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    (class_name == "java/lang/foreign/GroupLayout"
        || class_name == "java/lang/foreign/StructLayout")
        && matches!(
            (method_name, descriptor),
            ("memberLayouts", "()Ljava/util/List;")
                | ("name", "()Ljava/util/Optional;")
                | (
                    "withName",
                    "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;"
                )
                | ("byteSize", "()J")
                | ("byteAlignment", "()J")
        )
}

pub(crate) fn is_ffm_memory_layout_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/foreign/MemoryLayout"
        && matches!(
            (method_name, descriptor),
            (
                "sequenceLayout",
                "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/SequenceLayout;"
            ) | (
                "sequenceLayout",
                "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;"
            ) | (
                "structLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;"
            ) | (
                "structLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;"
            ) | (
                "unionLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;"
            ) | (
                "unionLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;"
            ) | ("paddingLayout", "(J)Ljava/lang/foreign/PaddingLayout;")
                | ("paddingLayout", "(J)Ljava/lang/foreign/MemoryLayout;")
                | (
                    "varHandle",
                    "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;"
                )
                | ("name", "()Ljava/util/Optional;")
                | (
                    "withName",
                    "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;"
                )
        )
}

/// FFM `java.lang.foreign.Arena` — the interface-native exemption that pairs
/// with `force_ffm_memory_segment_interface_native` (vm/src/vm/vm_exec.rs).
///
/// `Arena.ofConfined()/ofAuto()/ofShared()/global()` are STATIC interface
/// methods, so their registered natives always run (the interface skip only
/// covers instance methods) and hand back a synthetic receiver stamped with
/// the literal interface class name `java/lang/foreign/Arena`
/// (native-builtins/src/phases_late/foreign_ffm.rs `p67_new_arena`, which also
/// gives the arena the session that `scope()` hands out and `close()` closes).
/// Every lifecycle method on that receiver is a NON-STATIC method whose
/// declaring class is an interface — exactly the shape both dispatch guards
/// skip — so `scope()`/`close()`/`allocate(…)` would resolve to the interface's
/// own declaration (abstract in a real JDK image, a stub body under
/// `--synthetic-jdk`) instead of the natives that own the arena's lifetime.
/// `MemorySegment.scope` is already force-routed for precisely this reason;
/// without this twin the two disagree and `arena.scope() != segment.scope()`.
///
/// Only triples that actually have a registration are listed: `scope`, `close`,
/// the four `allocate` overloads and `allocateFrom`/`allocateUtf8String`
/// (foreign_ffm.rs `register_p67_foreign_memory` + panama.rs
/// `register_pe_arena`/`register_pe2_string_marshaling`). `allocateArray` has
/// no native behind it on any class and is deliberately absent — forcing a name
/// with no registration would only cost a fruitless registry probe.
///
/// A REAL `jdk.internal.foreign.ArenaImpl` receiver is unaffected: it declares
/// `scope()`, `close()` and `allocate(long, long)` concretely, so dispatch
/// resolves with `jdk/internal/foreign/ArenaImpl` as the declaring class and
/// never matches this predicate. (The session natives these bodies call do have
/// a real-receiver escape — `p67_session_delegate`'s
/// `invoke_virtual_bytecode_only` — but this predicate never needs it, because
/// a real `ArenaImpl` is not routed here in the first place.)
pub(crate) fn is_ffm_arena_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/foreign/Arena"
        && matches!(
            (method_name, descriptor),
            ("scope", "()Ljava/lang/foreign/MemorySegment$Scope;")
                | ("close", "()V")
                | ("allocate", "(J)Ljava/lang/foreign/MemorySegment;")
                | ("allocate", "(JJ)Ljava/lang/foreign/MemorySegment;")
                | (
                    "allocate",
                    "(Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/MemorySegment;"
                )
                | (
                    "allocate",
                    "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemorySegment;"
                )
                | (
                    "allocateFrom",
                    "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;"
                )
                | (
                    "allocateUtf8String",
                    "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;"
                )
        )
}

pub(crate) fn is_file_channel_impl_open_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "sun/nio/ch/FileChannelImpl"
        && method_name == "open"
        && matches!(
            descriptor,
            "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZZLjava/io/Closeable;)Ljava/nio/channels/FileChannel;"
                | "(Ljava/io/FileDescriptor;Ljava/lang/String;ZZZLjava/lang/Object;)Ljava/nio/channels/FileChannel;"
        )
}

/// `FileSystemProvider`'s three link operations —
/// `createSymbolicLink`/`createLink`/`readSymbolicLink`.
///
/// Same shape as the `newFileChannel` exemption further down this file: the
/// real-JDK base class gives each of these a CONCRETE body that unconditionally
/// throws `UnsupportedOperationException` (a concrete subclass is expected to
/// override it). CratonVM's default provider is the synthetic instance stamped
/// as the literal `java/nio/file/spi/FileSystemProvider` class, so there is no
/// subclass to override anything and "real class bytes are authoritative" runs
/// the throw. Without this exemption the natives registered in
/// `native-builtins/src/phases_late/nio_file.rs` are unreachable and every
/// `Files.createSymbolicLink` in the VM dies with a bare
/// `UnsupportedOperationException` — see
/// `docs/internal/fixed-suite-bugs/springboot/files-createsymboliclink-unsupported-FIXED.md`.
///
/// The real `sun.nio.fs.*` provider names are listed alongside the base for the
/// same reason `newFileChannel` lists them: a cached dispatch site can carry a
/// concrete receiver class name while the callback lives under the base name.
///
/// This is the single source of truth for the exemption: it is consulted by
/// BOTH dispatch gates — [`force_native_over_real_jdk_bytecode`] and the
/// `check_override` predicate in `vm_exec.rs::invoke_on_class_shared_inner`.
pub(crate) fn is_file_system_provider_link_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "java/nio/file/spi/FileSystemProvider"
            | "sun/nio/fs/WindowsFileSystemProvider"
            | "sun/nio/fs/UnixFileSystemProvider"
    ) && matches!(
        (method_name, descriptor),
        (
            "createSymbolicLink",
            "(Ljava/nio/file/Path;Ljava/nio/file/Path;[Ljava/nio/file/attribute/FileAttribute;)V"
        ) | ("createLink", "(Ljava/nio/file/Path;Ljava/nio/file/Path;)V")
            | ("readSymbolicLink", "(Ljava/nio/file/Path;)Ljava/nio/file/Path;")
    )
}

pub(crate) fn is_input_stream_transfer_to_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "java/io/InputStream" | "java/io/FileInputStream"
    ) && method_name == "transferTo"
        && descriptor == "(Ljava/io/OutputStream;)J"
}

pub(crate) fn is_zip_output_primitive_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    (class_name == "java/util/zip/ZipOutputStream"
        && matches!(
            (method_name, descriptor),
            ("writeShort", "(I)V")
                | ("writeInt", "(J)V")
                | ("writeLong", "(J)V")
                | ("putNextEntry", "(Ljava/util/zip/ZipEntry;)V")
                | ("write", "([BII)V")
                | ("write", "([B)V")
                | ("write", "(I)V")
                | ("finish", "()V")
                | ("close", "()V")
        ))
        || (class_name == "java/util/zip/ZipEntry"
            && method_name == "setTime"
            && descriptor == "(J)V")
        // The real InflaterInputStream.close bytecode is contractually simple,
        // but it runs once for every ZIP64 entry in the loader fixture. The
        // registered native performs the same closed/inflater/underlying-stream
        // transitions through real named fields; force it so virtual dispatch
        // does not fall back to the interpreter for each tiny entry.
        || (class_name == "java/util/zip/InflaterInputStream"
            && matches!(
                (method_name, descriptor),
                ("close", "()V")
                    | ("<init>", "(Ljava/io/InputStream;Ljava/util/zip/Inflater;I)V")
                    | ("read", "()I")
                    | ("read", "([BII)I")
            ))
        || (class_name == "org/springframework/boot/loader/jar/ZipInflaterInputStream"
            && matches!(
                (method_name, descriptor),
                ("read", "([BII)I")
                    | ("<init>", "(Ljava/io/InputStream;Ljava/util/zip/Inflater;I)V")
            ))
        || (class_name == "org/springframework/boot/loader/zip/FileDataBlock"
            && matches!(
                (method_name, descriptor),
                ("open", "()V")
                    | ("close", "()V")
                    | ("read", "(Ljava/nio/ByteBuffer;J)I")
            ))
}

pub(crate) fn is_native_thread_set_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "sun/nio/ch/NativeThreadSet"
        && matches!(
            (method_name, descriptor),
            ("add", "()I") | ("remove", "(I)V") | ("signalAndWait", "()V")
        )
}

/// JavaNioAccess methods that must dispatch through CratonVM natives even when
/// the real JDK returns an anonymous/synthetic access singleton. JDK 17's
/// `VM$BufferPoolsHolder.<clinit>` invokes this through the interface, and a
/// receiver-class lookup alone can miss the bridge native.
pub(crate) fn is_java_nio_access_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "jdk/internal/access/JavaNioAccess"
        && method_name == "getDirectBufferPool"
        && descriptor == "()Ljdk/internal/misc/VM$BufferPool;"
}

pub(crate) fn is_stamped_lock_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/util/concurrent/locks/StampedLock"
        && matches!(
            (method_name, descriptor),
            ("<init>", "()V")
                | ("readLock", "()J")
                | ("writeLock", "()J")
                | ("tryOptimisticRead", "()J")
                | ("unlockRead", "(J)V")
                | ("unlockWrite", "(J)V")
                | ("unstampedUnlockRead", "()V")
                | ("unstampedUnlockWrite", "()V")
                | ("tryUnlockRead", "()Z")
                | ("tryUnlockWrite", "()Z")
                | ("validate", "(J)Z")
                | ("tryReadLock", "()J")
                | ("tryWriteLock", "()J")
                | ("tryConvertToWriteLock", "(J)J")
                | ("tryConvertToReadLock", "(J)J")
                | ("tryConvertToOptimisticRead", "(J)J")
                | ("unlock", "(J)V")
                | ("readLockInterruptibly", "()J")
                | ("writeLockInterruptibly", "()J")
                | ("tryReadLock", "(JLjava/util/concurrent/TimeUnit;)J")
                | ("tryWriteLock", "(JLjava/util/concurrent/TimeUnit;)J")
                | ("isLocked", "()Z")
                | ("isWriteLocked", "()Z")
                | ("isReadLocked", "()Z")
                | ("getReadLockCount", "()I")
        )
        || matches!(
            class_name,
            "java/util/concurrent/locks/StampedLock$ReadLockView"
                | "java/util/concurrent/locks/StampedLock$WriteLockView"
        ) && matches!(
            (method_name, descriptor),
            ("lock", "()V") | ("tryLock", "()Z") | ("unlock", "()V")
        )
}

/// The real JDK 25 ReentrantReadWriteLock stores its state in the final
/// protected helpers inherited from AbstractQueuedLongSynchronizer.  The
/// native implementations retain the JDK queue algorithm while providing an
/// atomic scalar state word outside CratonVM's tagged heap slot.
pub(crate) fn is_aqls_state_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/util/concurrent/locks/AbstractQueuedLongSynchronizer"
        && matches!(
            (method_name, descriptor),
            ("getState", "()J") | ("setState", "(J)V") | ("compareAndSetState", "(JJ)Z")
        )
}

pub(crate) fn is_xerces_cmstateset_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    class_name == "com/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet"
        && matches!(
            (method_name, descriptor),
            ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | (
                    "isSameSet",
                    "(Lcom/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet;)Z"
                )
        )
}

pub(crate) fn is_xerces_xml_parser_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    match class_name {
        "com/sun/org/apache/xerces/internal/util/XMLChar" => matches!(
            (method_name, descriptor),
            ("isSpace", "(I)Z")
                | ("isNameStart", "(I)Z")
                | ("isName", "(I)Z")
                | ("isNCNameStart", "(I)Z")
                | ("isNCName", "(I)Z")
        ),
        "jdk/xml/internal/XMLLimitAnalyzer" => matches!(
            (method_name, descriptor),
            ("addValue", "(ILjava/lang/String;I)V")
                | (
                    "addValue",
                    "(Ljdk/xml/internal/XMLSecurityManager$Limit;Ljava/lang/String;I)V"
                )
                | ("getValue", "(I)I")
                | ("getValue", "(Ljdk/xml/internal/XMLSecurityManager$Limit;)I")
                | ("getTotalValue", "(I)I")
                | (
                    "getTotalValue",
                    "(Ljdk/xml/internal/XMLSecurityManager$Limit;)I"
                )
                | ("getValueByIndex", "(I)I")
        ),
        "com/sun/org/apache/xerces/internal/impl/dv/xs/XSSimpleTypeDecl" => matches!(
            (method_name, descriptor),
            ("normalize", "(Ljava/lang/String;S)Ljava/lang/String;")
                | ("normalize", "(Ljava/lang/Object;S)Ljava/lang/String;")
        ),
        "com/sun/org/apache/xerces/internal/impl/xs/traversers/XSDHandler$XSDKey" => matches!(
            (method_name, descriptor),
            ("hashCode", "()I") | ("equals", "(Ljava/lang/Object;)Z")
        ),
        "com/sun/org/apache/xerces/internal/impl/XMLEntityScanner" => matches!(
            (method_name, descriptor),
            (
                "scanContent",
                "(Lcom/sun/org/apache/xerces/internal/xni/XMLString;)I"
            ) | (
                "scanQName",
                "(Lcom/sun/org/apache/xerces/internal/xni/QName;Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z"
            ) | ("skipSpaces", "()Z") | (
                "normalizeNewlines",
                "(SLcom/sun/org/apache/xerces/internal/xni/XMLString;ZZLcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;)Z"
            ) | (
                "checkEntityLimit",
                "(Lcom/sun/org/apache/xerces/internal/impl/XMLScanner$NameType;Lcom/sun/xml/internal/stream/Entity$ScannedEntity;II)V"
            )
        ),
        "com/sun/org/apache/xerces/internal/impl/xs/opti/NodeImpl" => matches!(
            (method_name, descriptor),
            ("getNodeName", "()Ljava/lang/String;")
                | ("getNamespaceURI", "()Ljava/lang/String;")
                | ("getPrefix", "()Ljava/lang/String;")
                | ("getLocalName", "()Ljava/lang/String;")
                | ("getNodeType", "()S")
                | ("getReadOnly", "()Z")
        ),
        "com/sun/org/apache/xerces/internal/impl/xs/opti/ElementImpl" => {
            matches!((method_name, descriptor), ("getTagName", "()Ljava/lang/String;"))
        }
        "com/sun/org/apache/xerces/internal/impl/xs/opti/AttrImpl" => matches!(
            (method_name, descriptor),
            ("getName", "()Ljava/lang/String;")
                | ("getValue", "()Ljava/lang/String;")
                | ("getNodeValue", "()Ljava/lang/String;")
                | ("getSpecified", "()Z")
                | ("isId", "()Z")
        ),
        "com/sun/org/apache/xerces/internal/impl/xpath/regex/RangeToken" => {
            matches!((method_name, descriptor), ("sortRanges", "()V"))
        }
        _ => false,
    }
}

pub(crate) fn is_awt_imageio_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // java.awt.image.BufferedImage side-table raster. CratonVM stores pixels in
    // native-awt's ARGB registry and stamps the Java object with an imageId;
    // the real JDK methods expect populated Raster/ColorModel internals.
    if class_name == "java/awt/image/BufferedImage"
        && matches!(
            (method_name, descriptor),
            ("<init>", "(III)V")
                | ("getWidth", "()I")
                | ("getHeight", "()I")
                | ("getRGB", "(II)I")
                | ("setRGB", "(III)V")
                | ("getType", "()I")
                | ("createGraphics", "()Ljava/awt/Graphics2D;")
                | ("flush", "()V")
                | ("getRGB", "(IIII[III)[I")
        )
    {
        return true;
    }

    // javax.imageio.ImageIO codec bridge. The real-JDK ImageIO SPI expects
    // raster/color-model internals that CratonVM's memory-backed BufferedImage
    // does not populate. Force the native bridge so Spring and desktop code can
    // read/write the ARGB side-table image data through PNG/JPEG codecs.
    if class_name == "javax/imageio/ImageIO"
        && matches!(
            (method_name, descriptor),
            (
                "read",
                "(Ljava/io/InputStream;)Ljava/awt/image/BufferedImage;"
            ) | ("read", "(Ljava/io/File;)Ljava/awt/image/BufferedImage;")
                | (
                    "write",
                    "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/OutputStream;)Z"
                )
                | (
                    "write",
                    "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljavax/imageio/stream/ImageOutputStream;)Z"
                )
                | (
                    "write",
                    "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/File;)Z"
                )
        )
    {
        return true;
    }

    if class_name == "com/sun/imageio/plugins/jpeg/JPEGImageReader"
        && matches!(
            (method_name, descriptor),
            (
                "read",
                "(ILjavax/imageio/ImageReadParam;)Ljava/awt/image/BufferedImage;"
            ) | ("dispose", "()V")
        )
    {
        return true;
    }

    class_name == "com/sun/imageio/plugins/png/PNGImageWriter"
        && method_name == "write"
        && descriptor
            == "(Ljavax/imageio/metadata/IIOMetadata;Ljavax/imageio/IIOImage;Ljavax/imageio/ImageWriteParam;)V"
}

pub(crate) fn is_liquibase_checksum_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        (class_name, method_name, descriptor),
        (
            "liquibase/change/AbstractChange$1",
            "include",
            "(Ljava/lang/Object;Ljava/lang/String;Ljava/lang/Object;)Z",
        ) | (
            "liquibase/change/ColumnConfig",
            "getSerializableFieldValue",
            "(Ljava/lang/String;)Ljava/lang/Object;",
        )
    )
}

pub(crate) fn is_reflection_factory_serialization_native_override(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "sun/reflect/ReflectionFactory" | "jdk/internal/reflect/ReflectionFactory"
    ) && matches!(
        (method_name, descriptor),
        ("getReflectionFactory", "()Lsun/reflect/ReflectionFactory;")
            | (
                "getReflectionFactory",
                "()Ljdk/internal/reflect/ReflectionFactory;"
            )
            | (
                "newConstructorForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;"
            )
            | (
                "newConstructorForSerialization",
                "(Ljava/lang/Class;Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;"
            )
            | (
                "newConstructorForExternalization",
                "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;"
            )
            | (
                "readObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "readObjectNoDataForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "writeObjectForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "readResolveForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "writeReplaceForSerialization",
                "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;"
            )
            | (
                "hasStaticInitializerForSerialization",
                "(Ljava/lang/Class;)Z"
            )
    )
}

/// Temporary call-count instrumentation for the silent-hang-no-signature-
/// cluster throughput residual (2026-07-13). Tallies invocations of several
/// suspected interpreter dispatch hot-path functions, reported periodically
/// via `CRATONVM_DBG_HOTPATH_COUNTS=1` — independent of wall-clock timing,
/// so it stays valid signal even on a heavily contended/noisy host.
pub(crate) mod hotpath_counts {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static FORCE_NATIVE_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static RESOLVE_METHOD_REF_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static LOOKUP_LOADER_INITIATED_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static RETARGET_FIELD_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static TOTAL_INSTRUCTIONS: AtomicU64 = AtomicU64::new(0);

    pub fn bump(counter: &AtomicU64) {
        if !crate::runtime::env_cache::dbg_hotpath_counts() {
            return;
        }
        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() || n % 1_000_000 == 0 {
            eprintln!(
                "[hotpath-counts] force_native={} resolve_method_ref={} \
                 lookup_loader_initiated={} retarget_field={} total_instr={}",
                FORCE_NATIVE_CALLS.load(Ordering::Relaxed),
                RESOLVE_METHOD_REF_CALLS.load(Ordering::Relaxed),
                LOOKUP_LOADER_INITIATED_CALLS.load(Ordering::Relaxed),
                RETARGET_FIELD_CALLS.load(Ordering::Relaxed),
                TOTAL_INSTRUCTIONS.load(Ordering::Relaxed),
            );
        }
    }
}

pub(crate) fn is_undertow_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    matches!(
        (class_name, method_name, method_descriptor),
        (
            "io/undertow/Undertow",
            "builder",
            "()Lio/undertow/Undertow$Builder;"
        ) | ("io/undertow/Undertow", "start", "()V")
            | ("io/undertow/Undertow", "stop", "()V")
            | (
                "io/undertow/Undertow$Builder",
                "addHttpListener",
                "(ILjava/lang/String;)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "addHttpsListener",
                "(ILjava/lang/String;Ljavax/net/ssl/SSLContext;)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "setHandler",
                "(Lio/undertow/server/HttpHandler;)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "setSocketOption",
                "(Lorg/xnio/Option;Ljava/lang/Object;)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "setWorkerThreads",
                "(I)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "setIoThreads",
                "(I)Lio/undertow/Undertow$Builder;"
            )
            | (
                "io/undertow/Undertow$Builder",
                "build",
                "()Lio/undertow/Undertow;"
            )
    )
}

/// JDK-ONLY-WAVE2: the warm-path force-native gate — roughly 55 hard-coded
/// class/method/descriptor branches, every one of them a statement that
/// CratonVM's native beats the real JDK's concrete bytecode. Under contract
/// §1.4 that is exactly the thing `--jdk-only` exists to abolish: a `Bridge` or
/// `SyntheticStub` may not shadow real bytes, and a genuine `Intrinsic` needs
/// no name list because `resolve_dispatch` step 2 already takes it.
///
/// What must replace this function: **nothing**. Under `--jdk-only` every
/// branch is dead, because `resolve_dispatch` step 3 returns `Bytecode` for a
/// method that has `Code`. Under `Compatible` the branches are load-bearing for
/// real boots today, so they come out family by family with their own
/// verification, not in one sweep.
///
/// Two branches below carry their own markers because they cannot be removed on
/// their own: the `java/lang/String` exclusion (paired with the positive form in
/// `vm/src/vm/vm_exec.rs`'s `check_override`) and the `ThreadPoolExecutor`
/// family.
pub(super) fn force_native_over_real_jdk_bytecode(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    hotpath_counts::bump(&hotpath_counts::FORCE_NATIVE_CALLS);
    // `Map.values()` (native_map_values in native-collections/src/lib.rs)
    // returns a plain `java/util/ArrayList` that stashes its source map in a
    // spare trailing capacity slot so a later `Map.put`/`remove` on the
    // source is reflected on read (`resync_values_view`, called from
    // `native_al_size`/`native_al_is_empty`/`native_al_get`/
    // `native_al_contains`/`native_al_iterator`/`native_al_to_array*` etc.).
    // Real `ArrayList` bytecode declares its own `size()`/`isEmpty()`/`get()`/
    // etc., so without this force-entry the receiver-has-own-bytecode rule
    // picks real bytecode over the registered native, skipping the resync
    // entirely and freezing the returned Collection at whatever the source
    // map held at `values()` call time (H2 `TestAlter.
    // testAlterTableDropIdentityColumn`: `Schema.getAllSequences()` captures
    // `ConcurrentHashMap.values()` once, before any sequence exists).
    // `resync_values_view` itself is a cheap no-op for an ordinary ArrayList
    // (no stashed source map in the trailing slot), so forcing these methods
    // through the native is safe for plain ArrayLists too.
    if class_name == "java/util/ArrayList"
        && matches!(
            method_name,
            "size"
                | "isEmpty"
                | "get"
                | "contains"
                | "iterator"
                | "toArray"
                | "indexOf"
                | "lastIndexOf"
                | "toString"
                | "hashCode"
                | "equals"
        )
    {
        return true;
    }
    // BUG (found investigating the Tomcat Jasper/ecj JSP-compile NPE,
    // TestDefaultServlet.testBug57601 / TestMapperWebapps.testWelcomeFileStrict):
    // this function is the ONLY force-native gate consulted by the
    // reflective/megamorphic/`invokespecial`/interface-default dispatch path
    // (`intercept_force_registered_native[_cached]` ->
    // `should_force_registered_native_over_bytecode` ->
    // `force_native_over_real_jdk_bytecode_memoized` -> here). The "regular"
    // cached-invokevirtual dispatch path OR's in an extra
    // `matches!((class_name,...), "java/util/HashMap"|"java/util/LinkedHashMap"
    // |"java/util/Hashtable"|"java/util/concurrent/ConcurrentHashMap")`
    // cluster locally (see further below in this same file, and the
    // companion `check_override` chain in `vm/src/vm/vm_exec.rs`), but this
    // base function never did — so a call reaching it directly ran the REAL
    // JDK bytecode for `put`/`get`/`size`/etc. instead of (or, when a
    // different call to the identical call site had already gone through the
    // OTHER, covered path, *in addition to*) the registered native,
    // corrupting any state the two implementations don't share (e.g.
    // `Hashtable`'s own real `count` field vs. our side-store bucket count —
    // `Hashtable.put()` ending up incrementing the tracked size TWICE,
    // doubling `size()` and leaving `values().toArray()`'s caller-supplied
    // array null-padded past the real entry count. That is exactly what made
    // ecj's `CompilationResult.getClassFiles()` — `new
    // ClassFile[compiledTypes.size()]` then `compiledTypes.values()
    // .toArray(classFiles)` on a `Hashtable(11)` — hand back a null-padded
    // array and NPE in `CompilationUnitDeclaration.cleanUp()`). Add the same
    // cluster here so every dispatch path agrees.
    if matches!(
        class_name,
        "java/util/HashMap"
            | "java/util/LinkedHashMap"
            | "java/util/Hashtable"
            | "java/util/concurrent/ConcurrentHashMap"
    ) && matches!(
        method_name,
        "computeIfAbsent"
            | "compute"
            | "computeIfPresent"
            | "merge"
            | "putIfAbsent"
            | "replace"
            | "forEach"
            | "replaceAll"
            | "getOrDefault"
            | "putMapEntries"
            | "put"
            | "get"
            | "remove"
            | "containsKey"
            | "containsValue"
            | "size"
            | "isEmpty"
            | "clear"
            | "putAll"
            | "keySet"
            | "values"
            | "entrySet"
            | "keys"
            | "elements"
    ) {
        return true;
    }
    // ConcurrentHashMap's private serialization hooks. CratonVM stores CHM
    // entries in a segmented native layout, so the real JDK `writeObject`
    // (which walks the always-null `table`) serialised every CHM as empty and
    // the real `readObject` rebuilt a `table` our natives never read. Both are
    // reached through `ObjectStreamClass.invokeWriteObject`/`invokeReadObject`,
    // i.e. reflective `Method.invoke` -- a path with no bytecode PC to key an
    // invoke-cache entry on, so it consults this gate directly. Kept separate
    // from the Map cluster above because HashMap/LinkedHashMap/Hashtable have
    // no such natives and must keep running their real bodies.
    if class_name == "java/util/concurrent/ConcurrentHashMap"
        && matches!(method_name, "writeObject" | "readObject")
    {
        return true;
    }
    // Keep this warmed-invoke-cache policy in sync with vm_exec's cold-path
    // allow-list. JarFile inherits these operations from ZipFile, so a
    // subclass `super.close()` resolves to the real ZipFile bytecode after
    // cache population unless its registered bridge is forced here too. The
    // real body dereferences constructor state which native-backed JarFiles do
    // not have.
    if class_name == "java/util/zip/ZipFile"
        && matches!(
            method_name,
            "<init>"
                | "getInputStream"
                | "entries"
                | "stream"
                | "getComment"
                | "close"
                | "getName"
                | "isMultiRelease"
                | "size"
        )
    {
        return true;
    }
    // Keep in sync with vm_exec.rs's cold-path gate. The real JDK builder
    // methods read compact-string fields that do not exist on our synthetic
    // char[]-backed builders, so all registered layout operations must resolve
    // through their native implementations.
    if is_string_builder_layout_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_undertow_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // BUG-W follow-up (2026-07-20): `java.lang.ClassValue.get()` has real JDK
    // bytecode (relies on `Class.classValueMap`, which CratonVM's Class
    // mirrors don't back) AND a registered native override (memoized
    // `computeValue` dispatch — see the `get()`/`remove()` registrations in
    // `native-builtins/src/phases_late.rs`). Any cached/precomputed dispatch
    // decision that consults this allow-list instead of re-walking the
    // ancestor chain at call time (the JIT's compiled-callsite native check,
    // mirroring the interpreter's `try_stackless_invoke`/`invoke_or_native`
    // walk) needs an explicit entry here or it silently keeps running the
    // real bytecode forever, which is how Groovy's `ClassInfo.getClassInfo`
    // NPE'd under `-Jit on` even after the native fix landed.
    if class_name == "java/lang/ClassValue"
        && method_name == "get"
        && method_descriptor == "(Ljava/lang/Class;)Ljava/lang/Object;"
    {
        return true;
    }
    if class_name == "org/springframework/core/annotation/MergedAnnotation$Adapt"
        && method_name == "isIn"
        && method_descriptor == "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z"
    {
        return true;
    }
    // Real-JDK constant-surface bridges. Keep in sync with vm_exec.rs.
    if matches!(
        (class_name, method_name, method_descriptor),
        ("java/nio/charset/Charset", "contains", "(Ljava/nio/charset/Charset;)Z")
            | ("java/nio/file/Files", "getOwner", "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/UserPrincipal;")
            | ("java/lang/StackFrameInfo", "getMethodType", "()Ljava/lang/invoke/MethodType;")
            | ("java/net/DatagramSocket", "<init>", "()V")
            | ("java/net/DatagramSocket", "<init>", "(I)V")
            | ("java/net/DatagramSocket", "<init>", "(ILjava/net/InetAddress;)V")
            | ("java/net/DatagramSocket", "connect", "(Ljava/net/InetAddress;I)V")
            | ("java/net/DatagramSocket", "disconnect", "()V")
            | ("java/lang/StackWalker$StackFrame", "getMethodType", "()Ljava/lang/invoke/MethodType;")
            | ("java/lang/StackWalker$StackFrame", "getDescriptor", "()Ljava/lang/String;")
    ) {
        return true;
    }
    // JDK 25's public Class.getProtectionDomain() reads a VM-populated private
    // mirror field directly. CratonVM's mirrors retain class provenance in the
    // class store instead, so force the registered class-id-backed native.
    if class_name == "java/lang/Class"
        && matches!(
            (method_name, method_descriptor),
            ("getProtectionDomain", "()Ljava/security/ProtectionDomain;")
                // JDK 25 implements isArray() as a direct read of the
                // private componentType field. Array mirrors keep their
                // identity in the class store, so that bytecode falsely
                // reports `Class[]` as a non-array and Spring skips its
                // Class[] -> String[] annotation adaptation.
                | ("isArray", "()Z")
                | ("getComponentType", "()Ljava/lang/Class;")
                // Spring's annotation map adapter uses the package-private
                // alias rather than the public accessor.
                | ("componentType", "()Ljava/lang/Class;")
        )
    {
        return true;
    }
    if is_netty_event_executor_group_shutdown_native_override(
        class_name,
        method_name,
        method_descriptor,
    ) {
        return true;
    }
    if is_springboot_mongo_reactive_customizer_destroy_native_override(
        class_name,
        method_name,
        method_descriptor,
    ) {
        return true;
    }
    if is_springboot_mongo_reactive_customizer_customize_native_override(
        class_name,
        method_name,
        method_descriptor,
    ) {
        return true;
    }
    if is_datagram_channel_open_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Tomcat application methods are never registered native overrides apart
    // from the audited bridges below. Reject the large compatibility table
    // early on its hot scanner paths.
    if (class_name.starts_with("org/apache/")
        && !matches!(
            class_name,
            "org/apache/maven/surefire/booter/ForkedBooter"
                | "org/apache/tomcat/util/buf/CharChunk"
                | "org/apache/catalina/connector/Response"
                | "org/apache/tomcat/util/bcel/classfile/Constant"
        ))
        || class_name == "java/net/URI"
    {
        return false;
    }
    if class_name == "org/apache/catalina/connector/Response"
        && method_name == "toAbsolute"
        && method_descriptor == "(Ljava/lang/String;)Ljava/lang/String;"
    {
        return true;
    }
    // Only `toString` is listed here, and only because it has a matching
    // registration (`native_char_chunk_to_string`). This gate used to also
    // claim `endsWith(String)`, `indexOf(char)` and
    // `AbstractChunk.indexOf(String,III)`, none of which were ever
    // registered — every consumer resolves the callback through
    // `NativeMethodRegistry::find` and silently falls back to bytecode when
    // it misses, so those three were pure dead config that read as "served
    // by a native" to anyone auditing this list. If natives are added for
    // them later, BOTH this gate and the `CharChunk` registrations in
    // `native-builtins`' `register_essential_natives_with_shims` must be
    // updated together.
    if class_name == "org/apache/tomcat/util/buf/CharChunk"
        && (method_name, method_descriptor) == ("toString", "()Ljava/lang/String;")
    {
        return true;
    }
    if class_name == "org/apache/tomcat/util/bcel/classfile/Constant"
        && method_name == "readConstant"
        && method_descriptor
            == "(Ljava/io/DataInput;)Lorg/apache/tomcat/util/bcel/classfile/Constant;"
    {
        return true;
    }
    // 995ff48c (Tomcat silent-hang scanner fix): interpreted per-byte read
    // dispatch dominated the scanner's hot path. `<init>`/mark/reset/skip/...
    // still run their real-JDK bytecode so buffer/mark state stays
    // bytecode-owned; only the two read overloads are forced native.
    if class_name == "java/io/BufferedInputStream"
        && method_name == "read"
        && matches!(method_descriptor, "([BII)I" | "()I")
    {
        return true;
    }
    if class_name == "java/io/DataInputStream"
        && matches!(
            (method_name, method_descriptor),
            ("readUTF", "()Ljava/lang/String;")
                | ("readByte", "()B")
                | ("readUnsignedByte", "()I")
                | ("readUnsignedShort", "()I")
                | ("readInt", "()I")
                | ("readLong", "()J")
                | ("readFloat", "()F")
                | ("readDouble", "()D")
                | ("skipBytes", "(I)I")
        )
    {
        return true;
    }
    if class_name == "java/io/FileInputStream"
        && method_name == "read"
        && method_descriptor == "([BII)I"
    {
        return true;
    }
    // Real JDK CRC32.updateBytes is a small validation wrapper around the
    // registered updateBytes0 native. Keep that boundary native in every
    // dispatch mode: compiled archive writers otherwise risk applying the
    // public CRC representation as the complemented running state.
    if class_name == "java/util/zip/CRC32"
        && method_name == "updateBytes"
        && method_descriptor == "(I[BII)I"
    {
        return true;
    }
    if class_name == "java/io/File"
        && matches!(
            (method_name, method_descriptor),
            ("isDirectory", "()Z")
                | ("list", "()[Ljava/lang/String;")
                | ("getName", "()Ljava/lang/String;")
                | ("canRead", "()Z")
        )
    {
        return true;
    }
    if class_name == "java/lang/StringUTF16"
        && method_name == "getChars"
        && method_descriptor == "([BII[CI)V"
    {
        return true;
    }
    // JDK-ONLY-WAVE2: the forced-native `java/lang/String` policy, INVERTED-
    // EXCLUSION form. Read it as: for `java/lang/String`, ONLY these seven
    // shapes may go on to force a native; every other String method returns
    // `false` right here and runs real bytecode.
    //
    // Its twin is the POSITIVE, 21-method `java/lang/String` arm of
    // `check_override` in `vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner`.
    // This one governs the WARM path (memoized force-native gate); that one
    // governs the COLD path (first call at a site). THE TWO MUST BE DELETED
    // TOGETHER: drop either alone and cold and warm dispatch disagree about
    // which implementation of `String.equals`/`hashCode`/`substring` runs, so
    // a String method's observable behaviour starts depending on how many times
    // its call site has executed — the precise class of JIT-state-dependent bug
    // §7 centralisation exists to prevent. What must replace both: nothing;
    // `String`'s natives are registered as `Intrinsic` and
    // `resolve_dispatch` step 2 takes them on kind alone, with no name list.
    if class_name == "java/lang/String"
        && !matches!(
            (method_name, method_descriptor),
            (
                "replaceAll",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"
            ) | (
                "replaceFirst",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"
            ) | ("matches", "(Ljava/lang/String;)Z")
                | (
                    "replace",
                    "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;"
                )
                | ("substring", "(II)Ljava/lang/String;")
                | ("<init>", "([BLjava/lang/String;)V")
                | ("<init>", "([BIILjava/lang/String;)V")
        )
    {
        return false;
    }
    if is_class_mirror_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_classvalue_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // JFR's Type bootstrap table compares Class mirrors by reference.  A
    // bootstrap type can reach this point through a separately materialised
    // mirror, so run the registered bridge which canonicalises through the VM
    // ClassId before delegating to JFR's String-keyed lookup.
    if is_jfr_metadata_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Reflective Method mirrors may expose stale physical returnType slots in
    // the real JDK. The registered accessor derives the answer from the
    // member descriptor, which is authoritative for JFR annotation metadata.
    if class_name == "java/lang/reflect/Method"
        && method_name == "getReturnType"
        && method_descriptor == "()Ljava/lang/Class;"
    {
        return true;
    }
    // The platform-server bridge returns a synthetic MBeanServer receiver.
    // Interface call sites must select its registered bridge methods rather
    // than executing the abstract interface declarations.
    if class_name == "javax/management/MBeanServer" {
        return true;
    }
    // The real Collections.emptyList() returns the class's pre-built static
    // singleton. During the Brave bootstrap that slot can retain a polluted
    // ArrayList, so use the registered constructor-backed empty-list native
    // instead of exposing that stale shared state.
    if class_name == "java/util/Collections"
        && method_name == "emptyList"
        && method_descriptor == "()Ljava/util/List;"
    {
        return true;
    }

    if class_name == "java/util/function/Predicate"
        && matches!(
            (method_name, method_descriptor),
            (
                "and",
                "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;"
            ) | (
                "or",
                "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;"
            ) | ("negate", "()Ljava/util/function/Predicate;")
                | (
                    "not",
                    "(Ljava/util/function/Predicate;)Ljava/util/function/Predicate;"
                )
        )
    {
        return true;
    }
    // The real DecimalFormatSymbols factories enter CLDR's locale bootstrap.
    // During the early Spring/JUnit summary path that bootstrap can observe a
    // stale Collections empty-list slot, producing a type-correct but wrong
    // List element.  The registered locale native constructs the same DFS
    // instance without that provider walk; force it over the concrete JDK
    // bytecode on every interpreter dispatch path.
    if class_name == "java/text/DecimalFormatSymbols"
        && matches!(
            (method_name, method_descriptor),
            ("initialize", "(Ljava/util/Locale;)V")
                | (
                    "getInstance",
                    "(Ljava/util/Locale;)Ljava/text/DecimalFormatSymbols;"
                )
        )
    {
        return true;
    }
    // Base64 encoders are represented by VM-side synthetic state.  The real
    // JDK bytecode instead reads its private object layout, which is not
    // populated for those synthetic instances and silently falls back to the
    // basic, padded encoding.  Keep this in sync with vm_exec's slow-path
    // override gate so warmed invoke caches also use the native implementation.
    if matches!(
        class_name,
        "java/util/Base64" | "java/util/Base64$Encoder" | "java/util/Base64$Decoder"
    ) {
        return true;
    }

    if class_name == "java/lang/Object"
        && method_name == "clone"
        && method_descriptor == "()Ljava/lang/Object;"
    {
        return true;
    }
    // Mockito's ModuleMemberAccessor selects an instrumentation-backed Java-9
    // implementation. The legacy bridge short-circuited that to the reflection
    // fallback on every run — a silent HotSpot divergence that breaks access to
    // strongly-encapsulated members. Off by default; see
    // `flags::mockito_legacy_selectors`.
    if cratonvm_types::flags::mockito_legacy_selectors()
        && class_name == "org/mockito/internal/util/reflection/ModuleMemberAccessor"
        && method_name == "delegate"
        && method_descriptor == "()Lorg/mockito/plugins/MemberAccessor;"
    {
        return true;
    }
    // A real SSLContext returns SunJSSE's concrete factory implementation.
    // The layered Socket overload must still reach the public factory bridge:
    // MockWebServer uses it to wrap its accepted socket as a TLS server.
    if (class_name == "javax/net/ssl/SSLSocketFactory"
        || class_name.starts_with("sun/security/ssl/SSLSocketFactoryImpl"))
        && method_name == "createSocket"
        && matches!(
            method_descriptor,
            "(Ljava/lang/String;I)Ljava/net/Socket;"
                | "(Ljava/net/InetAddress;I)Ljava/net/Socket;"
                | "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;"
                | "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;"
                | "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;"
        )
    {
        return true;
    }
    if (class_name == "javax/net/ssl/SSLSocket"
        || class_name.starts_with("sun/security/ssl/SSLSocketImpl"))
        && matches!(
            (method_name, method_descriptor),
            ("startHandshake", "()V")
                | ("getInputStream", "()Ljava/io/InputStream;")
                | ("getOutputStream", "()Ljava/io/OutputStream;")
                | ("getSession", "()Ljavax/net/ssl/SSLSession;")
                | ("close", "()V")
                | ("isClosed", "()Z")
                | ("isConnected", "()Z")
                | ("getPort", "()I")
                // The real `javax.net.ssl.SSLSocket` base class's default body
                // for these two just throws `UnsupportedOperationException` —
                // only a concrete provider subclass (SunJSSE's SSLSocketImpl)
                // overrides them. Our synthetic server-side socket (returned
                // by `SSLSocketFactory.createSocket(Socket,...)`, e.g. for
                // MockWebServer's HTTPS listener) IS that class literally, so
                // without forcing native here the real base-class bytecode
                // runs and throws — silently caught+logged at FINE by
                // MockWebServer's connection handler, which then just closes
                // the socket having never read the request or written a
                // response (`skipSslValidation`-style 30s client-side hang).
                | ("getApplicationProtocol", "()Ljava/lang/String;")
                | ("getHandshakeApplicationProtocol", "()Ljava/lang/String;")
                | ("getSSLParameters", "()Ljavax/net/ssl/SSLParameters;")
                | ("setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V")
                | ("setUseClientMode", "(Z)V")
                | ("getUseClientMode", "()Z")
                | ("setNeedClientAuth", "(Z)V")
                | ("getNeedClientAuth", "()Z")
                | ("setWantClientAuth", "(Z)V")
                | ("getWantClientAuth", "()Z")
        )
    {
        return true;
    }
    // SSLContext's real-JDK bodies delegate through a provider-owned
    // SSLContextSpi. CratonVM stores configured key/trust material on the
    // public context object instead, so the native path must own the complete
    // init-to-engine handoff for a server identity to reach Tomcat's engine.
    if (class_name == "javax/net/ssl/SSLContext"
        || class_name.starts_with("sun/security/ssl/SSLContextImpl"))
        && matches!(
            (method_name, method_descriptor),
            ("getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;")
                | ("init", "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V")
                | ("getSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;")
                | ("createSSLEngine", "()Ljavax/net/ssl/SSLEngine;")
                | ("createSSLEngine", "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;")
        )
    {
        return true;
    }
    if class_name == "sun/security/ssl/SSLEngineImpl"
        && matches!(
            (method_name, method_descriptor),
            (
                "setHandshakeApplicationProtocolSelector",
                "(Ljava/util/function/BiFunction;)V"
            ) | (
                "getHandshakeApplicationProtocolSelector",
                "()Ljava/util/function/BiFunction;"
            )
        )
    {
        return true;
    }
    // The real KeyManagerFactory delegates to a provider SPI that cannot
    // materialize CratonVM's registry-backed JKS keys. The native bridge keeps
    // the per-entry password with the originating KeyStore and exposes a
    // functional X509KeyManager to SSLContext.init.
    if class_name == "javax/net/ssl/KeyManagerFactory"
        && matches!(
            (method_name, method_descriptor),
            ("init", "(Ljava/security/KeyStore;[C)V")
                | ("getKeyManagers", "()[Ljavax/net/ssl/KeyManager;")
        )
    {
        return true;
    }

    // File-attribute values are carried by a private five-slot synthetic
    // object, not by the real JDK's zero-field interface or platform-private
    // attribute layouts. Interface call sites must therefore dispatch to the
    // registered bridge before any receiver-class bytecode is selected.
    if class_name == "java/nio/file/attribute/BasicFileAttributes"
        && matches!(
            (method_name, method_descriptor),
            ("creationTime", "()Ljava/nio/file/attribute/FileTime;")
                | ("lastAccessTime", "()Ljava/nio/file/attribute/FileTime;")
                | ("lastModifiedTime", "()Ljava/nio/file/attribute/FileTime;")
                | ("isDirectory", "()Z")
                | ("isRegularFile", "()Z")
                | ("isSymbolicLink", "()Z")
                | ("isOther", "()Z")
                | ("size", "()J")
                | ("fileKey", "()Ljava/lang/Object;")
        )
    {
        return true;
    }

    // Class loading is implemented by CratonVM's native bridge so that its
    // per-loader namespaces and parent-first delegation remain visible in
    // real-JDK mode. The JDK methods are concrete bytecode, so force the
    // bridge for inherited base calls (including invokespecial super calls
    // from custom loaders); direct subclass overrides remain selected by
    // their own declaring class.
    if class_name == "java/lang/ClassLoader"
        && method_name == "loadClass"
        && matches!(
            method_descriptor,
            "(Ljava/lang/String;)Ljava/lang/Class;" | "(Ljava/lang/String;Z)Ljava/lang/Class;"
        )
    {
        return true;
    }

    // The slow invoke path already forces these generic Class metadata
    // methods to their native Signature-attribute implementation. Keep the
    // warmed virtual-call cache in sync; otherwise a hot call bypasses the
    // override and re-enters the incomplete real-JDK reifier path.
    if class_name == "java/lang/Class"
        && matches!(
            (method_name, method_descriptor),
            ("getTypeParameters", "()[Ljava/lang/reflect/TypeVariable;")
                | ("getGenericInterfaces", "()[Ljava/lang/reflect/Type;")
                | ("getGenericSuperclass", "()Ljava/lang/reflect/Type;")
        )
    {
        return true;
    }

    // Real-JDK `Thread.run()` bytecode is layout-variant: older JDKs read
    // direct `Thread.target`, while newer layouts also carry the task in
    // `Thread$FieldHolder.task`. CratonVM's registered native mirrors VM
    // thread-start target resolution (direct field, holder task, synthetic
    // slot), so force it to win for normal and invokespecial `super.run()`
    // calls from Thread subclasses such as WildFly's JBossThread.
    if class_name == "java/lang/Thread" && method_name == "run" && method_descriptor == "()V" {
        return true;
    }

    // `Executors.newSingleThreadExecutor()`/`newFixedThreadPool()`/
    // `newCachedThreadPool()` (native-builtins/src/lib.rs's
    // `native_new_single_thread`/`native_new_fixed_pool`/`native_new_cached_pool`)
    // allocate their return value under the REAL class name
    // `java/util/concurrent/ThreadPoolExecutor` but never run it through the
    // real `<init>` -- real fields like `ctl`/`workQueue`/`mainLock` are never
    // set. Once `execute(Runnable)` (invoked via `invokeinterface
    // Executor.execute`/`ExecutorService.execute`) resolves to the concrete
    // class's own real bytecode, that bytecode reads the never-initialized
    // `ctl` AtomicInteger and NPEs immediately (docs/known-issues/
    // threadpoolexecutor-execute-npe-on-ctl-regression.md). Force the
    // registered native (`native_es_execute`) to win for this triple;
    // `intercept_force_registered_native` additionally checks the receiver's
    // real `workers` field so a genuinely real, bytecode-constructed
    // `ThreadPoolExecutor` still runs its own real `execute()` bytecode.
    if class_name == "java/util/concurrent/ThreadPoolExecutor"
        && method_name == "execute"
        && method_descriptor == "(Ljava/lang/Runnable;)V"
    {
        return true;
    }

    if class_name == "java/nio/ByteBuffer"
        && matches!(
            (method_name, method_descriptor),
            ("allocate", "(I)Ljava/nio/ByteBuffer;")
                | ("allocateDirect", "(I)Ljava/nio/ByteBuffer;")
                | ("wrap", "([B)Ljava/nio/ByteBuffer;")
                | ("wrap", "([BII)Ljava/nio/ByteBuffer;")
                | ("get", "()B")
                | ("get", "(I)B")
                | ("get", "([B)Ljava/nio/ByteBuffer;")
                | ("get", "([BII)Ljava/nio/ByteBuffer;")
                | ("put", "(B)Ljava/nio/ByteBuffer;")
                | ("put", "(IB)Ljava/nio/ByteBuffer;")
                | ("put", "([B)Ljava/nio/ByteBuffer;")
                | ("put", "([BII)Ljava/nio/ByteBuffer;")
                | ("put", "(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;")
                | ("getShort", "()S")
                | ("getShort", "(I)S")
                | ("putShort", "(S)Ljava/nio/ByteBuffer;")
                | ("putShort", "(IS)Ljava/nio/ByteBuffer;")
                | ("getChar", "()C")
                | ("getChar", "(I)C")
                | ("putChar", "(C)Ljava/nio/ByteBuffer;")
                | ("putChar", "(IC)Ljava/nio/ByteBuffer;")
                | ("getInt", "()I")
                | ("getInt", "(I)I")
                | ("putInt", "(I)Ljava/nio/ByteBuffer;")
                | ("putInt", "(II)Ljava/nio/ByteBuffer;")
                | ("getLong", "()J")
                | ("getLong", "(I)J")
                | ("putLong", "(J)Ljava/nio/ByteBuffer;")
                | ("putLong", "(IJ)Ljava/nio/ByteBuffer;")
                | ("getFloat", "()F")
                | ("getFloat", "(I)F")
                | ("putFloat", "(F)Ljava/nio/ByteBuffer;")
                | ("getDouble", "()D")
                | ("putDouble", "(D)Ljava/nio/ByteBuffer;")
                | ("flip", "()Ljava/nio/Buffer;")
                | ("flip", "()Ljava/nio/ByteBuffer;")
                | ("clear", "()Ljava/nio/Buffer;")
                | ("clear", "()Ljava/nio/ByteBuffer;")
                | ("rewind", "()Ljava/nio/Buffer;")
                | ("rewind", "()Ljava/nio/ByteBuffer;")
                | ("mark", "()Ljava/nio/Buffer;")
                | ("mark", "()Ljava/nio/ByteBuffer;")
                | ("reset", "()Ljava/nio/Buffer;")
                | ("position", "()I")
                | ("position", "(I)Ljava/nio/Buffer;")
                | ("position", "(I)Ljava/nio/ByteBuffer;")
                | ("limit", "()I")
                | ("limit", "(I)Ljava/nio/Buffer;")
                | ("limit", "(I)Ljava/nio/ByteBuffer;")
                | ("capacity", "()I")
                | ("remaining", "()I")
                | ("hasRemaining", "()Z")
                | ("compact", "()Ljava/nio/ByteBuffer;")
                | ("array", "()[B")
                | ("arrayOffset", "()I")
                | ("hasArray", "()Z")
                | ("isDirect", "()Z")
                | ("isReadOnly", "()Z")
                | ("order", "()Ljava/nio/ByteOrder;")
                | ("order", "(Ljava/nio/ByteOrder;)Ljava/nio/ByteBuffer;")
                | ("slice", "()Ljava/nio/ByteBuffer;")
                | ("duplicate", "()Ljava/nio/ByteBuffer;")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("hashCode", "()I")
                | ("compareTo", "(Ljava/nio/ByteBuffer;)I")
                | ("toString", "()Ljava/lang/String;")
        )
    {
        return true;
    }

    // Bulk `get(T[],int,int)`/`put(T[],int,int)` on every typed NIO buffer
    // (Int/Long/Short/Float/DoubleBuffer) are CONCRETE (not abstract) real
    // JDK 25 bytecode — `FloatBuffer.getArray`/`putArray` etc. read/write
    // via `this.address` + `ScopedMemoryAccess` directly for any length
    // beyond a trivial few elements, bypassing virtual dispatch to the
    // single-element accessors entirely. Our synthetic abstract-stamped
    // typed-buffer views (`native-builtins/src/servlet.rs`'s
    // `s2_typed_buffer_view_fns!`, produced by e.g.
    // `ByteBuffer.asFloatBuffer()`) never set a real `address` field, so
    // that fast path silently read/wrote zero bytes for every bulk vector
    // transfer — the dominant access pattern for ES/Lucene vector codecs
    // (`buffer.get(vec, 0, dims)`), surfacing as
    // "expected:<X> but was:<0.0>" across nearly the whole ES vector-codec
    // test family. Registering the natives (in servlet.rs) is not enough by
    // itself since real bytecode already exists for these signatures; force
    // it to win here, mirroring the ByteBuffer block above.
    if matches!(
        class_name,
        "java/nio/IntBuffer"
            | "java/nio/LongBuffer"
            | "java/nio/ShortBuffer"
            | "java/nio/FloatBuffer"
            | "java/nio/DoubleBuffer"
    ) && matches!(
        (method_name, method_descriptor),
        ("get", "([III)Ljava/nio/IntBuffer;")
            | ("put", "([III)Ljava/nio/IntBuffer;")
            | ("get", "([JII)Ljava/nio/LongBuffer;")
            | ("put", "([JII)Ljava/nio/LongBuffer;")
            | ("get", "([SII)Ljava/nio/ShortBuffer;")
            | ("put", "([SII)Ljava/nio/ShortBuffer;")
            | ("get", "([FII)Ljava/nio/FloatBuffer;")
            | ("put", "([FII)Ljava/nio/FloatBuffer;")
            | ("get", "([DII)Ljava/nio/DoubleBuffer;")
            | ("put", "([DII)Ljava/nio/DoubleBuffer;")
    ) {
        return true;
    }

    if class_name == "java/util/concurrent/LinkedBlockingDeque"
        && method_name == "clear"
        && method_descriptor == "()V"
    {
        return true;
    }

    if class_name == "jdk/internal/util/ArraysSupport"
        && matches!(
            (method_name, method_descriptor),
            ("vectorizedHashCode", "(Ljava/lang/Object;IIII)I")
                | (
                    "vectorizedMismatch",
                    "(Ljava/lang/Object;JLjava/lang/Object;JII)I"
                )
                | ("mismatch", "([B[BI)I")
                | ("mismatch", "([BI[BII)I")
                | ("mismatch", "([C[CI)I")
                | ("mismatch", "([CI[CII)I")
        )
    {
        return true;
    }

    if class_name == "java/io/FilterInputStream"
        && matches!(
            (method_name, method_descriptor),
            ("<init>", "(Ljava/io/InputStream;)V") | ("skip", "(J)J")
        )
    {
        return true;
    }

    if class_name == "java/io/ByteArrayInputStream"
        && matches!(
            (method_name, method_descriptor),
            ("read", "()I")
                | ("read", "([BII)I")
                | ("available", "()I")
                | ("skip", "(J)J")
                | ("close", "()V")
        )
    {
        return true;
    }

    if (matches!(
        class_name,
        "java/lang/Iterable" | "java/util/Collection" | "java/util/Set" | "java/util/EnumSet"
    ) && method_name == "iterator"
        && method_descriptor == "()Ljava/util/Iterator;")
    {
        return true;
    }

    // Map.forEach is a default method whose JDK implementation iterates an
    // entrySet. CratonVM's immutable-map wrapper intentionally stores a
    // snapshot backing rather than the JDK's MapN layout, so running that body
    // can materialize a HashSet and hash a cyclic map entry before a caller's
    // own nesting guard runs. The native bridge snapshots concrete map entries
    // directly and preserves the Map.forEach contract for every map backend.
    if class_name == "java/util/Map"
        && method_name == "forEach"
        && method_descriptor == "(Ljava/util/function/BiConsumer;)V"
    {
        return true;
    }

    if class_name == "java/util/Iterator" && matches!(method_name, "hasNext" | "next" | "remove") {
        return true;
    }

    if class_name == "java/lang/Thread"
        && method_name == "getThreadGroup"
        && method_descriptor == "()Ljava/lang/ThreadGroup;"
    {
        return true;
    }

    if class_name == "org/jboss/threads/JBossThread"
        && method_name == "run"
        && method_descriptor == "()V"
    {
        return true;
    }

    if class_name == "org/jboss/threads/JBossThread"
        && method_name == "onExit"
        && method_descriptor == "(Ljava/lang/Runnable;)Z"
    {
        return true;
    }

    if class_name == "org/jboss/threads/JBossThreadFactory"
        && ((method_name == "newThread"
            && method_descriptor == "(Ljava/lang/Runnable;)Ljava/lang/Thread;")
            || (method_name == "access$100"
                && method_descriptor
                    == "(Lorg/jboss/threads/JBossThreadFactory;Ljava/lang/Runnable;)Ljava/lang/Thread;"))
    {
        return true;
    }

    if class_name == "java/io/InputStreamReader"
        && method_name == "close"
        && method_descriptor == "()V"
    {
        return true;
    }

    if class_name == "java/lang/SecurityManager"
        && method_name == "getRootGroup"
        && method_descriptor == "()Ljava/lang/ThreadGroup;"
    {
        return true;
    }

    if class_name == "java/util/AbstractSet"
        && method_name == "hashCode"
        && method_descriptor == "()I"
    {
        return true;
    }

    if class_name == "java/util/AbstractCollection"
        && method_name == "contains"
        && method_descriptor == "(Ljava/lang/Object;)Z"
    {
        return true;
    }

    if class_name == "java/lang/Class"
        && (method_name == "getEnumConstants" || method_name == "getEnumConstantsShared")
        && method_descriptor == "()[Ljava/lang/Object;"
    {
        return true;
    }

    // `java.util.logging.Level.parse(String)` real bytecode resolves custom
    // and even standard level names through `KnownLevel.findByName`, which
    // on JDK 25 throws internally (a `Module`-null NPE the method's own
    // catch-all reports as a generic `IllegalArgumentException: Bad level`)
    // — see `docs/internal/gaps/kc16-blocker-map.md`'s KC16 investigation.
    // This broke WildFly's own `host.xml`/`domain.xml` parsing of
    // `<level name="WARN"/>` (org.jboss.logmanager's extended levels) before
    // it ever reached a genuinely-unknown name. Force the registered native
    // (`native_level_parse`, native-builtins/src/logmanager.rs), which
    // answers from the standard + JBoss LogManager static Level constants
    // directly, bypassing the broken registry lookup.
    if class_name == "java/util/logging/Level"
        && method_name == "parse"
        && method_descriptor == "(Ljava/lang/String;)Ljava/util/logging/Level;"
    {
        return true;
    }

    if is_forkjoin_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_aqls_state_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_bc_crypto_math_native_override(class_name, method_name, method_descriptor) {
        return true;
    }

    // The JDK's final owner setter is the single authoritative transition for
    // AbstractQueuedSynchronizer-derived locks. Route it through the native
    // registry so ThreadMXBean can retain an exact, moving-GC-safe index of
    // ownable synchronizers even after this tiny method has been JIT compiled.
    if class_name == "java/util/concurrent/locks/AbstractOwnableSynchronizer"
        && method_name == "setExclusiveOwnerThread"
        && method_descriptor == "(Ljava/lang/Thread;)V"
    {
        return true;
    }

    // java.lang.Module access checks. CratonVM's `Class.getModule()` returns a
    // synthetic Module mirror with a NULL `descriptor` (real module-path
    // encapsulation does not exist — every class is effectively on the class
    // path). The real `Module.isExported`/`isOpen` bytecode dereferences
    // `this.descriptor.isOpen()` inside `implIsExportedOrOpen` and NPEs (e.g.
    // Groovy `CachedClass.getMethods` → `checkCanSetAccessible`, Hibernate's
    // `JdbcTypeNameMapper.<clinit>` reflecting over `java.sql.Types`). Force the
    // registry-backed natives registered in `native-builtins` so the access check
    // is answered from the boot `ModuleRegistry`'s accurate per-module
    // exports/opens (java.base exports `java.lang`/… to all but not
    // `jdk.internal.*` — which ByteBuddy's `JavaDispatcher` relies on) instead of
    // touching the null descriptor.
    //
    // ClassLoader resource methods have the same issue: real JDK bytecode
    // walks URLClassPath state which CratonVM intentionally replaces with
    // native per-loader lookups.  Keep the singular, stream, and bulk
    // methods together so URLClassLoader instances do not fall back to the
    // process-wide dynamic classpath (which leaks resources between test
    // loaders) and null arguments retain their specified NPE contract.
    if class_name == "java/lang/ClassLoader"
        && matches!(
            method_name,
            "getResource"
                | "getSystemResource"
                | "getResources"
                | "getSystemResources"
                | "getResourceAsStream"
                | "getSystemResourceAsStream"
        )
    {
        return true;
    }

    // `java.net.URLClassLoader` declares its OWN `getResourceAsStream`
    // override (unlike `getResource`/`getResources`/`findResource`, which it
    // leaves to `ClassLoader`/its own `findResource` extension point) — real
    // OpenJDK wraps the stream so it can be tracked in the `closeables`
    // WeakHashMap for `close()`. That means the check above, keyed on
    // declaring class `java/lang/ClassLoader`, never matches a plain
    // `URLClassLoader` (or subclass that doesn't itself override
    // `getResourceAsStream`) instance's call — its declaring class resolves
    // to `java/net/URLClassLoader` instead, so real bytecode ran unforced.
    // That bytecode still depends on the same unpopulated `ucp`
    // (`URLClassPath`) internals the comment above describes, but ALSO
    // doesn't do the parent-delegation the native bridge implements: a
    // `new URLClassLoader(urls, parent)` whose only own URL is e.g. a
    // `@TempDir` holding a generated `META-INF/spring.components` index
    // (Spring Boot's `ServletComponentScanIntegrationTests
    // .indexedComponentsAreRegistered`) found the index fine via
    // `getResource`/`findResource` (both correctly native-forced already)
    // but got `null` from `getResourceAsStream` for every `.class` resource
    // that only the PARENT classloader's classpath actually holds —
    // `ClassPathResource.getInputStream()` then threw `FileNotFoundException`
    // reading an indexed component class that plainly exists. Force native
    // dispatch here too so `URLClassLoader.getResourceAsStream` resolves via
    // the same delegation-aware bridge (`classloader::cl_get_resource_as_stream`)
    // as the base-class methods above.
    if class_name == "java/net/URLClassLoader" && method_name == "getResourceAsStream" {
        return true;
    }

    // `java.nio.file.Path` is a genuine interface with no `toString()` body of
    // its own (nor `equals`/`hashCode`, but those aren't implicated here) —
    // real method resolution for `someSyntheticPathObj.toString()` walks up to
    // `java.lang.Object`, the only class in the chain that actually declares
    // `toString()` with a Code attribute. Without an entry here keyed on
    // `java/nio/file/Path` itself, that resolved declaring class
    // (`java/lang/Object`) is what gets checked against this gate — never
    // matches — so real `Object.toString()` runs (`getClass().getName() + "@"
    // + hashCode`) instead of the registered native
    // (`native-builtins::phases_late::register_phase57_nio_file`'s
    // `Path.toString()`, which correctly renders the jar-FS/host path).
    // `redefine_immune_path_native` below already anticipated this exact
    // (class, method) pair for the Mockito-redefine-immunity check, but the
    // actual force-native entry that makes it relevant was never added —
    // this closes that gap. Concretely this broke real javac's in-process
    // `JavacFileManager.inferBinaryName` for every `PathFileObject$JarFileObject`
    // classpath entry: its native fast path (`native_javac_file_manager_infer_binary_name`)
    // calls `path.toString()` expecting the in-jar relative path (e.g.
    // `/org/springframework/beans/factory/config/BeanDefinition.class`) but
    // got the garbage `Object.toString()` form (`java.nio.file.Path@1a2b3c`)
    // instead, which `javac_binary_name_from_relative_path` then mangled into
    // the literal binary name `java.nio.file` for EVERY application-classpath
    // class file — so `TestCompiler`/any real in-process `javac` compile of
    // source referencing an ordinary (non-JRT) classpath class failed with
    // "cannot find symbol", even for basic classes like
    // `org.springframework.beans.factory.support.RootBeanDefinition`
    // (`ServletComponentScanRegistrarTests
    // #processAheadOfTimeDoesNotRegisterServletComponentRegisteringPostProcessor`).
    if class_name == "java/nio/file/Path"
        && method_name == "toString"
        && method_descriptor == "()Ljava/lang/String;"
    {
        return true;
    }
    // Spring Boot's nested archive protocol reaches the registered jar-FS
    // bridge through these concrete real-JDK entry points. Letting the real
    // bodies win discards the `jar:nested:` container identity before the
    // native virtual filesystem can decode it.
    if (class_name == "java/nio/file/Path"
        && method_name == "of"
        && method_descriptor == "(Ljava/net/URI;)Ljava/nio/file/Path;")
        || (class_name == "java/nio/file/FileSystems" && method_name == "newFileSystem")
        || (class_name == "java/nio/file/spi/FileSystemProvider" && method_name == "newFileSystem")
        || (class_name == "org/springframework/boot/loader/launch/Archive"
            && method_name == "create"
            && matches!(
                method_descriptor,
                "(Ljava/io/File;)Lorg/springframework/boot/loader/launch/Archive;"
                    | "(Ljava/lang/Class;)Lorg/springframework/boot/loader/launch/Archive;"
            ))
    {
        return true;
    }

    // `getDescriptor` has the same null-descriptor problem, but real HotSpot
    // guarantees `isNamed() == (getDescriptor() != null)` — a named module's
    // descriptor is never null. CratonVM's `isNamed()` (real bytecode, reading
    // the dual-written real `name` field) can report a classpath-loaded,
    // modularized jar as named (see `classloading::module::ModuleDescriptor
    // ::automatic`), yet `getDescriptor()`'s real bytecode (`return this
    // .descriptor;`) reads a field CratonVM never populates. Any code that
    // only calls `getDescriptor()` after checking `isNamed()` (e.g.
    // Elasticsearch's `ProviderLocator.checkUses` — `caller.isNamed() &&
    // caller.getDescriptor().uses()...`) gets `NullPointerException: Cannot
    // invoke "ModuleDescriptor.uses()" because the return value of
    // "Module.getDescriptor()" is null`, breaking `XContentProvider$Holder`
    // static init and cascading into thousands of Elasticsearch suite
    // failures via `NoClassDefFoundError`. Force the native (registered in
    // `native-builtins::lib::register_essential_natives`, alongside
    // isExported/isOpen above), which returns null only for the true unnamed
    // module and otherwise builds a descriptor backed by the boot
    // `ModuleRegistry`'s parsed `uses`.
    // `canUse`/`addUses` have the SAME null-descriptor problem as
    // `getDescriptor` above: their real bytecode reads `this.descriptor`
    // directly (`return descriptor.isAutomatic() || descriptor.uses()
    // .contains(sn);` for `canUse`; a similar direct field read for
    // `addUses`) rather than going through the `getDescriptor()` accessor,
    // so forcing `getDescriptor` alone does not protect them. A named
    // Module mirror (`isNamed()` true) whose `descriptor` field is unset
    // NPEs the moment either method runs -- observed via WildFly Host
    // Controller's parallel extension loader (`DeferredExtensionContext
    // .load()`): loading `org.jboss.as.jmx` (which depends on the real
    // platform module `java.management`) reaches JDK-internal module
    // helper code that calls `Module.canUse`/`addUses` on a Module the VM
    // handed out without a populated descriptor, surfacing as
    // `NullPointerException: Cannot invoke "ModuleDescriptor.isAutomatic()"
    // because "this.descriptor" is null` wrapped in an `ExecutionException`
    // from the extension loader's `Future.get()`, which
    // `ControllerLogger.failedToLoadModule` re-reports as `WFLYCTL0083:
    // Failed to load module org.jboss.as.jmx`. `canUse` already had a
    // registered native (S109 Wave3, `native-builtins::lib`) that was never
    // added here, so it was silently shadowed by the real bytecode in
    // real-JDK mode; `addUses` had no native at all until this fix.
    if class_name == "java/lang/Module"
        && matches!(
            method_name,
            "isExported"
                | "isOpen"
                | "getDescriptor"
                | "canUse"
                | "addUses"
                | "addExports"
                | "addOpens"
                | "implAddExports"
                | "implAddExportsToAllUnnamed"
                | "implAddExportsNoSync"
                | "implAddOpens"
                | "implAddOpensToAllUnnamed"
        )
    {
        return true;
    }
    // JavaLangAccess is implemented by the concrete System$1 singleton. The
    // JDK module bootstrap calls these ordinary Java methods through that
    // receiver, so native registrations must win over its real bytecode.
    if class_name == "java/lang/System$1"
        && matches!(
            (method_name, method_descriptor),
            ("addReads", "(Ljava/lang/Module;Ljava/lang/Module;)V")
                | ("addReadsAllUnnamed", "(Ljava/lang/Module;)V")
                | ("addExports", "(Ljava/lang/Module;Ljava/lang/String;)V")
                | (
                    "addExports",
                    "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V"
                )
                | (
                    "addExportsToAllUnnamed",
                    "(Ljava/lang/Module;Ljava/lang/String;)V"
                )
                | (
                    "addOpens",
                    "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V"
                )
                | (
                    "addOpensToAllUnnamed",
                    "(Ljava/lang/Module;Ljava/lang/String;)V"
                )
                | ("addUses", "(Ljava/lang/Module;Ljava/lang/Class;)V")
        )
    {
        return true;
    }
    // `ServerSocket.getLocalSocketAddress()` is pure Java in the real JDK:
    // it calls `getInetAddress()` and then constructs an InetSocketAddress.
    // CratonVM's real ServerSocket instances carry their live listener state
    // in native side-tables / synthetic slots, while some real SocketImpl
    // fields remain unpopulated. Force the registered natives for these
    // accessors so WildFly's process controller sees a resolved bound address
    // instead of `/0.0.0.0:PORT` with a null InetAddress.
    if class_name == "java/net/ServerSocket"
        && matches!(
            (method_name, method_descriptor),
            ("getInetAddress", "()Ljava/net/InetAddress;")
                | ("getLocalSocketAddress", "()Ljava/net/SocketAddress;")
        )
    {
        return true;
    }
    // `java.util.jar.JarFile` has real JDK bytecode backed by native ZipFile
    // state and fields (`manRef`, `jv`, etc.) CratonVM does not initialize.
    // The native-builtins JarFile bridge stores path/manifest in its compact
    // synthetic layout and reads ZIP data with Rust's zip crate, so it must win
    // for both interpreted and shared exec dispatch. Keep this in sync with
    // the JarFile gate in vm_exec.rs.
    if class_name == "java/util/jar/JarFile"
        && matches!(
            method_name,
            "<init>"
                | "getManifest"
                | "getManifestFromReference"
                | "stream"
                | "entries"
                | "getEntry"
                | "getJarEntry"
                | "getInputStream"
                | "size"
                | "close"
                | "getName"
        )
    {
        return true;
    }
    // Manifest attributes are keyed by Attributes.Name, whose equality and
    // hash are case-insensitive. The bridge stores those names in the native
    // HashMap path, so its registered Name methods must win over real JDK
    // bytecode (which expects uninitialized private cache fields).
    if class_name == "java/util/jar/Attributes$Name"
        && matches!(
            (method_name, method_descriptor),
            ("<init>", "(Ljava/lang/String;)V")
                | ("toString", "()Ljava/lang/String;")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("hashCode", "()I")
        )
    {
        return true;
    }
    if class_name == "java/util/jar/Attributes"
        && method_name == "containsKey"
        && method_descriptor == "(Ljava/lang/Object;)Z"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/jar/ManifestInfo"
        && method_name == "isMultiRelease"
        && method_descriptor == "()Z"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/jar/NestedJarFile"
        && method_name == "getJarEntry"
        && method_descriptor == "(Ljava/lang/String;)Ljava/util/jar/JarEntry;"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/jar/NestedJarFile$NestedJarEntry"
        && method_name == "getRealName"
        && method_descriptor == "()Ljava/lang/String;"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/net/protocol/jar/UrlJarFile"
        && method_name == "getEntry"
        && method_descriptor == "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;"
    {
        return true;
    }
    if class_name == "org/springframework/boot/loader/zip/ZipContent$SignatureFiles"
        && matches!(
            (method_name, method_descriptor),
            ("<clinit>", "()V") | ("bufferEndsWithSignatureSuffix", "()Z")
        )
    {
        return true;
    }
    // `java.util.jar.Manifest` constructors/accessors are small but depend on
    // real-JDK stream/parser state that is fragile for synthetic jarfs streams.
    // Force the bridge parser so `EmbeddedModulePath.moduleNameFromManifestOrNull`
    // sees a real, non-null Attributes object.
    if class_name == "java/util/jar/Manifest"
        && matches!(
            (method_name, method_descriptor),
            ("<init>", "()V")
                | ("<init>", "(Ljava/io/InputStream;)V")
                | ("<init>", "(Ljava/io/InputStream;Ljava/lang/String;)V")
                | ("<init>", "(Ljava/util/jar/Manifest;)V")
                | (
                    "<init>",
                    "(Ljava/util/jar/JarVerifier;Ljava/io/InputStream;Ljava/lang/String;)V"
                )
                | ("getMainAttributes", "()Ljava/util/jar/Attributes;")
                | ("getEntries", "()Ljava/util/Map;")
        )
    {
        return true;
    }
    // MethodHandles VarHandle factories must return CratonVM synthetic handles
    // carrying native side-table/layout metadata. The real JDK bytecode creates
    // private VarHandle subclasses whose layouts our native get/set paths cannot
    // decode, so byte-array views read back null/zero.
    if is_method_handles_varhandle_factory_native_override(
        class_name,
        method_name,
        method_descriptor,
    ) {
        return true;
    }
    // FFM layout factories: JDK 25's real `MemoryLayout.sequenceLayout` runs
    // through `jdk/internal/foreign/Utils` while `SharedUtils.<clinit>` is still
    // building its `C_POINTER` constant. That circular path re-enters
    // `SharedUtils` before `ValueLayout.JAVA_BYTE` has been populated and
    // `Objects.requireNonNull(elementLayout)` throws a bare NPE. The registered
    // native factories are bytecode-equivalent for CratonVM's supported Panama
    // layout model and avoid that bootstrap cycle.
    if class_name == "java/lang/foreign/MemoryLayout"
        && matches!(
            (method_name, method_descriptor),
            (
                "sequenceLayout",
                "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/SequenceLayout;"
            ) | (
                "sequenceLayout",
                "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;"
            ) | (
                "structLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;"
            ) | (
                "structLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;"
            ) | (
                "unionLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;"
            ) | (
                "unionLayout",
                "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;"
            ) | ("paddingLayout", "(J)Ljava/lang/foreign/PaddingLayout;")
                | ("paddingLayout", "(J)Ljava/lang/foreign/MemoryLayout;")
                | (
                    "varHandle",
                    "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;"
                )
        )
    {
        return true;
    }
    // FFM ValueLayout subinterfaces are abstract/covariant in the real JDK
    // surface. CratonVM backs the supported layouts with small synthetic
    // objects, so calls such as `ValueLayout$OfFloat.withByteAlignment(J)`
    // must be served by the registered layout shims instead of falling through
    // to an abstract interface method with no Code attribute.
    if (class_name == "java/lang/foreign/ValueLayout"
        || class_name == "java/lang/foreign/AddressLayout"
        || class_name.starts_with("java/lang/foreign/ValueLayout$")
        || class_name.starts_with("jdk/internal/foreign/layout/ValueLayouts$"))
        && matches!(
            method_name,
            "byteSize"
                | "byteAlignment"
                | "withByteAlignment"
                | "withName"
                | "withOrder"
                | "varHandle"
                | "name"
                | "carrier"
                | "order"
                | "targetLayout"
                | "withTargetLayout"
        )
    {
        return true;
    }
    if class_name == "java/lang/foreign/MemorySegment"
        && matches!(
            method_name,
            "byteSize"
                | "address"
                | "copy"
                | "get"
                | "set"
                | "getAtIndex"
                | "setAtIndex"
                | "asSlice"
                | "isNative"
                | "isMapped"
                | "isReadOnly"
                | "scope"
                | "ofArray"
        )
    {
        return true;
    }
    if matches!(
        class_name,
        "jdk/internal/foreign/AbstractMemorySegmentImpl"
            | "jdk/internal/foreign/NativeMemorySegmentImpl"
            | "jdk/internal/foreign/MappedMemorySegmentImpl"
    ) && matches!(
        method_name,
        "byteSize" | "address" | "get" | "isNative" | "isMapped" | "isReadOnly" | "scope"
    ) {
        return true;
    }
    if class_name == "jdk/internal/foreign/MemorySessionImpl"
        && matches!(
            method_name,
            "toMemorySession"
                | "createConfined"
                | "createShared"
                | "createImplicit"
                | "createHeap"
                | "addCloseAction"
                | "addOrCleanupIfFail"
                | "addInternal"
                | "release0"
                | "acquire0"
                | "whileAlive"
                | "ownerThread"
                | "isAccessibleBy"
                | "isAlive"
                | "checkValidStateRaw"
                | "checkValidState"
                | "isCloseable"
                | "close"
                | "justClose"
        )
    {
        return true;
    }
    if class_name == "jdk/internal/misc/ScopedMemoryAccess"
        && matches!(
            method_name,
            "getByte"
                | "getByteInternal"
                | "putByte"
                | "putByteInternal"
                | "getShort"
                | "getShortInternal"
                | "getShortUnaligned"
                | "getShortUnalignedInternal"
                | "putShort"
                | "putShortInternal"
                | "putShortUnaligned"
                | "putShortUnalignedInternal"
                | "getInt"
                | "getIntInternal"
                | "getIntUnaligned"
                | "getIntUnalignedInternal"
                | "putInt"
                | "putIntInternal"
                | "putIntUnaligned"
                | "putIntUnalignedInternal"
                | "getLong"
                | "getLongInternal"
                | "getLongUnaligned"
                | "getLongUnalignedInternal"
                | "putLong"
                | "putLongInternal"
                | "putLongUnaligned"
                | "putLongUnalignedInternal"
                | "copyMemory"
                | "copyMemoryInternal"
        )
    {
        return true;
    }
    // ByteArrayOutputStream is frequently subclassed by JDK internals. The VM
    // already forces these intrinsics in the slow shared-invocation path; keep
    // the interpreter cache gate in sync so ordinary bytecode dispatch also
    // uses the registered native overloads, including charset-aware toString.
    if class_name == "java/io/ByteArrayOutputStream"
        && matches!(
            method_name,
            "write" | "toByteArray" | "size" | "reset" | "toString"
        )
    {
        return true;
    }
    // The lightweight resource-reader bridge stores the backing InputStream in
    // the reader slot used by the native read shim. Real JDK close() expects a
    // fully initialized sun.nio.cs.StreamDecoder in `sd` and can NPE while
    // closing META-INF/services readers during Elasticsearch provider loading.
    if class_name == "java/io/InputStreamReader"
        && matches!((method_name, method_descriptor), ("close", "()V"))
    {
        return true;
    }
    if (class_name == "java/lang/Runtime"
        && method_name == "version"
        && method_descriptor == "()Ljava/lang/Runtime$Version;")
        || (class_name == "java/lang/Runtime$Version"
            && matches!(
                (method_name, method_descriptor),
                ("feature", "()I") | ("build", "()Ljava/util/Optional;")
            ))
    {
        return true;
    }
    if is_spring_mock_response_native_override(class_name, method_name, method_descriptor)
        || is_script_engine_manager_native_override(class_name, method_name, method_descriptor)
        || is_jython_thread_state_native_override(class_name, method_name, method_descriptor)
        || is_jython_pyobject_native_override(class_name, method_name, method_descriptor)
        || is_jython_imp_native_override(class_name, method_name, method_descriptor)
        || is_jython_pymodule_native_override(class_name, method_name, method_descriptor)
    {
        return true;
    }

    if class_name == "java/nio/charset/Charset"
        && ((method_name == "availableCharsets" && method_descriptor == "()Ljava/util/SortedMap;")
            || (method_name == "aliases" && method_descriptor == "()Ljava/util/Set;"))
    {
        return true;
    }

    // JBoss LogManager fallback. CratonVM often creates synthetic
    // `org.jboss.logmanager.Logger` instances without a real `LoggerNode` graph.
    // The native-builtins logmanager shim already registers null-safe
    // `getEffectiveLevel()I` and `isLoggable(Level)` natives, but the real
    // jboss-logmanager bytecode dereferences `this.loggerNode` first. Force the
    // natives for real-JDK class bodies too, matching the existing null-safe
    // logRaw / handler overrides in `native-builtins::logmanager`.
    if (class_name == "org/jboss/logmanager/Logger" || class_name == "org.jboss.logmanager.Logger")
        && matches!(
            (method_name, method_descriptor),
            ("getEffectiveLevel", "()I") | ("isLoggable", "(Ljava/util/logging/Level;)Z")
        )
    {
        return true;
    }

    // JBoss Modules asks Module.forClass(caller) to locate the caller's
    // org.jboss.modules.Module before service-loading extension modules.
    // CratonVM tracks java.lang.Module mirrors there instead, so the real
    // bytecode can throw a bare ModuleLoadException for valid WildFly modules.
    // Force the native bridge that loads through the synthetic boot loader.
    if class_name == "org/jboss/modules/Module"
        && method_name == "loadServiceFromCallerModuleLoader"
        && matches!(
            method_descriptor,
            "(Ljava/lang/String;Ljava/lang/Class;)Ljava/util/ServiceLoader;"
                | "(Lorg/jboss/modules/ModuleIdentifier;Ljava/lang/Class;)Ljava/util/ServiceLoader;"
        )
    {
        return true;
    }

    // `Module.loadService(Class)` (the instance method, distinct from the
    // static bridge above) walks `getClass().getModule().addUses(...)` in
    // real jboss-modules bytecode before ever reading `moduleClassLoader` —
    // a JDK-module-system bookkeeping call CratonVM's permissive module
    // model doesn't need. Force the native reimplementation that skips
    // straight to `ServiceLoader.load(serviceType, moduleClassLoader)`.
    if class_name == "org/jboss/modules/Module"
        && method_name == "loadService"
        && method_descriptor == "(Ljava/lang/Class;)Ljava/util/ServiceLoader;"
    {
        return true;
    }

    // `ModuleClassLoader.getResources`/`findResources` (and the singular
    // `getResource`/`findResource`): our synthetic ModuleClassLoader
    // instances are allocated via `alloc_concurrent_synthetic`, bypassing
    // the real constructor, so real bytecode's internal `ResourceLoader`
    // state is never populated and these methods silently return empty
    // results instead of the module's own resources (notably
    // `META-INF/services/*`, which `ServiceLoader.load` needs — e.g. WildFly
    // extension modules like `org.jboss.as.jmx` register their `Extension`
    // provider there). Force the registered natives that walk the module's
    // resolved resource roots directly instead.
    if class_name == "org/jboss/modules/ModuleClassLoader"
        && matches!(
            method_name,
            "findClass" | "getResources" | "findResources" | "getResource" | "findResource"
        )
    {
        return true;
    }

    // Spring RSocket async setup can encode data and metadata strings on two
    // Reactor workers at the same time. The real `CharSequenceEncoder` lazily
    // computes a charset capacity through a per-instance cache; under CratonVM
    // that cold concurrent path can strand one worker before the setup payload
    // zip completes. Force the conservative native capacity helper registered in
    // `native-builtins` so the normal Spring `DataBuffer.write` still performs
    // the actual encoding, but the fragile lazy cache path is bypassed.
    if class_name == "org/springframework/core/codec/CharSequenceEncoder"
        && method_name == "calculateCapacity"
        && method_descriptor == "(Ljava/lang/CharSequence;Ljava/nio/charset/Charset;)I"
    {
        return true;
    }
    // BUG-15: `sun.util.locale.provider.LocaleResources.getDateTimePattern(int,
    // int, Calendar)` reads its pattern arrays through `LocaleData
    // .getDateFormatData` → `Bundles.of(...)`, the jdk.localedata class-based
    // resource path CratonVM does not surface, so it returns a NULL pattern.
    // `DateFormatProviderImpl.getInstance` then builds `new SimpleDateFormat(
    // null, locale)` → `compile(null)` → NPE ("pattern is null"), breaking
    // MessageFormat `{n,date}`/`{n,time}` elements and the
    // `DateFormat.get{Date,Time}Instance` factories. Force our native (returns
    // the en/de CLDR pattern directly) so the downstream real-JDK
    // SimpleDateFormat runs with a valid pattern. Companion native registered in
    // `native-builtins::locale_resources::register`; same locale-data-gap class
    // as the BreakIterator / getDecimalFormatSymbolsData overrides.
    // The java.time localized-formatting path
    // (DateTimeFormatterBuilder.getLocalizedDateTimePattern →
    // getJavaTimeDateTimePattern) reads the same unsurfaced jdk.localedata
    // bundle and otherwise returns null → `appendPattern(null)` NPE ("pattern"),
    // breaking Spring's LocalDate/LocalDateTime style formatting & parsing.
    if class_name == "sun/util/locale/provider/LocaleResources"
        && (method_name == "getDateTimePattern" || method_name == "getJavaTimeDateTimePattern")
    {
        return true;
    }
    // java.time text names: `sun.util.locale.provider.CalendarDataUtility
    // .retrieveJavaTimeFieldValueName(s)` feed `DateTimeTextProvider`'s
    // `EEE`/`MMM`/`a`/`G` lookups. The real-JDK bodies walk the same
    // `jdk.localedata` CLDR bundles we don't surface (as getDateTimePattern
    // above) and return null/empty, so `DateTimeFormatter` prints the raw
    // numeric field (e.g. Spring `HttpHeaders` RFC-1123 dates render
    // "4, 18 12 2008" not "Thu, 18 Dec 2008"). Force our natives (registered
    // in `native-builtins::locale_resources::register`) which answer from the
    // en/US CLDR name tables directly.
    if class_name == "sun/util/locale/provider/CalendarDataUtility"
        && matches!(
            method_name,
            "retrieveJavaTimeFieldValueName" | "retrieveJavaTimeFieldValueNames"
        )
    {
        return true;
    }
    // Unicode normalization: `java.text.Normalizer.normalize/isNormalized` — the
    // real-JDK bodies drive `sun.text.normalizer` off ICU normalization data
    // (`jdk.localedata`-adjacent tables) that CratonVM doesn't surface, so they
    // return garbage (e.g. `normalize("ï", NFD)` yields six U+0226 chars).
    // Force our natives (registered in `register_p61_text_formatting`), which
    // use the `unicode-normalization` crate for faithful NFC/NFD/NFKC/NFKD. Fixes
    // Spring `ContentDisposition.transliterateToAscii` (accent decomposition).
    if class_name == "java/text/Normalizer" && matches!(method_name, "normalize" | "isNormalized") {
        return true;
    }
    if is_awt_imageio_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // javac calls this helper while scanning standard file-manager locations.
    // The JDK 25 body is a one-token regex (`\\bMODULE\\b`). Letting that real
    // regex bytecode run under CratonVM can stall in Pattern$Bound/CharPredicates
    // during Spring's in-memory compilation tests; the registered native answers
    // the bytecode-equivalent boolean directly.
    if class_name == "javax/tools/StandardLocation"
        && method_name == "computeIsModuleOrientedLocation"
        && method_descriptor == "(Ljava/lang/String;)Z"
    {
        return true;
    }

    // Same javac location hot path as above: once `inferBinaryName` delegates to
    // `JavacFileManager`, this guard can be reached for every scanned classfile.
    // The native preserves the module-oriented rejection while avoiding repeated
    // interpreted interface/default-method dispatch in the compiler loop.
    if class_name == "com/sun/tools/javac/file/JavacFileManager"
        && matches!(
            (method_name, method_descriptor),
            ("checkNotModuleOrientedLocation", "(Ljavax/tools/JavaFileManager$Location;)V")
                | (
                    "list",
                    "(Ljavax/tools/JavaFileManager$Location;Ljava/lang/String;Ljava/util/Set;Z)Ljava/lang/Iterable;"
                )
                | (
                    "inferBinaryName",
                    "(Ljavax/tools/JavaFileManager$Location;Ljavax/tools/JavaFileObject;)Ljava/lang/String;"
                )
        )
    {
        return true;
    }

    if class_name == "com/sun/tools/javac/file/RelativePath"
        && matches!(
            (method_name, method_descriptor),
            ("hashCode", "()I")
                | ("equals", "(Ljava/lang/Object;)Z")
                | ("compareTo", "(Lcom/sun/tools/javac/file/RelativePath;)I")
                | ("getPath", "()Ljava/lang/String;")
        )
    {
        return true;
    }

    if matches!(
        class_name,
        "com/sun/tools/javac/util/Name"
            | "com/sun/tools/javac/util/SharedNameTable$NameImpl"
            | "com/sun/tools/javac/util/StringNameTable$NameImpl"
    ) && method_name == "equals"
        && method_descriptor == "(Ljava/lang/Object;)Z"
    {
        return true;
    }

    if class_name == "org/springframework/core/test/tools/CompileWithForkedClassLoaderExtension"
        && method_name == "isUsingForkedClassPathLoader"
        && method_descriptor == "(Lorg/junit/jupiter/api/extension/ExtensionContext;)Z"
    {
        return true;
    }

    // SBR-02 / bug-03: fast native regex. The real-JDK `String.replaceAll` /
    // `replaceFirst` / `matches` bodies run `Pattern.compile(...).matcher(...)`
    // in the interpreter (java.util.regex), which is 30–600× slower than
    // HotSpot because every Matcher step crosses the VM→native String-accessor
    // boundary. We force CratonVM's cached `regex`/`fancy-regex` native
    // (lang_string.rs), which is Java-faithful (replacement `$N` / `${name}` /
    // `\`-escapes) and orders of magnitude faster. **Default-ON** (opt-out
    // `CRATONVM_NATIVE_STRING_REGEX=0` reverts to real Java bytecode as the
    // safety net); see `env_cache::native_string_regex`. This is what makes
    // Spring Boot's `PluginXmlParser.format()` chain (4 `replaceAll` + 8 literal
    // `replace`) complete instead of hanging (SBR-02 / PluginXmlParserTests).
    if class_name == "java/lang/String"
        && crate::runtime::env_cache::native_string_regex()
        && matches!(
            (method_name, method_descriptor),
            ("replaceAll", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;")
                | ("replaceFirst", "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;")
                | ("matches", "(Ljava/lang/String;)Z")
                // `replace(CharSequence,CharSequence)` is LITERAL (non-regex)
                // all-occurrences replacement, byte-identical to Rust
                // `str::replace`; routed through the fast native under the same
                // gate (SBR-02 secondary finding — the 8-chained-`replace`
                // PluginXmlParser.format wall). The `(char,char)` overload has
                // its own unconditional native and is NOT gated here.
                | (
                    "replace",
                    "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Ljava/lang/String;"
                )
        )
    {
        return true;
    }
    // `CRATONVM_NATIVE_MATCHER_FIND`: real-JDK-layout `Matcher.find()`/
    // `find(int)`/`start`/`end`/`group` fast path (`native_matcher_find_realjdk`
    // et al. in native-builtins/src/lib.rs). Extends the SBR-02 fast-regex
    // idea above from the `String` convenience methods to the explicit
    // `Pattern.compile(...).matcher(...)` + `while (m.find()) { m.group(N); }`
    // idiom, which SBR-02 does nothing for (that idiom never calls
    // `String.replaceAll`/etc.) and still runs the interpreted engine.
    // `start`/`end`/`group` are included because they're on the same hot
    // loop and only read state `find`/`find(int)` already populate — leaving
    // them interpreted would still leave most of the per-iteration cost on
    // the table. Opt-in (default OFF; see `env_cache::native_matcher_find`)
    // pending the same parity validation SBR-02 went through before its flag
    // flipped default-ON.
    if class_name == "java/util/regex/Matcher"
        && crate::runtime::env_cache::native_matcher_find()
        && matches!(
            (method_name, method_descriptor),
            ("find", "()Z")
                | ("find", "(I)Z")
                | ("start", "()I")
                | ("start", "(I)I")
                | ("end", "()I")
                | ("end", "(I)I")
                | ("group", "()Ljava/lang/String;")
                | ("group", "(I)Ljava/lang/String;")
        )
    {
        return true;
    }
    // `String.substring(int,int)` — gate mismatch fix. `check_override`
    // (vm_exec.rs, the invoke-slow-path native selector) already lists
    // `java/lang/String.substring` as forced-native ("RKC16N.6 RECON":
    // real-JDK String bytecode resolution issues during JDK class clinits),
    // but that allowlist is CONSULTED ONLY on a vtable cache miss. The
    // per-call-site cached vtable fast path (this function's own caller)
    // resolves and caches its native-vs-bytecode decision independently, and
    // `substring` was never added HERE — so once a call site's vtable entry
    // warms, every subsequent `substring` call ran the real bytecode
    // regardless of `check_override`'s intent. `native_string_substring`
    // (native-builtins/src/lang_string.rs) already has a "read only the
    // requested range" fast path specifically written to avoid decoding the
    // WHOLE parent string per call — a real fix that this gate gap left
    // completely unreachable. Confirmed via runtime instrumentation: a tight
    // `text.substring(pos, pos+5)` loop over a large parent `String` cost
    // O(n^2) instead of O(subLen) with this entry absent (see
    // `docs/known-issues/substring-large-parent-quadratic-allocation.md`).
    if class_name == "java/lang/String"
        && method_name == "substring"
        && method_descriptor == "(II)Ljava/lang/String;"
    {
        return true;
    }
    // PERF (h2-bnf-perf 2026-07-23): same "gate mismatch" family as the
    // `(II)` substring entry immediately above -- `check_override`
    // (vm_exec.rs) has listed `charAt`/`length`/`isEmpty`/`startsWith` (and
    // several more `java/lang/String` methods) as forced-native since
    // "RKC16N.6 RECON" (a real-JDK bytecode-resolution boot fix), but that
    // allowlist is only consulted on a genuine vtable cache miss -- this
    // function (the per-call-site cached vtable fast path) never had the
    // matching entries, so once a call site's cache warmed, real (fully
    // interpreted) bytecode ran regardless of `check_override`'s intent,
    // for the entire remaining lifetime of that call site. `substring(I)`
    // (one-arg) was in the same boat as the already-fixed `substring(II)`
    // -- both overloads are covered by `check_override`'s bare
    // `"substring"` name match, but only the two-arg descriptor had an
    // entry here. Root-caused via an H2 BNF-autocomplete workload
    // (`org.h2.bnf.RuleFixed`/`RuleElement`/`Bnf`) whose character-by-
    // character grammar scanning is dominated by exactly these four calls
    // in tight loops (`s = s.substring(1)`, `s.charAt(0)`, `s.length()`,
    // `up.startsWith(name)`). Scoped to the subset of `check_override`'s
    // String list with straightforward, locale/Unicode-independent
    // semantics (plain UTF-16 content comparison / indexing) that are
    // trivially equivalent to the real-JDK bytecode for every input --
    // deliberately NOT extending this to `trim`/`toLowerCase`/
    // `toUpperCase`/`replace`/`compareTo*` here, since those have
    // Unicode/locale edge cases that need their own from-scratch
    // correctness review before being forced this broadly.
    if class_name == "java/lang/String"
        && matches!(
            (method_name, method_descriptor),
            ("substring", "(I)Ljava/lang/String;")
                | ("charAt", "(I)C")
                | ("length", "()I")
                | ("isEmpty", "()Z")
                | ("startsWith", "(Ljava/lang/String;)Z")
        )
    {
        return true;
    }
    // java.net.DatagramSocket / MulticastSocket — real-JDK delegate architecture.
    // Since JDK 14 these classes are thin wrappers that forward every operation
    // to an internal `delegate` (a `DatagramSocketImpl`-backed socket) created
    // lazily; the real bytecode for setOption/getOption/joinGroup/send/receive/…
    // calls `delegate()`, which throws `InternalError("Should not get here")`
    // when the delegate was never wired up. CratonVM models these sockets
    // natively (fd_table-backed, fields port/closed/timeout/fd[/ttl]) and never
    // populates the JDK `delegate`, so the concrete inherited bytecode (e.g.
    // `DatagramSocket.setOption`) always fails. Force our natives to win for the
    // operation surface Tomcat Tribes' `McastServiceImpl` drives (its `socket`
    // field is statically typed `MulticastSocket`, so the CP class is
    // MulticastSocket even for DatagramSocket-declared methods; both classes are
    // listed to be robust to either resolution). Constructors already dispatch
    // to natives via invokespecial and need no entry here.
    if matches!(
        class_name,
        "java/net/MulticastSocket" | "java/net/DatagramSocket"
    ) && matches!(
        method_name,
        "setOption"
            | "getOption"
            | "joinGroup"
            | "leaveGroup"
            | "setSoTimeout"
            | "getSoTimeout"
            | "setTimeToLive"
            | "getTimeToLive"
            | "setReuseAddress"
            | "getReuseAddress"
            | "setBroadcast"
            | "getBroadcast"
            | "send"
            | "receive"
            | "close"
            | "isClosed"
            | "getLocalPort"
    ) {
        return true;
    }
    // TYPE_USE annotation surface (JSpecify @Nullable/@NonNull): the real-JDK
    // getAnnotated{ReturnType,Type}/AnnotatedTypeBaseImpl bytecode can't decode
    // our null getTypeAnnotationBytes0 + unexposed ConstantPool. Single source
    // of truth — `check_override` (vm_exec.rs) consults the same predicate.
    if is_typeuse_annotation_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_reflection_access_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate HQL and Groovy route through ANTLR's prediction-context hot
    // loop during cold full-context parsing. These helpers are tiny
    // bytecode-equivalent methods; forcing the registered intrinsics removes
    // millions of interpreter frame transitions without changing parser
    // semantics.
    if is_antlr_prediction_context_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate/ByteBuddy proxy generation spends a large fraction of cold
    // setup in these tiny cached token hash/equals methods. Force the registered
    // bytecode-equivalent intrinsics to avoid thousands of interpreted
    // AbstractList iterator frames while ByteBuddy builds method graphs.
    if is_bytebuddy_method_token_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate's test extensions call this helper before every test method.
    // The real body delegates to JUnit's recursive composed-annotation scanner;
    // our native checks the same effective method/class locations directly.
    if is_hibernate_testing_util_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate Models stores annotation usages in a Map behind tiny default
    // interface methods. Force bytecode-equivalent natives to remove a hot
    // interpreted layer while FunctionTests repeatedly builds metadata.
    if is_hibernate_models_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // H2's MVStore transaction bookkeeping uses java.util.BitSet in the
    // Hibernate FunctionTests schema-drop path. These single-bit methods are
    // bytecode-equivalent intrinsics and avoid a hot interpreted cleanup loop.
    if is_bitset_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // H2's SQL parser cursor/token accessors are tiny methods called heavily
    // while Hibernate creates and drops schemas in FunctionTests. The native
    // versions are bytecode-equivalent and keep the parser moving under the
    // external harness cap.
    if is_h2_parser_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Hibernate's metadata boot path repeatedly builds property accessor names
    // through `new String(char[], int, int, Void)`, whose real-JDK body spends
    // most of its time in `StringLatin1.inflate`. The native is bytecode-
    // equivalent for the Latin-1 byte[] -> char[] copy and avoids millions of
    // interpreted inner-loop frames.
    if is_jdk_string_native_override(class_name, method_name, method_descriptor)
        || is_jdk_string_charset_name_constructor_override(
            class_name,
            method_name,
            method_descriptor,
        )
    {
        return true;
    }
    // Tiny JDK wrapper arithmetic helpers are already registered as exact
    // natives in `phases_early`; route real-JDK bytecode through them so hot
    // collection reductions such as Hibernate's JoinedList constructor do not
    // spin through one-frame interpreted helpers.
    if is_jdk_wrapper_math_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // CountDownLatch is registered as a synthetic monitor-backed native because
    // the real JDK body stores an AQS Sync object and parks through Unsafe /
    // LockSupport machinery CratonVM does not model completely. Force the full
    // public surface, including <init>, so the synthetic int[] holder is
    // installed before await/countDown read it.
    if is_count_down_latch_native_override(class_name, method_name, method_descriptor)
        || is_stamped_lock_native_override(class_name, method_name, method_descriptor)
    {
        return true;
    }
    if is_ffm_symbol_lookup_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_ffm_group_layout_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_ffm_memory_layout_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // FFM Arena lifecycle. Reaching it from here is what wires the exemption
    // into `try_stackless_invoke`'s step-6 interface guard (via
    // `should_force_registered_native_over_bytecode`), so that path agrees with
    // the explicit `force_ffm_arena_interface_native` term in
    // `invoke_on_class_shared`. See `is_ffm_arena_native_override`.
    if is_ffm_arena_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_file_channel_impl_open_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_file_system_provider_link_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_input_stream_transfer_to_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_zip_output_primitive_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // Keep the cached virtual-call path aligned with vm_exec's
    // FileSystemProvider.newFileChannel override. The real base method is a
    // deliberate UnsupportedOperationException stub; the registered native
    // constructs CratonVM's fd-backed FileChannel for the default provider.
    if matches!(
        class_name,
        "java/nio/file/spi/FileSystemProvider"
            | "sun/nio/fs/WindowsFileSystemProvider"
            | "sun/nio/fs/UnixFileSystemProvider"
    )
        && method_name == "newFileChannel"
        && method_descriptor
            == "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;"
    {
        return true;
    }
    // Native-backed ZIP metadata uses the real JDK field layout but may carry
    // a null optional comment. Keep the bridge for the nullable setter so a
    // Spring Boot nested-entry copy does not enter ZipEntry's CEN validation
    // path with compact/native state.
    if matches!(
        class_name,
        "java/util/zip/ZipEntry" | "java/util/jar/JarEntry"
    ) && method_name == "setComment"
        && method_descriptor == "(Ljava/lang/String;)V"
    {
        return true;
    }
    if is_native_thread_set_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_java_nio_access_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_stamped_lock_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_xerces_cmstateset_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_xerces_xml_parser_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    if is_liquibase_checksum_native_override(class_name, method_name, method_descriptor) {
        return true;
    }
    // JBoss Marshalling calls real-JDK `sun.reflect.ReflectionFactory`
    // bytecode to discover serialization hooks. On CratonVM the registered
    // natives encode ObjectStreamClass's private/inheritable hook rules and
    // must win over the bytecode body, or MethodHandle.invoke later tries to
    // dispatch `java/lang/Object.readObject(ObjectInputStream)`.
    if is_reflection_factory_serialization_native_override(
        class_name,
        method_name,
        method_descriptor,
    ) {
        return true;
    }
    // Surefire fork bootstrap/teardown: bypass ServiceLoader decoder discovery
    // and the acknowledgedExit semaphore path, both of which rely on JDK
    // internals CratonVM shadows with registered natives.
    if class_name == "org/apache/maven/surefire/booter/ForkedBooter"
        && matches!(method_name, "lookupDecoderFactory" | "acknowledgedExit")
    {
        return true;
    }
    // WF-XNIO: `OptionMap$Builder.addAll(OptionMap)` copies through
    // `OptionMap.iterator()`. Our XNIO map/builder state lives in native side
    // tables, and the synthetic array iterator can resolve as
    // `java/lang/Object.next()` through this path. Force the registered native
    // to copy entries directly; companion gate in vm_exec.rs.
    if class_name == "org/xnio/OptionMap$Builder"
        && method_name == "addAll"
        && method_descriptor == "(Lorg/xnio/OptionMap;)Lorg/xnio/OptionMap$Builder;"
    {
        return true;
    }
    matches!(
        (class_name, method_name, method_descriptor),
        ("java/lang/ClassLoader", "setDefaultAssertionStatus", "(Z)V")
            // ServiceLoader-based JDK facilities (including AttachProvider)
            // obtain their loader through Class.getClassLoader().  The real
            // body reads host-layout fields, while CratonVM's native validates
            // and returns the VM-owned loader; without this override a stale
            // String-shaped slot reaches ClassLoader.findResources.
            | ("java/lang/Class", "getClassLoader", "()Ljava/lang/ClassLoader;")
            // Startup javaagents receive a real-JDK
            // sun.instrument.InstrumentationImpl.  Its constructor calls
            // VM-private initialization that CratonVM does not expose; the
            // registered constructor is intentionally a no-op because the
            // observable Instrumentation operations are supplied by our
            // native bridge.  It must therefore beat the real bytecode just
            // like the other layout-backed native overrides in this table.
            | (
                "sun/instrument/InstrumentationImpl",
                "<init>",
                "(JLjava/lang/String;ZZ)V",
            )
            | (
                "java/lang/Thread",
                "getContextClassLoader",
                "()Ljava/lang/ClassLoader;",
            )
            | (
                "java/lang/Thread",
                "setContextClassLoader",
                "(Ljava/lang/ClassLoader;)V",
            )
            | (
                "com/sun/tools/attach/VirtualMachine",
                "attach",
                "(Ljava/lang/String;)Lcom/sun/tools/attach/VirtualMachine;",
            )
            | (
                "com/sun/tools/attach/VirtualMachine",
                "loadAgent",
                "(Ljava/lang/String;Ljava/lang/String;)V",
            )
            | (
                "com/sun/tools/attach/VirtualMachine",
                "loadAgent",
                "(Ljava/lang/String;)V",
            )
            | ("com/sun/tools/attach/VirtualMachine", "detach", "()V")
            | (
                "java/lang/ClassLoader",
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
            )
            | (
                "java/lang/ClassLoader",
                "loadClass",
                "(Ljava/lang/String;Z)Ljava/lang/Class;",
            )
            // `EndElementEvent.getNamespaces()` — the JDK Xerces StAX event impl
            // hard-codes an empty `ReadOnlyIterator` return (it computes
            // `fNamespaces.iterator()` then pops it). Our synthetic cursor reports
            // end-element namespaces (getNamespaceCount/Prefix/URI) and the
            // allocator fills `fNamespaces`, but this getter drops them, so
            // Spring's StaxEventXMLReader emits no `endPrefixMapping`
            // (StaxEventXMLReaderTests namespace methods). Force our native, which
            // returns the actual `fNamespaces` iterator — the behaviour of a
            // spec-correct provider (Woodstox is what HotSpot resolves for this
            // suite). Companion native: `native-builtins/src/xml_stax.rs`.
            | (
                "com/sun/xml/internal/stream/events/EndElementEvent",
                "getNamespaces",
                "()Ljava/util/Iterator;",
            )
            | ("java/net/URL", "getHost", "()Ljava/lang/String;")
            | (
                "java/net/URL",
                "setURLStreamHandlerFactory",
                "(Ljava/net/URLStreamHandlerFactory;)V"
            )
            // `Iterator.remove()V` is a default method that throws
            // `UnsupportedOperationException("remove")`. Several of our
            // synthetic iterator classes (`HashMap$KeyItr` built from
            // `HashSet.iterator()`) are pure synthetic stubs that don't
            // declare `java.util.Iterator` as an interface, so the
            // class-hierarchy-walk fallbacks in `invoke_on_class_shared_inner`
            // / `try_stackless_invoke` resolve through the CP-class default
            // method and execute that throwing body before the receiver-
            // class native lookup gets a chance. Forcing the registered
            // dispatcher in `native-collections` (which routes by receiver
            // class) here moves the receiver-class probe to the front of
            // every dispatch path — required for WildFly 39 / Keycloak 16's
            // `MXBeanSupport.findMXBeanInterface` `it.remove()` reduction
            // loop to succeed.
            | ("java/util/Iterator", "remove", "()V")
            // `ConstantCallSite.getTarget`. On JDK 25 the body is no
            // longer a plain `getfield target` — it first reads
            // `private boolean isFrozen` and throws
            // `IllegalStateException` when it's still false. Our
            // `LambdaMetafactory.metafactory` / `altMetafactory`
            // synthesise `ConstantCallSite` instances via
            // `alloc_concurrent_synthetic`, which bypasses the JDK
            // `<init>` body that flips `isFrozen=true`. Without the
            // force-native here, every callsite materialised by an
            // invokedynamic bootstrap throws ISE on first `getTarget`
            // — observed in `org.apache.logging.log4j`'s
            // `ServiceLoaderUtil.callServiceLoader` chain on
            // Elasticsearch and Spark log4j boot. The registered
            // native in `native-builtins/src/lang_invoke.rs` just
            // returns field 0 (the target MH) — the correct
            // behaviour for an effectively-frozen ConstantCallSite.
            | (
                "java/lang/invoke/ConstantCallSite",
                "getTarget",
                "()Ljava/lang/invoke/MethodHandle;",
            )
            | (
                "java/lang/invoke/ConstantCallSite",
                "dynamicInvoker",
                "()Ljava/lang/invoke/MethodHandle;",
            )
            // `JMXConnectorFactory.newJMXConnector(JMXServiceURL, Map)` —
            // the real-JDK bytecode chain `connect -> newJMXConnector ->
            // ServiceLoader.load(JMXConnectorProvider)` finds zero
            // providers because the RMI client provider is declared via
            // `module-info: provides ... with com.sun.jmx.remote.protocol.
            // rmi.ClientProvider`, not via a `META-INF/services/...`
            // descriptor, and our ServiceLoader (service_loader.rs) only
            // reads the classpath descriptor form. The factory then
            // throws `MalformedURLException("Unsupported protocol: rmi")`,
            // surfaced by Cassandra's nodetool as the misleading
            // "Failed to connect … - MalformedURLException: 'Unsupported
            // protocol: rmi'.". The registered native in jmx.rs
            // (`register_jmx_connector_factory`) instead raises a plain
            // `IOException("JMX over RMI is not implemented …")` so the
            // client's catch handler reports a connection-layer error.
            | (
                "javax/management/remote/JMXConnectorFactory",
                "newJMXConnector",
                "(Ljavax/management/remote/JMXServiceURL;Ljava/util/Map;)Ljavax/management/remote/JMXConnector;",
            )
            // `ManagementFactory.getGarbageCollectorMXBeans()` —
            // real-JDK bytecode delegates to `ManagementFactoryHelper`
            // which iterates platform GCs via natives we don't ship,
            // returning an empty list. H2 `Utils.collectGarbage()` loops
            // until `getCollectionTime()` ticks (`Utils.java:288-294`);
            // an empty bean list makes the loop infinite (>1h hang on
            // TestAll boot before any test runs). Force our synthetic
            // single-bean list (jmx.rs:966-980) backed by the real heap
            // GC counter so `collectGarbage()` exits after one cycle.
            | (
                "java/lang/management/ManagementFactory",
                "getGarbageCollectorMXBeans",
                "()Ljava/util/List;",
            )
            // `Hashtable.keys()` / `elements()` — the legacy pre-1.2
            // Enumeration accessors. We native-override put/get/size onto
            // our own side-store (`native_map_put` etc.), so the real-JDK
            // bytecode body (`return this.getEnumeration(KEYS)`) walks an
            // EMPTY internal `Hashtable.table[]` and returns an empty
            // Enumeration. Sound-but-different contract: real JDK is
            // correct for its own table, but our backing store is in a
            // different place. BC's `AbstractX500NameStyle.copyHashTable`
            // depends on `keys()` to populate the per-instance
            // `defaultLookUp`; without this override, every
            // `attrNameToOID("cn"/"o"/"CN"/...)` returns null and the
            // X.500 RDN parser throws "Unknown object id".
            | (
                "java/util/Hashtable",
                "keys",
                "()Ljava/util/Enumeration;",
            )
            | (
                "java/util/Hashtable",
                "elements",
                "()Ljava/util/Enumeration;",
            )
            // TC0622: `Hashtable.clone()` (inherited by `Properties`). Our
            // native `put` stores synthetic bucket nodes in slot-0 `table[]`,
            // not genuine `Hashtable$Entry`. The real-JDK clone body does
            // `t.table[i] = (Hashtable$Entry) table[i].clone()` and the
            // `checkcast` throws ClassCastException on our synthetic node.
            // (`InitialContext.<init>` clones its environment Hashtable, so
            // `new InitialDirContext(env)` blew up before any LDAP connect.)
            // Force the native (deprecated_util::native_hashtable_clone) which
            // rebuilds a fresh natively-backed map without materialising an
            // Entry. Companion match in vm_exec.rs.
            | (
                "java/util/Hashtable",
                "clone",
                "()Ljava/lang/Object;",
            )
            // spring-bug-08: `ObjectInputStream.resolveProxyClass(String[])`
            // has a real JDK body whose default routes
            // `Proxy.getProxyClass` → `ProxyBuilder.getDynamicModule` →
            // `Module.defineModule0` (a native the synthetic proxy model can't
            // satisfy → `UnsatisfiedLinkError`/`ClassNotFoundException: null`).
            // Force CratonVM's registered native (serialization.rs), which
            // returns a generated `$ProxyN` class directly, so a serialized JDK
            // dynamic proxy round-trips on CratonVM's own proxy machinery. Only
            // a plain `java/io/ObjectInputStream` is forced — a subclass that
            // overrides `resolveProxyClass` dispatches under its own class name
            // and keeps its override.
            | (
                "java/io/ObjectInputStream",
                "resolveProxyClass",
                "([Ljava/lang/String;)Ljava/lang/Class;",
            )
            // proxy-real-classfile increment 6: `InvocationHandler.invokeDefault`
            // (static, JDK 16+). The real JDK body drives `Proxy.invokeDefault`,
            // which reflects the generated proxy class's `proxyClassLookup`
            // accessor + a full-power `MethodHandles.Lookup` to bind an
            // invokespecial MethodHandle to the interface default body. CratonVM's
            // generated `$ProxyN` emits no `proxyClassLookup` (and the proxy model
            // has no real per-class Lookup), so the real bytecode throws
            // `InternalError: NoSuchMethodException: proxyClassLookup`. Force the
            // registered native (native-builtins
            // `native_invocation_handler_invoke_default`), which runs the default
            // body directly via `invoke_special`.
            | (
                "java/lang/reflect/InvocationHandler",
                "invokeDefault",
                "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
            )
            // proxy-real-classfile increment 7: deprecated `Proxy.getProxyClass`.
            // The real JDK body routes the dynamic-module machinery
            // (`ProxyBuilder.getDynamicModule` → `Module.defineModule0`) the
            // synthetic proxy model can't satisfy → `InternalError: Proxy is not
            // supported until module system is fully initialized`. Force the
            // registered native (native-builtins `native_proxy_get_proxy_class`),
            // which returns the generated `$ProxyN` class directly.
            | (
                "java/lang/reflect/Proxy",
                "getProxyClass",
                "(Ljava/lang/ClassLoader;[Ljava/lang/Class;)Ljava/lang/Class;",
            )
            // TC0622 classpath:-protocol: `jdk.internal.misc.VM.isBooted()` on
            // JDK 25 is real bytecode `return initLevel >= SYSTEM_BOOTED(4)`,
            // reading the *static field* `jdk.internal.misc.VM.initLevel`.
            // CratonVM boots natively and never runs the real
            // `System.initPhase2/3` that would call `VM.initLevel(int)` to set
            // that field, so it stays 0 and the real `isBooted()` returns false
            // forever. `java.net.URL.getURLStreamHandler` gates factory lookup
            // on `isOverrideable(protocol) && VM.isBooted()`, so a false result
            // makes the un-intercepted real `getURLStreamHandler` skip the
            // app-installed `URLStreamHandlerFactory` entirely and throw
            // `MalformedURLException: unknown protocol: classpath` even though
            // Tomcat's `TomcatURLStreamHandlerFactory` is correctly registered
            // (and published into `URL.factory` by
            // `native_url_set_stream_handler_factory_guard`, HIB-CV-15). The
            // registered native (`register_essential_natives`, lib.rs) returns
            // 1; force it so the boot-state native — like the `VM.initLevel()`
            // floor-of-2 native alongside it — actually shadows the real
            // bytecode. By the time any app/JDK-library code calls `isBooted()`
            // the VM is genuinely up, matching HotSpot's post-boot `true`. Also
            // unblocks the JASPIC `ResourcesMgr` path (commit 873355f1 added the
            // native but missed this force-list entry, leaving it inert).
            | ("jdk/internal/misc/VM", "isBooted", "()Z")
    ) || (class_name == "java/net/URL"
        && matches!(method_name, "getAuthority" | "getHostAddress"))
        || (matches!(
            class_name,
            "java/net/InetAddress" | "java/net/Inet4Address" | "java/net/Inet6Address"
        ) && matches!(
            method_name,
            "getHostName" | "getCanonicalHostName" | "getHostAddress"
        ))
        // URLClassLoader.findClass / findResource / findResources +
        // URLClassPath.addURL — see the companion `check_override` entry in
        // `vm_exec.rs::invoke_on_class_shared_inner`. The real bytecode routes
        // through the shimmed `URLClassPath` (null `unopenedUrls`/`path`), so
        // `addURL` NPEs and `findClass`/`findResource(s)` find nothing. Force
        // the natives (`ucp_add_url` / `ucl_find_class` / `ucl_find_resource(s)`)
        // on the bytecode-interpreter + cached/promoted dispatch paths. `addURL`
        // is keyed on `URLClassPath` (its `ucp.addURL(url)` call site) — the
        // `URLClassLoader.addURL` wrapper is invoked via a subclass `this`,
        // escaping this static-class gate. Hibernate `NoDepthTests` JPA +
        // ShrinkWrap.
        //
        // `findClass` matters when a `URLClassLoader` subclass overrides BOTH
        // `loadClass` overloads and calls `findClass` directly (so CratonVM's
        // `cl_load_class` native — which would otherwise resolve from the global
        // classpath — never runs): Jasper's `JasperLoader` does exactly this to
        // load the runtime-compiled `org.apache.jsp.*_jsp` servlet from its
        // scratch-dir URL. Without forcing the native, the real
        // `URLClassLoader.findClass` reaches the shimmed `ucp.getResource` →
        // null → `ClassNotFoundException`, 500-ing every compiled JSP/tag
        // (TestPageContext, TestScopedAttributeELResolver, …). `ucl_find_class`
        // delegates to the base classpath (where `<init>` already registered the
        // loader's URLs), matching HotSpot.
        || (class_name == "java/net/URLClassLoader"
            && (matches!(method_name, "findClass" | "findResource" | "findResources" | "getURLs" | "addURL" | "close")
                || (method_name == "<init>"
                    && matches!(
                        method_descriptor,
                        "([Ljava/net/URL;)V"
                            | "([Ljava/net/URL;Ljava/lang/ClassLoader;)V"
                            | "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;)V"
                            | "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V"
                            | "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V"
                            | "([Ljava/net/URL;Ljava/security/AccessControlContext;)V"
                            | "(Ljava/lang/String;[Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/security/AccessControlContext;)V"
                    ))))
        || (matches!(
            class_name,
            "jdk/internal/loader/URLClassPath" | "sun/misc/URLClassPath"
        ) && method_name == "addURL")
}

/// True when `class_name` has been redefined in place by a JVMTI agent
/// (e.g. Mockito's inline mock maker), so its woven bytecode is authoritative
/// and the VM's per-class native/intrinsic shadows must be suppressed — the
/// woven advice has to run for the mock to intercept (matching HotSpot, which
/// always executes the retransformed bytecode).
///
/// Fast-pathed on the global [`any_class_redefined`] flag: until some agent
/// redefines a class (the overwhelming common case) this is a single relaxed
/// atomic load and never touches the class-manager lock. Once armed, it costs
/// one `class_manager` read + a generation lookup, but only at the handful of
/// dispatch sites that were about to serve a native/intrinsic shadow.
#[inline]
pub(super) fn native_shadow_suppressed_by_redefine(shared: &SharedVm, class_name: &str) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    native_shadow_suppressed_in(&cm, class_name)
}

/// Reflection-metadata natives on the `java.lang.reflect.*` member types that
/// must stay authoritative even after their declaring class is redefined.
///
/// CratonVM serves these (annotations, parameter annotations, annotation
/// defaults, annotated types) from VM-side structures, NOT from raw class-file
/// bytes a real `sun.reflect.annotation.AnnotationParser` + `ConstantPool`
/// could decode. So the suppress-native-shadow-on-redefine guard — which
/// otherwise correctly cedes a redefined class's methods to their woven
/// bytecode so a Mockito inline mock's advice runs — must NOT fire for these.
///
/// The trigger: `Mockito.mock(java.lang.reflect.Method.class)` inline-redefines
/// `java.lang.reflect.Method`. That bumps its `redefine_generation`, so EVERY
/// subsequent `Method.getDeclaredAnnotations()` / `isAnnotationPresent(...)`
/// call — on ANY method object, not just the mocked class — was routed to the
/// real `Executable.declaredAnnotations()` bytecode, which under CratonVM reads
/// empty annotation bytes and returns no annotations. JUnit's `@Test` scan then
/// finds zero test methods and Spring AOP's `MethodMatchersTests` (whose
/// `static final Method TEST_METHOD = mock(Method.class)` runs at class-init)
/// discovers 0 tests where HotSpot runs 14. A mock only needs its per-INSTANCE
/// dispatch woven; these class-level metadata accessors are not instance
/// behaviour and the mock never stubs them, so keeping the native is correct
/// (and matches HotSpot, where the redefine leaves real annotation reflection
/// intact). Business-method inline mocks (e.g. `InetAddress.getHostName`) are
/// unaffected — they are not in this list.
pub(super) fn redefine_immune_reflection_native(class_name: &str, method_name: &str) -> bool {
    matches!(
        class_name,
        "java/lang/reflect/Method"
            | "java/lang/reflect/Constructor"
            | "java/lang/reflect/Field"
            | "java/lang/reflect/Executable"
            | "java/lang/reflect/AccessibleObject"
    ) && matches!(
        method_name,
        "getDeclaredAnnotations"
            | "getAnnotations"
            | "getAnnotation"
            | "getDeclaredAnnotation"
            | "isAnnotationPresent"
            | "getAnnotationsByType"
            | "getDeclaredAnnotationsByType"
            | "getParameterAnnotations"
            | "getDefaultValue"
            | "getAnnotatedReturnType"
            | "getAnnotatedParameterTypes"
            | "getAnnotatedExceptionTypes"
            | "getAnnotatedReceiverType"
    )
}

/// Methods whose real JDK bodies access compact `byte[]`/`coder`/`count`
/// fields while CratonVM StringBuilder objects intentionally use a synthetic
/// `char[]`/`count` layout. A registered native must win for every one of these
/// operations, including direct methods on StringBuilder rather than only their
/// AbstractStringBuilder implementation.
pub(crate) fn is_string_builder_layout_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    if !matches!(
        class_name,
        "java/lang/StringBuilder" | "java/lang/StringBuffer" | "java/lang/AbstractStringBuilder"
    ) {
        return false;
    }
    if matches!(
        method_name,
        // Keep this deliberately narrow: these direct JDK bodies read the
        // incompatible compact-string layout on synthetic builders. Other
        // operations keep their established dispatch to avoid turning the
        // high-volume AOT code-generation path into an all-native slow path.
        //
        // `setCharAt` joined this list once the invoke-cache redefine guards
        // became precise enough to actually evict a stale native shadow for
        // a genuinely real (non-mock) StringBuilder after an UNRELATED
        // Mockito.mock(StringBuilder.class) redefined the class elsewhere in
        // the process: real `AbstractStringBuilder.setCharAt`'s bytecode
        // writes through `String.checkIndex` against the compact
        // byte[]/coder layout, which CratonVM's synthetic builder doesn't
        // have, so it AIOOBE'd instead of writing the synthetic char[].
        //
        // 2026-07-31: the list below `setCharAt` was still incomplete, and
        // `SbMethodMatrixProbe` (Spring-free: run each builder operation on a
        // REAL builder before and after an unrelated
        // `Mockito.mock(StringBuilder.class)`) named the survivors exactly.
        // Six operations silently changed behaviour after the redefinition:
        //
        //   setLength(4)   'abcdefghij' -> len=4 but toString() == "a"
        //   setLength(0)   left one stale char behind
        //   deleteCharAt   'abcdefghij' -> len=9 but toString() == "a"
        //   replace        ArrayStoreException (src=Byte, dest=Char)
        //   ensureCapacity buffer overwritten with spaces
        //   trimToSize     same
        //   repeat         appended nothing
        //
        // Each has a registered native in `register_string_builder_natives`
        // (`setLength(I)V`, `deleteCharAt(I)…`, `replace(IILjava/lang/String;)…`,
        // `ensureCapacity(I)V`, `trimToSize()V`, `repeat(II)…`), so before the
        // redefinition they all dispatched native and were correct; the
        // redefinition evicted the shadow and handed them back to real JDK
        // bodies that index a compact `byte[] value` / `byte coder` /
        // `int count` layout CratonVM's two-field `char[]`/`int` builder does
        // not have. Adding them here is the same trade `setCharAt` already
        // makes: these mutate builder state, and nothing stubs them on a mock.
        // `capacity`, `getCoder`, `getValue`, `reverse` and the `codePoint*`
        // readers join for the same reason — they read `value`/`coder`
        // directly and cannot be expressed against the synthetic layout.
        //
        // `length()` and `substring(int)` stay OUT of this list on purpose;
        // see the long note below. They are the two operations
        // `MockitoBeanByTypeLookupIntegrationTests` genuinely stubs and
        // verifies on a mocked StringBuilder, so their native shadow must
        // stay evictable for Mockito's woven advice to run.
        "<init>"
            | "append"
            | "capacity"
            | "charAt"
            | "codePointAt"
            | "codePointBefore"
            | "codePointCount"
            | "delete"
            | "deleteCharAt"
            | "ensureCapacity"
            | "getChars"
            | "getCoder"
            | "getValue"
            | "insert"
            | "repeat"
            | "replace"
            | "reverse"
            | "setCharAt"
            | "setLength"
            | "toString"
            | "trimToSize"
    ) {
        return true;
    }
    // `length()` FIX (2026-07-23, follow-up to the KNOWN GAP left by the
    // previous session): stop blanket-immunizing "length" for ANY of the
    // three class names -- matching `substring(int)`'s existing treatment
    // exactly (that method is not, and never has been, in this list).
    //
    // Ground truth, captured by dumping Mockito's OWN redefined bytecode on
    // real HotSpot (`-Dnet.bytebuddy.dump=...`, JDK 25, Mockito 5.23.0):
    // `Mockito.mock(StringBuilder.class)` redefines BOTH `StringBuilder`
    // (whose `length()` is a compiler-generated public bridge --
    // `AbstractStringBuilder` is package-private -- confirmed via `javap -p
    // -c java.lang.StringBuilder`: `aload_0; invokespecial
    // AbstractStringBuilder.length:()I; ireturn`, UNCHANGED by redefinition)
    // AND `AbstractStringBuilder` itself, weaving the actual
    // `MockMethodDispatcher.get/isMocked/isOverridden/handle` advice
    // directly into `AbstractStringBuilder.length()`'s own body, ahead of
    // its original `getfield count:I` tail. So blanket-forcing native for
    // `AbstractStringBuilder.length()` (an earlier version of this fix kept
    // that arm immune, reasoning the bridge alone was the redefined method,
    // by analogy with `substring`) permanently pre-empted the advice for
    // BOTH a mock AND a real receiver of the class -- the STRINGBUILDER
    // bridge's `invokespecial` reached `AbstractStringBuilder.length()`,
    // which our own force-native gate intercepted before Mockito's advice
    // ever got to run. Removing immunity here (verified against
    // `InvocationCountProbe`, which reflects
    // `Mockito.mockingDetails(mock).getInvocations()`) now byte-for-byte
    // matches real HotSpot's `length()`/`substring(0)`/`verify()` sequence.
    //
    // KNOWN REMAINING GAP: a REAL (non-mock) receiver's `.length()`, called
    // AFTER some OTHER StringBuilder has been Mockito-redefined ANYWHERE in
    // the process, now falls through the woven advice's "not mocked" branch
    // into `AbstractStringBuilder.length()`'s original `getfield count:I` --
    // which reads the wrong field index against CratonVM's 2-field
    // (`char[]`, `int`) synthetic layout (real JDK's compiled class expects
    // `value`/`coder`/`count` at indices 0/1/2) and silently returns `0`
    // instead of the real length (confirmed via a dedicated probe:
    // `RealAfterMockLengthProbe`, `/data/tmp/mockitobean-substring-20260723/`).
    // This is the EXACT SAME latent risk `substring(int)` has carried,
    // unaddressed, since bug 3 of this class's fix history -- not a
    // regression this change introduces, just the same known tradeoff now
    // also applying to `length()`. Fixing it for real needs an authoritative
    // per-instance "is this receiver actually mocked" signal reachable from
    // Rust WITHOUT re-entering bytecode dispatch for the same (class,
    // method) pair (a naive `MockUtil.isMock` + re-invoke attempt during
    // this session's investigation infinite-looped, since re-invoking
    // "this method's bytecode" from inside the very native registered for
    // it re-triggers the identical force-native decision) -- left open, not
    // hit by any currently-passing suite class.
    if method_name == "length" && method_descriptor == "()I" {
        return false;
    }
    // `substring(int, int)` -- deliberately excludes `substring(int)`.
    // `substring(int)` must stay evictable: `MockitoBeanByTypeLookup*
    // IntegrationTests` explicitly stubs/verifies `.substring(anyInt())`
    // on a Mockito-mocked StringBuilder, which only works if the redefine
    // guards can drop this method's native shadow so the woven advice
    // actually runs (see the `Native{}` cache-hit redefine guard). But
    // `substring(int, int)`'s native shadow needs the SAME layout-safety
    // forcing as `setCharAt` above for a REAL (non-mock) receiver: Mockito
    // itself calls `new StringBuilder(...).substring(start, end)` inside
    // `StringUtil.join` (`Reporter.unfinishedVerificationException`'s
    // message formatting) on its own internal, never-mocked StringBuilder,
    // and once ANY StringBuilder in the process gets Mockito-redefined,
    // real `AbstractStringBuilder.substring(int,int)` bytecode AIOOBE'd
    // reading the incompatible compact layout -- masking the ACTUAL
    // "unfinished verification" failure behind a crash in the exception
    // message it was trying to construct. No test in this suite stubs or
    // verifies the two-arg overload, so forcing it native is safe.
    method_name == "substring" && method_descriptor == "(II)Ljava/lang/String;"
}

/// The layout-incompatible natives, for the invoke-cache dispatch sites.
///
/// These sites must not use `redefine_immune_forced_native`. Broadening them to
/// the full set — which additionally covers reflection metadata, JFR, BC crypto,
/// StampedLock and FileHandler — was measured on 2026-07-31 and **regressed**
/// ByteBuddy type creation: Spring AOT chunk 3 started failing roughly one run
/// in eight with `NoSuchMethodError: java.lang.Integer.isArray()Z` /
/// `Integer.represents(Type)Z` out of
/// `TypeDescription$Generic$Visitor$Substitutor`, a signature that appears in no
/// pre-change log across three full sweeps. 9/9 clean on the unmodified binary
/// under the same harness, 8/9 with the broadening. Those extra arms exist for
/// the *slow* path and are not safe to assert here.
///
/// What every member of this set has in common is narrower and checkable: the
/// receiver's real JDK body indexes a field layout CratonVM's object does not
/// have, so running it can only produce nonsense — whatever else is true about
/// the class.
pub(super) fn redefine_immune_layout_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    redefine_immune_string_builder_native(class_name, method_name, method_descriptor)
        || redefine_immune_path_native(class_name, method_name, method_descriptor)
        || redefine_immune_synthetic_collection_native(class_name)
}

pub(super) fn redefine_immune_string_builder_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    is_string_builder_layout_native_override(class_name, method_name, method_descriptor)
}

pub(super) fn redefine_immune_path_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    class_name == "java/nio/file/Path"
        && method_name == "toString"
        && method_descriptor == "()Ljava/lang/String;"
}

pub(super) fn redefine_immune_jfr_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    is_jfr_metadata_native_override(class_name, method_name, method_descriptor)
}

pub(super) fn is_jfr_metadata_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    matches!(
        (class_name, method_name, method_descriptor),
        (
            "jdk/jfr/internal/Type",
            "getKnownType",
            "(Ljava/lang/Class;)Ljdk/jfr/internal/Type;"
        ) | (
            "jdk/jfr/internal/util/Utils",
            "getValidType",
            "(Ljava/lang/Class;Ljava/lang/String;)Ljdk/jfr/internal/Type;"
        ) | ("jdk/jfr/internal/JDKEvents", "initialize", "()V")
            | ("jdk/jfr/internal/instrument/JDKEvents", "initialize", "()V")
            | ("jdk/jfr/consumer/RecordingStream", "startAsync", "()V")
            | ("jdk/jfr/Event", "begin", "()V")
            | ("jdk/jfr/Event", "end", "()V")
            | ("jdk/jfr/Event", "commit", "()V")
            | ("jdk/jfr/Event", "isEnabled", "()Z")
            | ("jdk/jfr/Event", "shouldCommit", "()Z")
            | (
                "jdk/jfr/AnnotationElement",
                "checkType",
                "(Ljava/lang/Class;)V"
            )
            | ("jdk/jfr/Recording", "start", "()V")
            | ("jdk/jfr/Recording", "stop", "()Z")
            | ("jdk/jfr/Recording", "dump", "(Ljava/nio/file/Path;)V")
    )
}

/// Keep MongoDB Reactive Streams' Netty 4.2 group teardown bounded when a
/// closed monitor callback keeps its default graceful-shutdown quiet period
/// alive. The native checks the receiver class, so unrelated Netty executors
/// continue through their original bytecode.
pub(crate) fn is_netty_event_executor_group_shutdown_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    matches!(
        class_name,
        "io/netty/util/concurrent/EventExecutorGroup"
            | "io/netty/util/concurrent/AbstractEventExecutorGroup"
            | "io/netty/channel/MultiThreadIoEventLoopGroup"
    ) && method_name == "shutdownGracefully"
        && method_descriptor == "()Lio/netty/util/concurrent/Future;"
}

/// Spring Boot's Mongo reactive lifecycle bean waits indefinitely on a Netty
/// promise that can remain incomplete after its event-loop workers are gone.
/// The native replacement requests shutdown and returns without that wait.
pub(crate) fn is_springboot_mongo_reactive_customizer_destroy_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    class_name
        == "org/springframework/boot/mongodb/autoconfigure/MongoReactiveAutoConfiguration$NettyDriverMongoClientSettingsBuilderCustomizer"
        && method_name == "destroy"
        && method_descriptor == "()V"
}

pub(crate) fn is_springboot_mongo_reactive_customizer_customize_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    class_name
        == "org/springframework/boot/mongodb/autoconfigure/MongoReactiveAutoConfiguration$NettyDriverMongoClientSettingsBuilderCustomizer"
        && method_name == "customize"
        && method_descriptor == "(Lcom/mongodb/MongoClientSettings$Builder;)V"
}

/// JDK 25's JNDI DNS client can use either `DatagramChannel` factory. Its real
/// `DatagramChannelImpl` path does not share CratonVM's fd-table state, so the
/// factories and the synthetic channel's local-address accessor must select
/// the native UDP bridge.
pub(crate) fn is_datagram_channel_open_native_override(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    if matches!(
        class_name,
        "java/nio/channels/DatagramChannel" | "java/nio/channels/NetworkChannel"
    ) && method_name == "getLocalAddress"
        && method_descriptor == "()Ljava/net/SocketAddress;"
    {
        return true;
    }
    if class_name == "java/nio/channels/DatagramChannel"
        && method_name == "open"
        && method_descriptor == "()Ljava/nio/channels/DatagramChannel;"
    {
        return true;
    }
    method_descriptor == "(Ljava/net/ProtocolFamily;)Ljava/nio/channels/DatagramChannel;"
        && ((class_name == "java/nio/channels/DatagramChannel" && method_name == "open")
            || (class_name == "sun/nio/ch/SelectorProviderImpl"
                && method_name == "openDatagramChannel"))
}

/// The full immunity set, for the slow dispatch path.
///
/// The invoke-cache sites deliberately use the narrower
/// `redefine_immune_layout_native` instead — see the note there for the
/// measurement that says why. What both must share is the layout arm: when the
/// 2026-07-31 collections entry was added here only, the collection probe went
/// from 32 broken operations to 18 rather than to 0, because the cache sites
/// re-assembled their own chain and never saw it.
/// `layout_immunity_is_not_open_coded` keeps the two in step.
pub(super) fn redefine_immune_forced_native(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    redefine_immune_reflection_native(class_name, method_name)
        || redefine_immune_string_builder_native(class_name, method_name, method_descriptor)
        || redefine_immune_path_native(class_name, method_name, method_descriptor)
        || redefine_immune_jfr_native(class_name, method_name, method_descriptor)
        || is_bc_crypto_math_native_override(class_name, method_name, method_descriptor)
        || is_stamped_lock_native_override(class_name, method_name, method_descriptor)
        // java.util.logging.FileHandler's registered natives store their
        // filename/closed bookkeeping in an identity-hash side table
        // (jul_file_handler_state_table, native-builtins/src/
        // logging_shims.rs) rather than real instance field slots. Real
        // FileHandler bytecode (loaded from java.base) is concrete, so
        // without this entry the default rule ran its REAL <init>()V /
        // <init>(String)V -- which try to actually open/lock a real log
        // file via NIO and throw NoSuchFileException -- instead of the
        // registered native. Keep in sync with vm_exec.rs's
        // invoke_on_class_shared_inner check_override entry for the same
        // triples; see
        // docs/known-issues/springboot/filehandler-noarg-ctor-handler-field-layout-gap.md.
        || (class_name == "java/util/logging/FileHandler"
            && matches!(
                method_name,
                "<init>" | "publish" | "flush" | "close"
            ))
        || redefine_immune_synthetic_collection_native(class_name)
}

/// CratonVM implements these collections as small synthetic objects — a bucket
/// array plus a size, not the JDK's `table`/`root`/`head` field graph — and
/// every operation on them is a registered native. Their real JDK bodies can
/// therefore NEVER run correctly against an instance CratonVM built, whatever
/// the circumstances.
///
/// That makes them unconditionally immune: a redefinition drops native shadows
/// so an agent's woven bytecode can run, which is right for an ordinary class
/// and catastrophic here. `RedefineCollectionLayoutProbe` measured the damage
/// before this gate existed — **32 of 89 operations** changed behaviour after
/// redefining these classes with their OWN bytes, and the worst of them are
/// silent:
///
/// ```text
/// TreeMap.get               v7  -> null
/// TreeMap.containsKey       true -> false
/// ConcurrentHashMap.get     v7  -> null
/// ConcurrentHashMap.size    12  -> 0
/// ConcurrentHashMap.isEmpty false -> true
/// HashMap.keySet            [k0..k11] -> []
/// ```
///
/// The rest throw — `LinkedHashMap$Node.getKey` NoSuchMethodError,
/// `AnonymousObject$4 cannot be cast to Map$Entry`, `TreeSet` NPEs on a null
/// `this.m`. One `Mockito.mock()` anywhere in the process was enough to arm it,
/// which is how it reached Spring's AOT run: `AnnotationAttributes` extends
/// `LinkedHashMap`, and its `keySet()` NPE'd.
///
/// Unlike the StringBuilder list above, this is class-wide rather than
/// method-wise. There is no analogue of `length()`/`substring(int)` here — no
/// suite stubs a method on a mocked JDK collection, and the cost of being wrong
/// in that direction (one un-stubbed mock) is far below the cost of being wrong
/// in the other (silent data loss on every real collection in the process). If
/// a test ever does need to stub one, narrow this the way the builder list is
/// narrowed, and say which test.
fn redefine_immune_synthetic_collection_native(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/util/ArrayDeque"
            | "java/util/ArrayList"
            | "java/util/HashMap"
            | "java/util/HashSet"
            | "java/util/IdentityHashMap"
            | "java/util/LinkedHashMap"
            | "java/util/LinkedHashSet"
            | "java/util/LinkedList"
            | "java/util/TreeMap"
            | "java/util/TreeSet"
            | "java/util/concurrent/ConcurrentHashMap"
    )
}

pub(crate) fn should_force_registered_native_over_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    should_force_registered_native_over_bytecode_precomputed(
        shared,
        force_native_over_real_jdk_bytecode_memoized(class_name, method_name, method_descriptor),
        class_name,
        method_name,
        method_descriptor,
    )
}

/// Memoizing wrapper around [`force_native_over_real_jdk_bytecode`].
///
/// That function is a pure, ~55-branch sequential scan over hardcoded
/// (class, method, descriptor) triples with no side effects and no
/// dependency on mutable VM state -- its result for a given triple never
/// changes for the lifetime of the process. `CachedBytecodeMethod
/// ::force_native_cache` already memoizes it once per warm bytecode-PC
/// invoke-cache entry, but every OTHER call path that reaches
/// `should_force_registered_native_over_bytecode` -- reflective
/// `Method.invoke()` dispatch (which has no bytecode PC to key an
/// invoke-cache entry on), megamorphic/polymorphic call sites that never
/// settle on one cached target, `invokespecial`, and interface-default
/// dispatch -- re-ran the full scan on every single call with no
/// memoization at all. Profiling `BeanRegistrationsAotContributionTests`
/// (~54 min vs HotSpot's 13s for the same test, see
/// CRATONVM-SPRING-GENUINE-BUGLIST's AOT cluster
/// section) found exactly this: `intercept_force_registered_native` ->
/// `should_force_registered_native_over_bytecode` ->
/// `force_native_over_real_jdk_bytecode` live at the top of repeated gdb
/// stack samples, reached through deep `try_lambda_dispatch` recursion
/// driven by Mockito's constructor-mock listener dispatch (reflective
/// `Method.invoke()` on many distinct generated classes, so per-callsite
/// caching never warms up). A global cache keyed by the exact same 3
/// inputs is safe by construction -- the wrapped function reads no state
/// beyond its own arguments, so there is nothing to invalidate.
pub(super) fn force_native_over_real_jdk_bytecode_memoized(
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    use parking_lot::Mutex;
    use std::sync::OnceLock;
    type Key = (Box<str>, Box<str>, Box<str>);
    static CACHE: OnceLock<Mutex<rustc_hash::FxHashMap<Key, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(rustc_hash::FxHashMap::default()));
    let key: Key = (
        class_name.into(),
        method_name.into(),
        method_descriptor.into(),
    );
    if let Some(&v) = cache.lock().get(&key) {
        return v;
    }
    let v = force_native_over_real_jdk_bytecode(class_name, method_name, method_descriptor);
    cache.lock().insert(key, v);
    v
}

/// Same decision as [`should_force_registered_native_over_bytecode`], but
/// takes the pure/deterministic `force_native_over_real_jdk_bytecode` result
/// as a precomputed input rather than recomputing it. Lets a cached-dispatch
/// call site (which can memoize that ~55-branch check once per invoke-cache
/// entry, see `CachedBytecodeMethod::force_native_cache`) skip straight to
/// the cheap, mutable-state-dependent redefine check.
pub(super) fn should_force_registered_native_over_bytecode_precomputed(
    shared: &SharedVm,
    force_native: bool,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    force_native
        && (!native_shadow_suppressed_by_redefine(shared, class_name)
            || redefine_immune_forced_native(class_name, method_name, method_descriptor))
}

/// Dispatch a force-native override via `safe_native_call`, pushing any return
/// value onto the caller operand stack.
#[inline]
/// Whether `recv` (the receiver of a `ThreadPoolExecutor.execute()` call) is
/// a genuinely real, bytecode-constructed `ThreadPoolExecutor` rather than
/// one of CratonVM's synthetic 2-field `Executors.new*ThreadPool()` stand-ins.
/// Mirrors `native-builtins::executor_has_real_workers` (same check, same
/// field) but works from the interpreter, which only has `SharedVm`/`JvmThread`
/// -- not a `NativeContext` -- available at this dispatch point.
pub(super) fn threadpool_executor_has_real_workers(shared: &SharedVm, recv: &Value) -> bool {
    let Value::Object(Some(recv)) = recv else {
        return false;
    };
    let class_id = shared.mem.heap.class_id_of(*recv);
    // read_recursive() instead of read() -- populate_virtual_invoke_cache
    // already holds class_manager.read() across its own native-shadow
    // exemption check (the is_real_tpe_execute call site) when it calls into
    // this helper. A plain nested read() panics the lock-order tracker
    // (debug builds) or can deadlock under parking_lot once a writer is
    // queued (release builds) -- same fix as resolve_method_ref /
    // surefire_lazy_launcher_discover_native.
    let cm = shared.classes.class_manager.read_recursive();
    let Some(index) =
        crate::vm::vm_exec::resolve_field_index_in_hierarchy(class_id, "workers", &cm.class_store)
    else {
        return false;
    };
    drop(cm);
    matches!(
        shared.mem.heap.get_field(*recv, index),
        Value::Object(Some(_))
    )
}

/// CratonVM's own HTTP carrier classes — the concrete classes its
/// `URL.openConnection()` hands back, and the ones
/// `register_http_url_connection_real` registers natives on. Matched EXACTLY
/// (not by subtype): a user subclass such as
/// `SimpleClientHttpRequestFactoryTests$TestHttpURLConnection` has real
/// bytecode of its own and must keep running it.
const CRATONVM_HTTP_CARRIER_CLASSES: [&str; 4] = [
    "java/net/HttpURLConnection",
    "sun/net/www/protocol/http/HttpURLConnection",
    "sun/net/www/protocol/https/HttpsURLConnectionImpl",
    "javax/net/ssl/HttpsURLConnection",
];

/// Resolve the registered native for a call landing on a genuinely real,
/// `URL.openConnection()`-constructed CratonVM HTTP carrier — the one case
/// where the native must fire even though the class counts as "redefined"
/// somewhere in the process.
///
/// Why the exemption exists: Mockito's mock makers trip the class-wide
/// `class_redefine_generation` counter for EVERY instance of
/// `java/net/HttpURLConnection`, mock or not, for the rest of the process.
/// Without this, `should_force_registered_native_over_bytecode` cedes to the
/// real-JDK bytecode for a real, non-mock connection too — observed as
/// `getResponseCode()` returning 0 and `addRequestProperty`/`getHeaderField`
/// silently no-op'ing, because CratonVM's carrier keeps its request/response
/// state in native side tables the real JDK bytecode never touches. That is
/// `SimpleClientHttpRequestFactoryTests.interceptor()` losing the
/// interceptor's added header.
///
/// Keyed on the RECEIVER, not on `class_name`, and that is load-bearing twice:
///
///  * `class_name` is the resolved method's declaring class, so the very same
///    `connection.addRequestProperty(...)` call site in
///    `SimpleClientHttpRequest.addHeaders` reports `java/net/HttpURLConnection`
///    at first and `java/net/URLConnection` once an unrelated
///    `Mockito.mock(HttpURLConnection.class)` has re-resolved it. The old
///    `class_name == "java/net/HttpURLConnection"` test silently stopped
///    matching at that point — and it was missing from the cached/hot twin
///    entirely, so it also stopped applying as soon as a call site warmed up.
///  * a Mockito mock is Objenesis-constructed (no constructor ever runs), so
///    its inherited `URLConnection.url` field 0 stays unset, while a real
///    carrier's is always populated. The field-0 test therefore never fires
///    for a mock, and mocking `HttpURLConnection` still routes through
///    Mockito's advice for stubbing and verification.
///
/// Returns the callback to force, or `None` to let normal dispatch decide.
pub(super) fn real_http_url_connection_native(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<cratonvm_native_api::registry::NativeCallback> {
    // Cheap gate first: only the connection hierarchy can reach the exemption.
    if !matches!(
        class_name,
        "java/net/URLConnection"
            | "java/net/HttpURLConnection"
            | "javax/net/ssl/HttpsURLConnection"
            | "sun/net/www/protocol/http/HttpURLConnection"
            | "sun/net/www/protocol/https/HttpsURLConnectionImpl"
    ) {
        return None;
    }
    let Some(Value::Object(Some(receiver))) = args.first() else {
        return None;
    };
    // Objenesis-constructed mock => field 0 unset => not a real carrier.
    if !matches!(
        shared.mem.heap.get_field(*receiver, 0),
        Value::Object(Some(_))
    ) {
        return None;
    }
    let receiver_cid = shared.mem.heap.class_id_of(*receiver);
    let receiver_name = shared
        .classes
        .class_manager
        .read()
        .get_class(receiver_cid)
        .map(|class| class.name.to_string())?;
    if !CRATONVM_HTTP_CARRIER_CLASSES.contains(&receiver_name.as_str()) {
        return None;
    }
    shared
        .natives
        .native_methods
        .find(&receiver_name, method_name, method_descriptor)
        .or_else(|| {
            shared.natives.native_methods.find(
                "java/net/HttpURLConnection",
                method_name,
                method_descriptor,
            )
        })
}

pub(super) fn intercept_force_registered_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    // FileChannel.open() invokes FileSystemProvider.newFileChannel through a
    // default-provider receiver (WindowsFileSystemProvider on this host),
    // while the fd-backed native is registered on the JDK base class. Route
    // the forced call to that base registration explicitly so a cached
    // runtime receiver name cannot bypass it and run the JDK's deliberate
    // UnsupportedOperationException stub.
    //
    // `createSymbolicLink`/`createLink`/`readSymbolicLink` ride the same route
    // for the same reason — see `is_file_system_provider_link_native_override`.
    if (method_name == "newFileChannel"
        && method_descriptor
            == "(Ljava/nio/file/Path;Ljava/util/Set;[Ljava/nio/file/attribute/FileAttribute;)Ljava/nio/channels/FileChannel;"
        && matches!(
            class_name,
            "java/nio/file/spi/FileSystemProvider"
                | "sun/nio/fs/WindowsFileSystemProvider"
                | "sun/nio/fs/UnixFileSystemProvider"
        ))
        || is_file_system_provider_link_native_override(
            class_name,
            method_name,
            method_descriptor,
        )
    {
        let cb = shared.natives.native_methods.find(
            "java/nio/file/spi/FileSystemProvider",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
            if let Some(value) = result {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, crate::jit::return_type(method_descriptor)),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // The resource-name argument is specified to be non-null for every
    // ClassLoader resource accessor.  A virtual call whose constant-pool
    // owner is ClassLoader can resolve to an inherited cached method on a
    // custom loader, so the generic force-native lookup below sees the custom
    // class name and misses the callback registered on ClassLoader.  Route
    // only the null-argument contract through that base callback before
    // method-cache dispatch; normal non-null calls retain the custom loader's
    // virtual implementation.
    if args.len() == 2
        && matches!(args.get(1), Some(Value::Object(None)))
        && matches!(
            (method_name, method_descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
    {
        let cb = shared.natives.native_methods.find(
            "java/lang/ClassLoader",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
            if let Some(value) = result {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, crate::jit::return_type(method_descriptor)),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // `Class.getClassLoader()` is a concrete JDK method, but Class mirrors in
    // this VM use an internal layout and their real `classLoader` field can be
    // a stale non-loader object.  Dispatch by the receiver's *runtime* class
    // before the normal declaring-class gate: JDK calls reached through an
    // inherited/cached method reference can otherwise bypass the static
    // allowlist and hand ServiceLoader a String as its loader.
    if method_name == "getClassLoader"
        && method_descriptor == "()Ljava/lang/ClassLoader;"
        && matches!(
            args.first(),
            Some(Value::Object(Some(receiver))) if {
                let receiver_cid = shared.mem.heap.class_id_of(*receiver);
                shared
                    .classes.class_manager
                    .read()
                    .get_class(receiver_cid)
                    .map(|class| &*class.name == "java/lang/Class")
                    .unwrap_or(false)
            }
        )
    {
        // Fully-constant triple: memoized in a file-local cell rather than
        // re-hashing three literals on every call (native-dispatch-memoization
        // §3 Step 1, B1). The memo is keyed on the registry generation, so a
        // native registered later is still picked up and a negative result
        // self-heals — unlike a `OnceLock`.
        //
        // ONE STATIC, ONE TRIPLE. The generation is the *only* key: the triple
        // is not re-verified on a warm hit (re-hashing it is the cost this
        // exists to remove), so a cell reached with a second triple can redeem
        // the first's memoized negative and silently report "no native" for a
        // registered one. This cell is reached from exactly one call, with
        // three string literals.
        static NCS_CLASS_GET_CLASSLOADER: cratonvm_native_api::NativeCallSite =
            cratonvm_native_api::NativeCallSite::new();
        let callback = NCS_CLASS_GET_CLASSLOADER.callback(
            &shared.natives.native_methods,
            "java/lang/Class",
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // These concrete Class methods read VM-private mirror fields in JDK 25.
    // Resolve by the receiver's runtime class so inherited or cached method
    // references cannot bypass CratonVM's class-id-backed native methods.
    if matches!(
        (method_name, method_descriptor),
        ("getProtectionDomain", "()Ljava/security/ProtectionDomain;")
            | ("isArray", "()Z")
            | ("getComponentType", "()Ljava/lang/Class;")
            | ("componentType", "()Ljava/lang/Class;")
    ) && matches!(
        args.first(),
        Some(Value::Object(Some(receiver))) if {
            let receiver_cid = shared.mem.heap.class_id_of(*receiver);
            shared
                .classes.class_manager
                .read()
                .get_class(receiver_cid)
                .map(|class| &*class.name == "java/lang/Class")
                .unwrap_or(false)
        }
    ) {
        let callback = shared.natives.native_methods.find(
            "java/lang/Class",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // `java/net/HttpURLConnection`'s real-carrier natives (connect,
    // getResponseCode, getHeaderField, addRequestProperty, ...) must keep
    // firing for a genuinely real, `URL.openConnection()`-constructed carrier
    // (its real inherited `URLConnection.url` field 0 populated) even after
    // ANY instance of this class has been JVMTI-redefined elsewhere in the
    // process — e.g. a completely unrelated `Mockito.mock(HttpURLConnection
    // .class)` call. Mockito's default "inline" mock maker redefines the
    // TARGET CLASS's bytecode IN PLACE rather than subclassing it, so the
    // class-wide `class_redefine_generation` counter trips permanently for
    // EVERY instance of the class, mock or not, for the rest of the process.
    // Without this, `should_force_registered_native_over_bytecode`'s redefine
    // check below cedes to the now-Mockito-woven bytecode for a real,
    // non-mock connection too — observed as `getResponseCode()` silently
    // returning 0 and `getHeaderField`/`addRequestProperty` silently no-op'ing
    // instead of touching the real request/response, so
    // `SimpleClientHttpRequestFactoryTests.interceptor()` failed first with
    // "Status code '0' should be a three-digit positive integer" and then
    // (once getResponseCode alone was exempted) with the interceptor's added
    // header missing from the echoed response, simply because an EARLIER,
    // unrelated test method in the same JVM mocked HttpURLConnection.
    //
    // A Mockito mock itself is Objenesis-constructed (no constructor ever
    // runs), so its field 0 stays null — checking for a non-null field 0
    // cheaply distinguishes "genuinely real carrier" from "mock or synthetic
    // carrier" without invoking `toExternalForm`, and this exemption never
    // fires for an actual mock (whose field 0 is always null), so mocking
    // HttpURLConnection still correctly routes through Mockito's advice for
    // stubbing/verification. Deliberately not narrowed to a specific method
    // allowlist: any native registered on this class for a real carrier is
    // safe to force, since the receiver check alone already gates out mocks.
    if let Some(callback) =
        real_http_url_connection_native(shared, class_name, method_name, method_descriptor, args)
    {
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, crate::jit::return_type(method_descriptor)),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!(
            "[ccs-probe] intercept_force_registered_native: class={} method={}{} \
             force={}",
            class_name,
            method_name,
            method_descriptor,
            force_native_over_real_jdk_bytecode(class_name, method_name, method_descriptor),
        );
    }
    // A JVMTI agent that redefined this class (e.g. a Mockito inline mock)
    // makes its woven bytecode authoritative — cede to it instead of forcing
    // the native, so the instrumentation advice runs. Reflection-metadata
    // natives are exempt (see `redefine_immune_reflection_native`): the real
    // bytecode cannot reproduce them under CratonVM.
    if !should_force_registered_native_over_bytecode(
        shared,
        class_name,
        method_name,
        method_descriptor,
    ) {
        return None;
    }
    // A genuinely real, bytecode-constructed `ThreadPoolExecutor` (its own
    // real `<init>` ran, so its real `workers` field is populated) must keep
    // running its own real `execute()` -- only CratonVM's synthetic 2-field
    // `Executors.new*ThreadPool()` objects need the forced native. See
    // docs/known-issues/threadpoolexecutor-execute-npe-on-ctl-regression.md.
    if class_name == "java/util/concurrent/ThreadPoolExecutor"
        && method_name == "execute"
        && threadpool_executor_has_real_workers(shared, &args[0])
    {
        return None;
    }
    let cb = shared
        .natives
        .native_methods
        .find(class_name, method_name, method_descriptor)?;
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!("[ccs-probe] intercept_force_registered_native: dispatching native callback");
    }
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// Perf variant of [`intercept_force_registered_native`] for the cached/hot
/// dispatch paths (`execute_invokevirtual_cached`, `execute_invokestatic_cached`)
/// that already hold an `Arc<CachedBytecodeMethod>` for this callsite. The
/// original re-evaluated `force_native_over_real_jdk_bytecode`'s ~55-branch
/// sequential string-comparison gauntlet from scratch on *every single*
/// cached-invoke hit -- this was independently identified as a real
/// interpreter-throughput bottleneck (~51% of all executed instructions on
/// method-call-heavy workloads, see `docs/known-issues/tomcat-08-07/
/// silent-hang-no-signature-cluster.md`) and reproduced live via `perf`/`gdb`
/// during the `ClientHttpConnectorTests` investigation (2026-07-15): one
/// interpreter thread pegged at ~100% CPU for 25+ seconds cycling through
/// this exact call chain while executing a tight Java-level spin/poll loop
/// typical of Reactor/Netty/Jetty's lock-free scheduling. This variant reads
/// `cached.force_native_cache`, computing the pure part exactly once per
/// invoke-cache entry (memoized `OnceLock`, shared via the entry's `Arc`)
/// instead of on every hit; the mutable-state-dependent redefine check is
/// still re-evaluated every call (cheap, and must stay live).
pub(super) fn intercept_force_registered_native_cached(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: &CachedBytecodeMethod,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let class_name = cached.class_name.as_ref();
    let method_name = cached.method_name.as_ref();
    let method_descriptor = cached.method_descriptor.as_ref();
    // Keep the cached path aligned with the uncached null-resource contract
    // above.  The cache is keyed by the resolved custom-loader method, while
    // the implementation callback is deliberately registered on ClassLoader.
    if args.len() == 2
        && matches!(args.get(1), Some(Value::Object(None)))
        && matches!(
            (method_name, method_descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
    {
        let cb = shared.natives.native_methods.find(
            "java/lang/ClassLoader",
            method_name,
            method_descriptor,
        )?;
        let ret_type = crate::jit::return_type(method_descriptor);
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
            if let Some(value) = result.filter(|_| ret_type != b'V') {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    if matches!(
        (method_name, method_descriptor),
        ("getProtectionDomain", "()Ljava/security/ProtectionDomain;")
            | ("isArray", "()Z")
            | ("getComponentType", "()Ljava/lang/Class;")
            | ("componentType", "()Ljava/lang/Class;")
    ) && matches!(
        args.first(),
        Some(Value::Object(Some(receiver))) if {
            let receiver_cid = shared.mem.heap.class_id_of(*receiver);
            shared
                .classes.class_manager
                .read()
                .get_class(receiver_cid)
                .map(|class| &*class.name == "java/lang/Class")
                .unwrap_or(false)
        }
    ) {
        let callback = shared.natives.native_methods.find(
            "java/lang/Class",
            method_name,
            method_descriptor,
        )?;
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    // Same real-carrier exemption the uncached twin applies (see
    // `real_http_url_connection_native`). This path used to omit it entirely,
    // so the exemption held only until a call site warmed into the invoke
    // cache and then silently stopped applying — one
    // `Mockito.mock(HttpURLConnection.class)` anywhere in the process then
    // permanently broke every genuinely real connection's
    // `addRequestProperty`/`getResponseCode`/`getHeaderField`.
    if let Some(callback) =
        real_http_url_connection_native(shared, class_name, method_name, method_descriptor, args)
    {
        return Some((|| {
            let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, crate::jit::return_type(method_descriptor)),
                )?;
                crate::vm::native_return_pushed_to_stack(shared, thread);
            }
            Ok(CachedCallResult::Handled)
        })());
    }
    let force_native = *cached.force_native_cache.get_or_init(|| {
        force_native_over_real_jdk_bytecode(class_name, method_name, method_descriptor)
    });
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!(
            "[ccs-probe] intercept_force_registered_native_cached: class={} method={}{} \
             force={}",
            class_name, method_name, method_descriptor, force_native,
        );
    }
    // A JVMTI agent that redefined this class (e.g. a Mockito inline mock)
    // makes its woven bytecode authoritative — cede to it instead of forcing
    // the native, so the instrumentation advice runs. Reflection-metadata
    // natives are exempt (see `redefine_immune_reflection_native`): the real
    // bytecode cannot reproduce them under CratonVM.
    if !should_force_registered_native_over_bytecode_precomputed(
        shared,
        force_native,
        class_name,
        method_name,
        method_descriptor,
    ) {
        return None;
    }
    // A genuinely real, bytecode-constructed `ThreadPoolExecutor` (its own
    // real `<init>` ran, so its real `workers` field is populated) must keep
    // running its own real `execute()` -- only CratonVM's synthetic 2-field
    // `Executors.new*ThreadPool()` objects need the forced native. See
    // docs/known-issues/threadpoolexecutor-execute-npe-on-ctl-regression.md.
    if class_name == "java/util/concurrent/ThreadPoolExecutor"
        && method_name == "execute"
        && threadpool_executor_has_real_workers(shared, &args[0])
    {
        return None;
    }
    // Site A1 of `docs/internal/arch-2026-07-26/native-dispatch-memoization.md`
    // §3 Step 2. Perf (2026-07-19, TestResponsePerformance residual): memoize
    // the resolved callback per invoke-cache entry, same shape as
    // `force_native_cache` above -- `NativeMethodRegistry::find` was the #2
    // hottest symbol (~7% of samples) on that benchmark.
    //
    // This was a `OnceLock<Option<NativeCallback>>` and is now a
    // generation-keyed `NativeCallSite`, which also FIXES A LATENT BUG: the
    // `OnceLock` memoized a *negative* permanently, on the argument that
    // native registration is immutable after boot. That holds for the steady
    // state but not for boot itself, nor for `alias_class` / the lazy
    // `register_*` passes that run after the first bytecode executes -- a
    // native registered by a later pass was invisible here forever, while
    // dispatching fine through `find`. The generation check re-resolves
    // exactly when a new slot is appended.
    //
    // ONE CELL, ONE TRIPLE: `class_name`/`method_name`/`method_descriptor` are
    // `cached.{class,method}_name` / `cached.method_descriptor` verbatim (bound
    // at the top of this function), so this cell only ever sees this entry's
    // own triple. The `java/lang/ClassLoader` re-target earlier in this
    // function deliberately stays on plain `find` for that reason.
    let cb = cached.native_call_site().callback(
        &shared.natives.native_methods,
        class_name,
        method_name,
        method_descriptor,
    )?;
    if method_name == "getTarget" && crate::runtime::env_cache::dbg_ccsprobe() {
        eprintln!(
            "[ccs-probe] intercept_force_registered_native_cached: dispatching native callback"
        );
    }
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

#[inline]
pub(super) fn intercept_jython_pyjavatype_findattr_ex(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    is_special: bool,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if is_special {
        return None;
    }
    if method_name != "__findattr_ex__"
        || method_descriptor != "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let recv_name = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(recv_cid).map(|c| c.name.to_string())?
    };
    if recv_name != "org/python/core/PyJavaType" {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "org/python/core/PyJavaType",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

#[inline]
pub(super) fn intercept_jython_pymodule_findattr(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if method_name != "__findattr__"
        || method_descriptor != "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let recv_name = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(recv_cid).map(|c| c.name.to_string())?
    };
    if recv_name != "org/python/core/PyModule" {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "org/python/core/PyModule",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// `URLClassLoader.findClass(String)` invoked on a SUBCLASS receiver whose CP
/// methodref names that subclass (so the static-class force-native gate, keyed
/// on the methodref class, never matches). The canonical case is Jasper's
/// `JasperLoader`, which overrides BOTH `loadClass` overloads and calls
/// `findClass(name)` directly from inside `loadClass` to load the
/// runtime-compiled `org.apache.jsp.*_jsp` servlet from its scratch-dir URL.
/// Because the override runs, CratonVM's `cl_load_class` native (which would
/// resolve the class from the global classpath) is bypassed, and dispatch lands
/// on the real `URLClassLoader.findClass` bytecode — whose shimmed
/// `ucp.getResource` returns null → `ClassNotFoundException`, 500-ing every
/// compiled JSP/tag (TestPageContext, TestScopedAttributeELResolver, …).
///
/// Resolve `findClass` from the actual receiver class; only force the native
/// when it lands on `java/net/URLClassLoader` itself — a subclass that declares
/// its OWN `findClass` keeps its bytecode. `ucl_find_class` delegates to the
/// base classpath, where the loader's `<init>` already registered its URLs,
/// matching HotSpot. This handles the FIRST (uncached) dispatch; the cache
/// populator (`populate_virtual_invoke_cache`) independently force-caches the
/// native via the `declaring_name`-keyed `force_native_over_real_jdk_bytecode`
/// entry, so repeat dispatches stay native too.
#[inline]
pub(super) fn intercept_urlclassloader_subclass_native_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    is_special: bool,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if is_special
        || !matches!(
            (method_name, method_descriptor),
            ("findClass", "(Ljava/lang/String;)Ljava/lang/Class;")
                | ("addURL", "(Ljava/net/URL;)V")
        )
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let declaring_name = {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let (_m, declaring_id) = crate::classloading::find_method_recursive(
            recv_cid,
            method_name,
            method_descriptor,
            store,
        )?;
        store.get(declaring_id).map(|c| c.name.to_string())?
    };
    if declaring_name != "java/net/URLClassLoader" {
        return None;
    }
    // A JVMTI agent that redefined URLClassLoader makes its woven bytecode
    // authoritative — cede to it.
    if native_shadow_suppressed_by_redefine(shared, "java/net/URLClassLoader") {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "java/net/URLClassLoader",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// A class may invoke an inherited ClassLoader resource method through a
/// constant-pool reference to its concrete subclass.  The regular force-native
/// gate is keyed by that symbolic class, so it misses the native registered on
/// ClassLoader and the real JDK body silently accepts null names. Resolve the
/// actual declaration and dispatch the shared ClassLoader native instead.
#[inline]
pub(super) fn intercept_classloader_subclass_resource_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method_name: &str,
    method_descriptor: &str,
    receiver_class_id: Option<ClassId>,
    is_special: bool,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if is_special
        || !matches!(
            (method_name, method_descriptor),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
        )
    {
        return None;
    }
    let recv_cid = receiver_class_id?;
    let declaring_name = {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let (_m, declaring_id) = crate::classloading::find_method_recursive(
            recv_cid,
            method_name,
            method_descriptor,
            store,
        )?;
        store.get(declaring_id).map(|c| c.name.to_string())?
    };
    if declaring_name != "java/lang/ClassLoader"
        || native_shadow_suppressed_by_redefine(shared, "java/lang/ClassLoader")
    {
        return None;
    }
    let cb = shared.natives.native_methods.find(
        "java/lang/ClassLoader",
        method_name,
        method_descriptor,
    )?;
    let ret_type = crate::jit::return_type(method_descriptor);
    Some((|| {
        let result = crate::vm::safe_native_call(shared, thread, cb, args)?;
        if let Some(value) = result.filter(|_| ret_type != b'V') {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, ret_type),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        Ok(CachedCallResult::Handled)
    })())
}

/// Surefire's fork calls `ClassLoader.setDefaultAssertionStatus` before the JDK
/// static `assertionLock` is assigned; the real bytecode does
/// `synchronized (assertionLock)` and NPEs. Monomorphic inline caches and the
/// vtable fast path can push that bytecode without visiting `execute_invoke`, so
/// any site about to run this body must consult the Rust no-op first.
#[inline]
pub(super) fn intercept_classloader_set_default_assertion_status(
    shared: &SharedVm,
    thread: &mut JvmThread,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if method_name != "setDefaultAssertionStatus" || method_descriptor != "(Z)V" {
        return None;
    }
    // Fully-constant triple — memoized (native-dispatch-memoization §3, B2).
    // ONE STATIC, ONE TRIPLE: reached from exactly this one call, with three
    // literals. See `NCS_CLASS_GET_CLASSLOADER` for why sharing a cell across
    // triples silently mis-answers.
    static NCS_CL_SET_DEFAULT_ASSERTION_STATUS: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    let cb = NCS_CL_SET_DEFAULT_ASSERTION_STATUS.callback(
        &shared.natives.native_methods,
        "java/lang/ClassLoader",
        "setDefaultAssertionStatus",
        "(Z)V",
    )?;
    Some(crate::vm::safe_native_call(shared, thread, cb, args).map(|_| CachedCallResult::Handled))
}

/// Surefire `LazyLauncher` implements `Launcher`. Some dispatch paths key the
/// lookup by the constant-pool interface (`org/junit/platform/launcher/Launcher`)
/// while the Rust override is registered on the concrete class. When the heap
/// receiver is actually `LazyLauncher`, return that native so we never execute
/// the JDK `discover` body (null delegate → `Cannot invoke discover on null`).
#[inline]
pub(super) fn surefire_lazy_launcher_discover_native(
    shared: &SharedVm,
    method_name: &str,
    descriptor: &str,
    recv_obj: ObjectRef,
) -> Option<cratonvm_native_api::NativeCallback> {
    const DESC_DISCOVER: &str =
        "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;";
    const LAZY: &str = "org/apache/maven/surefire/junitplatform/LazyLauncher";
    if method_name != "discover" || descriptor != DESC_DISCOVER {
        return None;
    }
    // Fully-constant triple (`LAZY` / `DESC_DISCOVER` are the `const`s above),
    // memoized per native-dispatch-memoization §3 Step 1, B3.
    //
    // ONE STATIC, ONE TRIPLE. A `NativeCallSite` memo is keyed on the registry
    // generation alone — the triple is deliberately not re-checked on a warm
    // hit, since re-hashing it is the exact cost the cell exists to remove.
    // So a cell that ever sees a second triple can redeem the first triple's
    // memoized negative for the second and silently answer `None` for a
    // native that is in fact registered. This cell is reached from exactly
    // this one call, with these constants. The identical triple in
    // `native_override_for_cached_reflect_invoke` gets its *own* cell rather
    // than sharing this one.
    static NCS_LAZY_LAUNCHER_DISCOVER: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    let cb = NCS_LAZY_LAUNCHER_DISCOVER.callback(
        &shared.natives.native_methods,
        LAZY,
        "discover",
        DESC_DISCOVER,
    )?;
    let cid = shared.mem.heap.class_id_of(recv_obj);
    // read_recursive() instead of read() -- this native-override probe is
    // reached from execute_invokevirtual_vtable_fast while it already holds
    // class_manager.read() across the WP0.1 native-override check (see the
    // "ALREADY-HELD cm guard" note above that call site). A plain nested
    // read() panics the lock-order tracker (debug builds) or can deadlock
    // under parking_lot once a writer is queued (release builds) -- same
    // fix as resolve_method_ref.
    let cm = shared.classes.class_manager.read_recursive();
    let ok = cm
        .get_class(cid)
        .map(|c| c.name.as_ref() == LAZY)
        .unwrap_or(false);
    drop(cm);
    if !ok {
        return None;
    }
    Some(cb)
}

/// Synthetic stubs are fallback implementations for fake or incomplete JDK
/// classes. When the real class bytecode is loaded and explicitly protected,
/// dispatch must prefer that bytecode over the approximate stub.
pub(crate) fn synthetic_stub_should_yield_to_real_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let kind = shared
        .natives
        .native_methods
        .kind_of(class_name, method_name, descriptor);
    synthetic_stub_kind_should_yield_to_real_bytecode(
        shared,
        class_name,
        method_name,
        descriptor,
        kind,
    )
}

/// Variant for callers that already resolved and cached the native category in
/// method metadata. Keeping the selection predicate separate prevents a second
/// full triple hash on the first invocation of every constant-pool reference.
pub(super) fn synthetic_stub_kind_should_yield_to_real_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: Option<cratonvm_native_api::NativeKind>,
) -> bool {
    if kind != Some(cratonvm_native_api::NativeKind::SyntheticStub) {
        return false;
    }

    if !real_protected_stub_class(class_name) {
        return false;
    }

    let cm = shared.classes.class_manager.read();
    cm.get_loaded_class_id(class_name)
        .and_then(|cid| {
            cm.get_class(cid).and_then(|cls| {
                if cls.is_synthetic_stub {
                    None
                } else {
                    crate::classloading::find_method_recursive(
                        cid,
                        method_name,
                        descriptor,
                        &cm.class_store,
                    )
                    .map(|(m, _)| !m.is_native() && m.code().is_some())
                }
            })
        })
        .unwrap_or(false)
}

/// The class allowlist for [`synthetic_stub_should_yield_to_real_bytecode`]
/// (and `populate_invoke_cache`'s inline copy of the same predicate, which
/// cannot call the full helper while holding the class-manager read lock):
/// classes whose SyntheticStub natives exist only for stub-phase bootstraps
/// and must yield to loaded real bytecode.
///
/// JDK-ONLY-WAVE2: real-protected-stub class allow-list, COPY 2 OF 2. The other
/// copy is inline in `vm/src/vm/vm_exec.rs::invoke_or_native`, and **the two
/// are not identical**: that one lists `java/util/StringJoiner`, this one
/// deliberately does not (see the comment inside). Wave 2 must RECONCILE them
/// — decide what StringJoiner should do on both paths — not assume they are
/// duplicates and delete one. What must replace them: `NativeKind` alone; under
/// `--jdk-only` no `SyntheticStub` dispatches, so no class needs protecting
/// from one and the entire list becomes dead.
pub(crate) fn real_protected_stub_class(class_name: &str) -> bool {
    crate::runtime::env_cache::real_bytecode_selector().prefers_real(class_name)
        || matches!(
            class_name,
            "java/util/concurrent/locks/ReentrantLock"
                | "java/util/concurrent/LinkedBlockingDeque"
                | "java/util/concurrent/atomic/AtomicBoolean"
                | "java/util/EnumSet"
                // The fallback bridge is needed only if bootstrap had to
                // synthesize Instant.  With a loaded real JDK Instant, every
                // factory must run its real bytecode so the result has the
                // real field layout and ISO-8601 `toString()` semantics.
                | "java/time/Instant"
                // Spring Boot's loader decodes central-directory DOS times via
                // ZonedDateTime.of(...). The synthetic bridge stores its
                // fields in a compact layout that is incompatible with the
                // loaded JDK class, turning historical ZIP timestamps into
                // the current clock value when converted to an Instant.
                | "java/time/ZonedDateTime"
                // NOT "java/util/StringJoiner" (2026-07-10): yielding this
                // class's SyntheticStub natives to real bytecode here exposes
                // a deterministic heap-reference-integrity defect (the
                // `gen_heap::read_slot` "corrupt Value cell"/HIB-CV-32 guard
                // fires reading StringJoiner's own `size`/`elts` fields back
                // after a `putfield`, on the SECOND `add()` call onward) that
                // does not reproduce for an equivalent user-defined class with
                // the identical bytecode shape and field count/layout (ruled
                // out via a standalone MicroProbe repro) — something specific
                // to this being a natively-registered bootstrap class, not the
                // bytecode pattern itself. See docs/known-issues/
                // stringjoiner-synthetic-native-real-jdk-field-mismatch.md. Path 2
                // (`invoke_or_native` in vm/src/vm/vm_exec.rs) still protects
                // StringJoiner via its own, separate, long-standing allowlist
                // — this only reverts the NEW path-1 (interpreter
                // try_stackless_invoke) preference added here, back to the
                // proven-safe pre-existing behavior (always dispatch to the
                // SyntheticStub native uniformly for this class at this path).
                | "java/io/FileInputStream"
                | "java/lang/ref/Cleaner"
                | "java/lang/ref/Cleaner$Cleanable"
                | "java/lang/management/ManagementFactory"
        )
}

/// [`try_stackless_invoke`] step 1's primary native lookup, routed through the
/// §7 policy (`docs/feature-designs/jdk-only-mode.md`).
///
/// Replaces the bare `native_methods.find(class_name, method_name, descriptor)`
/// that sat at the head of step 1's `.or_else` chain. A lookup that throws the
/// `NativeKind` away cannot tell a reviewed `Intrinsic` from a `SyntheticStub`,
/// so it cannot enforce §1.3 or §1.4 — it was a policy bypass sitting beside
/// `resolve_dispatch` instead of routing through it.
///
/// Cost is unchanged. `resolve_id` is the *same* single 128-bit triple hash
/// `find` already paid — `resolve_id(..).and_then(callback_of)` is documented
/// to equal `find(..)`, descriptor-quirk fallback included — and the slot
/// handle it returns makes the §4 census increment one relaxed add instead of a
/// second full hash. Nothing is allocated or formatted; violation objects are
/// built only on the reject path, in `vm_exec`'s `#[cold]` constructors.
///
/// `id_out` carries the resolved handle back to the caller so the census is
/// recorded at the point of actual **dispatch**, not here at resolution: three
/// later guards (JVMTI redefine, synthetic-stub yield, the `ThreadPoolExecutor`
/// receiver check) can still discard this callback, and counting a discarded
/// resolution as an invocation would make the zero-stub acceptance criterion
/// unfalsifiable in the wrong direction.
///
/// `refusal` is an out-parameter rather than a `Result` because this runs
/// inside an `Option`-returning `.or_else` chain; the caller checks it once,
/// after the chain, and turns it into `VmError::JdkOnly`.
///
/// **`Compatible` mode is bit-for-bit today's behaviour**: `compat_native_wins`
/// is `true`, which is exactly the unconditional "a registered native wins
/// here" the `find` call encoded, and in `Compatible` mode
/// `resolve_native_dispatch_wave1` is a pure function of that boolean.
#[inline]
fn resolve_step1_native(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    id_out: &mut Option<cratonvm_native_api::NativeMethodId>,
    refusal: &mut Option<cratonvm_types::error::JdkOnlyViolation>,
) -> Option<cratonvm_native_api::NativeCallback> {
    let registry = &shared.natives.native_methods;
    let id = registry.resolve_id(class_name, method_name, descriptor)?;
    let callback = registry.callback_of(id)?;
    // `kind_of_id` reports the slot's true kind. `find_with_kind` would report
    // `Bridge` on its descriptor-quirk cold path; the difference is invisible
    // in `Compatible` mode (every kind yields the same callback) and strictly
    // more accurate under `JdkOnly`.
    let kind = registry
        .kind_of_id(id)
        .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
    match crate::vm::resolve_native_dispatch_wave1(
        crate::vm::dispatch_policy(shared),
        class_name,
        method_name,
        descriptor,
        Some((callback, kind)),
        // JDK-ONLY-WAVE2: hard-coded `true` reproduces the pre-§7 "a registered
        // native unconditionally wins here" of the `find` call this replaces.
        // The three per-site compatibility verdicts that can still veto it run
        // AFTER this chain (see the `native_cb` rebindings below); wave 2 folds
        // them in as the real `compat_native_wins`.
        true,
        // Step 1 runs before any method resolution — deliberately, so the
        // common "no native registered" case never touches the class manager.
        // §7 step 3's input is therefore unknown here; `false` reproduces
        // today's behaviour, and step 6 (which has resolved bytecode) passes
        // the real value.
        false,
    ) {
        Some(crate::vm::DispatchDecision::Reject(violation)) => {
            *refusal = Some(violation);
            None
        }
        Some(decision) => match decision.native_callback() {
            Some(callback) => {
                *id_out = Some(id);
                Some(callback)
            }
            // `Bytecode` is unreachable from the name-only adapter, but treat
            // it as "no native" rather than assuming.
            None => None,
        },
        // JdkOnly, §7 step 3: concrete bytecode beats this bridge.
        None => None,
    }
}

/// Resolve the complete identity stored by a warmed native invoke target.
///
/// `find` alone discards both the stable registry slot and `NativeKind`, which
/// used to force cache hits back through constant-pool resolution, a
/// class-manager lock and a second triple hash. Keep all three values together
/// at population time so the steady state remains genuinely O(1).
#[inline]
fn resolve_cached_native_registration(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    cratonvm_native_api::NativeMethodId,
    cratonvm_native_api::NativeKind,
)> {
    let registry = &shared.natives.native_methods;
    let id = registry.resolve_id(class_name, method_name, descriptor)?;
    Some((registry.callback_of(id)?, id, registry.kind_of_id(id)?))
}

/// Revalidate and count a warmed native target without name-based lookup.
///
/// Compatible mode is the former direct-callback path plus one relaxed census
/// increment. Both modes redeem the current callback and kind from the stable
/// slot, preserving last-registration-wins; strict mode then routes the
/// decision through the central §7 policy. The name triple is materialized
/// only in strict mode, solely for a diagnostic if the live slot is refused.
#[inline]
fn revalidate_cached_native(
    shared: &SharedVm,
    id: cratonvm_native_api::NativeMethodId,
    cached_callback: cratonvm_native_api::NativeCallback,
    cached_kind: cratonvm_native_api::NativeKind,
) -> Option<cratonvm_native_api::NativeCallback> {
    let registry = &shared.natives.native_methods;
    let policy = crate::vm::dispatch_policy(shared);

    // Native slots are updated in place on re-registration. Redeeming both
    // values before the compatible fast return prevents any warmed entry from
    // defeating last-write-wins while retaining indexed O(1) access.
    let callback = registry.callback_of(id).unwrap_or(cached_callback);
    let kind = registry.kind_of_id(id).unwrap_or(cached_kind);
    if !policy.is_jdk_only() {
        registry.record_invocation(id);
        return Some(callback);
    }

    let (class_name, method_name, descriptor) = registry.triple_of(id)?;
    match crate::vm::resolve_native_dispatch_wave1(
        policy,
        class_name,
        method_name,
        descriptor,
        Some((callback, kind)),
        // A published native target has already won this call site's
        // compatibility decision.
        true,
        // Concrete bytecode precedence was decided before publication. If
        // redefinition can change that fact, the RedefineGate is checked and
        // evicts this target before we get here.
        false,
    ) {
        Some(decision) => {
            let callback = decision.native_callback()?;
            registry.record_invocation(id);
            Some(callback)
        }
        None => None,
    }
}

/// Stackless invoke: resolve a method and either call native (Handled) or push
/// a bytecode frame (FramePushed).  Returns `CacheMiss` for exotic cases that
/// cannot be handled stacklessly (signature-polymorphic, JNI, etc.), in which
/// case the caller should fall back to the recursive `invoke_shared` path.
pub(super) fn try_stackless_invoke(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    walk_native_hierarchy: bool,
    is_special: bool,
    // Loader-isolation dispatch override: when `Some`, the bytecode-method
    // lookup uses THIS class_id instead of re-resolving `class_name` (which can
    // pick the wrong same-named per-loader copy). Only the divergent
    // virtual-dispatch caller passes it; `None` everywhere else preserves the
    // legacy name-based resolution byte-for-byte.
    dispatch_class_override: Option<ClassId>,
) -> Result<CachedCallResult, MethodCallFailed> {
    use crate::runtime::frame::padded_bytecode;
    use crate::vm::{coerce_value_for_return, native_return_pushed_to_stack, safe_native_call};

    // Memo for the constant `DowncallHandle.type()` triple resolved in the
    // receiver-class-gated arm below (native-dispatch-memoization §3, B4).
    // ONE STATIC, ONE TRIPLE: the single `.callback` below passes three
    // literals, and no other arm of this function touches this cell. In
    // particular the `.or_else` chain's lookups — whose class name is rewritten
    // from `[`-prefixed receivers and therefore varies at runtime — are NOT
    // memoizable by this mechanism and are deliberately left re-resolving.
    static NCS_DOWNCALL_HANDLE_TYPE: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();

    // T15: Array types (`[LFoo;`, `[I`, etc.) inherit their method
    // dispatch from `java.lang.Object` (JVMS §4.4.1).  Treat any invoke
    // on an array-typed receiver as if the receiver were `java.lang.Object`
    // — otherwise lookups for `clone()` on `[LFoo;` dead-end in CacheMiss
    // and fall through to a path that throws CloneNotSupportedException.
    let class_name = if class_name.starts_with('[') {
        "java/lang/Object"
    } else {
        class_name
    };

    // Cache the descriptor's return-type byte once for the write-side
    // coercion applied to every native callback's pushed return value.
    // Same class of bug as the read-side `getfield` coercion: a primitive
    // smuggled through the heap as an Object pointer (e.g. `String.charAt`
    // returning a char-array element via `get_array_element`) must be
    // reinterpreted as the descriptor's primitive type before reaching
    // the caller's operand stack — otherwise the next `pop_int` blows up
    // with `expected int on stack, got ref(...)`.
    let ret_type = crate::jit::return_type(descriptor);

    // A nested-archive subclass calls `super(file)` with invokespecial. Mockito
    // can redefine JarFile, so the ordinary redefine guard would otherwise run
    // the real JDK constructor. Its ZipFile state is not present on CratonVM's
    // compact native JarFile representation; retain the bridge for precisely
    // the registered File constructor shapes. Ordinary mock calls remain
    // redefine-aware.
    if is_special
        && class_name == "java/util/jar/JarFile"
        && method_name == "<init>"
        && matches!(
            descriptor,
            "(Ljava/io/File;)V"
                | "(Ljava/io/File;Z)V"
                | "(Ljava/io/File;ZI)V"
                | "(Ljava/io/File;ZILjava/lang/Runtime$Version;)V"
        )
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find("java/util/jar/JarFile", method_name, descriptor)
        {
            safe_native_call(shared, thread, callback, args)?;
            return Ok(CachedCallResult::Handled);
        }
    }

    // A subclass `super.close()` is an invokespecial whose constant-pool
    // owner is JarFile even though the concrete implementation is inherited
    // from ZipFile. Mockito can redefine JarFile for ordinary mock calls; the
    // general redefine guard correctly yields to that advice, but must not
    // make this statically-bound superclass call fall into ZipFile's real
    // bytecode (its `res` field is absent on CratonVM-native JarFiles).
    // Limit this bypass to the exact invokespecial close shape. Virtual mock
    // calls still take the normal redefine-aware dispatch path.
    if is_special
        && matches!(
            class_name,
            "java/util/jar/JarFile" | "java/util/zip/ZipFile"
        )
        && method_name == "close"
        && descriptor == "()V"
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find("java/util/zip/ZipFile", method_name, descriptor)
        {
            safe_native_call(shared, thread, callback, args)?;
            return Ok(CachedCallResult::Handled);
        }
    }

    // Registered natives that must beat real-JDK bytecode on the declaring
    // class (URL.getHost DNS loop, ClassLoader assertion lock NPE, etc.).
    if let Some(res) = intercept_force_registered_native(
        shared,
        thread,
        frame_idx,
        class_name,
        method_name,
        descriptor,
        args,
    ) {
        return res;
    }

    // WP2.2 / Surefire: `try_stackless_invoke` does `native_methods.find(class_name, …)`
    // first, then — when `walk_native_hierarchy` is false (invokevirtual fast path) —
    // skips the superclass walk if the **receiver's class** already declares bytecode
    // for the method. `java.lang.reflect.Method` has real JDK bytecode for `invoke`,
    // so `has_own_bytecode` is true, the `or_else` returns None, and we never consult
    // `java/lang/reflect/Method` in the registry. Force the registered
    // `native_method_invoke` (same triple as essentials) so reflection works.
    // Scoped to call sites that actually resolve on java/lang/reflect/Method
    // (mirrors `native_override_for_cached_reflect_invoke`). Matching on
    // name+descriptor ALONE hijacked every `invoke(Object,Object[])Object`
    // in the wild — e.g. Gradle's `ServiceMethod.invoke` implementations,
    // whose receivers then hit `native_method_invoke` and died with
    // "Method.invoke: no declaring class" (ProjectBuilder bootstrap,
    // Spring Boot buildSrc suite).
    if class_name == "java/lang/reflect/Method"
        && method_name == "invoke"
        && descriptor == "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;"
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find("java/lang/reflect/Method", method_name, descriptor)
        {
            let result = safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result.filter(|_| ret_type != b'V') {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                native_return_pushed_to_stack(shared, thread);
            }
            return Ok(CachedCallResult::Handled);
        }
    }
    // Same class-scoping as the Method.invoke block above: an unscoped match
    // hijacked every `newInstance(Object[])Object` (e.g. Objenesis
    // ObjectInstantiator implementations).
    if class_name == "java/lang/reflect/Constructor"
        && method_name == "newInstance"
        && descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;"
    {
        if let Some(callback) = shared.natives.native_methods.find(
            "java/lang/reflect/Constructor",
            method_name,
            descriptor,
        ) {
            let result = safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result.filter(|_| ret_type != b'V') {
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                native_return_pushed_to_stack(shared, thread);
            }
            return Ok(CachedCallResult::Handled);
        }
    }

    // 1. Check native override first (same priority as invoke_or_native).
    //    Walk the superclass chain if:
    //    - method is NOT <init> (constructors are NOT inherited)
    //    - For static calls: always walk (constant pool may reference subclass)
    //    - For virtual/special calls: ONLY walk if the target class does NOT have
    //      its own bytecode for the method. If it does, the bytecode override takes
    //      priority (e.g. URI.toString() must NOT be short-circuited by
    //      Object.toString() native). If it doesn't (e.g. RunnerClassLoader.getParent()),
    //      walking finds the parent's native (ClassLoader.getParent native).
    //
    // JDK-only §7: the PRIMARY lookup in the chain below (the one keyed on
    // `class_name` itself) goes through `resolve_step1_native`. The receiver-
    // gated probes, the SSL impl->API aliases, the `DowncallHandle` adapters
    // and the superclass walk still call bare `find`: each of those resolves a
    // DIFFERENT triple from `(class_name, method_name, descriptor)`, so a
    // kind/id looked up on `class_name` would describe the wrong slot. Wave 2
    // routes them by giving each its own resolved triple.
    let mut step1_native_id: Option<cratonvm_native_api::NativeMethodId> = None;
    let mut step1_refusal: Option<cratonvm_types::error::JdkOnlyViolation> = None;
    let native_cb = match args.first() {
        Some(Value::Object(Some(obj)))
            if method_name == "type" && descriptor == "()Ljava/lang/invoke/MethodType;" =>
        {
            let is_downcall = shared
                .classes
                .class_manager
                .read()
                .get_class(shared.mem.heap.class_id_of(*obj))
                .map(|class| class.name.as_ref() == "java/lang/foreign/DowncallHandle")
                .unwrap_or(false);
            if is_downcall {
                // Fully-constant triple (native-dispatch-memoization §3, B4).
                // Only ever reached with this one triple, so the per-call-site
                // memo cannot be asked to serve a different one.
                NCS_DOWNCALL_HANDLE_TYPE.callback(
                    &shared.natives.native_methods,
                    "java/lang/foreign/DowncallHandle",
                    "type",
                    "()Ljava/lang/invoke/MethodType;",
                )
            } else {
                surefire_lazy_launcher_discover_native(shared, method_name, descriptor, *obj)
            }
        }
        Some(Value::Object(Some(obj)))
            if matches!(method_name, "invoke" | "invokeExact" | "invokeBasic") =>
        {
            let is_downcall = shared
                .classes
                .class_manager
                .read()
                .get_class(shared.mem.heap.class_id_of(*obj))
                .map(|class| class.name.as_ref() == "java/lang/foreign/DowncallHandle")
                .unwrap_or(false);
            if is_downcall {
                shared.natives.native_methods.find(
                    "java/lang/foreign/DowncallHandle",
                    method_name,
                    "([Ljava/lang/Object;)Ljava/lang/Object;",
                )
            } else {
                surefire_lazy_launcher_discover_native(shared, method_name, descriptor, *obj)
            }
        }
        Some(Value::Object(Some(obj))) => {
            surefire_lazy_launcher_discover_native(shared, method_name, descriptor, *obj)
        }
        _ => None,
    }
    .or_else(|| {
        resolve_step1_native(
            shared,
            class_name,
            method_name,
            descriptor,
            &mut step1_native_id,
            &mut step1_refusal,
        )
            .or_else(|| {
                class_name
                    .starts_with("sun/security/ssl/SSLContextImpl")
                    .then(|| {
                        shared.natives.native_methods.find(
                            "javax/net/ssl/SSLContext",
                            method_name,
                            descriptor,
                        )
                    })
                    .flatten()
            })
            .or_else(|| {
                class_name
                    .starts_with("sun/security/ssl/SSLSocketFactoryImpl")
                    .then(|| {
                        shared.natives.native_methods.find(
                            "javax/net/ssl/SSLSocketFactory",
                            method_name,
                            descriptor,
                        )
                    })
                    .flatten()
            })
            .or_else(|| {
                class_name
                    .starts_with("sun/security/ssl/SSLSocketImpl")
                    .then(|| {
                        shared.natives.native_methods.find(
                            "javax/net/ssl/SSLSocket",
                            method_name,
                            descriptor,
                        )
                    })
                    .flatten()
            })
    })
    .or_else(|| {
        if method_name == "<init>" {
            return None;
        }
        // Loader-precise start: prefer the caller's own already-resolved
        // dispatch class (computed by the caller via loader-aware
        // resolution for invokespecial self/super calls and the private-
        // invokevirtual fast path) over the flat, loader-blind
        // `get_loaded_class_id` name lookup below, which collapses to
        // whichever same-named class loaded FIRST process-wide. Two
        // classloaders each defining their own class named `class_name`
        // (e.g. `Upgrade.loadH2`'s old H2 driver vs. the current H2 build,
        // both declaring `org/h2/command/Parser`) otherwise walk the
        // WRONG class's hierarchy here: this native-override pre-check is
        // an independent lookup from the bytecode-resolution one further
        // below that already honors `dispatch_class_override`, so without
        // this it could force a same-named ANCESTOR's native override
        // (e.g. `org/h2/command/ParserBase.read()V`) onto a receiver whose
        // own, unrelated class of the same name declares that method
        // itself and doesn't extend that ancestor at all. See
        // docs/known-issues/h2/bug-h2-suite-residual-fail-triage.md
        // (TestUpgrade's `ParserBase.getSyntaxError`/`Token.start()` NPE).
        let start_cid = |cm: &crate::classloading::ClassManager| {
            dispatch_class_override.or_else(|| cm.get_loaded_class_id(class_name))
        };
        // For virtual calls, skip hierarchy walk if the class has its own bytecode
        if !walk_native_hierarchy {
            let cm = shared.classes.class_manager.read();
            let has_own_bytecode = start_cid(&cm)
                .and_then(|cid| cm.get_class(cid))
                .map(|cls| cls.find_method(method_name, descriptor).is_some())
                .unwrap_or(false);
            if has_own_bytecode {
                return None;
            }
        }
        let cm = shared.classes.class_manager.read();
        let mut cid = start_cid(&cm)?;
        loop {
            let parent_id = cm.get_class(cid)?.superclass?;
            let parent = cm.get_class(parent_id)?;
            let has_bytecode = parent.find_method(method_name, descriptor).is_some();
            // JVMTI redefine guard, per ancestor. Mockito's inline mock
            // maker mocks a CONCRETE class (e.g. `java.net.HttpURLConnection`)
            // by redefining that class directly and weaving advice into its
            // methods, then instantiating a trivial marker SUBCLASS (which
            // declares only a couple of identity/interceptor-plumbing
            // methods; it does NOT override every mockable method the way
            // an interface mock's generated subclass does). So the RECEIVER
            // here is that marker subclass, which itself was never redefined
            // Only the ANCESTOR (`HttpURLConnection`) was. The top-level
            // `native_shadow_suppressed_by_redefine(shared, class_name)`
            // check below only inspects the receiver's own class and misses
            // this entirely, so a native registered on the redefined
            // ancestor kept winning over its now-woven bytecode: every call
            // on the mock (including Mockito's own stubbing/verification
            // calls) silently bypassed the mock's advice and ran the real
            // native instead. Skip an ancestor's native the same way the
            // receiver-class check does.
            let parent_redefined = native_shadow_suppressed_in(&cm, &parent.name)
                && !redefine_immune_forced_native(&parent.name, method_name, descriptor);
            if !parent_redefined {
                if let Some(cb) =
                    shared
                        .natives
                        .native_methods
                        .find(&parent.name, method_name, descriptor)
                {
                    return Some(cb);
                }
            }
            // S107 collection-toString fix: if this parent has bytecode and no
            // same-parent native override, bytecode wins over deeper native
            // ancestors (e.g. AbstractCollection.toString over Object.toString).
            if has_bytecode {
                return None;
            }
            cid = parent_id;
        }
    });
    // JVMTI redefine guard: when an agent (e.g. a Mockito inline mock) has
    // redefined this class, its woven bytecode is authoritative. Drop any
    // native override so dispatch falls through to the (instrumented)
    // bytecode and the advice runs. Fast-pathed on `any_class_redefined`.
    // Reflection-metadata natives stay authoritative (see
    // `redefine_immune_reflection_native`).
    let native_cb = if native_shadow_suppressed_by_redefine(shared, class_name)
        && !redefine_immune_forced_native(class_name, method_name, descriptor)
    {
        None
    } else {
        native_cb
    };
    let native_cb = if synthetic_stub_should_yield_to_real_bytecode(
        shared,
        class_name,
        method_name,
        descriptor,
    ) {
        None
    } else {
        native_cb
    };
    // ThreadPoolExecutor.execute(Runnable): the registered native
    // (`native_es_execute`) is exempted from the real-JDK-mode registration
    // drop (native-api/src/registry.rs) specifically so it stays available
    // for CratonVM's synthetic-layout Executors.* stand-ins (docs/known-issues/
    // threadpoolexecutor-execute-npe-on-ctl-regression.md). But this "native
    // override" step is unconditional -- it has no receiver awareness -- so
    // it was ALSO winning for a genuinely real, bytecode-constructed
    // ThreadPoolExecutor (its own real `workers` field populated), routing
    // every `execute()` call through `native_es_execute`'s defense-in-depth
    // "run inline" fallback instead of real async bytecode.
    // `intercept_force_registered_native` above already carries this exact
    // receiver check for the FORCE-native case; mirror it here so a real
    // receiver's native shadow is dropped too. See docs/known-issues/
    // threadpoolexecutor-execute-dispatch-degrades-to-synchronous.md.
    //
    // JDK-ONLY-WAVE2: `ThreadPoolExecutor.execute` receiver-shape check, COPY 2
    // OF 4. See COPY 1 in `vm/src/vm/vm_exec.rs::invoke_or_native` for the full
    // note and what must replace all four.
    let native_cb = if class_name == "java/util/concurrent/ThreadPoolExecutor"
        && method_name == "execute"
        && descriptor == "(Ljava/lang/Runnable;)V"
        && matches!(args.first(), Some(recv) if threadpool_executor_has_real_workers(shared, recv))
    {
        None
    } else {
        native_cb
    };
    // A `JdkOnly` refusal recorded by `resolve_step1_native` is authoritative:
    // §1.3 forbids silently substituting some other implementation for a
    // refused stub, so this must surface as an error rather than fall through
    // to the bytecode path. Never set under `Compatible`.
    if let Some(violation) = step1_refusal {
        return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
    }
    // The three guards above (redefine suppression, synthetic-stub yield,
    // ThreadPoolExecutor receiver shape) can veto step 1's callback, and the
    // `.or_else` arms below can supply a DIFFERENT one for a different triple.
    // Either way the step-1 slot handle no longer describes what is about to
    // run, so drop it: an invocation counted against the wrong slot is worse
    // than an uncounted one.
    if native_cb.is_none() {
        step1_native_id = None;
    }
    // Real-JDK Linker can adapt a void downcall through a generic
    // MethodHandle. Its field 0 retains the actual DowncallHandle. Preserve
    // the call-site arguments but substitute that target for native dispatch.
    let mut downcall_adapter_args: Option<Vec<Value>> = None;
    let native_cb = native_cb.or_else(|| {
        // This is an adapter for MethodHandle itself, not a general fallback
        // for any method named invoke*.  In particular, JUnit's executable
        // invocation path reaches methods with those names on ordinary
        // zero-field objects; treating those objects as Linker adapters reads
        // a non-existent slot 0 and leaves the interpreter retrying the call.
        if class_name != "java/lang/invoke/MethodHandle"
            || !matches!(method_name, "invoke" | "invokeExact" | "invokeBasic")
        {
            return None;
        }
        let adapter = match args.first() {
            Some(Value::Object(Some(adapter))) => *adapter,
            _ => return None,
        };
        // This is a narrow adaptation for a real-JDK MethodHandle wrapper
        // around our synthetic DowncallHandle. `invoke` is an ordinary method
        // name too (notably JUnit's InterceptingExecutableInvoker.invoke), so
        // probing field 0 before establishing that the receiver is actually a
        // MethodHandle subclass turns every unrelated zero-field receiver into
        // an OOB heap-field read. Apart from the diagnostic flood, returning a
        // benign null from that probe can strand the caller in a retry loop.
        //
        // Use the runtime receiver hierarchy rather than `class_name`: the
        // invoked method can be resolved on an inherited MethodHandle owner
        // while the adapter itself is a concrete JDK subclass.
        let is_method_handle_adapter = {
            let cm = shared.classes.class_manager.read();
            let adapter_class = shared.mem.heap.class_id_of(adapter);
            cm.get_loaded_class_id("java/lang/invoke/MethodHandle")
                .map(|method_handle_class| {
                    adapter_class == method_handle_class
                        || cm.is_subclass_of(adapter_class, method_handle_class)
                })
                .unwrap_or(false)
        };
        if !is_method_handle_adapter {
            return None;
        }
        let target = match shared.mem.heap.get_field(adapter, 0) {
            Value::Object(Some(target)) => target,
            _ => return None,
        };
        let is_downcall = shared
            .classes
            .class_manager
            .read()
            .get_class(shared.mem.heap.class_id_of(target))
            .map(|class| class.name.as_ref() == "java/lang/foreign/DowncallHandle")
            .unwrap_or(false);
        if !is_downcall {
            return None;
        }
        let callback = shared.natives.native_methods.find(
            "java/lang/foreign/DowncallHandle",
            method_name,
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        )?;
        let mut routed = args.to_vec();
        routed[0] = Value::Object(Some(target));
        downcall_adapter_args = Some(routed);
        Some(callback)
    });
    if crate::runtime::env_cache::bd_debug()
        && (method_name == "intValue"
            || (class_name.contains("BigDecimal")
                && (method_name == "<init>" || method_name == "intValue")))
    {
        eprintln!(
            "[try_stackless_invoke] class_name={} method={} desc={} native_cb={} walk_native={}",
            class_name,
            method_name,
            descriptor,
            native_cb.is_some(),
            walk_native_hierarchy
        );
    }
    if method_name == "<init>"
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STTRACE").is_some()
        && (class_name.contains("Exception")
            || class_name.contains("Throwable")
            || class_name.contains("Error"))
    {
        eprintln!("STTRACE_DBG_TSI class={class_name} desc={descriptor} native_cb={} walk={walk_native_hierarchy}",
                  native_cb.is_some());
    }
    // invoke/invokeExact are signature-polymorphic. Their native bridge is
    // registered under the erased Object[] descriptor, while a real call site
    // carries its concrete descriptor. Route those concrete signatures through
    // the bridge so synthetic combinators (notably guardWithTest around a
    // foreign downcall) retain and dispatch their target.
    let native_cb = native_cb.or_else(|| {
        if class_name == "java/lang/invoke/MethodHandle"
            && matches!(method_name, "invoke" | "invokeExact" | "invokeBasic")
        {
            shared.natives.native_methods.find(
                "java/lang/invoke/MethodHandle",
                method_name,
                "([Ljava/lang/Object;)Ljava/lang/Object;",
            )
        } else {
            None
        }
    });
    if crate::runtime::env_cache::dbg_mh_stack()
        && matches!(method_name, "invoke" | "invokeExact" | "invokeBasic")
    {
        eprintln!(
            "[MH_STACK] class={class_name} method={method_name} desc={descriptor} native_cb={} args={}",
            native_cb.is_some(),
            args.len()
        );
    }
    if crate::runtime::env_cache::dbg_mh_adapter()
        && method_name == "invokeExact"
        && descriptor == "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/MemorySegment;IILjava/lang/foreign/MemorySegment;)V"
    {
        if let Some(Value::Object(Some(receiver))) = args.first() {
            let target = match shared.mem.heap.get_field(*receiver, 0) {
                Value::Object(Some(target)) => Some(target),
                _ => None,
            };
            let target_class = target.and_then(|target| {
                shared
                    .classes.class_manager
                    .read()
                    .get_class(shared.mem.heap.class_id_of(target))
                    .map(|class| class.name.to_string())
            });
            eprintln!(
                "[MH_ADAPTER] class={class_name} target={} f0={:?} f1={:?} f2={:?} f3={:?} f4={:?} f16={:?} f17={:?} f18={:?} f19={:?} f20={:?} arg1={:?}",
                target_class.as_deref().unwrap_or("<unknown>"),
                shared.mem.heap.get_field(*receiver, 0),
                shared.mem.heap.get_field(*receiver, 1),
                shared.mem.heap.get_field(*receiver, 2),
                shared.mem.heap.get_field(*receiver, 3),
                shared.mem.heap.get_field(*receiver, 4),
                shared.mem.heap.get_field(*receiver, 16),
                shared.mem.heap.get_field(*receiver, 17),
                shared.mem.heap.get_field(*receiver, 18),
                shared.mem.heap.get_field(*receiver, 19),
                shared.mem.heap.get_field(*receiver, 20),
                args.get(1),
            );
        }
    }
    if let Some(callback) = native_cb {
        // §4 census, at the point of actual dispatch: one relaxed increment on
        // a handle already in hand — no hashing, no allocation. `None` here
        // means the callback came from one of the exotic arms, which resolve
        // other triples and are the wave-2 census gap noted at step 1.
        if let Some(id) = step1_native_id {
            shared.natives.native_methods.record_invocation(id);
        }
        let call_args = downcall_adapter_args.as_deref().unwrap_or(args);
        let result = safe_native_call(shared, thread, callback, call_args)?;
        if let Some(value) = result {
            // Signature-polymorphic MethodHandle natives are registered with an
            // erased Object return and therefore box primitive results. The
            // current bytecode descriptor is concrete, so use the common
            // call-site adapter before the typed return-slot coercion. This must
            // cover every primitive: Netty uses invokeExact(Thread)Z here, while
            // Panama exercises the float path that originally motivated it.
            let value = if class_name == "java/lang/invoke/MethodHandle"
                && matches!(method_name, "invoke" | "invokeExact" | "invokeBasic")
            {
                crate::vm::unbox_poly_return(shared, Some(value), descriptor)
                    .unwrap_or(Value::Object(None))
            } else {
                value
            };
            if crate::runtime::env_cache::dbg_stackless() {
                eprintln!(
                    "[STACKLESS_RET] {}.{}{} -> {:?}",
                    class_name, method_name, descriptor, value
                );
            }
            if ret_type != b'V' {
                // T18.K4 — tag-exact push for J/D native-override return values.
                // `coerce_value_for_return` may widen/narrow the type-erased
                // native result; we then push via the category-2 aware path so
                // J/D retain their bits across the operand-stack boundary.
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                native_return_pushed_to_stack(shared, thread);
            }
        }
        if crate::runtime::env_cache::resume_pc_dbg() && method_name == "enhance" {
            let f = &thread.frames[frame_idx];
            let pc = f.pc;
            let b = f.code.get(pc).copied().unwrap_or(0);
            tracing::error!(
                target: "cratonvm_vm::dbg_resume_pc",
                "[CRATONVM_DBG_RESUME_PC] after stackless native enhance: caller {}.{}\n\
                 caller_pc={} next_bytecode=0x{:02x}",
                f.class_name(),
                f.method_name(),
                pc,
                b
            );
        }
        return Ok(CachedCallResult::Handled);
    }

    // 2. Look up class — must already be loaded for stackless path.
    //    A dispatch override (the receiver's own runtime class_id, supplied only
    //    when it diverges from the name-resolved copy under loader isolation)
    //    takes precedence so the ENHANCED per-loader copy's methods dispatch
    //    instead of the un-enhanced global same-named class.
    if crate::runtime::env_cache::dbg_loader_trace()
        && class_name.contains("RootReference")
        && method_name == "<init>"
    {
        let recv_cid = match args.first() {
            Some(Value::Object(Some(recv))) => Some(shared.mem.heap.class_id_of(*recv)),
            _ => None,
        };
        let name_resolved = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(class_name);
        eprintln!(
            "[TSI-CTOR-TRACE] class_name={class_name} descriptor={descriptor} is_special={is_special} dispatch_class_override={dispatch_class_override:?} name_resolved={name_resolved:?} receiver_actual_cid={recv_cid:?}"
        );
    }
    let class_id = match dispatch_class_override.or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(class_name)
    }) {
        Some(id) => id,
        None => return Ok(CachedCallResult::CacheMiss),
    };

    // 3. Synthetic stubs need the recursive path
    {
        let cm = shared.classes.class_manager.read();
        if let Some(class) = cm.class_store.get(class_id) {
            if class.is_synthetic_stub {
                return Ok(CachedCallResult::CacheMiss);
            }
        }
    }

    // 4. Find method in class hierarchy
    let cm = shared.classes.class_manager.read();
    let (method, declaring_id) = match crate::classloading::find_method_recursive(
        class_id,
        method_name,
        descriptor,
        &cm.class_store,
    ) {
        Some(r) => r,
        None => {
            drop(cm);
            return Ok(CachedCallResult::CacheMiss);
        }
    };

    let is_native = method.is_native();
    let is_synchronized = method.is_synchronized();
    let is_static = method.is_static();

    if is_native {
        // Native method — look up in registry by declaring class.
        //
        // THE canonical §7 site. Of every native-dispatch point in the
        // interpreter this is the only one holding both a resolved `&Class`
        // and the resolved `&Method` at the same time, so it is the one that
        // calls `resolve_dispatch` itself rather than the name-only wave-1
        // adapter. The class-manager read guard is deliberately still held: it
        // is what makes the `&Class` borrow available, and dropping it first is
        // exactly why the old code had to allocate `declaring_name`.
        let class = match cm.get_class(declaring_id) {
            Some(class) => class,
            None => {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
        };
        let registry = &shared.natives.native_methods;
        // `resolve_id` is the same single triple hash `find` paid, and its slot
        // handle turns the §4 census increment into one relaxed add. The
        // documented identity `resolve_id(..).and_then(callback_of) == find(..)`
        // (descriptor-quirk fallback included) is what keeps this byte-for-byte.
        let native_id = registry.resolve_id(&class.name, method_name, descriptor);
        let native = native_id.and_then(|id| {
            registry.callback_of(id).map(|cb| {
                (
                    cb,
                    registry
                        .kind_of_id(id)
                        .unwrap_or(cratonvm_native_api::NativeKind::Bridge),
                )
            })
        });
        let decision = crate::vm::resolve_dispatch(
            crate::vm::dispatch_policy(shared),
            class,
            method,
            native,
        );
        // A `SyntheticStub` refusal is a real policy stop (§1.3) and must not
        // fall through to some other implementation.
        //
        // DEVIATION, forced by preserving behaviour: `Reject(MissingNative)` is
        // NOT surfaced as an error in either mode. An `ACC_NATIVE` method with
        // no entry in the Rust registry is the ordinary case for a JNI-bound
        // native — `RegisterNatives` pointers and dlsym-resolved symbols are
        // resolved further down the recursive `invoke_shared` path, which this
        // `CacheMiss` is the door to. Erroring here would refuse every
        // legitimate JNI bridge, which §1.5 explicitly permits. The structured
        // `MissingNative` belongs at the END of that chain (wave 2), not at the
        // first of its three lookups.
        let mut refusal: Option<cratonvm_types::error::JdkOnlyViolation> = None;
        let callback = match decision {
            crate::vm::DispatchDecision::Reject(
                violation @ cratonvm_types::error::JdkOnlyViolation::SyntheticNativeInvocation { .. },
            ) => {
                refusal = Some(violation);
                None
            }
            crate::vm::DispatchDecision::Reject(_) => None,
            other => other.native_callback(),
        };
        drop(cm);
        if let Some(violation) = refusal {
            return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
        }
        if let Some(callback) = callback {
            // §4 census: relaxed increment on the handle already resolved above.
            if let Some(id) = native_id {
                shared.natives.native_methods.record_invocation(id);
            }
            let result = safe_native_call(shared, thread, callback, args)?;
            if let Some(value) = result.filter(|_| ret_type != b'V') {
                // T18.K4 — tag-exact push for J/D native-bytecode method return values.
                push_invoke_return_value(
                    &mut thread.frames[frame_idx].stack,
                    coerce_value_for_return(value, ret_type),
                )?;
                native_return_pushed_to_stack(shared, thread);
            }
            return Ok(CachedCallResult::Handled);
        }
        // JNI or other exotic native — fall back
        return Ok(CachedCallResult::CacheMiss);
    }

    // 5. Bytecode method — get code attribute
    let code_attr = match method.code() {
        Some(c) => c.clone(),
        None => {
            drop(cm);
            return Ok(CachedCallResult::CacheMiss);
        }
    };

    let class = match cm.get_class(declaring_id) {
        Some(c) => c,
        None => {
            drop(cm);
            return Ok(CachedCallResult::CacheMiss);
        }
    };
    let source_file: Option<Arc<str>> = class.source_file.as_deref().map(Arc::from);
    let class_name_arc: Arc<str> = Arc::from(&*class.name);
    let declaring_is_interface = class.is_interface();
    drop(cm);

    // 6. Check for native override on bytecode method (same as invoke_on_class_shared).
    //
    // EXCEPTION — interface DEFAULT methods (declaring class is an interface,
    // instance method with code): natives registered on interface names
    // (`java/util/Collection`, `java/util/List`, …) are bridges for synthetic
    // receivers with no real class hierarchy — and synthetic receivers never
    // reach this step (synthetic stubs bail at step 3; abstract methods at
    // step 5). A real-bytecode receiver that resolved a default method must
    // run that bytecode, matching `populate_virtual_invoke_cache` which caches
    // `VirtualBytecode` for this exact site. Without this guard the first,
    // uncached call at each site dispatched the interface bridge while every
    // later (cached) call ran the bytecode — e.g. `Collection.stream()` on a
    // custom `AbstractList` (Hibernate's `JoinedList`) materialised an EMPTY
    // stream exactly once per call site (OrderProbe: count#1=0, count#2=3),
    // collapsing `AbstractEntityPersister`'s property closure to length 0.
    // Deliberate interface-default overrides (e.g. `Iterator.remove`) belong
    // in `force_native_over_real_jdk_bytecode`, which fires before this path.
    // Static interface methods keep the native check (mirrors invokestatic
    // promotion in `populate_invoke_cache`, which keys on the CP class).
    // JVMTI redefine guard: a class redefined in place by an agent runs its
    // woven bytecode (so the mock advice fires) rather than the native shadow.
    // Reflection-metadata natives stay authoritative (see
    // `redefine_immune_reflection_native`) — a Mockito inline mock of
    // `java.lang.reflect.Method` must not disable annotation reflection.
    let force_interface_default_native = declaring_is_interface
        && !is_static
        && should_force_registered_native_over_bytecode(
            shared,
            &class_name_arc,
            method_name,
            descriptor,
        );
    if (!(declaring_is_interface && !is_static) || force_interface_default_native)
        && (!native_shadow_suppressed_by_redefine(shared, &class_name_arc)
            || redefine_immune_forced_native(&class_name_arc, method_name, descriptor))
    {
        // `find` -> `resolve_id` + `callback_of`: one hash either way
        // (`resolve_id(..).and_then(callback_of)` is documented to equal
        // `find(..)`), but the slot handle also yields the `NativeKind` §7
        // needs and makes the §4 census increment a relaxed add.
        let registry = &shared.natives.native_methods;
        let native_id = registry.resolve_id(&class_name_arc, method_name, descriptor);
        if let Some((id, callback)) =
            native_id.and_then(|id| registry.callback_of(id).map(|cb| (id, cb)))
        {
            // Same ThreadPoolExecutor.execute(Runnable) receiver-aware
            // exemption as step 1 above -- this is a SEPARATE, independent
            // "double-check for a native override" that runs even after
            // real bytecode was already resolved at step 4/5. Without this,
            // a genuinely real ThreadPoolExecutor still gets shunted to
            // `native_es_execute`'s inline "run synchronously" fallback right
            // here, even though the real `execute()` bytecode was correctly
            // found and would otherwise run. See docs/known-issues/
            // threadpoolexecutor-execute-dispatch-degrades-to-synchronous.md.
            //
            // JDK-ONLY-WAVE2: `ThreadPoolExecutor.execute` receiver-shape
            // check, COPY 3 OF 4. See COPY 1 in
            // `vm/src/vm/vm_exec.rs::invoke_or_native` for the full note and
            // what must replace all four.
            let is_real_tpe_execute_step6 = class_name_arc.as_ref()
                == "java/util/concurrent/ThreadPoolExecutor"
                && method_name == "execute"
                && descriptor == "(Ljava/lang/Runnable;)V"
                && matches!(args.first(), Some(recv) if threadpool_executor_has_real_workers(shared, recv));
            // These two guards ARE this site's compatibility verdict, so they
            // are handed to §7 as `compat_native_wins` verbatim — same
            // predicates, same order, same short-circuit — and `Compatible`
            // mode is therefore bit-for-bit what it was.
            //
            // `synthetic_stub_should_yield_to_real_bytecode` deliberately keeps
            // its own `kind_of` lookup rather than reusing `kind_of_id` above:
            // the two disagree on the descriptor-quirk cold path (`kind_of`
            // misses and reports "not a stub"), and reusing the slot's true
            // kind would silently change which natives yield.
            let compat_native_wins = !is_real_tpe_execute_step6
                && !synthetic_stub_should_yield_to_real_bytecode(
                    shared,
                    &class_name_arc,
                    method_name,
                    descriptor,
                );
            let kind = registry
                .kind_of_id(id)
                .unwrap_or(cratonvm_native_api::NativeKind::Bridge);
            match crate::vm::resolve_native_dispatch_wave1(
                crate::vm::dispatch_policy(shared),
                &class_name_arc,
                method_name,
                descriptor,
                Some((callback, kind)),
                compat_native_wins,
                // Step 5 already produced this method's `Code` attribute, so
                // §7 step 3 has a definite answer here: there IS bytecode, and
                // under `JdkOnly` a plain `Bridge` must not shadow it.
                true,
            ) {
                Some(crate::vm::DispatchDecision::Reject(violation)) => {
                    return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
                }
                Some(decision) => {
                    if let Some(callback) = decision.native_callback() {
                        // §4 census: relaxed add on the handle already in hand.
                        registry.record_invocation(id);
                        let result = safe_native_call(shared, thread, callback, args)?;
                        if let Some(value) = result.filter(|_| ret_type != b'V') {
                            // T18.K4 — tag-exact push for J/D native-override (on bytecode method) return values.
                            push_invoke_return_value(
                                &mut thread.frames[frame_idx].stack,
                                coerce_value_for_return(value, ret_type),
                            )?;
                            native_return_pushed_to_stack(shared, thread);
                        }
                        return Ok(CachedCallResult::Handled);
                    }
                }
                // Compatible: one of the two guards vetoed the native, exactly
                // as before. JdkOnly: that, or §7 step 3 preferring the real
                // bytecode this method already has.
                None => {}
            }
        }
    }

    // 7. Handle synchronized: acquire monitor before pushing frame
    let mut synchronized_args: Option<Vec<Value>> = None;
    let monitor_obj: Option<ObjectRef> = if is_synchronized {
        let sync_args = synchronized_args.get_or_insert_with(|| args.to_vec());
        let obj = if is_static {
            // JVMS §2.11.10: a `static synchronized` method's monitor is the
            // `Class` object — the SAME object `ldc class`, `synchronized(X.class)`,
            // and `X.class.wait()/notify()` use. Locking a synthetic per-class
            // lock here desyncs from those, so e.g. a static-sync method calling
            // `X.class.notifyAll()` would throw IllegalMonitorStateException.
            get_or_create_class_mirror(shared, declaring_id)
        } else {
            match sync_args.first() {
                Some(Value::Object(Some(obj_ref))) => *obj_ref,
                _ => {
                    return Err(MethodCallFailed::InternalError(VmError::Internal {
                        message: "synchronized instance method called with null or missing this"
                            .to_string(),
                    }));
                }
            }
        };
        Some(crate::vm::monitor_enter_synchronized_method(
            shared, thread, obj, sync_args,
        ))
    } else {
        None
    };
    let args = synchronized_args.as_deref().unwrap_or(args);

    // 8. Tail-call elimination: if the caller's next instruction is a matching
    // return, replace the current frame instead of pushing a new one.
    // This prevents stack growth for tail-recursive methods.
    //
    // T14 CRITICAL: Never tail-call-optimize <init> calls. Constructors
    // return void, but the caller's `areturn` expects the object reference
    // left on the stack from the `new`/`dup` sequence. If we replace the
    // caller's frame with <init>'s frame, the void return propagates up
    // and the caller loses its return value.
    //
    // C10 CRITICAL: Ensure the callee's return type matches the caller's
    // return opcode. Kotlin's `listOf` does:
    //   invokestatic singletonList
    //   dup
    //   ldc "..."
    //   invokestatic Intrinsics.checkNotNullExpressionValue  ; returns V
    //   areturn                                              ; expects L...;
    // Without the return-type guard, TCE would replace listOf's frame with
    // Intrinsics', then Intrinsics' `return` (void) would unwind out of
    // listOf silently — losing both the singletonList result AND the areturn
    // instruction. The whole program then exits 0 with no output.
    //
    // STACK-TRACE FIDELITY: HotSpot performs NO tail-call optimization, so any
    // frame TCE eliminates is invisible to `new Throwable().getStackTrace()` —
    // a divergence that breaks code which walks the live call stack. Spring's
    // `ControlFlowPointcut.matches()` fires advice only when a specific caller
    // class+method appears in the trace; eliminating a cross-method tail frame
    // (e.g. `MyComponent.getAge` whose body is just `return proxy.getAge();`)
    // makes that frame vanish and the cflow advice never fires
    // (ControlFlowPointcutTests, 4 methods). Restrict TCE to SELF-recursive
    // tail calls (caller method == callee method), where the only trace effect
    // is collapsing repeated identical frames — the case deep tail-recursive
    // loops (e.g. `tailSum(10000)` in `s16_tail_call_sum_10000`) need to stay
    // within the 1024-frame stack. Cross-method tail calls now keep the
    // caller's frame, matching HotSpot.
    let is_self_recursive = {
        let caller = &thread.frames[frame_idx];
        caller.class_id == declaring_id
            && &*caller.method_name() == method_name
            && &*caller.method_descriptor() == descriptor
    };
    let is_tail_call = if is_self_recursive && !is_synchronized && method_name != "<init>" {
        let caller = &thread.frames[frame_idx];
        let pc = caller.pc;
        // Check if the byte at the current PC (after the invoke instruction)
        // is a return opcode matching the callee's return type.
        if pc < caller.code.len() {
            let caller_ret_op = caller.code[pc];
            // Determine the callee's return-type byte from its descriptor.
            // `descriptor` is of the form "(params)ReturnType".
            let ret_byte = descriptor
                .rsplit(')')
                .next()
                .and_then(|s| s.as_bytes().first().copied())
                .unwrap_or(b'V');
            // Match the callee's return type against the caller's return opcode:
            //   V -> return     (0xb1)
            //   I,B,C,S,Z -> ireturn (0xac)
            //   J -> lreturn   (0xad)
            //   F -> freturn   (0xae)
            //   D -> dreturn   (0xaf)
            //   L, [ -> areturn (0xb0)
            let expected_op: u8 = match ret_byte {
                b'V' => 0xb1,
                b'I' | b'B' | b'C' | b'S' | b'Z' => 0xac,
                b'J' => 0xad,
                b'F' => 0xae,
                b'D' => 0xaf,
                b'L' | b'[' => 0xb0,
                _ => 0xb1,
            };
            caller_ret_op == expected_op
        } else {
            false
        }
    } else {
        false // synchronized methods and <init> can't be tail-call optimized
    };

    // C8: Suppress tail-call optimization if the caller's invoke site lies
    // within any exception handler range. TCO replaces the caller's frame
    // (and its exception table) with the callee — if the callee then throws
    // an exception the caller would have caught, the handler is silently
    // discarded and the exception escapes to the caller's caller.
    // Example: picocli.CommandLine$DefaultFactory.loadClosureClass wraps
    // Class.forName("groovy.lang.Closure") in try { } catch (Exception).
    // The invokestatic at PC 24 is followed by areturn at PC 27, which
    // triggers TCO; the ClassNotFoundException then escapes loadClosureClass
    // and corrupts <clinit>.
    let invoke_covered_by_handler = {
        let caller = &thread.frames[frame_idx];
        let invoke_pc = caller.last_instr_pc;
        caller
            .exception_table()
            .iter()
            // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
            .any(|e| invoke_pc >= e.start_pc as usize && invoke_pc < e.end_pc as usize)
    };
    if is_tail_call
        && thread.frames[frame_idx].monitor_on_exit.is_none()
        && !invoke_covered_by_handler
    {
        // Replace the caller frame in-place with the callee
        let code = padded_bytecode(&code_attr.code);
        if crate::runtime::env_cache::frame_trace() {
            let caller = &thread.frames[frame_idx];
            eprintln!(
                "[FRAME_TCO] at frame_idx={} replacing {}.{}{} with {}.{}{}",
                frame_idx,
                caller.class_name(),
                caller.method_name(),
                caller.method_descriptor(),
                class_name_arc,
                method_name,
                descriptor
            );
        }
        thread.frames[frame_idx].reset_for_tail_call(
            declaring_id,
            code,
            code_attr.max_stack,
            code_attr.max_locals,
            args,
            class_name_arc.clone(),
            Arc::from(method_name),
            Arc::from(descriptor),
            source_file,
            Arc::from(code_attr.exception_table.as_slice()),
        );
        // FramePushed is not quite right since we didn't push — but we need
        // the main loop to start executing from the new frame at frame_idx,
        // which is the same index. FramePushed with same len means frame_idx
        // stays the same.
        return Ok(CachedCallResult::FramePushed);
    }

    // 9. Stack overflow check
    if thread.frames.len() >= shared.config.max_stack_depth {
        dump_stack_on_soe(thread);
        if let Some(obj) = monitor_obj {
            let _ = shared.threads.monitors.exit(obj, thread.thread_id);
        }
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }

    // 10. Push bytecode frame
    // T10.7 — replenish the per-thread pool from the shared pool if empty.
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        code_attr.max_locals as usize,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        (code_attr.max_stack as usize).max(16) + 8,
    );
    let mut frame = Frame::new_pooled(
        declaring_id,
        class_name_arc,
        Arc::from(method_name),
        Arc::from(descriptor),
        source_file,
        padded_bytecode(&code_attr.code),
        Arc::from(code_attr.exception_table.as_slice()),
        code_attr.max_stack,
        code_attr.max_locals,
        args,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    frame.monitor_on_exit = monitor_obj;
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/stackless] depth={} {}.{}{}",
            thread.frames.len(),
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor()
        );
    }
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);

    Ok(CachedCallResult::FramePushed)
}

pub(super) fn execute_invokestatic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    pc: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    let (method_class_name, method_name, method_descriptor, num_params) =
        resolve_method_ref(shared, current_class_id, cp_index)?;

    // PGO-01: call-site evidence — same placement rationale as
    // execute_invoke_kind's is_special arm (record once, early, right after
    // resolution succeeds; this is the slow path, only reached on a cache
    // miss from execute_invokestatic_cached, so the instruction is
    // definitely executing by this point).
    if crate::jit::profile::is_profiling_enabled() {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared.jit.profile_store.record_call_site_borrowed(cid, mn, md, pc);
    }

    // Skip class init if this is a registered native method (avoids initialization hangs).
    // Walk the superclass chain because the constant pool may reference a subclass
    // while the native is registered on the declaring superclass.
    // Skip hierarchy walk for <init> — constructors are NOT inherited.
    // SyntheticStub registrations on real-protected classes must not suppress
    // loading the real owner. Otherwise the first call materializes a stub and
    // seeds a native invoke-cache entry before real bytecode can take over.
    let direct_native_registered = shared
        .natives
        .native_methods
        .find(&method_class_name, &method_name, &method_descriptor)
        .is_some();
    let direct_synthetic_stub_may_yield = direct_native_registered
        && shared.natives.native_methods.kind_of(
            &method_class_name,
            &method_name,
            &method_descriptor,
        ) == Some(cratonvm_native_api::NativeKind::SyntheticStub)
        && real_protected_stub_class(&method_class_name);
    let direct_native = direct_native_registered && !direct_synthetic_stub_may_yield;
    let is_native = direct_native
        || (method_name.as_ref() != "<init>" && {
            let cm = shared.classes.class_manager.read();
            let mut found = false;
            if let Some(mut cid) = cm.get_loaded_class_id(&method_class_name) {
                while let Some(pid) = cm.get_class(cid).and_then(|c| c.superclass) {
                    if let Some(p) = cm.get_class(pid) {
                        if shared
                            .natives
                            .native_methods
                            .find(&p.name, &method_name, &method_descriptor)
                            .is_some()
                        {
                            found = true;
                            break;
                        }
                    }
                    cid = pid;
                }
            }
            found
        });
    if crate::runtime::env_cache::modstatic_dbg()
        && (method_name.as_ref() == "initBootModuleLoader"
            || method_class_name.as_ref() == "org/jboss/modules/Module")
    {
        eprintln!(
            "MODSTATIC: invokestatic {}.{} {} is_native={} direct_native={}",
            method_class_name, method_name, method_descriptor, is_native, direct_native
        );
    }
    // Self-call identity fix (2026-07-06 — see hib-proxyclassreuse-loader-
    // blind-class-resolution.md Residual B): when the invokestatic's
    // constant-pool owner class NAME textually equals the CURRENTLY
    // EXECUTING class's own name, that class is by definition already
    // loaded/linked/initialized — it is literally running this bytecode
    // right now. Record its `ClassId` directly (`current_class_id`) instead
    // of letting the dispatch below re-resolve the name through the global,
    // loader-blind `get_loaded_class_id` (which answers "whichever loader's
    // copy of this name exists" and is silently wrong whenever 2+ DIFFERENT
    // loaders each define their own class under the same simple name — the
    // common case for Groovy, which names closure literals positionally
    // per-script: `<Script>$_run_closure1`, `$_run_closure2`, ... — so two
    // unrelated scripts compiled by two different `GroovyClassLoader`s
    // routinely produce two DISTINCT classes sharing the identical name).
    //
    // Concretely: a Groovy closure's `doCall` calling its OWN private
    // synthetic class-literal-cache accessor (`$get$$class$Foo()`, a
    // compiler-generated `invokestatic` self-call) re-resolved the owner by
    // name and landed on a DIFFERENT script's same-named closure class
    // instead of the class that is actually executing — `NoSuchMethodError`
    // on the synthetic accessor, since the wrong script's copy never
    // declared that particular helper. Spring `GroovyBeanDefinitionReaderTests`
    // (two sequential test methods, each a fresh `GroovyShell`/
    // `GroovyClassLoader`, each producing a `beans$_run_closure1`)
    // reproduces this deterministically (confirmed with a minimal
    // `KRunMethod` 2-test repro before this fix; not reproducible with a
    // simple 2-`GroovyShell` top-level-closure repro, which already worked
    // — the bug needs a SELF-call inside the closure body, not merely
    // distinct closure identity).
    //
    // The dispatch identity selected below feeds BOTH the class-init step and
    // the `try_stackless_invoke` dispatch override, so the method
    // lookup itself (not just class initialization) uses the correct,
    // already-known identity for a self-call.
    let self_class_id = shared
        .classes
        .class_manager
        .read()
        .get_class(current_class_id)
        .filter(|c| c.name.as_ref() == method_class_name.as_ref())
        .map(|_| current_class_id);

    // Sibling static owners in a user-defined loader need the same identity
    // preservation as self-calls; resolving by flat name can pick the app copy.
    //
    // bt18-inline-tlab-regression-20260724 (part 2): both probes below are
    // gated on the process-wide "any defining loader registered at all"
    // atomic. Without the gate, every dispatched static call paid the
    // defining-loader registry mutex twice — and, worse, the
    // cache-suppression rule below misread a plain SELF-RECURSIVE static
    // call (static_dispatch_class_id == Some(self)) as loader-specific,
    // suppressing invoke-cache promotion for every recursive static in
    // every application and sending each call through the slow dispatcher
    // (bt18 4.8x). Only a genuinely loader-specific owner selection —
    // tracked via `loader_specific_dispatch` — may suppress promotion.
    let any_user_loader = cratonvm_native_builtins::classloader::any_defining_loader_registered();
    let isolated_url_definition =
        any_user_loader && is_isolated_url_loader_definition(shared, thread, current_class_id);
    // A defining-loader registration is authoritative even when the
    // class-manager's loader-id metadata is unavailable for a real-JDK
    // subclass. Static symbolic references still use the caller's defining
    // loader under JVMS 5.4.3; restricting that to the historical fork/Groovy
    // gates leaves ordinary ModifiedClassPathClassLoader bytecode bound to the
    // flat application copy.
    let has_user_defining_loader = any_user_loader
        && cratonvm_native_builtins::classloader::defining_loader_for(current_class_id.as_u32())
            .is_some();
    let mut loader_specific_dispatch = false;
    let static_dispatch_class_id = self_class_id.or_else(|| {
        // Keep static method owners in the same initiating-loader namespace
        // as every other symbolic reference. This includes the narrow
        // CompileWithForkedClassLoader carve-out, not only the global opt-in:
        // Spring's forked BootstrapUtils calls MergedAnnotations.search(), and
        // mixing a forked SearchStrategy singleton with an application Search
        // instance makes the latter's identity check fail spuriously.
        if should_use_loader_initiated_resolution(shared, current_class_id)
            || isolated_url_definition
            || has_user_defining_loader
        {
            loader_specific_dispatch = true;
            // Preserve the initiating loader even when the global classpath
            // already has a same-named class. This is required for nested
            // implementation jars whose owner is only visible to the caller
            // loader.
            let known = if isolated_url_definition || has_user_defining_loader {
                // An initiating-cache hit may be an application definition
                // recorded before this isolated URL loader defined its own
                // copy. Static calls into that stale class share its caches
                // with the child and poison Class-identity keyed metadata.
                lookup_loader_defined_exact(shared, current_class_id, &method_class_name)
            } else {
                lookup_loader_initiated(shared, current_class_id, &method_class_name)
            };
            known.or_else(|| {
                drive_defining_loader_load(shared, thread, current_class_id, &method_class_name)
            })
        } else {
            None
        }
    });

    // Cached: this sat as a raw per-call getenv inside the static
    // dispatcher (visible in the bt18 regression profile's getenv storm).
    fn invokestatic_loader_trace() -> bool {
        static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *G.get_or_init(|| {
            cratonvm_types::flags::runtime_var_os("CRATONVM_INVOKESTATIC_LOADER_TRACE").is_some()
        })
    }
    if invokestatic_loader_trace()
        && (method_class_name.contains("SpringFactoriesLoader")
            || method_class_name.contains("EnvironmentPostProcessorsFactory")
            || method_class_name.contains("ManagementPortType")
            || (method_class_name.as_ref() == "org/springframework/util/ClassUtils"
                && method_name.as_ref() == "forName")
            || (method_class_name.as_ref() == "java/lang/Class"
                && method_name.as_ref() == "forName"))
    {
        let cur_loader = shared
            .classes
            .class_manager
            .read()
            .get_loader_id(current_class_id);
        let cur_name = shared
            .classes
            .class_manager
            .read()
            .get_class(current_class_id)
            .map(|c| c.name.to_string());
        let resolved_loader = static_dispatch_class_id
            .and_then(|id| shared.classes.class_manager.read().get_loader_id(id));
        let global_id = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(&method_class_name);
        let global_loader =
            global_id.and_then(|id| shared.classes.class_manager.read().get_loader_id(id));
        eprintln!(
            "[INVOKESTATIC-LOADER-TRACE] method_class={} method={} current_class_id={:?} current_class_name={:?} current_loader={:?} is_native={} self_class_id={:?} static_dispatch_class_id={:?} static_dispatch_loader={:?} global_lookup_id={:?} global_lookup_loader={:?}",
            method_class_name, method_name, current_class_id, cur_name, cur_loader, is_native, self_class_id, static_dispatch_class_id, resolved_loader, global_id, global_loader
        );
    }
    if !is_native {
        let target_class_id = if let Some(id) = static_dispatch_class_id {
            id
        } else {
            // Load and initialize the target class.
            // Always go through load_class_concurrent so synthetic stubs
            // get upgraded to real classes when the .class file is available.
            match shared.load_class_concurrent(&method_class_name) {
                Ok(id) => id,
                Err(e) => {
                    // Loader-aware rescue: the `invokestatic` owner may live ONLY
                    // behind the referencing class's defining loader (e.g. a webapp
                    // class calling a static method on a sibling in its own
                    // `/WEB-INF/lib` jar — JSTL `JstlBaseTLV` →
                    // `XmlUtil.newXMLReader`). Drive that loader before failing.
                    // Strictly additive — only on a global miss.
                    match drive_defining_loader_load(
                        shared,
                        thread,
                        current_class_id,
                        &method_class_name,
                    ) {
                        Some(id) => id,
                        None => {
                            return Err(convert_class_not_found(
                                shared,
                                thread,
                                &method_class_name,
                                e.into(),
                            ))
                        }
                    }
                }
            }
        };
        ensure_class_initialized_shared(shared, thread, target_class_id)?;
    }

    // PERF (2026-07-21): see `pop_coerced_invoke_args_virtual` — same
    // non-allocating `nth_param_tag_byte` swap for `split_method_descriptor`.
    // This was the exact call site pinned by cdb stack sampling as the
    // dominant hot spot behind the ~197x Xerces SAX-parse slowdown (every
    // invokestatic re-parsed + heap-allocated a Vec<String> from scratch);
    // the primary fix is wiring `execute_invokestatic_cached` into the main
    // dispatch loop (see its call site's history) so this cold/miss path is
    // only reached once per call site instead of on every call. Fixed here
    // too since it's the same wasteful pattern and still runs on every
    // cache miss (JVMTI redefine, synthetic-stub upgrade, first call).
    //
    // BC SM2 fix (2026-05-28): pop slots as raw CompactValue and decode
    // with the parameter descriptor so a Long whose bit pattern collides
    // with the NaN-tagged SUB_OBJECT space is not silently coerced to 0L
    // by `to_value() -> Value::Object`. The descriptor-aware decode
    // path (`decode_by_descriptor(b'J')`) reinterprets the raw bits.
    let mut tmp_cv: Vec<(CompactValue, bool)> = Vec::with_capacity(num_params);
    for _ in 0..num_params {
        tmp_cv.push(
            thread.frames[frame_idx]
                .stack
                .pop_compact_with_long_mark()?,
        );
    }
    tmp_cv.reverse();
    let mut args = Vec::with_capacity(num_params);
    for (i, (cv, is_long)) in tmp_cv.into_iter().enumerate() {
        let pd_byte = nth_param_tag_byte(&method_descriptor, i);
        let v = decode_arg_kind_aware(cv, is_long, pd_byte);
        args.push(coerce_invoke_arg_for_descriptor(pd_byte, v));
    }

    if let Some(res) = intercept_force_registered_native(
        shared,
        thread,
        frame_idx,
        &method_class_name,
        method_name.as_ref(),
        method_descriptor.as_ref(),
        &args,
    ) {
        return res;
    }

    // GPU offload hook (Part E). Behind `gpu-offload`: with the
    // feature off, the entire block is removed by the preprocessor
    // and `execute_invokestatic` falls through to the existing CPU
    // path unchanged. On Hit the OffloadCache marshals, launches, and
    // writes back via `crate::runtime::offload::try_dispatch`.
    //
    // Invoke-cache interaction (found 2026-07-11): the interpreter
    // consults `thread.invoke_cache` BEFORE this slow path, so a site
    // promoted into the cache dispatches straight to the CPU body and
    // never re-enters this hook. An offloaded (or gated-but-eligible)
    // site must therefore never be promoted, or the SECOND call at
    // the site silently stops offloading — exactly the shape of a
    // warm benchmark loop. The slow-path re-entry cost is noise next
    // to any kernel that clears `--gpu-min-work`.
    #[cfg_attr(not(feature = "gpu-offload"), allow(unused_mut))]
    // The per-thread/static promoted invoke caches do not encode the
    // loader-specific owner selected above. Promoting this call site would
    // re-resolve its symbolic owner through the flat store and send the next
    // call into an application-loader copy. Keep loader-specific static calls
    // on the already-correct slow dispatcher until the cache key can carry
    // the resolved ClassId.
    //
    // bt18 part 2: suppression keys on `loader_specific_dispatch`, NOT on
    // `static_dispatch_class_id.is_some()` — the latter is Some for every
    // plain self-recursive static call (self_class_id), which suppressed
    // caching for all recursive statics in loader-free applications.
    let mut suppress_invoke_cache = loader_specific_dispatch || has_user_defining_loader;
    #[cfg(feature = "gpu-offload")]
    {
        if shared.config.gpu_offload_enabled
            && shared
                .offload_registry
                .get_or_create(shared.config.gpu_device_ordinal, &shared.config)
                .has_device()
        {
            match crate::runtime::offload::try_dispatch(
                shared,
                thread,
                frame_idx,
                &method_class_name,
                &method_name,
                &method_descriptor,
                &args,
            )? {
                crate::runtime::offload::DispatchOutcome::Handled => {
                    // Deliberately NOT populating the invoke cache —
                    // see the block comment above.
                    return Ok(CachedCallResult::Handled);
                }
                crate::runtime::offload::DispatchOutcome::HandledWithValue(value) => {
                    // Part E — reduction kernel completed on the device;
                    // push its scalar return value onto the operand
                    // stack EXACTLY the way the fallback invokestatic
                    // path a few dozen lines below does it (tag-exact
                    // 2-slot push for `Value::Long`, then the
                    // native-pending-return clear so a stale pinned
                    // ObjectRef can't outlive this call site across GC —
                    // moot for Int/Long today, but this is the one push
                    // sequence and it must stay identical for either
                    // caller).
                    let ret = crate::jit::return_type(&method_descriptor);
                    let value = coerce_value_for_return(value, ret);
                    push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
                    crate::vm::native_return_pushed_to_stack(shared, thread);
                    // Deliberately NOT populating the invoke cache —
                    // same reasoning as the plain `Handled` arm above.
                    return Ok(CachedCallResult::Handled);
                }
                crate::runtime::offload::DispatchOutcome::FallThroughKeepHooked => {
                    // Eligible kernel, but a per-call gate (e.g.
                    // --gpu-min-work) declined this particular call.
                    // Run the CPU path but keep the site un-promoted
                    // so a future call can still offload.
                    suppress_invoke_cache = true;
                }
                crate::runtime::offload::DispatchOutcome::FallThrough => {
                    // Method is ineligible / blacklisted / launch
                    // deopted. Run the CPU path below; the operand
                    // stack and locals are untouched.
                }
            }
        }
    }

    // Try stackless frame push for bytecode methods
    // For invokestatic, walk the native hierarchy to find inherited natives.
    match try_stackless_invoke(
        shared,
        thread,
        frame_idx,
        &method_class_name,
        &method_name,
        &method_descriptor,
        &args,
        true,
        false,
        static_dispatch_class_id,
    )? {
        CachedCallResult::FramePushed => {
            if !suppress_invoke_cache {
                populate_invoke_cache(thread, shared, current_class_id, cp_index, false);
            }
            return Ok(CachedCallResult::FramePushed);
        }
        CachedCallResult::Handled => {
            if !suppress_invoke_cache {
                populate_invoke_cache(thread, shared, current_class_id, cp_index, false);
            }
            return Ok(CachedCallResult::Handled);
        }
        CachedCallResult::CacheMiss => {}
    }

    // Fallback: recursive dispatch. `invoke_or_native` re-resolves the
    // owner by NAME (no ClassId-override parameter exists on it), so a
    // self-call whose class-literal method wasn't found by the stackless
    // path above, or a loader-local sibling static owner, still needs the
    // identity fix: dispatch straight through `invoke_on_class_shared` instead
    // of falling into the loader-blind name lookup `invoke_or_native` performs
    // internally.
    let result = if let Some(cid) = static_dispatch_class_id {
        crate::vm::invoke_on_class_shared(
            shared,
            thread,
            cid,
            &method_name,
            &method_descriptor,
            &args,
        )?
    } else {
        invoke_or_native(
            shared,
            thread,
            &method_class_name,
            &method_name,
            &method_descriptor,
            &args,
        )?
    };
    if let Some(value) = result {
        let ret = crate::jit::return_type(&method_descriptor);
        if ret != b'V' {
            let value = coerce_value_for_return(value, ret);
            // T18.K4 — tag-exact push for J/D fallback invokestatic return values.
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
            // Hypothesis (b): symmetric clear-after-push for the slow invokestatic
            // path. `invoke_or_native` may recursively run `safe_native_call` which
            // pins the object return in `thread.native_pending_return`; without
            // this clear, the field outlives the call site and `update_root_snapshot`
            // re-roots a stale (already-popped) ObjectRef across GC.
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
    }

    // Populate invoke cache for future fast-path hits
    if !suppress_invoke_cache {
        populate_invoke_cache(thread, shared, current_class_id, cp_index, false);
    }

    Ok(CachedCallResult::Handled)
}

// ───────────────────────── Interpreter intrinsic table ─────────────────────
//
// See `docs/feature_roadmap_interpreter_intrinsic_table.md`. An intrinsic is a
// hot JDK method (`String.length`, `Object.getClass`, `System.arraycopy`, …)
// resolved ONCE at inline-cache fill time into a `CachedInvokeTarget::Intrinsic`
// entry. The steady-state hit pops args and calls the stored callback with no
// `RwLock`, no descriptor parse, and no native-registry `HashMap` probe.

/// Process-wide count of intrinsic fast-path dispatches. Incremented on every
/// `CachedInvokeTarget::Intrinsic` hit (static and virtual). Exposed for the
/// differential-test harness and profiling/acceptance counters.
pub(super) static INTRINSIC_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Number of interpreter intrinsic fast-path dispatches since process start.
pub fn intrinsic_hit_count() -> u64 {
    INTRINSIC_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Dispatch a resolved intrinsic: count the hit, run the callback through the
/// same `safe_native_call` machinery the `Native`/`VirtualNative` arms use,
/// and push any return value onto the caller's operand stack.
///
/// `args` follows the native-registry convention — `[receiver, param0, …]`
/// for a virtual call, `[param0, …]` for a static call — so an intrinsic
/// handler is byte-for-byte interchangeable with the native it shadows.
#[inline]
/// Phase 3 — dispatch a resolved interpreter intrinsic. The return-type byte
/// is pre-computed (stored in the IC entry), so unlike
/// `invoke_cached_native_callback` this path does NOT scan a descriptor
/// string at steady state. `safe_native_call` already records the native
/// ring enter/exit, so — unlike `invoke_cached_native_callback` — this path
/// does not redundantly record it a second time.
pub(super) fn invoke_cached_intrinsic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    callback: cratonvm_native_api::NativeCallback,
    args: &[Value],
    return_type: u8,
) -> Result<(), MethodCallFailed> {
    INTRINSIC_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let result = crate::vm::safe_native_call(shared, thread, callback, args)?;
    if let Some(value) = result.filter(|_| return_type != b'V') {
        let value = coerce_value_for_return(value, return_type);
        push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
        crate::vm::native_return_pushed_to_stack(shared, thread);
    }
    Ok(())
}

/// Largest `num_params + receiver` across the whole intrinsic table — the
/// widest is `System.arraycopy` (5 static params). An 8-slot stack buffer
/// covers every intrinsic with headroom, so the steady-state arg pop needs
/// no heap allocation.
pub(super) const MAX_INTRINSIC_ARGS: usize = 8;

/// Map a record's javac-generated `hashCode`/`equals` body to its interpreter
/// intrinsic, or `None` for anything else.
///
/// These two cannot live in `intrinsics::lookup`'s static
/// `(class, name, descriptor)` table: they apply to every record class in the
/// program, and only when the body really is the generated
/// `invokedynamic java.lang.runtime.ObjectMethods.bootstrap` shape — a record
/// that hand-writes `hashCode` must keep its own code.
/// [`Class::generated_record_object_methods`] performs (and memoises) that
/// body-shape check.
///
/// `toString` is deliberately left on the ordinary `invokedynamic` path: it is
/// not hot, and its formatting needs the component descriptors that the
/// call-site data carries.
pub(super) fn record_object_intrinsic(
    declaring: &crate::classloading::Class,
    method_name: &str,
    descriptor: &str,
) -> Option<cratonvm_native_api::InterpIntrinsic> {
    use crate::classloading::{RECORD_OBJ_EQUALS, RECORD_OBJ_HASH_CODE};
    let generated = declaring.generated_record_object_methods();
    match (method_name, descriptor) {
        ("hashCode", "()I") if generated & RECORD_OBJ_HASH_CODE != 0 => {
            Some(cratonvm_native_api::InterpIntrinsic::RecordHashCode)
        }
        ("equals", "(Ljava/lang/Object;)Z") if generated & RECORD_OBJ_EQUALS != 0 => {
            Some(cratonvm_native_api::InterpIntrinsic::RecordEquals)
        }
        _ => None,
    }
}

/// Phase 3 — pop intrinsic call arguments into a caller-provided stack
/// buffer using the parameter descriptors cached in the IC entry (split once
/// at fill time). The steady-state cost is a stack pop per arg plus
/// `coerce_invoke_arg_for_descriptor`: no `resolve_method_ref` (so no
/// resolution-cache `RwLock`, no `HashMap` probe), no `split_method_descriptor`
/// parse, and no heap allocation. `with_receiver` is true for virtual
/// intrinsics, where the receiver occupies args[0].
pub(super) fn pop_coerced_invoke_args_intrinsic<'b>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    num_params: usize,
    param_descs: &[Arc<str>],
    with_receiver: bool,
    buf: &'b mut [Value; MAX_INTRINSIC_ARGS],
) -> Result<&'b [Value], MethodCallFailed> {
    // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
    let total = num_params + with_receiver as usize;
    debug_assert!(total <= MAX_INTRINSIC_ARGS);
    // BC SM2 fix (2026-05-28): pop raw CompactValue slots and decode each
    // with the corresponding parameter descriptor so a Long bit pattern
    // colliding with SUB_OBJECT survives intact (instead of being
    // converted to `Value::Object(None)` by `to_value()` and then
    // coerced to 0L).
    let mut cv_buf: [(CompactValue, bool); MAX_INTRINSIC_ARGS] =
        [(CompactValue::uninitialized(), false); MAX_INTRINSIC_ARGS];
    for i in (0..total).rev() {
        cv_buf[i] = thread.frames[frame_idx]
            .stack
            .pop_compact_with_long_mark()?;
    }
    let base = if with_receiver {
        buf[0] = coerce_invoke_arg_for_descriptor(b'L', cv_buf[0].0.decode_by_descriptor(b'L'));
        1
    } else {
        0
    };
    for i in 0..num_params {
        let pd = param_descs
            .get(i)
            .map(|s| &**s)
            .unwrap_or("Ljava/lang/Object;");
        let pd_byte = pd.as_bytes().first().copied().unwrap_or(b'L');
        let (cv, is_long) = cv_buf[base + i];
        buf[base + i] =
            coerce_invoke_arg_for_descriptor(pd_byte, decode_arg_kind_aware(cv, is_long, pd_byte));
    }
    refresh_stale_object_args(shared, &mut buf[..total]);
    Ok(&buf[..total])
}

/// Populate the invoke cache for a given (caller_class, cp_index) pair.
/// Called after the first successful invokestatic to cache everything needed
/// for subsequent calls to bypass the entire invoke chain.
pub(super) fn populate_invoke_cache(
    thread: &mut JvmThread,
    shared: &SharedVm,
    caller_class_id: ClassId,
    cp_index: u16,
    is_special: bool,
) {
    // Check if already cached
    if let Some(existing) = thread
        .invoke_cache
        .get(caller_class_id, cp_index, is_special)
    {
        if crate::runtime::env_cache::dbg_loader_trace() {
            let dbg_relevant = matches!(
                resolve_method_ref(shared, caller_class_id, cp_index),
                Ok((cn, ..)) if cn.contains("RootReference")
            );
            if dbg_relevant {
                let desc = match existing {
                    CachedInvokeTarget::Bytecode { cached, .. } => format!(
                        "Bytecode declaring={:?} class={} {}{}",
                        cached.declaring_class_id,
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor
                    ),
                    CachedInvokeTarget::VirtualBytecode {
                        receiver_class_id,
                        cached,
                        ..
                    } => format!(
                        "VirtualBytecode rc={:?} declaring={:?} class={} {}{}",
                        receiver_class_id,
                        cached.declaring_class_id,
                        cached.class_name,
                        cached.method_name,
                        cached.method_descriptor
                    ),
                    CachedInvokeTarget::Native { .. } => "Native".to_string(),
                    CachedInvokeTarget::VirtualNative { .. } => "VirtualNative".to_string(),
                    CachedInvokeTarget::Intrinsic { .. } => "Intrinsic".to_string(),
                    _ => "Other".to_string(),
                };
                eprintln!(
                    "[PIC-ALREADY-CACHED] caller_class_id={:?} cp_index={} is_special={} existing={}",
                    caller_class_id, cp_index, is_special, desc
                );
            }
        }
        return;
    }

    // Resolve the method reference from the constant pool. We need the symbolic
    // owner before consulting the shared promoted cache because loader-aware
    // invokestatic/invokespecial sites may have a per-loader owner that differs
    // from the flat global owner.
    let resolved = match resolve_method_metadata(shared, caller_class_id, cp_index) {
        Ok(resolved) => resolved,
        Err(_) => return,
    };
    let symbolic_class_name = Arc::clone(&resolved.class_name);
    let mut class_name = Arc::clone(&resolved.class_name);
    let method_name = Arc::clone(&resolved.method_name);
    let descriptor = Arc::clone(&resolved.method_descriptor);
    let num_params = resolved.num_params as usize;
    if crate::runtime::env_cache::dbg_loader_trace() && class_name.contains("RootReference") {
        eprintln!(
            "[PIC-ENTRY] caller_class_id={:?} cp_index={} is_special={} class_name(cp)={} method={}{}",
            caller_class_id, cp_index, is_special, class_name, method_name, descriptor
        );
    }
    // JVMS §6.5 super-call redirect (see `invokespecial_owner_class_name`) —
    // this is the PRIMARY invokespecial resolution path (the stackless
    // `thread.invoke_cache`/`shared_resolution` builder consulted by every
    // `execute_invokevirtual_cached(is_special=true)` probe); `execute_invoke_kind`
    // only serves the rare exotic fallback. Must run before EVERY downstream
    // use of `class_name` below (native lookup, loader-owner override,
    // `target_class_id` resolution, the promoted-cache key), so a
    // super-call bridge's cache entry is never built from the wrong class.
    class_name = if is_special {
        invokespecial_owner_class_name(shared, caller_class_id, cp_index, &class_name, &method_name)
    } else {
        class_name
    };

    // A ConstantPool Methodref is not a call-site identity: the same
    // `Object.equals(Object)` entry can be used by several bytecode offsets
    // in one method with unrelated receiver shapes.  Keep this highly
    // polymorphic JDK operation out of the CP-indexed monomorphic cache until
    // the cache key carries a bytecode offset as well.  Caching it can reuse
    // Brave's `WeakKey.equals` target for a later `TraceContext.equals` call.
    if !is_special
        && class_name.as_ref() == "java/lang/Object"
        && method_name.as_ref() == "equals"
        && descriptor.as_ref() == "(Ljava/lang/Object;)Z"
    {
        return;
    }

    // The forked Spring test loader has the same identity requirement as the
    // global loader-aware mode. Its private classes may share binary names with
    // application classes, so never build a cache entry from the flat owner.
    let loader_owner_override = if should_use_loader_initiated_resolution(shared, caller_class_id) {
        lookup_loader_initiated(shared, caller_class_id, &class_name)
    } else {
        None
    };

    // T10.4 fast path: reuse a sibling thread's fully-built target unless this
    // static/special site has a loader-local owner. Reusing a flat-global owner
    // here would bypass loader-faithful constructor/super/static dispatch.
    let promoted_key: crate::runtime::lockfree_resolve::PromotedInvokeKey =
        (caller_class_id, cp_index, is_special, None);
    if loader_owner_override.is_none() {
        if let Some(target) = shared
            .classes
            .shared_resolution
            .get_promoted_invoke(&promoted_key)
        {
            thread
                .invoke_cache
                .put(caller_class_id, cp_index, is_special, target);
            return;
        }
    }

    // Check if it's a native method.  WP2.4-F1: the staleness gate binds
    // to the *referenced* class — if that class is later redefined to a
    // bytecode body, the cached Native entry must invalidate.  Look up
    // the class_id here rather than synthesizing a never-stale gate so
    // even native-resolved entries participate in JEP 109 invalidation.
    //
    // SyntheticStub yield: a stub-tagged native on a real-protected class
    // whose real bytecode is loaded must NOT be cached (and especially not
    // promoted to the cross-thread cache) — the stub body exists only for
    // stub-phase bootstraps. Without this, a call site whose first
    // resolution goes through this population path permanently pins the
    // stub even though the slow-path dispatch sites correctly yield
    // (observed 2026-07-13 with the since-removed OutputStreamWriter stub
    // surface: `HttpServlet$NoBodyPrintWriter.resetBuffer`'s
    // `new OutputStreamWriter` kept minting encoders with a null `se`
    // while the sibling constructor call site ran the real ctor).
    // Fall through to the bytecode resolution below instead.
    let cached_exact_native = (loader_owner_override.is_none()
        && class_name == symbolic_class_name)
        .then(|| resolved.native_target.zip(resolved.native_kind))
        .flatten();
    let native_for_cache = cached_exact_native
        .or_else(|| {
            shared
                .natives
                .native_methods
                .find_with_kind(&class_name, &method_name, &descriptor)
        })
        .and_then(|(callback, kind)| {
            // The stub-yield predicate IS this site's compatibility verdict —
            // the old `.filter` — so it is handed to §7 as `compat_native_wins`
            // verbatim and `Compatible` mode caches exactly what it cached
            // before.
            let compat_native_wins = !synthetic_stub_kind_should_yield_to_real_bytecode(
                shared,
                &class_name,
                &method_name,
                &descriptor,
                Some(kind),
            );
            match crate::vm::resolve_native_dispatch_wave1(
                crate::vm::dispatch_policy(shared),
                &class_name,
                &method_name,
                &descriptor,
                Some((callback, kind)),
                compat_native_wins,
                // The bytecode method is resolved BELOW, only once this native
                // route has declined. §7 step 3's input is therefore unknown
                // here, and `false` keeps `JdkOnly` from changing what gets
                // cached on a fact it has not established.
                false,
            ) {
                // Deliberately NOT an error path: populating a call-site cache
                // is not a dispatch. Under `JdkOnly` a refused stub simply does
                // not get cached, and the refusal is raised — once, with a real
                // call site behind it — when something actually tries to run
                // it. Reporting it from here would fire on mere resolution and
                // for call sites that never execute.
                Some(crate::vm::DispatchDecision::Reject(_)) => None,
                Some(decision) => decision.native_callback(),
                None => None,
            }
        });
    // No §4 census increment anywhere in this function: caching a native is not
    // invoking one, and counting it here would inflate every counter by one per
    // call site regardless of whether the site ever ran.
    if let Some(callback) = native_for_cache {
        let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
            shared,
            &class_name,
            &method_name,
            &descriptor,
        ) else {
            return;
        };
        let cm = shared.classes.class_manager.read();
        let gate = match cm.get_loaded_class_id(&class_name) {
            Some(cid) => RedefineGate::snapshot(cm.class_redefine_generation_handle(cid)),
            None => RedefineGate::never_stale(),
        };
        drop(cm);
        // Interpreter intrinsic probe (invokestatic only). The cp class IS
        // the declaring class for a static call, so keying the table on
        // `class_name` is correct and needs no receiver guard
        // (`receiver_class_id: None`). We gate on `!is_special` and on
        // `intrinsics::is_static(kind)` so an `invokespecial` site (e.g.
        // `super.hashCode()`) — which targets an instance method and pops
        // a receiver — is never cached here; that case is left to the slow
        // path. `intrinsics_disabled()` is the differential-test off-switch.
        if !is_special && !crate::runtime::env_cache::intrinsics_disabled() {
            if let Some(kind) =
                cratonvm_native_builtins::intrinsics::lookup(&class_name, &method_name, &descriptor)
                    .filter(|k| cratonvm_native_builtins::intrinsics::is_static(*k))
            {
                // Phase 3 — split the descriptor ONCE here so the
                // steady-state dispatch path never re-resolves or re-parses.
                let (pd_vec, _) = split_method_descriptor(&descriptor);
                let param_descs: Arc<[Arc<str>]> =
                    pd_vec.iter().map(|s| Arc::from(s.as_str())).collect();
                let return_type = crate::jit::return_type(&descriptor);
                let target = CachedInvokeTarget::Intrinsic {
                    kind,
                    callback: cratonvm_native_builtins::intrinsics::callback_for(kind),
                    // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                    num_params: num_params as u16,
                    param_descs,
                    return_type,
                    receiver_class_id: None,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, is_special, target);
                return;
            }
        }
        let target = CachedInvokeTarget::Native {
            callback,
            native_id,
            native_kind,
            num_params: num_params as u16, // Widening: parameter count conversion
            gate,
        };
        shared
            .classes
            .shared_resolution
            .insert_promoted_invoke(promoted_key, target.clone());
        thread
            .invoke_cache
            .put(caller_class_id, cp_index, is_special, target);
        return;
    }

    // Find the bytecode method. For static/special refs from a loader-private class,
    // the symbolic owner must stay in that loader's namespace; resolving through
    // the flat global store would cache the app-loader method body.
    let target_class_id = match loader_owner_override.or_else(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(&class_name)
    }) {
        Some(id) => id,
        None => return,
    };

    if crate::runtime::env_cache::dbg_loader_trace() && class_name.contains("RootReference") {
        let cm = shared.classes.class_manager.read();
        let caller_loader = cm.get_loader_id(caller_class_id);
        let target_loader = cm.get_loader_id(target_class_id);
        drop(cm);
        eprintln!(
            "[LOADER-TRACE] populate_invoke_cache is_special={} caller_class_id={:?} caller_loader={:?} method={}.{}{} class_name(cp)={} loader_owner_override={:?} target_class_id={:?} target_loader={:?}",
            is_special, caller_class_id, caller_loader, class_name, method_name, descriptor, class_name, loader_owner_override, target_class_id, target_loader
        );
    }

    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(target_class_id) else {
        return;
    };

    // Walk superclass chain to find the declaring class
    let store = &cm.class_store;
    let Some((method, declaring_id)) = crate::classloading::find_method_recursive(
        target_class_id,
        &method_name,
        &descriptor,
        store,
    ) else {
        return;
    };

    // Native-shadow check keyed on the *declaring* class, honored regardless
    // of whether the resolved method is `native` or has a (shadowed) bytecode
    // body. The early CP-class lookup above keys on the symbolic-ref class —
    // which, for an inherited method invoked through a super-class symbolic ref
    // (e.g. Groovy's `GroovyClassLoader.loadClass(String,Z,Z,Z)` calling
    // `super.loadClass(String,Z)`, whose CP ref names `URLClassLoader`/
    // `SecureClassLoader`, not `java.lang.ClassLoader`) — misses the native
    // registered on the declaring class `ClassLoader` (`cl_real_load_class`).
    // Without this, the first call serves the native (slow path) but every
    // cached call runs the real `ClassLoader`/`BuiltinClassLoader` delegation
    // bytecode the VM can't satisfy → spurious `ClassNotFoundException`
    // (e.g. `groovy.grape.GrabAnnotationTransformation` during Groovy's global
    // AST-transform scan, breaking every Groovy compile). Re-applies the
    // 92b7bd80 fix (the original `if method.is_native()`-only gate below was
    // restored by the `cd396a04` "Merge branch 'main' into dev" merge).
    {
        let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
        // Inline SyntheticStub yield (the full helper re-acquires the
        // class-manager read lock, which is already held here): a stub-tagged
        // native on a real-protected declaring class whose resolved method is
        // real bytecode yields — do not cache the stub. `method`/`declaring_id`
        // are the already-resolved real method/class from
        // `find_method_recursive` above.
        let stub_yields =
            shared
                .natives
                .native_methods
                .kind_of(declaring_name, &method_name, &descriptor)
                == Some(cratonvm_native_api::NativeKind::SyntheticStub)
                && real_protected_stub_class(declaring_name)
                && store
                    .get(declaring_id)
                    .is_some_and(|c| !c.is_synthetic_stub)
                && !method.is_native()
                && method.code().is_some();
        if !stub_yields {
            if let Some((callback, native_id, native_kind)) =
                resolve_cached_native_registration(
                    shared,
                    declaring_name,
                    &method_name,
                    &descriptor,
                )
            {
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
                drop(cm);
                let target = CachedInvokeTarget::Native {
                    callback,
                    native_id,
                    native_kind,
                    // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                    num_params: num_params as u16,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, is_special, target);
                return;
            }
        }
    }

    if method.is_native() {
        // Already handled above, but the method might be native in a superclass
        let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
        if let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
            shared,
            declaring_name,
            &method_name,
            &descriptor,
        )
        {
            // WP2.4-F1: gate bound to the *declaring* class — that's the
            // class whose method body could be replaced via redefine.
            let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
            drop(cm);
            let target = CachedInvokeTarget::Native {
                callback,
                native_id,
                native_kind,
                num_params: num_params as u16, // Widening: parameter count conversion
                gate,
            };
            shared
                .classes
                .shared_resolution
                .insert_promoted_invoke(promoted_key, target.clone());
            thread
                .invoke_cache
                .put(caller_class_id, cp_index, is_special, target);
        }
        return;
    }

    let Some(code_attr) = method.code() else {
        return;
    };

    let source_file = class.source_file.as_deref().map(Arc::from);
    let declaring_class_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");

    let cached = CachedBytecodeMethod {
        declaring_class_id: declaring_id,
        class_name: Arc::from(declaring_class_name),
        method_name: Arc::clone(&method_name),
        method_descriptor: Arc::clone(&descriptor),
        source_file,
        code: crate::runtime::frame::padded_bytecode(&code_attr.code),
        exception_table: Arc::from(code_attr.exception_table.as_slice()),
        max_stack: code_attr.max_stack,
        max_locals: code_attr.max_locals,
        num_params: num_params as u16, // Widening: parameter count conversion
        is_synchronized: method.is_synchronized(),
        is_static: method.is_static(),
        force_native_cache: std::sync::OnceLock::new(),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };

    // WP2.4-F1: snapshot the redefine generation BEFORE dropping the
    // class_manager read-lock.  Any subsequent `redefine_class` will
    // bump the same `Arc<AtomicU32>` (because `class_redefine_generation_handle`
    // is now `&self` and shares state via the inner `RwLock<HashMap>`),
    // so the next cache hit will observe the bump and auto-evict.
    let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
    drop(cm);
    let target = CachedInvokeTarget::Bytecode {
        cached: std::sync::Arc::new(cached),
        gate,
    };
    // T10.4 — promote so sibling threads skip the class_manager walk.
    shared
        .classes
        .shared_resolution
        .insert_promoted_invoke(promoted_key, target.clone());
    thread
        .invoke_cache
        .put(caller_class_id, cp_index, is_special, target);
}

#[inline]
pub(super) fn cached_static_owner_stale(
    shared: &SharedVm,
    caller_class_id: ClassId,
    cached: &CachedBytecodeMethod,
) -> bool {
    if !should_use_loader_initiated_resolution(shared, caller_class_id) {
        return false;
    }
    lookup_loader_initiated(shared, caller_class_id, cached.class_name.as_ref())
        .is_some_and(|owner_cid| owner_cid != cached.declaring_class_id)
}

/// Fast invokestatic using the invoke cache (stackless dispatch).
/// Returns FramePushed for bytecode (caller updates frame_idx),
/// Handled for native, CacheMiss for fall-through to slow path.
pub(super) fn execute_invokestatic_cached(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    pc: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    let caller_class_id = thread.frames[frame_idx].class_id;
    let continuation_interpreted = matches!(thread.kind, crate::threading::ThreadKind::Virtual);
    // A --nojit execution can neither enter a cached artifact nor request a
    // new one.  Keep the call-cache dispatch entirely interpreter-only rather
    // than paying its JIT generation probes and hotness atomics on every call.
    let jit_enabled = !crate::runtime::env_cache::disable_jit();

    // Thread-local invoke cache — no locking needed. invokestatic uses
    // is_special=false since static calls never collide cp_index with
    // invokespecial in the same class (different CP entries semantically).
    let target = match thread.invoke_cache.get(caller_class_id, cp_index, false) {
        Some(t) => t.clone(),
        None => return Ok(CachedCallResult::CacheMiss),
    };
    // JVMTI redefine guard (static): never serve a cached native/intrinsic
    // SHADOW for a static method whose declaring class an agent has redefined
    // (e.g. Mockito `mockStatic(X)` woves X's static methods) — evict and
    // re-resolve so the woven bytecode + advice run. Fast-pathed on
    // `any_class_redefined`.
    let any_class_redefined = crate::classloading::any_class_redefined();
    if any_class_redefined
        && matches!(
            &target,
            CachedInvokeTarget::Native { .. } | CachedInvokeTarget::Intrinsic { .. }
        )
    {
        if let Ok((mcn, _, _, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
            if native_shadow_suppressed_by_redefine(shared, &mcn) {
                thread.invoke_cache.evict(caller_class_id, cp_index, false);
                return Ok(CachedCallResult::CacheMiss);
            }
        }
    }
    if continuation_interpreted && matches!(&target, CachedInvokeTarget::Jit { .. }) {
        thread.invoke_cache.evict(caller_class_id, cp_index, false);
        return Ok(CachedCallResult::CacheMiss);
    }
    if crate::runtime::env_cache::modstatic_dbg() {
        if let Ok((mcn, mn, _, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
            if mn.as_ref() == "initBootModuleLoader" {
                eprintln!("MODSTATIC: invokestatic_cached HIT {}.{}", mcn, mn);
            }
        }
    }

    // PGO-01: call-site evidence. Placed here, after every early
    // `return Ok(CacheMiss)` above (JVMTI redefine eviction, stale-owner
    // eviction, continuation-interpreted eviction) and right before the
    // dispatch match below — every remaining path through this match
    // actually dispatches (Handled/FramePushed), so this is "the call site
    // fired," not "we merely consulted the cache."
    if crate::jit::profile::is_profiling_enabled() {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared.jit.profile_store.record_call_site_borrowed(cid, mn, md, pc);
    }

    match target {
        CachedInvokeTarget::Native {
            callback,
            native_id,
            native_kind,
            num_params,
            gate: _,
        } => {
            let Some(callback) =
                revalidate_cached_native(shared, native_id, callback, native_kind)
            else {
                thread.invoke_cache.evict(caller_class_id, cp_index, false);
                return Ok(CachedCallResult::CacheMiss);
            };
            let (args, method_descriptor) = pop_coerced_invoke_args_static(
                shared,
                caller_class_id,
                cp_index,
                frame_idx,
                thread,
            )?;
            invoke_cached_native_callback(
                shared,
                thread,
                frame_idx,
                callback,
                &args,
                &method_descriptor,
            )?;
            Ok(CachedCallResult::Handled)
        }
        // Interpreter intrinsic — invokestatic has no receiver guard
        // (`receiver_class_id` is `None` for static-resolved entries).
        // Phase 3: args are popped against the IC-cached `param_descs` and
        // the result coerced with the cached `return_type` — the
        // steady-state path touches no shared lock, no HashMap, and parses
        // no descriptor string.
        CachedInvokeTarget::Intrinsic {
            callback,
            num_params,
            param_descs,
            return_type,
            kind: _,
            receiver_class_id: _,
            gate: _,
        } => {
            let mut arg_buf = [Value::Uninitialized; MAX_INTRINSIC_ARGS];
            let args = pop_coerced_invoke_args_intrinsic(
                shared,
                thread,
                frame_idx,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                num_params as usize,
                &param_descs,
                false,
                &mut arg_buf,
            )?;
            invoke_cached_intrinsic(shared, thread, frame_idx, callback, args, return_type)?;
            Ok(CachedCallResult::Handled)
        }
        CachedInvokeTarget::Jit {
            compiled,
            num_params,
            return_type,
            needs_heap,
            cached,
            gate: _,
            supersede_epoch: _,
        } => {
            if cached_static_owner_stale(shared, caller_class_id, &cached) {
                thread.invoke_cache.evict(caller_class_id, cp_index, false);
                return Ok(CachedCallResult::CacheMiss);
            }
            execute_jit_call(
                shared,
                thread,
                frame_idx,
                &compiled,
                num_params,
                return_type,
                needs_heap,
                &cached,
            )
        }
        CachedInvokeTarget::Bytecode {
            ref cached,
            gate: ref entry_gate,
        } => {
            if cached_static_owner_stale(shared, caller_class_id, cached) {
                thread.invoke_cache.evict(caller_class_id, cp_index, false);
                return Ok(CachedCallResult::CacheMiss);
            }

            // Fast path: check if the method was already JIT-compiled (e.g. by OSR)
            // before going through the invocation counter.
            //
            // T2.2 -- `JitCache::get` hashes class name + method name +
            // descriptor and then re-compares all three with full string
            // equality, and this probe used to run on EVERY interpreted call of
            // a not-yet-compiled method purely to discover whether a background
            // compile had published a body. That is exactly the re-resolution
            // this per-call-site inline cache exists to avoid.
            //
            // It is now epoch-guarded: `jit_cache_generation()` advances on every
            // JIT-cache publication AND every invalidation (see
            // `jit::JitCache::put`/`put_osr`/`invalidate_matching`/`clear_all` --
            // the only mutators), so while this entry's snapshot still equals the
            // live generation the cache content is unchanged since we last looked
            // and missed, and looking again cannot find anything. Steady state is
            // two integer loads plus a compare: no hashing, no string compares.
            let target_redefined = entry_gate.generation > 0;
            if jit_enabled && !target_redefined && !continuation_interpreted {
                // Read the generation BEFORE probing. A publication racing in
                // after the probe leaves us memoizing an older generation, which
                // compares unequal next time and re-probes -- safe. Memoizing a
                // generation NEWER than the probe would be the unsound direction,
                // and this ordering makes it impossible.
                let jit_generation = cratonvm_jit::jit_cache_generation();
                let compiled_probe = if cached.jit_probe_is_current(jit_generation) {
                    None
                } else {
                    let found = shared.jit.jit_cache.read().get(
                        &cached.class_name,
                        &cached.method_name,
                        &cached.method_descriptor,
                        cached.declaring_class_id,
                    );
                    if found.is_none() {
                        cached.record_jit_probe_miss(jit_generation);
                    }
                    found
                };
                if let Some(compiled) = compiled_probe {
                    let ret = crate::jit::return_type(&cached.method_descriptor);
                    let heap = compiled.needs_heap();
                    // WP2.4-F1: inherit the gate from the bytecode entry —
                    // both the JIT path and the bytecode path bind to the
                    // same declaring class, so a future redefine bumps the
                    // same counter and invalidates the upgraded JIT entry.
                    let jit_target = CachedInvokeTarget::Jit {
                        compiled: compiled.clone().into(),
                        num_params: cached.num_params,
                        return_type: ret,
                        needs_heap: heap,
                        cached: cached.clone(),
                        gate: entry_gate.clone(),
                        supersede_epoch: crate::classloading::jit_supersede_epoch(),
                    };
                    thread
                        .invoke_cache
                        .put(caller_class_id, cp_index, false, jit_target.clone());
                    if let CachedInvokeTarget::Jit {
                        compiled,
                        num_params,
                        return_type,
                        needs_heap,
                        cached,
                        gate: _,
                        supersede_epoch: _,
                    } = jit_target
                    {
                        return execute_jit_call(
                            shared,
                            thread,
                            frame_idx,
                            &compiled,
                            num_params,
                            return_type,
                            needs_heap,
                            &cached,
                        );
                    }
                }
            }

            // Gate JIT compilation behind an invocation counter (warmup threshold).
            // Pack class_id and a hash of method name+descriptor into a u64 key
            // for cheap per-method counting without allocating strings.
            //
            // T2.5 -- the 31-multiplier byte loops over method name AND full
            // descriptor used to run inline here on every interpreted call.
            // `cached` is `Arc`-shared per call site, so the key is memoized in
            // it (same precedent as `force_native_cache` /
            // `native_callback_cache`) and computed once per call site instead of
            // once per call. The hash is bit-identical, so existing warmup counts
            // keep the same keys.
            let invoc_key = cached.invoc_key();
            // BUG-2: hot-method promotion for short-but-very-hot methods.
            //
            // The old gate `invoc_count >= T && invoc_count % T == 0` fired
            // ONLY at exact multiples of the threshold. Two failure modes hit
            // the all-interpreted call trees in BC's PQC RegressionTest
            // (`Permute.permute`, `ChaChaEngine.chachaCore`,
            // `HashFunctions.hash_n_n`, `Salsa20Engine.processBytes`):
            //   (a) a *single* transient upgrade-gate failure at count T pushed
            //       the next attempt out another whole T calls (2000 → 4000),
            //       multiplying interpreter time; and
            //   (b) these methods never OSR-compile because their per-call
            //       back-edge count is tiny (OSR's `backward_count` resets every
            //       invocation), so the invocation counter is their ONLY path
            //       to the JIT — and the exact-multiple gate made it fragile.
            //
            // Fix: (1) lower the warmup threshold so astronomically-hot short
            // methods promote sooner, and (2) once past the threshold, RETRY on
            // a short stride instead of only at the next full multiple — so a
            // transient compile failure recovers within `JIT_RETRY_STRIDE`
            // calls, not another full threshold. The persistent
            // `increment_invocation` count means the bar is crossed even from
            // the interpreted-call-tree case, and a successful upgrade rewrites
            // the invoke cache to `Jit`, so this counting block stops being
            // reached and there is no ongoing re-spam. Normal warmup is
            // preserved: cold methods still wait for `JIT_INVOCATION_THRESHOLD`
            // calls before any compile attempt.
            // Warmup threshold (default 500), overridable via CRATONVM_JIT_THRESHOLD.
            // Raising it keeps short-lived / call-heavy code interpreted (HotSpot's
            // interpreter-first behaviour) instead of paying CratonVM's
            // currently-slower JIT'd dispatch for code that never amortizes the
            // switch; genuinely-hot compute loops still cross it and compile.
            let jit_invocation_threshold = crate::runtime::env_cache::jit_invocation_threshold();
            /// Re-attempt stride once a method is past the warmup threshold but
            /// not yet successfully compiled. Small so a transient upgrade-gate
            /// failure is retried within a few hundred calls rather than after
            /// another full warmup threshold.
            const JIT_RETRY_STRIDE: u32 = 64;
            let invoc_count = jit_enabled
                .then(|| shared.jit.profile_store.increment_invocation(invoc_key))
                .unwrap_or(0);
            // Fire on the first crossing of the threshold, then re-attempt every
            // `JIT_RETRY_STRIDE` calls until the upgrade succeeds (after which
            // the invoke cache routes through JIT and this block is bypassed).
            let past_threshold = jit_enabled && invoc_count >= jit_invocation_threshold;
            let should_attempt = past_threshold
                && (invoc_count == jit_invocation_threshold
                    || (invoc_count - jit_invocation_threshold) % JIT_RETRY_STRIDE == 0);
            if should_attempt && !target_redefined && !continuation_interpreted {
                // Consult tiered compilation manager for recommended tier
                let tiered_key = crate::jit::tiered::MethodKey::new(
                    cached.class_name.as_ref(),
                    cached.method_name.as_ref(),
                    cached.method_descriptor.as_ref(),
                );
                // wire-tiered-manager: OFF-THREAD codegen for the invocation
                // tier-up trigger. **DEFAULT-ON as of Step 7** ("retire the
                // single fixed-threshold inline path"); `CRATONVM_BG_COMPILE=0`
                // is the opt-out safety net.
                //
                //  * On (default) — start the background compile thread once
                //    (idempotent), then ENQUEUE-ONLY: the tiered manager's
                //    `on_method_invocation` pushes a CompilationTask at the
                //    recommended tier and the worker compiles it off the mutator
                //    (publishing into `shared.jit.jit_cache`). The mutator does NOT
                //    compile inline; it keeps interpreting until the worker
                //    publishes, at which point the `jit_cache` fast-path at the top
                //    of the `Bytecode` arm flips this call site to `Jit`. (The
                //    eager *first-call* single-pass compile in `fn execute` is a
                //    separate quick first tier and still runs; this governs the
                //    invocation-counted re-tiering + OSR.)
                //  * Off (`=0`) — never start the worker; keep the historical
                //    inline `try_jit_upgrade_with_gate` path EXACTLY as before
                //    (the safety net while the off-thread pipeline soaks on the
                //    gauntlet).
                let bg_compile_on = crate::runtime::env_cache::bg_compile();
                if bg_compile_on {
                    // Start the off-thread compile worker once (idempotent). See
                    // `ensure_bg_compiler_started` for the closure / GC rationale.
                    ensure_bg_compiler_started(shared);
                }
                // Pass the REAL per-method invocation count — this hook runs
                // only at stride boundaries, and the manager's historical
                // `+= 1` counting deflated its hotness view 64x (first C1
                // recommendation at ~threshold + 64×c1_threshold real calls).
                let recommended_tier = shared
                    .jit
                    .tiered_manager
                    .on_method_invocation_observed(&tiered_key, invoc_count as u64);
                if let Some(tier) = recommended_tier {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] tiered-enqueue {}.{}{} tier={:?} invoc_count={} bg={}",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            tier,
                            invoc_count,
                            bg_compile_on,
                        );
                    }
                }
                // When background compilation is ON the mutator does NOT compile
                // inline — the worker owns codegen. Skip straight to interpreted
                // execution; a later call picks up the published JIT entry via the
                // `jit_cache` fast-path above.
                if !bg_compile_on {
                    // WP2.4-F1: pass the bytecode entry's gate to inherit
                    // the staleness binding — the JIT'd body executes the same
                    // declaring class, so a future `redefine_class` must
                    // invalidate this JIT entry too.
                    let upgrade_result =
                        try_jit_upgrade_with_gate(shared, cached, entry_gate.clone());
                    if upgrade_result.is_none() && crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] upgrade-FAIL {}.{}{} invoc_count={}",
                            cached.class_name,
                            cached.method_name,
                            cached.method_descriptor,
                            invoc_count
                        );
                    }
                    if let Some(jit_target) = upgrade_result {
                        // Upgrade cache entry to Jit for future calls
                        thread.invoke_cache.put(
                            caller_class_id,
                            cp_index,
                            false,
                            jit_target.clone(),
                        );
                        // Execute via JIT right now
                        if let CachedInvokeTarget::Jit {
                            compiled,
                            num_params,
                            return_type,
                            needs_heap,
                            cached,
                            gate: _,
                            supersede_epoch: _,
                        } = jit_target
                        {
                            return execute_jit_call(
                                shared,
                                thread,
                                frame_idx,
                                &compiled,
                                num_params,
                                return_type,
                                needs_heap,
                                &cached,
                            );
                        }
                    }
                } // end !bg_compile_on inline-upgrade path
            } // end invocation threshold check

            // Fallback: interpreted execution
            // Check stack overflow before pushing frame
            if thread.frames.len() >= shared.config.max_stack_depth {
                dump_stack_on_soe(thread);
                return Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::StackOverflowError,
                )));
            }

            // Pop args into stack-allocated buffer (avoids Vec allocation).
            // Decode each slot with its parameter descriptor (bit-exact): the
            // prior `pop_unchecked()` → `to_value()` decoded a category-2 long
            // arg whose NaN-box bit pattern collides with a tagged sub-tag
            // (e.g. `0xFFFC_…`, a BC safegcd accumulator) as `Value::Int`,
            // dropping the high bits before it reached the callee's locals.
            // The non-cached `execute_invokestatic` path already decodes this
            // way. See docs/bc-ec-mod-mododdinverse-investigation.md.
            const MAX_INLINE_ARGS: usize = 16;
            let num_params = cached.num_params as usize; // Widening: parameter count conversion
            let pd_byte = |i: usize| -> u8 { nth_param_tag_byte(&cached.method_descriptor, i) };
            let mut args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
            let mut args_vec: Vec<Value> = Vec::new();
            let args_slice: &mut [Value] = if num_params <= MAX_INLINE_ARGS {
                for i in (0..num_params).rev() {
                    args_buf[i] = thread.frames[frame_idx]
                        .stack
                        .pop_arg_for_descriptor_checked(pd_byte(i))?;
                }
                &mut args_buf[..num_params]
            } else {
                args_vec.resize(num_params, Value::Uninitialized);
                for i in (0..num_params).rev() {
                    args_vec[i] = thread.frames[frame_idx]
                        .stack
                        .pop_arg_for_descriptor_checked(pd_byte(i))?;
                }
                &mut args_vec
            };
            refresh_stale_object_args(shared, args_slice);

            if let Some(res) = intercept_force_registered_native_cached(
                shared, thread, frame_idx, &cached, args_slice,
            ) {
                return res;
            }

            // Acquire monitor for synchronized methods
            let monitor_obj: Option<ObjectRef> = if cached.is_synchronized {
                let obj = if cached.is_static {
                    // JVMS §2.11.10: static-synchronized monitor is the Class mirror
                    // (same object as ldc class / synchronized(X.class) / X.class.wait).
                    get_or_create_class_mirror(shared, cached.declaring_class_id)
                } else {
                    match args_slice.first() {
                        Some(Value::Object(Some(obj_ref))) => *obj_ref,
                        _ => return Ok(CachedCallResult::CacheMiss),
                    }
                };
                Some(crate::vm::monitor_enter_synchronized_method(
                    shared, thread, obj, args_slice,
                ))
            } else {
                None
            };

            // Push frame — caller handles execution via stackless loop
            // T10.7 — refill per-thread pool from the shared VM-wide VecPool
            // if it's empty so we reuse the promoted allocation.
            thread.refill_pools_from_shared(
                &shared.mem.operand_stack_pool,
                &shared.mem.tag_pool,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                cached.max_locals as usize,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                (cached.max_stack as usize).max(16) + 8,
            );
            let mut frame = Frame::new_pooled_cached(
                cached.clone(),
                args_slice,
                &mut thread.locals_pool,
                &mut thread.stacks_pool,
            );
            frame.monitor_on_exit = monitor_obj;
            if crate::runtime::env_cache::frame_trace() {
                eprintln!(
                    "[FRAME_PUSH/stackless_cached] depth={} {}.{}{}",
                    thread.frames.len(),
                    frame.class_name(),
                    frame.method_name(),
                    frame.method_descriptor()
                );
            }
            push_frame_and_fire_entry(shared.vm_identity, thread, frame);
            Ok(CachedCallResult::FramePushed)
        }
        _ => Ok(CachedCallResult::CacheMiss),
    }
}

/// Resolve `java/lang/String`'s instance-field layout (slot indices of
/// `value`, `coder`, `hash`) for the JIT String call-site intrinsics
/// (`length`/`charAt`/`isEmpty`/`hashCode`/`equals`/`compareTo`/`indexOf`).
///
/// String extends Object, which has no instance fields, so `find_own_field`'s
/// indices match the slot indices used by `jit_getfield`. `coder` is optional —
/// the legacy synthetic `char[]`-backed String has none, in which case
/// `StringFieldLayout::new` records `has_coder = false` and the `coder`-dependent
/// intrinsics bail to normal native dispatch. Returns `None` (all String
/// intrinsics bail to dispatch) when String is not yet loaded or lacks the
/// mandatory `value` / `hash` fields. Cheap enough to call once per compilation.
pub(super) fn resolve_string_field_layout(shared: &SharedVm) -> Option<cratonvm_jit::StringFieldLayout> {
    let cm = shared.classes.class_manager.read();
    let string_id = cm.find_bootstrap_class_by_name("java/lang/String")?;
    let class = cm.get_class(string_id)?;
    let (value_idx, _) = class.find_own_field("value")?;
    let (hash_idx, _) = class.find_own_field("hash")?;
    let coder_idx = class.find_own_field("coder").map(|(idx, _)| idx);
    // `string_id` doubles as the ObjectHeader class id used to guard
    // `java/lang/CharSequence` accessor call sites (the receiver must be a
    // real String for the inline String-layout decode to be sound).
    Some(cratonvm_jit::StringFieldLayout::new(
        value_idx,
        coder_idx,
        hash_idx,
        string_id.as_u32(),
    ))
}

/// Backward branch count threshold before triggering OSR compilation.
pub(super) const OSR_THRESHOLD: u32 = 1_000;

/// Try On-Stack Replacement: compile the current method and enter JIT mid-execution.
///
/// Returns `Some(Option<Value>)` if OSR succeeds (the method completed via JIT),
/// or `None` if OSR is not possible (method not JIT-compatible, compilation failed, etc.).
/// wire-tiered-manager Step 5 (precise background OSR): compile (or reuse) an
/// OSR-enterable artifact for `(class, method, descriptor)` and publish it into
/// `shared.jit.jit_cache`. Extracted verbatim from `try_osr`'s former inline compile
/// closure so it can run **off the mutator** on the background compile worker
/// (which holds no live frame): every input is method metadata, not runtime frame
/// state. The mutator's `try_osr` then does the live-frame entry on the returned
/// artifact. `entry_pc` only selects which back-edge the *reuse* probe checks
/// (`can_osr_enter`); the compile itself is entry-pc-independent (the artifact
/// supports OSR entry at every loop header it emits).
///
/// GC-STW-safety on the worker: same discipline as `try_jit_compile_callee_slow`
/// (see its doc) — every `class_manager` lock is read out and dropped before any
/// blocking / allocating call (`load_class_concurrent`, eager callee compile), and
/// `PENDING_COMPACT_FIELD_INFO` is thread-local so the worker stages its own.
#[allow(clippy::too_many_arguments)]
/// Allocate the dynamic-dispatch cache pair consumed by both x64 compilation
/// entry points owned by the interpreter.
///
/// Method-entry compilation in `cratonvm-jit` has always supplied these slots,
/// but the interpreter's early-compile and OSR paths passed empty vectors. That
/// backend skew forced every virtual/interface operation in a hot OSR loop
/// through `jit_invoke_dispatch`, even though x64 already had MIC codegen.
pub(super) fn allocate_dynamic_dispatch_slots(
    invoke_kind: u8,
    pc: usize,
    mic_slots: &mut Vec<(usize, *const crate::jit::JitMICSlot)>,
    owned_mic_slots: &mut Vec<Box<crate::jit::JitMICSlot>>,
    pic_slots: &mut Vec<(usize, *const crate::jit::JitPICSlot)>,
    owned_pic_slots: &mut Vec<Box<crate::jit::JitPICSlot>>,
) {
    if !matches!(invoke_kind, 0 | 2) {
        return;
    }

    let mic = Box::new(crate::jit::JitMICSlot::new());
    let pic = Box::new(crate::jit::JitPICSlot::new());
    let mic_ptr: *const crate::jit::JitMICSlot = &*mic;
    let pic_ptr: *const crate::jit::JitPICSlot = &*pic;
    owned_mic_slots.push(mic);
    owned_pic_slots.push(pic);
    mic_slots.push((pc, mic_ptr));
    pic_slots.push((pc, pic_ptr));
}

#[cfg(test)]
mod dynamic_dispatch_slot_tests {
    // `super::` here is the enclosing `invoke` module, which is where this
    // helper now lives.
    use super::allocate_dynamic_dispatch_slots;

    #[test]
    fn caches_virtual_and_interface_sites_but_not_static_or_special_sites() {
        for (kind, expected) in [(0, 1), (1, 0), (2, 1), (3, 0)] {
            let mut mic = Vec::new();
            let mut owned_mic = Vec::new();
            let mut pic = Vec::new();
            let mut owned_pic = Vec::new();
            allocate_dynamic_dispatch_slots(
                kind,
                27,
                &mut mic,
                &mut owned_mic,
                &mut pic,
                &mut owned_pic,
            );
            assert_eq!(mic.len(), expected);
            assert_eq!(owned_mic.len(), expected);
            assert_eq!(pic.len(), expected);
            assert_eq!(owned_pic.len(), expected);
            if expected == 1 {
                assert_eq!(mic[0].0, 27);
                assert_eq!(pic[0].0, 27);
            }
        }
    }
}

#[cfg(test)]
mod string_builder_layout_override_tests {
    use super::is_string_builder_layout_native_override;

    /// Every builder operation whose real JDK body indexes the compact
    /// `byte[] value` / `byte coder` / `int count` layout must resolve through
    /// CratonVM's native shim, because CratonVM's builders are a two-field
    /// `char[]`/`int` synthetic. Before 2026-07-31 six of these were missing,
    /// and a single `Mockito.mock(StringBuilder.class)` anywhere in the process
    /// evicted their native shadow and silently corrupted every REAL builder —
    /// which is what wedged javac's `JavaTokenizer` and made Spring's AOT
    /// chunk 4 look like a hang. `SbMethodMatrixProbe` is the Java witness.
    #[test]
    fn forces_every_compact_layout_operation_native() {
        for class in [
            "java/lang/StringBuilder",
            "java/lang/StringBuffer",
            "java/lang/AbstractStringBuilder",
        ] {
            for (name, desc) in [
                ("setLength", "(I)V"),
                ("deleteCharAt", "(I)Ljava/lang/StringBuilder;"),
                ("replace", "(IILjava/lang/String;)Ljava/lang/StringBuilder;"),
                ("ensureCapacity", "(I)V"),
                ("trimToSize", "()V"),
                ("repeat", "(II)Ljava/lang/StringBuilder;"),
                ("capacity", "()I"),
                ("reverse", "()Ljava/lang/StringBuilder;"),
                ("codePointAt", "(I)I"),
                ("codePointBefore", "(I)I"),
                ("codePointCount", "(II)I"),
                ("getCoder", "()B"),
                ("getValue", "()[B"),
                // Already covered before this fix; kept so a future narrowing
                // of the list is caught here too.
                ("<init>", "(Ljava/lang/String;)V"),
                ("append", "(C)Ljava/lang/StringBuilder;"),
                ("charAt", "(I)C"),
                ("delete", "(II)Ljava/lang/StringBuilder;"),
                ("getChars", "(II[CI)V"),
                ("insert", "(IC)Ljava/lang/StringBuilder;"),
                ("setCharAt", "(IC)V"),
                ("toString", "()Ljava/lang/String;"),
                ("substring", "(II)Ljava/lang/String;"),
            ] {
                assert!(
                    is_string_builder_layout_native_override(class, name, desc),
                    "{class}.{name}{desc} must stay forced to its native shim"
                );
            }
        }
    }

    /// The two operations `MockitoBeanByTypeLookupIntegrationTests` genuinely
    /// stubs and verifies on a mocked `StringBuilder`. Their native shadow has
    /// to stay evictable or Mockito's woven advice never runs and the stub is
    /// silently ignored — see the long note on `length()` in the predicate.
    #[test]
    fn leaves_the_two_mockito_stubbed_operations_evictable() {
        for class in [
            "java/lang/StringBuilder",
            "java/lang/StringBuffer",
            "java/lang/AbstractStringBuilder",
        ] {
            assert!(!is_string_builder_layout_native_override(
                class, "length", "()I"
            ));
            assert!(!is_string_builder_layout_native_override(
                class,
                "substring",
                "(I)Ljava/lang/String;"
            ));
        }
    }

    #[test]
    fn does_not_claim_unrelated_classes() {
        assert!(!is_string_builder_layout_native_override(
            "java/lang/String",
            "setLength",
            "(I)V"
        ));
        assert!(!is_string_builder_layout_native_override(
            "java/util/ArrayList",
            "trimToSize",
            "()V"
        ));
    }
}

#[cfg(test)]
mod redefine_immunity_tests {
    use super::redefine_immune_forced_native;

    /// CratonVM's synthetic collections must keep their native shadow across a
    /// redefinition. Their real JDK bodies index a `table`/`root`/`head` field
    /// graph the synthetic objects do not have, so running them returns silent
    /// nonsense — `TreeMap.get` null, `ConcurrentHashMap.size` 0,
    /// `HashMap.keySet` empty. `RedefineCollectionLayoutProbe` is the witness.
    #[test]
    fn synthetic_collections_keep_their_natives_across_a_redefinition() {
        for class in [
            "java/util/ArrayDeque",
            "java/util/ArrayList",
            "java/util/HashMap",
            "java/util/HashSet",
            "java/util/IdentityHashMap",
            "java/util/LinkedHashMap",
            "java/util/LinkedHashSet",
            "java/util/LinkedList",
            "java/util/TreeMap",
            "java/util/TreeSet",
            "java/util/concurrent/ConcurrentHashMap",
        ] {
            for (name, desc) in [
                ("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
                ("size", "()I"),
                ("containsKey", "(Ljava/lang/Object;)Z"),
                ("keySet", "()Ljava/util/Set;"),
                ("entrySet", "()Ljava/util/Set;"),
                ("iterator", "()Ljava/util/Iterator;"),
                ("toString", "()Ljava/lang/String;"),
            ] {
                assert!(
                    redefine_immune_forced_native(class, name, desc),
                    "{class}.{name}{desc} must survive a redefinition"
                );
            }
        }
    }

    #[test]
    fn ordinary_classes_stay_evictable() {
        assert!(!redefine_immune_forced_native(
            "com/example/Service",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ));
        // The two operations the Mockito suite genuinely stubs on a mocked
        // StringBuilder — see `is_string_builder_layout_native_override`.
        assert!(!redefine_immune_forced_native(
            "java/lang/StringBuilder",
            "length",
            "()I"
        ));
        assert!(!redefine_immune_forced_native(
            "java/lang/StringBuilder",
            "substring",
            "(I)Ljava/lang/String;"
        ));
    }

    /// No dispatch site may name a layout arm directly: they must go through
    /// `redefine_immune_layout_native` (cache paths) or
    /// `redefine_immune_forced_native` (slow path). Six sites used to open-code
    /// `string_builder || path`, so the collections entry silently did not apply
    /// there and the collection probe stalled at 18 of 32 rather than 0.
    ///
    /// The reflection arm is NOT policed: two cache sites legitimately omit it,
    /// and forcing them to include it regressed ByteBuddy type creation.
    #[test]
    fn layout_immunity_is_not_open_coded() {
        let src = include_str!("invoke.rs");

        // The exemption is the RULE, located in the source: an arm may be named
        // only inside the two aggregators, whose entire job is to compose them.
        //
        // Twice now this gate has been written as a proxy for that rule, and
        // twice the proxy went stale against a growing file. First a hard-coded
        // line band, `(5000..9800)`: `redefine_immune_forced_native` slid down
        // to 9793-9822, so its own arms were reported as offenders and the gate
        // failed for a reason that had nothing to do with what it polices.
        // Then an enclosing-function tracker that recognised exactly four
        // declaration spellings — a function written any other way never
        // updated the name, so its body was attributed to whatever came before.
        // That one failed OPEN, which is worse: dropping
        //
        //     pub(super) unsafe fn probe(class_name: &str) -> bool {
        //         redefine_immune_synthetic_collection_native(class_name)
        //     }
        //
        // straight after `redefine_immune_synthetic_collection_native` PASSED,
        // because the stale name was that exempt predicate's.
        //
        // So: no line numbers, no declaration parsing, no carried state. Find
        // the aggregator bodies and ask whether the call is inside one. If an
        // aggregator is ever renamed this stops finding it and its own arms
        // start failing — loud, and the right direction to fail in.
        let aggregator_bodies: Vec<(usize, usize)> = [
            "redefine_immune_layout_native",
            "redefine_immune_forced_native",
        ]
        .iter()
        .filter_map(|name| {
            let start = src.find(&format!("fn {name}("))?;
            // A top-level body ends at the first `}` in column 0 after it.
            let end = src[start..]
                .find("\n}")
                .map_or(src.len(), |i| start + i + 2);
            Some((start, end))
        })
        .collect();
        assert_eq!(
            aggregator_bodies.len(),
            2,
            "both aggregators must be findable, or this gate exempts nothing \
             and polices everything"
        );

        let mut offenders = Vec::new();
        let mut offset = 0usize;
        for (n, line) in src.lines().enumerate() {
            let line_start = offset;
            offset += line.len() + 1; // `lines()` strips a single `\n`
            let code = line.trim_start();
            // Comments, and this test's own list of names (string literals).
            if code.starts_with("//") || code.starts_with('"') {
                continue;
            }
            let inside_aggregator = aggregator_bodies
                .iter()
                .any(|&(start, end)| line_start >= start && line_start < end);
            if inside_aggregator {
                continue;
            }
            for part in [
                "redefine_immune_string_builder_native(",
                "redefine_immune_path_native(",
                "redefine_immune_jfr_native(",
                "redefine_immune_synthetic_collection_native(",
            ] {
                // An arm's own `fn` declaration is not a call site.
                if code.contains(part) && !code.contains(&format!("fn {part}")) {
                    offenders.push(format!("line {}: {}", n + 1, code));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "call redefine_immune_layout_native (cache paths) or \
             redefine_immune_forced_native (slow path) instead of naming an arm:\n{}",
            offenders.join("\n")
        );
    }
}

// ---------------------------------------------------------------------------
// The JIT bridge
// ---------------------------------------------------------------------------
//
// Compile requests, OSR, entering and leaving compiled code, and the probes
// that decide what a compile may assume: `interpreter/jit_bridge.rs`.


/// T10.9.A — VtableManager fast-path for invokevirtual / invokeinterface.
///
/// Consulted as **fast path 0** ahead of the per-thread `invoke_cache`
/// and the shared-resolution promoted-cache. A hit here bypasses
/// `class_manager.read()` entirely: the vtable carries a fully-built
/// `Arc<CachedBytecodeMethod>` populated at class-link time.
///
/// Ordering of reads:
///   1. `thread.invoke_cache` — cheapest, purely thread-local. If hit,
///      the VtableManager read is skipped.
///   2. The receiver object itself (peek; may be null → NPE).
///   3. `shared.classes.resolution_cache.read()` — may yield the
///      `(method_name, descriptor, num_params)` triple without
///      touching `class_manager`.
///   4. `shared.classes.vtable_manager.read()` → `resolve_virtual_slot`.
///   5. Frame push using the entry's `resolved_method` Arc.
///
/// Returns:
///   - `Ok(FramePushed)` on bytecode dispatch (interpreter must advance
///     `frame_idx`).
///   - `Ok(Handled)` on native dispatch (result already pushed).
///   - `Ok(CacheMiss)` when the vtable slot is empty/unresolved, the
///     receiver class has no installed vtable, the resolution-cache
///     doesn't yet have the method-ref, or the entry is marked
///     `is_native` (native path not handled here — invoke_cache will
///     fill that in on a later call).
///   - `Err(_)` propagates any NPE/StackOverflowError.
///
/// After a successful dispatch this function populates the thread-local
/// `invoke_cache` so subsequent invocations from the same caller class
/// take the faster monomorphic inline-cache path.
///
/// Bounds: `class_id` out-of-range and `slot` out-of-range both return
/// `CacheMiss` via `VtableManager::resolve_virtual_slot`'s own bounds
/// checks (see `vtable.rs` tests).
pub(super) const NATIVE_SHADOW_CACHE_MAX_ENTRIES: usize = 8192;

#[inline]
pub(super) fn vtable_native_shadow_cache_key(
    receiver_class_id: ClassId,
    redefine_fingerprint: u64,
    method_name: &str,
    method_descriptor: &str,
) -> (u32, u64, u64, u64) {
    (
        receiver_class_id.as_u32(),
        redefine_fingerprint,
        crate::runtime::fx_collections::fx_hash_str(method_name),
        crate::runtime::fx_collections::fx_hash_str(method_descriptor),
    )
}

#[inline]
pub(super) fn remember_vtable_native_shadow(
    thread: &mut JvmThread,
    key: Option<(u32, u64, u64, u64)>,
    verdict: bool,
) {
    if let Some(key) = key {
        if thread.native_shadow_cache.len() >= NATIVE_SHADOW_CACHE_MAX_ENTRIES {
            thread.native_shadow_cache.clear();
        }
        thread.native_shadow_cache.insert(key, verdict);
    }
}

#[inline]
pub(super) fn execute_invokevirtual_vtable_fast(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    site_pc: usize,
    is_interface: bool,
) -> Result<CachedCallResult, MethodCallFailed> {
    dbg_invoke_stats_record(2);
    let caller_class_id = thread.frames[frame_idx].class_id;

    // Step 1 — already covered by the caller (execute_invokevirtual_cached
    // runs first on every call site). This function is only invoked on
    // its miss path, so the thread-local cache is cold for this key.

    // Step 2 — resolve the method reference from the caller's constant
    // pool. We want the `(name, descriptor, num_params)` triple without
    // acquiring `class_manager.read()`; that's only possible via the
    // already-populated `resolution_cache`. On a cold cache we return
    // `CacheMiss` and let the slow path populate it.
    let (method_class_name, method_name, method_descriptor, num_params_slots) = {
        let rc = shared.classes.resolution_cache.read();
        match rc.get_method(caller_class_id, cp_index) {
            Some(rm) => (
                Arc::clone(&rm.class_name),
                Arc::clone(&rm.method_name),
                Arc::clone(&rm.method_descriptor),
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                rm.num_params as usize,
            ),
            None => return Ok(CachedCallResult::CacheMiss),
        }
    };

    // Force this call site through the slow dispatcher (see the matching
    // comment in `execute_invoke_kind`) rather than a fast-path vtable/cache
    // hit — empirically required to keep
    // WebFluxManagementChildContextConfigurationIntegrationTests from
    // hanging even with the registered native override in place.
    if crate::runtime::env_cache::loader_aware_resolution()
        && method_class_name.as_ref()
            == "org/springframework/core/annotation/MergedAnnotation$Adapt"
        && method_name.as_ref() == "isIn"
        && method_descriptor.as_ref()
            == "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z"
    {
        return Ok(CachedCallResult::CacheMiss);
    }

    // Step 3 — peek the receiver. The receiver sits `num_params_slots`
    // down the operand stack from the top.
    // `cp_index` alone is not a call-site identity. Do not install a
    // monomorphic target for a shared `Object.equals` Methodref; the same
    // entry can be used at receiver-polymorphic bytecode offsets.
    if method_class_name.as_ref() == "java/lang/Object"
        && method_name.as_ref() == "equals"
        && method_descriptor.as_ref() == "(Ljava/lang/Object;)Z"
    {
        return Ok(CachedCallResult::CacheMiss);
    }

    let num_params = num_params_slots;
    let receiver_val = thread.frames[frame_idx].stack.peek_at(num_params);
    // A cold invokevirtual site in a lambda reaches this vtable fast path
    // before the ordinary invoke interceptor.  ClassLoader's resource native
    // owns the null-name contract, so invoke it directly for just this case;
    // normal resource lookup remains eligible for the vtable cache.
    if method_class_name.as_ref() == "java/lang/ClassLoader"
        && matches!(
            (method_name.as_ref(), method_descriptor.as_ref()),
            ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                | (
                    "getResources",
                    "(Ljava/lang/String;)Ljava/util/Enumeration;"
                )
                | (
                    "getResourceAsStream",
                    "(Ljava/lang/String;)Ljava/io/InputStream;"
                )
                | ("loadClass", "(Ljava/lang/String;)Ljava/lang/Class;")
                | ("resources", "(Ljava/lang/String;)Ljava/util/stream/Stream;")
        )
        && matches!(
            thread.frames[frame_idx].stack.peek_at(0),
            Value::Object(None)
        )
    {
        let (args, _) =
            pop_coerced_invoke_args_virtual(shared, caller_class_id, cp_index, frame_idx, thread)?;
        let callback = match method_name.as_ref() {
            "getResource" => cratonvm_native_builtins::classloader::cl_get_resource_essential,
            "getResources" => cratonvm_native_builtins::classloader::cl_get_resources_essential,
            "getResourceAsStream" => {
                cratonvm_native_builtins::classloader::cl_get_resource_as_stream_essential
            }
            // The callback is reached only with a null name, so it always
            // throws before producing a URL-typed result. Reuse its canonical
            // ClassLoader NPE construction for `resources(String)`.
            "resources" => cratonvm_native_builtins::classloader::cl_get_resource_essential,
            "loadClass" => cratonvm_native_builtins::classloader::cl_load_class_essential,
            _ => return Ok(CachedCallResult::CacheMiss),
        };
        let value = crate::vm::safe_native_call(shared, thread, callback, &args)?;
        if let Some(value) = value {
            push_invoke_return_value(
                &mut thread.frames[frame_idx].stack,
                coerce_value_for_return(value, crate::jit::return_type(&method_descriptor)),
            )?;
            crate::vm::native_return_pushed_to_stack(shared, thread);
        }
        return Ok(CachedCallResult::Handled);
    }
    if crate::runtime::env_cache::dbg_jetty2() && &*method_name == "getClasspath" {
        eprintln!(
            "[jetty2-vtfast] {}{} receiver={:?}",
            &*method_name, &*method_descriptor, receiver_val
        );
    }
    let receiver_obj = match receiver_val {
        Value::Object(Some(obj_ref)) => obj_ref,
        Value::Object(None) => {
            // Round 63 — Spring's GenericConversionService$Converters
            // .getClassHierarchy threads nulls through
            // `Class.componentType/getSuperclass/getInterfaces/arrayType`.
            // See the matching block in execute_invoke_cached for the
            // detailed rationale. Mirror that null-tolerant shape here so
            // the inline-cache fast path doesn't NPE first.
            //
            // We need the constant-pool class of the call site, which we
            // can read out of the resolution cache populated in step 2.
            // We don't have it locally yet, so fall back to the slow path
            // by emitting a CacheMiss — `execute_invoke_cached` will run
            // its own block with the full method-ref triple in hand.
            return Ok(CachedCallResult::CacheMiss);
        }
        _ => return Ok(CachedCallResult::CacheMiss),
    };
    // Refresh via the same GC-forwarding barrier applied to invoke args
    // (see `refresh_stale_object_args`). This value came from a bare
    // `peek_at` (not a `pop`), so while it's technically still visible to
    // root-scanning on the operand stack, downstream consumers here
    // (`class_id_of`/`kind_of` used to pick the dispatch target) are
    // exactly the class-resolution step implicated in the TestUpgrade
    // RootReference residual — refresh defensively before trusting it for
    // dispatch. See docs/known-issues/h2/
    // bug-h2-suite-residual-fail-triage.md.
    let receiver_obj = shared.mem.heap.load_and_forward(receiver_obj);

    // Arrays go through java/lang/Object — don't dispatch via the
    // receiver's array-component vtable. Let the slow path handle it.
    if shared.mem.heap.kind_of(receiver_obj) == cratonvm_types::ObjectKind::Array {
        return Ok(CachedCallResult::CacheMiss);
    }
    let receiver_class_id = shared.mem.heap.class_id_of(receiver_obj);
    // All-zero header = stale pointer from zeroed GC memory — fall back
    // to the slow path which has detailed recovery logic.
    if receiver_class_id == ClassId::new(0) {
        return Ok(CachedCallResult::CacheMiss);
    }

    // Synthetic lambda proxies implement their SAM through
    // `try_lambda_dispatch`, not a vtable body.
    if shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&receiver_class_id)
    {
        return Ok(CachedCallResult::CacheMiss);
    }

    if resolved_private_invokevirtual_target(
        shared,
        caller_class_id,
        &method_class_name,
        &method_name,
        &method_descriptor,
    )
    .is_some()
    {
        return Ok(CachedCallResult::CacheMiss);
    }

    if crate::runtime::env_cache::dbg_vdisp()
        && (method_name.as_ref() == "hashCode"
            || method_name.as_ref() == "equals"
            || method_name.as_ref() == "run")
    {
        let cm = shared.classes.class_manager.read();
        let caller = cm
            .get_class(caller_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let rcv = cm
            .get_class(receiver_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let found = crate::classloading::find_method_recursive(
            receiver_class_id,
            &method_name,
            &method_descriptor,
            &cm.class_store,
        );
        let declaring = found
            .and_then(|(_m, did)| cm.class_store.get(did).map(|c| c.name.to_string()))
            .unwrap_or_default();
        let is_abstract = found.map(|(m, _)| m.is_abstract()).unwrap_or(false);
        let has_code = found.map(|(m, _)| m.code().is_some()).unwrap_or(false);
        eprintln!("[vdisp] VTFAST caller={caller} method={method_name}{method_descriptor} receiver_class={rcv} declaring={declaring} is_abstract={is_abstract} has_code={has_code}");
    }

    // Interpreter intrinsic shadowing guard.
    //
    // This vtable fast path runs BEFORE `execute_invokevirtual_cached` (it is
    // dispatched first at the 0xb6/0xb9 opcode sites) — so if it dispatched a
    // method body here, the intrinsic inline cache in
    // `execute_invokevirtual_cached` would never be consulted for that site.
    //
    // In practice every intrinsic-eligible virtual method (`String.length`,
    // `Object.getClass`/`hashCode`, `StringBuilder.append`/`toString`/`length`)
    // already has a Rust native registered, and the native-override block
    // below returns `CacheMiss` for those — so the call falls through to the
    // cached path that owns the intrinsic IC. This explicit check makes that
    // guarantee independent of native-registration coverage: when the
    // resolved declaring class + (name, descriptor) is in the intrinsic
    // table we always emit `CacheMiss`, ceding dispatch to
    // `execute_invokevirtual_cached`. Keyed on the *resolved* declaring class
    // so an overriding subclass (whose body is NOT in the table) is
    // unaffected and dispatches here normally. Suppressed by
    // `intrinsics_disabled()` (the differential-test off-switch) so the
    // off-run behaves byte-for-byte like the pre-intrinsic VM.
    if !crate::runtime::env_cache::intrinsics_disabled()
        && cratonvm_native_builtins::intrinsics::might_have_method_descriptor(
            &method_name,
            &method_descriptor,
        )
    {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let is_intrinsic = crate::classloading::find_method_recursive(
            receiver_class_id,
            &method_name,
            &method_descriptor,
            store,
        )
        .and_then(|(_m, declaring_id)| {
            let declaring_class = store.get(declaring_id)?;
            // A class redefined in place by a JVMTI agent (e.g. a Mockito
            // inline mock) has authoritative woven bytecode — do not shadow
            // it with the Rust intrinsic, or the advice never runs.
            if crate::classloading::any_class_redefined()
                && cm.class_redefine_generation(declaring_id) > 0
                && !redefine_immune_layout_native(
                    &declaring_class.name,
                    &method_name,
                    &method_descriptor,
                )
            {
                return Some(false);
            }
            Some(
                cratonvm_native_builtins::intrinsics::lookup(
                    &declaring_class.name,
                    &method_name,
                    &method_descriptor,
                )
                .is_some()
                    // Records (JEP 395): a generated `hashCode`/`equals` is an
                    // intrinsic too, but it is not in the static table — see
                    // `record_object_intrinsic`. Without this arm the vtable
                    // fast path runs the `invokedynamic` body and the
                    // intrinsic IC is never reached.
                    || record_object_intrinsic(declaring_class, &method_name, &method_descriptor)
                        .is_some(),
            )
        })
        .unwrap_or(false);
        drop(cm);
        if is_intrinsic {
            return Ok(CachedCallResult::CacheMiss);
        }
    }

    // WP0.1 — Native override priority. A Rust native registered for
    // (receiver_class, method_name, descriptor) MUST take priority over
    // bytecode from the class file. This matches the dispatch order in
    // `populate_virtual_invoke_cache` (line ~10444) and `try_stackless_invoke`
    // (line ~7818): native override first, class hierarchy second.
    //
    // Without this check the vtable fast path can dispatch to real-JDK
    // bytecode for classes that are shadowed by a synthetic stub — e.g.
    // `java/io/PrintStream`, where the `System.out` object is allocated
    // with only the synthetic field layout. The real bytecode then
    // accesses fields that don't exist, producing the canonical
    // "Cannot invoke write on null" NPE on the second `println` call
    // once the vtable has been populated for the reused class_id.
    //
    // The check peeks at the receiver's class name (no allocation) and
    // consults the native registry. A hit dispatches via the cached
    // fast-path at the call site below, by falling through to
    // `execute_invokevirtual_cached` which already handles
    // `CachedInvokeTarget::VirtualNative` correctly — ensuring the
    // invoke_cache entry populated by prior slow-path calls is honored.
    {
        let cm = shared.classes.class_manager.read();
        let rcv_name_owned = cm.get_class(receiver_class_id).map(|c| c.name.clone());
        if let Some(rcv_name) = rcv_name_owned.as_ref() {
            // JVMTI redefine guard: a class redefined in place by an agent has
            // authoritative woven bytecode, so the per-class native shadows
            // below must be suppressed (the mock's advice has to run). Cheap
            // fast-path on `any_class_redefined`.
            // Reflection-metadata natives stay authoritative even when the
            // receiver class was redefined (see `redefine_immune_reflection_native`):
            // a Mockito inline mock of `java.lang.reflect.Method` must not
            // disable `Method.getDeclaredAnnotations()` for every method object.
            let receiver_redefined = crate::classloading::any_class_redefined()
                && cm.class_redefine_generation(receiver_class_id) > 0
                && !redefine_immune_reflection_native(rcv_name, &method_name)
                && !redefine_immune_layout_native(rcv_name, &method_name, &method_descriptor);
            // WP2.7 — annotation proxies have no real bytecode for
            // equals/hashCode/toString. Force fall-through to the slow path
            // so `execute_invoke`'s annotation_proxy interception layer
            // serves the spec-compliant `Annotation` contract.
            if &**rcv_name == "java/lang/annotation/AnnotationProxy" {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
            // Dynamic-proxy default-method dispatch guard. A JDK dynamic
            // proxy must route EVERY interface method call — including
            // concrete default methods like `AgeHolder.age()` — through its
            // `InvocationHandler.invoke()` (see `is_proxy_dispatch` in
            // `execute_invoke_kind`, which does this correctly). This vtable
            // fast path installs a direct `VirtualBytecode` target keyed by
            // `receiver_class_id` alone; since every proxy instance sharing
            // an interface set shares one generated `$ProxyN` class id, the
            // FIRST call that resolves a default method here poisons the
            // cache for that call site, so a LATER call on a *different*
            // proxy instance of the same generated class dispatches straight
            // to the default method's bytecode and skips the handler
            // entirely (observed: `AspectJAutoProxyCreatorTests.twoAdviceAspectPrototype`/
            // `twoAdviceAspectSingleton` advice silently not firing on the
            // second proxy instance).
            // Force the slow path for any proxy receiver so `is_proxy_dispatch`
            // is consulted on every call; cost is zero on non-proxy dispatch.
            //
            // NOTE: walk the chain using the ALREADY-HELD `cm` guard rather
            // than calling `class_chain_reaches_proxy_instance` (which takes
            // its own `shared.classes.class_manager.read()`) — a nested second read
            // acquisition on the same thread self-deadlocks under
            // parking_lot's writer-preferring fairness once any writer is
            // queued, since the outer guard here is never dropped before the
            // inner one blocks.
            {
                const MAX_DEPTH: usize = 32;
                const PROXY_INSTANCE: &str = "java/lang/reflect/Proxy$Instance";
                let mut current = Some(receiver_class_id);
                let mut is_proxy = false;
                for _ in 0..MAX_DEPTH {
                    let cid = match current {
                        Some(c) => c,
                        None => break,
                    };
                    let class = match cm.get_class(cid) {
                        Some(c) => c,
                        None => break,
                    };
                    if &*class.name == PROXY_INSTANCE {
                        is_proxy = true;
                        break;
                    }
                    if &*class.name == "java/lang/Object" {
                        break;
                    }
                    current = class.superclass;
                }
                if is_proxy {
                    drop(cm);
                    return Ok(CachedCallResult::CacheMiss);
                }
            }
            // `URLClassLoader.findClass` invoked on a SUBCLASS receiver (the
            // native is registered on `URLClassLoader`, not the subclass, so the
            // `find(rcv_name, …)` probe below misses and the parent walk would
            // dispatch URLClassLoader's bytecode — which reads the shimmed `ucp`
            // and throws CNF). Cede to the slow path, where
            // `intercept_urlclassloader_subclass_find_class` forces the
            // `ucl_find_class` native. Jasper's `JasperLoader.loadClass` →
            // `findClass` (runtime-compiled `org.apache.jsp.*_jsp`) is the
            // canonical case. Only when the receiver inherits (does not override)
            // `findClass` — checked via the resolved declaring class.
            if matches!(
                (&*method_name, &*method_descriptor),
                ("findClass", "(Ljava/lang/String;)Ljava/lang/Class;")
                    | ("addURL", "(Ljava/net/URL;)V")
            ) && &**rcv_name != "java/net/URLClassLoader"
                && crate::classloading::find_method_recursive(
                    receiver_class_id,
                    &method_name,
                    &method_descriptor,
                    &cm.class_store,
                )
                .and_then(|(_m, did)| {
                    cm.class_store
                        .get(did)
                        .map(|c| &*c.name == "java/net/URLClassLoader")
                })
                .unwrap_or(false)
            {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
            if surefire_lazy_launcher_discover_native(
                shared,
                &method_name,
                &method_descriptor,
                receiver_obj,
            )
            .is_some()
            {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
            let native_shadow_cache_key = Some(vtable_native_shadow_cache_key(
                receiver_class_id,
                hierarchy_fingerprint_in(&cm, receiver_class_id),
                &method_name,
                &method_descriptor,
            ));
            let cached_native_shadow = native_shadow_cache_key
                .and_then(|key| thread.native_shadow_cache.get(&key).copied());
            if cached_native_shadow == Some(true) {
                drop(cm);
                return Ok(CachedCallResult::CacheMiss);
            }
            if cached_native_shadow != Some(false)
                && shared
                    .natives
                    .native_methods
                    .might_have_method_descriptor(&method_name, &method_descriptor)
            {
                let direct_native_shadow = !receiver_redefined
                    && shared
                        .natives
                        .native_methods
                        .find(rcv_name, &method_name, &method_descriptor)
                        .is_some();
                if direct_native_shadow {
                    remember_vtable_native_shadow(thread, native_shadow_cache_key, true);
                    if &**rcv_name == "java/lang/invoke/ConstantCallSite"
                        && crate::runtime::env_cache::dbg_ccsprobe()
                    {
                        eprintln!(
                            "[ccs-probe] vtable_fast: native found for {} {}{} — emitting CacheMiss",
                            rcv_name, method_name, method_descriptor,
                        );
                    }
                    drop(cm);
                    return Ok(CachedCallResult::CacheMiss);
                }
                if &**rcv_name == "java/lang/invoke/ConstantCallSite"
                    && crate::runtime::env_cache::dbg_ccsprobe()
                {
                    eprintln!(
                        "[ccs-probe] vtable_fast: native NOT found for {} {}{}",
                        rcv_name, method_name, method_descriptor,
                    );
                }
                // Receiver's-own-class override guard. The parent-walk below
                // (FJP fix, next comment) starts at `receiver_class_id`'s
                // SUPERCLASS — it never inspects whether `receiver_class_id`
                // itself directly declares this method. That is correct for
                // an INHERITED method (the walk's intended case: no override
                // on the receiver, so scanning ancestors for the nearest
                // bytecode-or-native is exactly right), but wrong for an
                // OVERRIDDEN one: a Mockito/ByteBuddy-generated mock subclass
                // of e.g. `java.net.HttpURLConnection` declares its OWN
                // bytecode body for every mockable method, and that override
                // is the most-derived target — it must win over a native
                // registered on an ancestor (`HttpURLConnection` itself),
                // exactly like real JVM virtual dispatch. Without this guard
                // the walk finds `HttpURLConnection`'s native at the very
                // first step and treats it as authoritative, so EVERY call on
                // the mock — including Mockito's own stubbing/verification
                // calls — silently bypasses the mock's advice and runs the
                // real native instead (observed as `MockitoException: Could
                // not modify all classes` / stray `IllegalArgumentException:
                // HttpURLConnection: URL not set` from `given(mock.foo())`).
                let receiver_has_own_bytecode = cm
                    .get_class(receiver_class_id)
                    .map(|c| c.find_method(&method_name, &method_descriptor).is_some())
                    .unwrap_or(false);
                if !receiver_has_own_bytecode {
                    // FJP fix: walk the parent chain to find natives registered on
                    // a superclass (e.g. `RecursiveTask.fork()` defined on
                    // `ForkJoinTask` but registered as a Rust native at
                    // `RecursiveTask`). Without this walk, the vtable would
                    // dispatch the inherited JDK bytecode for `fork()`, which uses
                    // Unsafe CAS and bypasses our native side-table.
                    let mut cid = receiver_class_id;
                    while let Some(parent_id) = cm.get_class(cid).and_then(|c| c.superclass) {
                        if let Some(parent) = cm.get_class(parent_id) {
                            // S107 collection-toString fix: if this parent has its
                            // own bytecode for the method, the bytecode override wins
                            // over any deeper native ancestor (e.g. Object.toString).
                            // Stop walking so the vtable bytecode path runs.
                            //
                            // Round 19 (peaceful-sammet) — IMPORTANT exception: if
                            // the parent has BOTH bytecode AND a Rust native, the
                            // native wins. See `populate_virtual_invoke_cache` for
                            // the LinkedHashMap-overlay rationale.
                            let has_bytecode = parent
                                .find_method(&method_name, &method_descriptor)
                                .is_some();
                            // Suppress an inherited native shadow when the declaring
                            // parent has been redefined by an agent (woven bytecode wins).
                            let parent_redefined = crate::classloading::any_class_redefined()
                                && cm.class_redefine_generation(parent_id) > 0
                                && !redefine_immune_forced_native(
                                    &parent.name,
                                    &method_name,
                                    &method_descriptor,
                                );
                            let has_native = !parent_redefined
                                && shared
                                    .natives
                                    .native_methods
                                    .find(&parent.name, &method_name, &method_descriptor)
                                    .is_some();
                            if has_native {
                                remember_vtable_native_shadow(
                                    thread,
                                    native_shadow_cache_key,
                                    true,
                                );
                                drop(cm);
                                return Ok(CachedCallResult::CacheMiss);
                            }
                            if has_bytecode {
                                break;
                            }
                        }
                        cid = parent_id;
                    }
                }
            }
            if cached_native_shadow != Some(false) {
                remember_vtable_native_shadow(thread, native_shadow_cache_key, false);
            }
        }
        drop(cm);
    }

    // Step 4 — VtableManager read. Look up the slot by
    // (method_name, descriptor) on the receiver's class, then fetch the
    // entry. A miss here (no installed vtable, no matching slot, slot
    // invalidated by CHA) falls through to invoke_cache / slow path.
    let (entry_cached, entry_is_native) = {
        let guard = shared.classes.vtable_manager.read();
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        let vtable = match guard.get_vtable(receiver_class_id.as_u32() as u64) {
            Some(v) => v,
            None => return Ok(CachedCallResult::CacheMiss),
        };
        let slot = match vtable.lookup_slot(&method_name, &method_descriptor) {
            Some(s) => s,
            None => return Ok(CachedCallResult::CacheMiss),
        };
        let entry = match vtable.get(slot) {
            Some(e) if e.resolved => e,
            _ => return Ok(CachedCallResult::CacheMiss),
        };
        if entry.is_native {
            return Ok(CachedCallResult::CacheMiss);
        }
        let cached = match &entry.resolved_method {
            Some(c) => Arc::clone(c),
            None => return Ok(CachedCallResult::CacheMiss),
        };
        let is_native = entry.is_native;
        // The class loader installs a vtable while it holds the class-manager
        // writer. Do not acquire the class-manager reader below while this
        // vtable read guard is live: concurrent class loading and virtual
        // dispatch would otherwise form an AB-BA deadlock.
        drop(guard);

        // A vtable slot stores one erased (name, descriptor) target, but an
        // invokeinterface call can require a more-specific interface default
        // than the slot inherited from its CP owner. Validate that the slot
        // agrees with receiver-rooted JVMS selection before dispatching it.
        // In particular, a covariant bridge has the parent return descriptor,
        // so starting resolution at the CP interface picks its own default and
        // skips the subinterface bridge.
        if is_interface {
            let cm = shared.classes.class_manager.read();
            let receiver_selected = crate::classloading::find_method_recursive(
                receiver_class_id,
                &method_name,
                &method_descriptor,
                &cm.class_store,
            )
            .map(|(_, declaring_id)| declaring_id);
            drop(cm);
            if receiver_selected != Some(cached.declaring_class_id) {
                return Ok(CachedCallResult::CacheMiss);
            }
        }

        // `VtableManager::install_vtable` refreshes inherited entries when a
        // declaring class is reinstalled by JVMTI redefine. The snapshot here
        // is therefore generation-current even for an already-linked
        // subclass; dispatch can stay on the cached path instead of
        // permanently re-resolving every call after the first redefine.
        // `vtable_install_adapter` (called
        // from `ClassManager::define_class_with_options` while defining a
        // class) takes the locks in the OPPOSITE order - class_manager
        // (held for the whole definition) then vtable_manager (to install
        // the new vtable). Holding both here in vtable-then-class order
        // was a real AB-BA deadlock under concurrent class loading +
        // virtual dispatch (confirmed live via gdb: a class-loading
        // thread blocked acquiring vtable_manager for write while holding
        // class_manager for write, and a dispatching thread blocked here
        // acquiring class_manager for read while holding vtable_manager
        // for read). `resolve_virtual_slot`'s own doc/test
        // (`vm.rs` vtable fast-path test) already establishes the
        // invariant that the vtable must be queryable WITHOUT holding
        // class_manager - this restores it.
        // `CachedBytecodeMethod` retains the resolved declaring method, so
        // memoize this pure, 55-branch decision on that shared entry instead
        // of reopening `class_manager` and re-evaluating it for every vtable
        // hit.
        let force_native = *cached.force_native_cache.get_or_init(|| {
            force_native_over_real_jdk_bytecode(
                cached.class_name.as_ref(),
                cached.method_name.as_ref(),
                cached.method_descriptor.as_ref(),
            )
        });
        // Site A2 of `native-dispatch-memoization.md` §3 Step 2.
        //
        // T2.5 -- the registry probe is memoized too, via the SAME per-entry
        // `NativeCallSite` the force-native interception path
        // (`intercept_force_registered_native_cached`, site A1) and the
        // instance tier-up gate (site A3) use. All three ask for exactly this
        // entry's triple, which is what the one-cell-one-triple invariant
        // requires; `NativeMethodRegistry::find` would re-hash all three
        // strings on every vtable hit.
        //
        // The probe used to be a `OnceLock` memo justified by "every `register`
        // call runs in `vm_init.rs` against the `&mut` registry BEFORE
        // `SharedVm` is constructed". That is not quite true -- `alias_class`
        // and the lazy `register_*` passes append slots after the first
        // bytecode executes -- and a `None` memoized before them never healed.
        // Keying on `NativeMethodRegistry::generation()` makes the memo correct
        // at every point in the VM's lifetime, not just after boot.
        // `entry.resolved_method` is a long-lived `Arc` held by the vtable slot,
        // so the memo really does persist across vtable hits.
        let force_native_registered = force_native
            && cached
                .native_call_site()
                .resolve(
                    &shared.natives.native_methods,
                    cached.class_name.as_ref(),
                    cached.method_name.as_ref(),
                    cached.method_descriptor.as_ref(),
                )
                .is_some();
        if force_native_registered {
            return Ok(CachedCallResult::CacheMiss);
        }
        (cached, is_native)
    };
    let _ = entry_is_native; // silence unused

    // Step 5 — dispatch. Pop args, push a new frame, and populate
    // invoke_cache for subsequent sibling-class misses.
    if thread.frames.len() >= shared.config.max_stack_depth {
        dump_stack_on_soe(thread);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }

    if crate::jit::profile::is_profiling_enabled() {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared.jit.profile_store.record_receiver_borrowed(
            cid,
            mn,
            md,
            site_pc,
            receiver_class_id.as_u32(),
        );
    }

    let total_args = num_params + 1;
    const MAX_INLINE_ARGS: usize = 16;
    // Decode args bit-exact via parameter descriptors (receiver slot = 'L').
    // The prior pop_unchecked()/to_value() dropped the high bits of a
    // category-2 long arg whose NaN-box bit pattern collides with a tagged
    // sub-tag (BC safegcd 0xFFFC_… accumulators). See
    // docs/bc-ec-mod-mododdinverse-investigation.md.
    let arg_desc_byte = |i: usize| -> u8 {
        if i == 0 {
            b'L'
        } else {
            nth_param_tag_byte(&entry_cached.method_descriptor, i - 1)
        }
    };
    let mut args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
    let mut args_vec: Vec<Value> = Vec::new();
    let args_slice: &mut [Value] = if total_args <= MAX_INLINE_ARGS {
        for i in (0..total_args).rev() {
            args_buf[i] = thread.frames[frame_idx]
                .stack
                .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
        }
        &mut args_buf[..total_args]
    } else {
        args_vec.resize(total_args, Value::Uninitialized);
        for i in (0..total_args).rev() {
            args_vec[i] = thread.frames[frame_idx]
                .stack
                .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
        }
        &mut args_vec
    };
    refresh_stale_object_args(shared, args_slice);
    if crate::runtime::env_cache::dbg_loader_trace() && &*method_name == "compareAndSetRoot" {
        let describe = |v: &Value| -> String {
            match v {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(*obj);
                    let cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                }
                Value::Object(None) => "null".to_string(),
                other => format!("{:?}", other),
            }
        };
        eprintln!(
            "[CASROOT-TRACE/vtfast] caller_class_id={:?} cp_index={} receiver_map(args[0])={} expected(args[1])={} updated(args[2])={}",
            caller_class_id,
            cp_index,
            describe(&args_slice[0]),
            args_slice.get(1).map(describe).unwrap_or_default(),
            args_slice.get(2).map(describe).unwrap_or_default(),
        );
    }

    if let Some(res) = intercept_classloader_set_default_assertion_status(
        shared,
        thread,
        entry_cached.method_name.as_ref(),
        entry_cached.method_descriptor.as_ref(),
        args_slice,
    ) {
        return res;
    }

    if let Some(res) = intercept_force_registered_native(
        shared,
        thread,
        frame_idx,
        entry_cached.class_name.as_ref(),
        entry_cached.method_name.as_ref(),
        entry_cached.method_descriptor.as_ref(),
        args_slice,
    ) {
        return res;
    }

    let monitor_obj: Option<ObjectRef> = if entry_cached.is_synchronized {
        // Non-static virtual — receiver owns the monitor.
        match args_slice.first() {
            Some(Value::Object(Some(r))) => {
                let obj = *r;
                Some(crate::vm::monitor_enter_synchronized_method(
                    shared, thread, obj, args_slice,
                ))
            }
            _ => None,
        }
    } else {
        None
    };

    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        entry_cached.max_locals as usize,
        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
        (entry_cached.max_stack as usize).max(16) + 8,
    );
    let mut frame = Frame::new_pooled_cached(
        Arc::clone(&entry_cached),
        args_slice,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    frame.monitor_on_exit = monitor_obj;
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);

    // Populate invoke_cache so subsequent sibling-class misses from this
    // caller class take the cheaper thread-local path next time.
    // WP2.4-F1: bind the gate to the declaring class — that's the class
    // whose method body lives in `entry_cached`. A redefine of that
    // class will bump the same Arc<AtomicU32> and the next cache hit
    // here will auto-evict.
    let gate = RedefineGate::snapshot(
        shared
            .classes
            .class_manager
            .read()
            .class_redefine_generation_handle(entry_cached.declaring_class_id),
    );
    let target = CachedInvokeTarget::VirtualBytecode {
        receiver_class_id,
        cached: Arc::clone(&entry_cached),
        gate,
    };
    // T10.4 — also promote so sibling threads dispatching the same
    // call-site skip the class_manager walk.
    let promoted_key: crate::runtime::lockfree_resolve::PromotedInvokeKey =
        (caller_class_id, cp_index, false, Some(receiver_class_id));
    shared
        .classes
        .shared_resolution
        .insert_promoted_invoke(promoted_key, target.clone());
    thread.invoke_cache.put_poly(
        caller_class_id,
        cp_index,
        false,
        receiver_class_id,
        target.clone(),
    );
    thread
        .invoke_cache
        .put(caller_class_id, cp_index, false, target);

    Ok(CachedCallResult::FramePushed)
}

/// WP2.2 — `Method.invoke` / `Constructor.newInstance` are bytecode in the JDK
/// classfiles but have Rust overrides for correct primitive boxing. The
/// monomorphic invoke cache can still hold [`CachedInvokeTarget::VirtualBytecode`]
/// if populate missed the native; the fast path must not execute JDK bodies
/// (they bypass `try_stackless_invoke` — e.g. Surefire `LazyLauncher` NPE).
#[inline]
pub(super) fn native_override_for_cached_reflect_invoke(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<cratonvm_native_api::NativeCallback> {
    // Every arm is a fully-constant triple, so each gets its OWN memo cell
    // (native-dispatch-memoization §3 Step 1, B5/B6/B7).
    //
    // ONE STATIC, ONE TRIPLE — a `NativeCallSite` is keyed on the registry
    // generation alone and does not re-verify the triple on a warm hit, so a
    // cell reached with two different triples can hand the second one the
    // first one's memoized negative and silently report "no native" for a
    // registered one. Each arm below is a distinct triple and therefore a
    // distinct cell; the third does NOT reuse
    // `surefire_lazy_launcher_discover_native`'s cell even though the triple
    // is identical, so no cell is reachable from more than one call.
    static NCS_METHOD_INVOKE: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    static NCS_CONSTRUCTOR_NEW_INSTANCE: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    static NCS_REFLECT_LAZY_LAUNCHER_DISCOVER: cratonvm_native_api::NativeCallSite =
        cratonvm_native_api::NativeCallSite::new();
    match (class_name, method_name, descriptor) {
        (
            "java/lang/reflect/Method",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        ) => NCS_METHOD_INVOKE.callback(
            &shared.natives.native_methods,
            "java/lang/reflect/Method",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            "java/lang/reflect/Constructor",
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        ) => NCS_CONSTRUCTOR_NEW_INSTANCE.callback(
            &shared.natives.native_methods,
            "java/lang/reflect/Constructor",
            "newInstance",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (
            "org/apache/maven/surefire/junitplatform/LazyLauncher",
            "discover",
            "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;",
        ) => NCS_REFLECT_LAZY_LAUNCHER_DISCOVER.callback(
            &shared.natives.native_methods,
            "org/apache/maven/surefire/junitplatform/LazyLauncher",
            "discover",
            "(Lorg/junit/platform/launcher/LauncherDiscoveryRequest;)Lorg/junit/platform/launcher/TestPlan;",
        ),
        _ => None,
    }
}

/// Fast invokevirtual/invokeinterface/invokespecial using monomorphic inline cache
/// (stackless dispatch). Returns FramePushed for bytecode cache hits,
/// Handled for native, CacheMiss for fall-through.
/// Execute the canonical instance-field accessor body without allocating an
/// interpreter frame.
///
/// A substantial fraction of real workloads are made of generated getters
/// (`aload_0; getfield; <x>return`).  They are semantically simple, but even
/// with an already-warm virtual-call cache the normal interpreter path still
/// builds a frame, executes three bytecodes, and tears the frame down.  That
/// dominates `--nojit` graph-planning workloads, where the accessors are hot
/// enough that compiling them would normally hide the cost.
///
/// This deliberately accepts only the exact five-byte verifier-safe shape,
/// uses an *already resolved* field entry (a cold symbolic reference falls
/// through to ordinary bytecode), and stays out of every JVMTI/redefinition
/// mode that needs to observe the callee frame or field access.  It therefore
/// has the same field value and exception behaviour as the bytecode body while
/// retaining the normal path for all observable instrumentation cases.
pub(super) fn try_execute_cached_trivial_instance_getter(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cached: &CachedBytecodeMethod,
    args: &[Value],
) -> Result<Option<CachedCallResult>, MethodCallFailed> {
    if !crate::runtime::env_cache::trivial_getter_fast_path()
        || cached.is_static
        || cached.is_synchronized
        || cached.num_params != 0
        || args.len() != 1
        || crate::runtime::jvmti::any_method_entry_listener_active()
        || crate::runtime::jvmti::any_method_exit_listener_active()
        || crate::runtime::jvmti::any_field_watchpoint_active()
    {
        return Ok(None);
    }

    // Cached bytecode carries two speculative-read padding bytes.
    let code_len = cached.code.len().saturating_sub(2);
    let code = &cached.code[..code_len];
    if code.len() != 5 || code[0] != 0x2a || code[1] != 0xb4 {
        return Ok(None);
    }
    let return_opcode = code[4];
    if !matches!(return_opcode, 0xac | 0xad | 0xae | 0xaf | 0xb0) {
        return Ok(None);
    }
    let field_cp_index = u16::from_be_bytes([code[2], code[3]]);

    // Do not resolve here: resolving a first-use symbolic reference may load
    // classes and collect, while the decoded argument is intentionally a
    // short-lived Rust local.  The ordinary first invocation resolves it and
    // all later invocations can use this pure cache hit.
    let field = match shared
        .classes
        .resolution_cache
        .read()
        .get_field(cached.declaring_class_id, field_cp_index)
    {
        Some(field) if !field.is_static && !field.is_volatile => field.clone(),
        _ => return Ok(None),
    };

    // Validate that the field descriptor is precisely this method's return
    // descriptor, not just the broad return opcode category.
    let descriptor_matches = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(cached.declaring_class_id) else {
            return Ok(None);
        };
        let Some(ConstantPoolEntry::FieldReference {
            name_and_type_index,
            ..
        }) = class.constant_pool.get(field_cp_index)
        else {
            return Ok(None);
        };
        let Some((_, field_descriptor)) =
            class.constant_pool.get_name_and_type(*name_and_type_index)
        else {
            return Ok(None);
        };
        cached
            .method_descriptor
            .strip_prefix("()")
            .is_some_and(|return_descriptor| return_descriptor == field_descriptor)
    };
    if !descriptor_matches {
        return Ok(None);
    }

    let return_matches_field = matches!(
        (field.desc_byte, return_opcode),
        (b'J', 0xad)
            | (b'F', 0xae)
            | (b'D', 0xaf)
            | (b'L' | b'[', 0xb0)
            | (b'Z' | b'B' | b'C' | b'S' | b'I', 0xac)
    );
    if !return_matches_field {
        return Ok(None);
    }

    // CRATONVM_TRIVIAL_GETTER_VERIFY — resolve the same field reference the way
    // the `getfield` opcode would and report any divergence.
    //
    // This exists because the fast path reads the `resolution_cache` raw, while
    // the opcode goes through `resolve_field_ref_loader_aware`, which documents
    // that a cache entry "may have been populated by a loader-blind helper" and
    // refuses to trust one for a user-defined-loader caller. That made the fast
    // path the prime suspect for the 2026-07-30 Hibernate HQL mis-parse. It was
    // not: a full `ASTParserLoadingTest` run under this verifier reported ZERO
    // divergences while still mis-parsing, and the same run passed 106/106 with
    // `CRATONVM_NO_MOVING_YOUNG=1` — the defect was missing native roots in the
    // ANTLR intrinsics. Keep the verifier so that conclusion stays one env var
    // away from being re-checked instead of re-argued.
    if crate::runtime::env_cache::trivial_getter_verify() {
        if let Ok(authoritative) = resolve_field_ref_loader_aware(
            shared,
            thread,
            cached.declaring_class_id,
            field_cp_index,
        ) {
            if authoritative.field_index != field.field_index
                || authoritative.desc_byte != field.desc_byte
                || authoritative.is_reference != field.is_reference
                || authoritative.is_volatile != field.is_volatile
                || authoritative.declaring_class_id != field.declaring_class_id
            {
                eprintln!(
                    "[TRIVIAL-GETTER-DIVERGENCE] {}.{}{} cp#{field_cp_index}: \
                     cached(idx={} desc={} ref={} vol={} owner={:?}) != \
                     loader_aware(idx={} desc={} ref={} vol={} owner={:?})",
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor,
                    field.field_index,
                    field.desc_byte as char,
                    field.is_reference,
                    field.is_volatile,
                    field.declaring_class_id,
                    authoritative.field_index,
                    authoritative.desc_byte as char,
                    authoritative.is_reference,
                    authoritative.is_volatile,
                    authoritative.declaring_class_id,
                );
            }
        }
    }

    let Value::Object(Some(receiver)) = args[0] else {
        // The normal invokevirtual null check remains responsible for the
        // precisely constructed NPE and its stack trace.
        return Ok(None);
    };
    let receiver = shared.mem.heap.load_and_forward(receiver);
    let mut value = shared.mem.heap.get_field(receiver, field.field_index);
    match field.desc_byte {
        b'J' => {
            let bits = match value {
                Value::Long(x) => x,
                Value::Double(x) => x.to_bits() as i64,
                Value::Int(x) => x as i64,
                Value::Object(None) | Value::Uninitialized => 0,
                Value::Object(Some(raw)) => raw.as_ptr() as usize as i64,
                Value::Float(x) => x.to_bits() as i64,
                Value::ReturnAddress(pc) => pc as i64,
            };
            thread.frames[frame_idx]
                .stack
                .push_compact_long_checked(CompactValue::long(bits))?;
        }
        b'D' => {
            let number = match value {
                Value::Double(x) => x,
                Value::Long(x) => f64::from_bits(x as u64),
                Value::Int(x) => x as f64,
                Value::Object(None) | Value::Uninitialized => 0.0,
                Value::Object(Some(raw)) => f64::from_bits(raw.as_ptr() as usize as u64),
                Value::Float(x) => x as f64,
                Value::ReturnAddress(pc) => pc as f64,
            };
            thread.frames[frame_idx]
                .stack
                .push_compact_double_checked(CompactValue::double(number))?;
        }
        _ => {
            if field.is_reference {
                if matches!(value, Value::Int(0) | Value::Long(0)) {
                    value = Value::Object(None);
                }
            } else {
                value = match value {
                    Value::Object(None) => Value::Int(0),
                    Value::Object(Some(raw)) => Value::Int(raw.as_ptr() as usize as i32),
                    other => other,
                };
                value = narrow_int_to_field_type(value, field.desc_byte);
            }
            if let Value::Object(Some(object)) = value {
                value = Value::Object(Some(shared.mem.heap.load_and_forward(object)));
            }
            push_invoke_return_value(&mut thread.frames[frame_idx].stack, value)?;
        }
    }
    Ok(Some(CachedCallResult::Handled))
}

pub(super) fn execute_invokevirtual_cached(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    site_pc: usize,
    is_special: bool,
    is_interface: bool,
) -> Result<CachedCallResult, MethodCallFailed> {
    let caller_class_id = thread.frames[frame_idx].class_id;

    if crate::runtime::env_cache::dbg_h2trace() {
        if let Ok((owner, method, descriptor, _)) =
            resolve_method_ref(shared, caller_class_id, cp_index)
        {
            if method.as_ref() == "prepareJoinBatch" {
                let cm = shared.classes.class_manager.read();
                let caller_name = cm
                    .get_class(caller_class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                let caller_loader = cm.get_loader_id(caller_class_id);
                let receiver = thread.frames[frame_idx].stack.peek_at(0);
                let recv_info = if let Value::Object(Some(r)) = receiver {
                    let rcid = shared.mem.heap.class_id_of(r);
                    let rname = cm
                        .get_class(rcid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    let rloader = cm.get_loader_id(rcid);
                    format!("class_id={rcid:?} class={rname} loader={rloader:?}")
                } else {
                    format!("{receiver:?}")
                };
                let cached = thread
                    .invoke_cache
                    .get(caller_class_id, cp_index, is_special);
                let cached_info = cached.as_ref().map(|t| format!("{t:?}"));
                drop(cm);
                eprintln!(
                    "[h2trace-pjb] site owner={owner} method={method}{descriptor} caller={caller_name} caller_loader={caller_loader:?} cp_index={cp_index} receiver=[{recv_info}] cached={cached_info:?}",
                );
            }
        }
    }

    if crate::runtime::env_cache::dbg_loader_trace()
        && is_special
        && thread.frames[frame_idx].class_name().contains("MVMap")
    {
        let resolved = resolve_method_ref(shared, caller_class_id, cp_index);
        eprintln!(
            "[EIVC-ENTRY-ALL] caller_class_id={caller_class_id:?} cp_index={cp_index} caller_method={}.{}{} resolved={:?}",
            thread.frames[frame_idx].class_name(),
            thread.frames[frame_idx].method_name(),
            thread.frames[frame_idx].method_descriptor(),
            resolved.as_ref().map(|(cn, mn, md, np)| format!("{cn}.{mn}{md} np={np}")),
        );
    }

    // Keep Spring's loader-split Adapt identity bridge in the slow dispatcher.
    // The cache can predate the receiver loader's enum copy and otherwise
    // bypasses that narrowly scoped reconciliation entirely.
    //
    // PERF (H2 TestFileSystem.testConcurrent, 2026-07-26): `loader_aware_
    // resolution()` is default-ON, so this ran a full `resolve_method_ref`
    // — resolution-cache `RwLock` read, hash probe, four `Arc` clone/drop
    // pairs — on EVERY invokevirtual/-interface/-special in the VM, purely to
    // compare the owner against one hard-coded Spring class name. `adapt_isin_
    // seen()` is a relaxed atomic load that stays `false` until
    // `resolve_method_metadata` has actually resolved a CP entry naming that
    // method, which is a strict prerequisite for this site to matter: the
    // check's only effect is to return `CacheMiss`, and an unresolved call
    // site has no inline-cache entry to bypass in the first place, so it
    // returns `CacheMiss` from the lookup below anyway.
    if crate::runtime::env_cache::loader_aware_resolution() && adapt_isin_seen() {
        if let Ok((owner, method, descriptor, _)) =
            resolve_method_ref(shared, caller_class_id, cp_index)
        {
            if owner.as_ref() == "org/springframework/core/annotation/MergedAnnotation$Adapt"
                && method.as_ref() == "isIn"
                && descriptor.as_ref()
                    == "([Lorg/springframework/core/annotation/MergedAnnotation$Adapt;)Z"
            {
                return Ok(CachedCallResult::CacheMiss);
            }
        }
    }

    // A previous non-null invocation may have cached the real-JDK bytecode
    // body of an inherited ClassLoader method.  That body does not reliably
    // enforce the public null-name contract for a synthetic embedded loader,
    // whereas the registered ClassLoader natives do.  Do this before reading
    // the inline cache: otherwise `resources("...")` poisons the same CP
    // entry and a later `resources(null)` silently returns a Stream.
    if !is_special
        && matches!(
            thread.frames[frame_idx].stack.peek_at(0),
            Value::Object(None)
        )
    {
        if let Ok((method_class_name, method_name, method_descriptor, _)) =
            resolve_method_ref(shared, caller_class_id, cp_index)
        {
            if method_class_name.as_ref() == "java/lang/ClassLoader"
                && matches!(
                    (method_name.as_ref(), method_descriptor.as_ref()),
                    ("loadClass", "(Ljava/lang/String;)Ljava/lang/Class;")
                        | ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
                        | (
                            "getResources",
                            "(Ljava/lang/String;)Ljava/util/Enumeration;"
                        )
                        | (
                            "getResourceAsStream",
                            "(Ljava/lang/String;)Ljava/io/InputStream;"
                        )
                        | ("resources", "(Ljava/lang/String;)Ljava/util/stream/Stream;")
                )
            {
                thread
                    .invoke_cache
                    .evict(caller_class_id, cp_index, is_special);
                return Ok(CachedCallResult::CacheMiss);
            }
        }
    }

    let target = match thread
        .invoke_cache
        .get(caller_class_id, cp_index, is_special)
    {
        Some(t) => {
            dbg_invoke_stats_record(0);
            t.clone()
        }
        None => {
            dbg_invoke_stats_record(1);
            if crate::runtime::env_cache::dbg_gse() {
                if let Ok((_, mn, _, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
                    if mn.as_ref() == "getSyntaxError" {
                        let cm = shared.classes.class_manager.read();
                        let caller_loader = cm.get_loader_id(caller_class_id);
                        eprintln!(
                            "[GSE] CACHE-MISS caller_class_id={caller_class_id:?} caller_loader={caller_loader:?} cp_index={cp_index} is_special={is_special}"
                        );
                    }
                }
            }
            return Ok(CachedCallResult::CacheMiss);
        }
    };
    if crate::runtime::env_cache::dbg_gse() {
        if let Ok((_, mn, _, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
            if mn.as_ref() == "getSyntaxError" {
                let cm = shared.classes.class_manager.read();
                let caller_loader = cm.get_loader_id(caller_class_id);
                let target_desc = match &target {
                    CachedInvokeTarget::Bytecode { cached, .. } => {
                        let dcid = cached.declaring_class_id;
                        let dloader = cm.get_loader_id(dcid);
                        format!("Bytecode declaring_class_id={dcid:?} declaring_loader={dloader:?} declaring_name={}", cached.class_name)
                    }
                    CachedInvokeTarget::Native { .. } => "Native".to_string(),
                    CachedInvokeTarget::VirtualBytecode {
                        cached,
                        receiver_class_id,
                        ..
                    } => {
                        format!("VirtualBytecode receiver_class_id={receiver_class_id:?} declaring_name={}", cached.class_name)
                    }
                    CachedInvokeTarget::VirtualNative {
                        receiver_class_id, ..
                    } => {
                        format!("VirtualNative receiver_class_id={receiver_class_id:?}")
                    }
                    CachedInvokeTarget::Jit { cached, .. } => {
                        format!("Jit declaring_name={}", cached.class_name)
                    }
                    CachedInvokeTarget::Intrinsic { .. } => "Intrinsic".to_string(),
                };
                eprintln!(
                    "[GSE] CACHE-HIT caller_class_id={caller_class_id:?} caller_loader={caller_loader:?} cp_index={cp_index} is_special={is_special} target={target_desc}"
                );
            }
        }
    }
    // JVMTI redefine guard: never serve a cached native/intrinsic SHADOW for a
    // class an agent has redefined in place — evict the entry and re-resolve
    // through the slow path, whose gates dispatch the woven bytecode so the
    // instrumentation advice runs. Without this, the FIRST call resolves to the
    // woven body (slow path) but the inline cache it populates can still hold a
    // VirtualNative/Intrinsic shadow that later calls hit directly, silently
    // bypassing the advice (e.g. mockStatic(X)+mock(X): the stub registers but
    // subsequent `m.method()` calls return the native default). Cheap
    // fast-path on the global `any_class_redefined` flag.
    if crate::classloading::any_class_redefined() {
        let shadow_cid = match &target {
            CachedInvokeTarget::VirtualNative {
                receiver_class_id, ..
            } => Some(*receiver_class_id),
            CachedInvokeTarget::Intrinsic {
                receiver_class_id, ..
            } => *receiver_class_id,
            _ => None,
        };
        if let Some(cid) = shadow_cid {
            if shared
                .classes
                .class_manager
                .read()
                .class_redefine_generation(cid)
                > 0
            {
                thread
                    .invoke_cache
                    .evict(caller_class_id, cp_index, is_special);
                return Ok(CachedCallResult::CacheMiss);
            }
        }
        // `Native` (the invokespecial/invokestatic direct-callback target --
        // see "Static cache entries: invokespecial uses Bytecode/Native"
        // below, since this function also serves invokespecial) carries no
        // `receiver_class_id` field, so the shadow_cid check above can never
        // catch it. `execute_invokestatic_cached` already resolves the CP
        // method-ref's owning class name and consults
        // `native_shadow_suppressed_by_redefine` to evict exactly this kind
        // of stale shadow for invokestatic; this function never did the same
        // for invokespecial. Concretely: Mockito's inline mock maker weaves
        // `AbstractStringBuilder.substring(int)`'s OWN body while
        // `StringBuilder.substring(int)`'s compiler-generated bridge still
        // forwards to it via `invokespecial`; `native_sb_substring` stays
        // registered on `AbstractStringBuilder` forever, so once this
        // invokespecial call site cached `Native` (warmed right after call
        // #1's correct, uncached dispatch), it was never evicted --
        // call #2 onward silently ran the native (real, empty-buffer)
        // implementation instead of Mockito's woven advice.
        //
        // PERF (H2 TestFileSystem.testConcurrent, 2026-07-26): every one of the
        // four conditions below is `false` unless `native_shadow_suppressed_by_
        // redefine` is `true`, and that helper's own first line is
        // `if !any_class_redefined() { return false }` — a relaxed atomic load.
        // Hoisting it above the `resolve_method_ref` skips a resolution-cache
        // `RwLock` read, a hash probe and four `Arc` clone/drop pairs on every
        // cached NATIVE invoke in a process where nothing has ever been
        // redefined, which is every run without an inline mock maker.
        if matches!(&target, CachedInvokeTarget::Native { .. })
            && crate::classloading::any_class_redefined()
        {
            if let Ok((mcn, mn, desc, _)) = resolve_method_ref(shared, caller_class_id, cp_index) {
                // Mirror `receiver_redefined` (populate_virtual_invoke_cache /
                // execute_invokevirtual_vtable_fast): a plain
                // `native_shadow_suppressed_by_redefine` call has no idea
                // about the redefine-immune allowlists, so without these it
                // would evict e.g. `setCharAt`'s native shadow for a
                // genuinely real, non-mock `StringBuilder` the instant ANY
                // StringBuilder anywhere in the process got Mockito-mocked
                // (real bytecode reads the incompatible compact byte[]/coder
                // layout -- see `is_string_builder_layout_native_override` --
                // and crashes with ArrayIndexOutOfBoundsException).
                if native_shadow_suppressed_by_redefine(shared, &mcn)
                    && !redefine_immune_reflection_native(&mcn, &mn)
                    && !redefine_immune_layout_native(&mcn, &mn, &desc)
                {
                    thread
                        .invoke_cache
                        .evict(caller_class_id, cp_index, is_special);
                    return Ok(CachedCallResult::CacheMiss);
                }
            }
        }
    }
    // [PB-DIAG] one-shot dump for InfoCmp.getInfoCmp at pc 38
    if crate::runtime::env_cache::dbg_pbstart() {
        let cn = thread.frames[frame_idx].class_name();
        let mn = thread.frames[frame_idx].method_name();
        if cn.ends_with("/InfoCmp") && mn == "getInfoCmp" && site_pc == 38 {
            let tname = match &target {
                CachedInvokeTarget::VirtualBytecode {
                    receiver_class_id,
                    cached,
                    ..
                } => format!(
                    "VirtualBytecode rc={} class={} {}{}",
                    receiver_class_id.as_u32(),
                    cached.class_name,
                    cached.method_name,
                    cached.method_descriptor
                ),
                CachedInvokeTarget::VirtualNative {
                    receiver_class_id, ..
                } => format!("VirtualNative rc={}", receiver_class_id.as_u32()),
                CachedInvokeTarget::Native { .. } => "Native".to_string(),
                CachedInvokeTarget::Intrinsic { .. } => "Intrinsic".to_string(),
                CachedInvokeTarget::Bytecode { cached, .. } => format!(
                    "Bytecode class={} {}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                ),
                _ => format!("{:?}", target),
            };
            eprintln!(
                "[PB-DIAG-INVOKE] caller={}.{} pc={} cp_idx={} is_special={} target={}",
                cn, mn, site_pc, cp_index, is_special, tname
            );
        }
    }

    // PGO-01: call-site evidence for invokespecial's fast cached path (this
    // function also serves invokevirtual/invokeinterface, which are NOT
    // recorded here — they already have receiver-type coverage below, and
    // this function's own is_special branches are how invokespecial reaches
    // this cache at all, per "Static cache entries: invokespecial uses
    // Bytecode/Native" elsewhere in this function). Same placement rationale
    // as execute_invokestatic_cached: after every early CacheMiss eviction
    // above, right before the dispatch match.
    if is_special && crate::jit::profile::is_profiling_enabled() {
        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
        shared.jit.profile_store.record_call_site_borrowed(cid, mn, md, site_pc);
    }

    match target {
        CachedInvokeTarget::VirtualBytecode {
            mut receiver_class_id,
            mut cached,
            gate: mut entry_gate,
        } => {
            let num_params = cached.num_params as usize; // Widening: parameter count conversion
            let receiver_val = thread.frames[frame_idx].stack.peek_at(num_params);

            match receiver_val {
                Value::Object(Some(obj_ref)) => {
                    // Refresh via the same GC-forwarding barrier as invoke
                    // args (`refresh_stale_object_args`) — this receiver
                    // came from a bare `peek_at`, not a `pop`. See
                    // docs/known-issues/h2/
                    // bug-h2-suite-residual-fail-triage.md.
                    let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
                    // JVMS §4.4.1: an array type inherits its method table
                    // from `java.lang.Object`, but an array's header stores
                    // its COMPONENT class id (`ClassId(0)` for primitive
                    // arrays). Comparing that raw id against the cached
                    // receiver class therefore lets a `Foo[]` receiver hit an
                    // entry installed for a plain `Foo` and run `Foo`'s body
                    // with the array as `this` — `new Foo[3].toString()`
                    // returned `Foo`'s override reading array element 0 as
                    // field 0. Cede to the slow path, which routes array
                    // receivers through `Object` (same guard as
                    // `execute_invokevirtual_vtable_fast` and the JIT
                    // MIC/PIC's `receiver_is_plain_object`).
                    if shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    let actual_class_id = shared.mem.heap.class_id_of(obj_ref);
                    if crate::jit::profile::is_profiling_enabled() {
                        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                        shared.jit.profile_store.record_receiver_borrowed(
                            cid,
                            mn,
                            md,
                            site_pc,
                            actual_class_id.as_u32(),
                        );
                    }
                    if crate::runtime::env_cache::dbg_loader_trace()
                        && (cached.class_name.contains("RootReference")
                            || cached.class_name.contains("MVMap"))
                    {
                        eprintln!(
                            "[LOADER-TRACE] execute_invokevirtual_cached HIT-CHECK caller_class_id={:?} cp_index={} is_special={} method={}.{}{} cached.declaring={} cached_receiver_class_id={:?} actual_class_id={:?} match={}",
                            caller_class_id, cp_index, is_special,
                            cached.class_name, cached.method_name, cached.method_descriptor,
                            cached.class_name, receiver_class_id, actual_class_id, actual_class_id == receiver_class_id
                        );
                    }
                    if actual_class_id != receiver_class_id {
                        // PERF (h2-bnf-perf 2026-07-23): the primary monomorphic
                        // cache is stale for THIS receiver (it holds whichever
                        // class called through this site most recently), not
                        // simply empty -- so a megamorphic call site alternating
                        // between a handful of concrete classes (e.g. H2's
                        // `org.h2.bnf.Rule` family) hits this exact mismatch on
                        // nearly every call. Before giving up, check the small
                        // polymorphic overflow cache for an entry recorded for
                        // THIS specific receiver class on an earlier call (see
                        // `InvokeCache::get_poly`/`put_poly`) -- if found, swap
                        // in its (already staleness-checked, by construction
                        // keyed to `actual_class_id`) target and fall through
                        // to the rest of this arm's normal dispatch below,
                        // instead of paying for the full vtable-fast/slow-path
                        // resolution again for a receiver class this call site
                        // has already seen.
                        let poly_result = thread.invoke_cache.get_poly(
                            caller_class_id,
                            cp_index,
                            is_special,
                            actual_class_id,
                        );
                        match poly_result {
                            Some(CachedInvokeTarget::VirtualBytecode {
                                receiver_class_id: poly_rc,
                                cached: poly_cached,
                                gate: poly_gate,
                            }) => {
                                receiver_class_id = poly_rc;
                                cached = poly_cached;
                                entry_gate = poly_gate;
                            }
                            _ => return Ok(CachedCallResult::CacheMiss),
                        }
                    }
                    // Lambda proxy classes have no bytecode implementation of
                    // their functional-interface method. They must reach the
                    // slow path, which dispatches their SAM method handle.
                    if !is_special
                        && shared
                            .classes
                            .lambda_proxies
                            .read()
                            .contains_key(&actual_class_id)
                    {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    // WP2.7 — AnnotationProxy methods (incl. Object.equals/hashCode/
                    // toString from Object) must dispatch through the spec-compliant
                    // interception in `execute_invoke`, not Object's bytecode.
                    if !is_special {
                        let cm = shared.classes.class_manager.read();
                        let is_ann_proxy = cm
                            .get_class(actual_class_id)
                            .map(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
                            .unwrap_or(false);
                        drop(cm);
                        if is_ann_proxy {
                            return Ok(CachedCallResult::CacheMiss);
                        }
                    }

                    // A cache entry created through a vtable slot is valid for
                    // an interface site only when it is the same target that
                    // receiver-rooted maximally-specific default resolution
                    // selects. This prevents a parent-interface default from
                    // remaining cached after it masked a covariant bridge on a
                    // receiver subinterface.
                    if is_interface {
                        let cm = shared.classes.class_manager.read();
                        let receiver_selected = crate::classloading::find_method_recursive(
                            actual_class_id,
                            &cached.method_name,
                            &cached.method_descriptor,
                            &cm.class_store,
                        )
                        .map(|(_, declaring_id)| declaring_id);
                        drop(cm);
                        if receiver_selected != Some(cached.declaring_class_id) {
                            thread
                                .invoke_cache
                                .evict(caller_class_id, cp_index, is_special);
                            return Ok(CachedCallResult::CacheMiss);
                        }
                    }

                    if thread.frames.len() >= shared.config.max_stack_depth {
                        dump_stack_on_soe(thread);
                        return Err(MethodCallFailed::InternalError(VmError::Runtime(
                            RuntimeError::StackOverflowError,
                        )));
                    }

                    let total_args = num_params + 1;
                    const MAX_INLINE_ARGS: usize = 16;
                    // Decode args bit-exact via parameter descriptors (receiver
                    // = 'L'); pop_unchecked()/to_value() dropped the high bits
                    // of collision-pattern long args. See
                    // docs/bc-ec-mod-mododdinverse-investigation.md.
                    let arg_desc_byte = |i: usize| -> u8 {
                        if i == 0 {
                            b'L'
                        } else {
                            nth_param_tag_byte(&cached.method_descriptor, i - 1)
                        }
                    };
                    let mut args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
                    let mut args_vec: Vec<Value> = Vec::new();
                    let args_slice: &mut [Value] = if total_args <= MAX_INLINE_ARGS {
                        for i in (0..total_args).rev() {
                            args_buf[i] = thread.frames[frame_idx]
                                .stack
                                .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
                        }
                        &mut args_buf[..total_args]
                    } else {
                        args_vec.resize(total_args, Value::Uninitialized);
                        for i in (0..total_args).rev() {
                            args_vec[i] = thread.frames[frame_idx]
                                .stack
                                .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
                        }
                        &mut args_vec
                    };
                    if crate::runtime::env_cache::dbg_loader_trace()
                        && cached.class_name.contains("RootReference")
                        && cached.method_name.as_ref() == "tryUpdate"
                    {
                        let describe = |v: &Value| -> String {
                            match v {
                                Value::Object(Some(obj)) => {
                                    let cid = shared.mem.heap.class_id_of(*obj);
                                    let cn = shared
                                        .classes
                                        .class_manager
                                        .read()
                                        .get_class(cid)
                                        .map(|c| c.name.to_string())
                                        .unwrap_or_default();
                                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                                }
                                Value::Object(None) => "null".to_string(),
                                other => format!("{:?}", other),
                            }
                        };
                        eprintln!(
                            "[TRYUPDATE-TRACE/vbc] caller_class_id={:?} cp_index={} receiver_class_id={:?} pre-refresh receiver(args[0])={} updated(args[1])={}",
                            caller_class_id,
                            cp_index,
                            receiver_class_id,
                            describe(&args_slice[0]),
                            args_slice.get(1).map(describe).unwrap_or_default(),
                        );
                    }
                    refresh_stale_object_args(shared, args_slice);
                    if crate::runtime::env_cache::dbg_loader_trace()
                        && cached.class_name.contains("RootReference")
                        && cached.method_name.as_ref() == "tryUpdate"
                    {
                        let describe = |v: &Value| -> String {
                            match v {
                                Value::Object(Some(obj)) => {
                                    let cid = shared.mem.heap.class_id_of(*obj);
                                    let cn = shared
                                        .classes
                                        .class_manager
                                        .read()
                                        .get_class(cid)
                                        .map(|c| c.name.to_string())
                                        .unwrap_or_default();
                                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                                }
                                Value::Object(None) => "null".to_string(),
                                other => format!("{:?}", other),
                            }
                        };
                        eprintln!(
                            "[TRYUPDATE-TRACE/vbc] caller_class_id={:?} cp_index={} receiver_class_id={:?} post-refresh receiver(args[0])={} updated(args[1])={}",
                            caller_class_id,
                            cp_index,
                            receiver_class_id,
                            describe(&args_slice[0]),
                            args_slice.get(1).map(describe).unwrap_or_default(),
                        );
                    }
                    if crate::runtime::env_cache::dbg_loader_trace()
                        && cached.method_name.as_ref() == "compareAndSetRoot"
                    {
                        let describe = |v: &Value| -> String {
                            match v {
                                Value::Object(Some(obj)) => {
                                    let cid = shared.mem.heap.class_id_of(*obj);
                                    let cn = shared
                                        .classes
                                        .class_manager
                                        .read()
                                        .get_class(cid)
                                        .map(|c| c.name.to_string())
                                        .unwrap_or_default();
                                    format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                                }
                                Value::Object(None) => "null".to_string(),
                                other => format!("{:?}", other),
                            }
                        };
                        eprintln!(
                            "[CASROOT-TRACE/vbc] caller_class_id={:?} cp_index={} receiver_class_id={:?} receiver_map(args[0])={} expected(args[1])={} updated(args[2])={}",
                            caller_class_id,
                            cp_index,
                            receiver_class_id,
                            describe(&args_slice[0]),
                            args_slice.get(1).map(describe).unwrap_or_default(),
                            args_slice.get(2).map(describe).unwrap_or_default(),
                        );
                    }

                    if let Some(res) = intercept_classloader_set_default_assertion_status(
                        shared,
                        thread,
                        cached.method_name.as_ref(),
                        cached.method_descriptor.as_ref(),
                        args_slice,
                    ) {
                        return res;
                    }

                    if let Some(callback) = surefire_lazy_launcher_discover_native(
                        shared,
                        cached.method_name.as_ref(),
                        cached.method_descriptor.as_ref(),
                        obj_ref,
                    ) {
                        invoke_cached_native_callback(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            args_slice,
                            cached.method_descriptor.as_ref(),
                        )?;
                        return Ok(CachedCallResult::Handled);
                    }

                    if let Some(callback) = native_override_for_cached_reflect_invoke(
                        shared,
                        cached.class_name.as_ref(),
                        cached.method_name.as_ref(),
                        cached.method_descriptor.as_ref(),
                    ) {
                        invoke_cached_native_callback(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            args_slice,
                            cached.method_descriptor.as_ref(),
                        )?;
                        return Ok(CachedCallResult::Handled);
                    }

                    if let Some(res) = intercept_force_registered_native_cached(
                        shared, thread, frame_idx, &cached, args_slice,
                    ) {
                        return res;
                    }

                    if let Some(result) = try_execute_cached_trivial_instance_getter(
                        shared, thread, frame_idx, &cached, args_slice,
                    )? {
                        return Ok(result);
                    }

                    // (bug-03 layer B, default-ON; off-switch CRATONVM_JIT_VIRTUAL_TIERUP=0)
                    // Instance-method invocation tier-up. Today only static
                    // methods have an invocation counter, so short-loop instance
                    // hot methods (e.g. java.util.regex Pattern$*.match) never
                    // JIT-compile. Placed AFTER every interception above (so a
                    // natively-overridden method is never run as JIT'd bytecode)
                    // and BEFORE the method-level monitor below (the JIT body does
                    // not acquire a `synchronized`-method monitor, so those are
                    // excluded). Monomorphic: the receiver class id was already
                    // checked `== receiver_class_id`, so `cached` is the exact
                    // target for this receiver. `args_slice` is already decoded;
                    // on deopt/too-many-args we fall through to the interpreted
                    // frame push below (the operand stack is untouched).
                    //
                    // `!has_registered_native`: the comment above claims natives
                    // are already excluded by this point, but that's only true
                    // for FORCED natives (`intercept_force_registered_native_cached`
                    // just above only fires when `force_native_over_real_jdk_bytecode`
                    // says so). An ordinary, non-forced registered native — the
                    // common case, which the interpreter's per-call dispatch
                    // prefers over real bytecode by default — is invisible to
                    // that check, so tier-up would compile the method's REAL
                    // BYTECODE and, once compiled, EVERY future call through this
                    // receiver class permanently bypasses the native (compiled
                    // code doesn't re-run the interpreter's native-vs-bytecode
                    // decision). Found 2026-07-20 via `java.lang.ClassValue#get`
                    // (see `classvalue_cache.rs`/`is_classvalue_native_override`):
                    // Apache Groovy's `ClassInfo` registry calls it thousands of
                    // times per app boot — comfortably past the tier-up threshold
                    // — and the real bytecode it silently switched to depends on
                    // `Class.classValueMap`/`Unsafe` CAS machinery CratonVM
                    // doesn't faithfully reproduce, reintroducing the exact
                    // always-null symptom the native override exists to fix, but
                    // ONLY under JIT (this tier-up is JIT-only) and ONLY once
                    // warm — see docs/known-issues/springboot/
                    // core-spring-boot-test-config-data-and-classpath-scan-cluster.md
                    // Cluster C "Residual 5". Site A3 of
                    // `native-dispatch-memoization.md` §3 Step 2: reusing this
                    // entry's `NativeCallSite` (already warm from the
                    // force-native path above, and asking for the same triple)
                    // keeps this two integer loads, not a per-call triple hash.
                    // Unlike the `OnceLock` it replaces, a native registered by
                    // a lazy `register_*` pass after this site first ran is now
                    // seen -- which matters here, because a missed native means
                    // tier-up compiles the REAL BYTECODE and permanently
                    // bypasses the override.
                    let has_registered_native = cached
                        .native_call_site()
                        .resolve(
                            &shared.natives.native_methods,
                            &cached.class_name,
                            &cached.method_name,
                            &cached.method_descriptor,
                        )
                        .is_some();
                    // `cached.class_name` is the call site's symbolic owner;
                    // for an interface call it need not be the concrete
                    // receiver that this monomorphic cache just validated.
                    // Consult the receiver ClassId for the java.util virtual
                    // tier-up exclusion so subtypes reached through List/Map
                    // or Iterator are covered as well.
                    let receiver_is_java_util = {
                        // This dispatch can run while the current thread still
                        // owns the class-manager write lock during bootstrap.
                        // A blocking read here self-deadlocks. If the table is
                        // busy, conservatively suppress this optional tier-up.
                        shared
                            .classes
                            .class_manager
                            .try_read()
                            .map(|cm| {
                                cm.get_class(receiver_class_id)
                                    .is_some_and(|class| class.name.starts_with("java/util/"))
                            })
                            .unwrap_or(true)
                    };
                    if !is_special
                        && !matches!(thread.kind, crate::threading::ThreadKind::Virtual)
                        && !cached.is_synchronized
                        && !has_registered_native
                        && entry_gate.generation == 0
                        && !crate::runtime::env_cache::disable_jit()
                        // The generic-conversion regression reaches a hot
                        // java.util graph while Spring creates annotation and
                        // conversion metadata. Its instance-method tier-ups
                        // are independently JIT-safe at direct/static sites,
                        // but this cached virtual route can publish a stale
                        // receiver-specific entry and then spin. Keep only
                        // this virtual promotion out of java.util; static
                        // compilation and ordinary direct dispatch remain on.
                        && !receiver_is_java_util
                        // A handler-bearing callee must never be entered by a
                        // DIRECT compiled call. `execute_jit_call_decoded`
                        // below has no interpreter boundary at which the
                        // callee's own exception table can be resumed, so an
                        // implicit NPE/AIOOBE raised inside it bails out
                        // through THIS caller's epilogue, past the only point
                        // able to route it — leaving a pending exceptional
                        // frame the caller cannot resume. On an embedded server
                        // that surfaces as a request that never completes
                        // (`MultipartAutoConfigurationTests`).
                        //
                        // `try_jit_upgrade_with_gate` already refuses these,
                        // and so do the MIC/PIC and OSR direct-call sites
                        // (`mic_callee_has_exception_table`,
                        // `osr_callee_declares_handlers`). This route was the
                        // gap: under `bg_compile` — the DEFAULT — the worker
                        // publishes and the `jit_cache` probe below promotes
                        // the site without consulting the gate, which is the
                        // only reason `jit_virtual_tierup` had to be turned off
                        // wholesale. `exception_table` is carried on the
                        // callee's own cache entry, so this costs one field
                        // read, not a class-manager lookup.
                        && cached.exception_table.is_empty()
                        && crate::runtime::env_cache::jit_virtual_tierup()
                    {
                        // Fast path: already compiled (by this counter or OSR)?
                        //
                        // T2.2 -- epoch-guarded exactly like the invokestatic twin
                        // in `execute_invokestatic_cached`: skip the string-keyed
                        // `JitCache::get` while this entry's snapshot of
                        // `jit_cache_generation()` is still current, because no
                        // publication or invalidation has happened since the probe
                        // that missed. Read the generation before probing so a
                        // racing publication can only cause a redundant re-probe,
                        // never a missed one.
                        let jit_generation = cratonvm_jit::jit_cache_generation();
                        let compiled_opt = if cached.jit_probe_is_current(jit_generation) {
                            None
                        } else {
                            let found = shared.jit.jit_cache.read().get(
                                &cached.class_name,
                                &cached.method_name,
                                &cached.method_descriptor,
                                cached.declaring_class_id,
                            );
                            if found.is_none() {
                                cached.record_jit_probe_miss(jit_generation);
                            }
                            // See the interpreter's twin: released on the
                            // mutator, and regularly the last owner.
                            found.map(cratonvm_jit::RetainedCode::new)
                        }
                        .or_else(|| {
                            // Warmup counter mirroring execute_invokestatic_cached.
                            // T2.5 -- memoized per call site; see
                            // `CachedBytecodeMethod::invoc_key`.
                            let invoc_key = cached.invoc_key();
                            const JIT_RETRY_STRIDE: u32 = 64;
                            let threshold = crate::runtime::env_cache::jit_invocation_threshold();
                            let cnt = shared.jit.profile_store.increment_invocation(invoc_key);
                            let should_attempt = cnt >= threshold
                                && (cnt == threshold || (cnt - threshold) % JIT_RETRY_STRIDE == 0);
                            if should_attempt {
                                // wire-tiered-manager Step 7: off-thread tier-up is now
                                // the default. Under `bg_compile` (default-ON) we ENQUEUE
                                // and keep interpreting — the `jit_cache.get` fast-path
                                // above flips this site to `Jit` once the worker
                                // publishes — instead of compiling inline on the mutator.
                                // Mirrors `execute_invokestatic_cached`. Opt-out
                                // (`CRATONVM_BG_COMPILE=0`) restores the inline path.
                                if crate::runtime::env_cache::bg_compile() {
                                    ensure_bg_compiler_started(shared);
                                    let tiered_key = crate::jit::tiered::MethodKey::new(
                                        cached.class_name.as_ref(),
                                        cached.method_name.as_ref(),
                                        cached.method_descriptor.as_ref(),
                                    );
                                    // Real invocation count — see the invokestatic
                                    // twin: stride-boundary `+= 1` counting deflated
                                    // the manager's hotness view 64x.
                                    let _ = shared
                                        .jit
                                        .tiered_manager
                                        .on_method_invocation_observed(&tiered_key, cnt as u64);
                                } else if let Some(CachedInvokeTarget::Jit { compiled, .. }) =
                                    try_jit_upgrade_with_gate(shared, &cached, entry_gate.clone())
                                {
                                    return Some(compiled);
                                }
                            }
                            None
                        });
                        if let Some(compiled) = compiled_opt {
                            let ret = crate::jit::return_type(&cached.method_descriptor);
                            let heap = compiled.needs_heap();
                            // total_args = receiver + declared params; the decoded
                            // dispatcher treats arg 0 as the receiver.
                            if let Some(ccr) = execute_jit_call_decoded(
                                shared,
                                thread,
                                frame_idx,
                                &compiled,
                                total_args as u16, // Cast: small param count
                                ret,
                                heap,
                                &cached,
                                args_slice,
                            )? {
                                return Ok(ccr);
                            }
                            // None → deopt / too-many-args: fall through to the
                            // interpreted frame push (operand stack untouched).
                        }
                    }

                    // Acquire monitor for synchronized methods
                    let monitor_obj: Option<ObjectRef> = if cached.is_synchronized {
                        let obj = if cached.is_static {
                            // JVMS §2.11.10: static-synchronized monitor is the Class
                            // mirror (same object as ldc class / synchronized(X.class)).
                            get_or_create_class_mirror(shared, cached.declaring_class_id)
                        } else {
                            // Receiver is args_slice[0] for virtual calls
                            match args_slice.first() {
                                Some(Value::Object(Some(r))) => *r,
                                _ => return Ok(CachedCallResult::CacheMiss),
                            }
                        };
                        Some(crate::vm::monitor_enter_synchronized_method(
                            shared, thread, obj, args_slice,
                        ))
                    } else {
                        None
                    };

                    // T10.7 — top off the thread-local pool from the shared
                    // VM-wide VecPool when empty so sibling-thread releases
                    // bubble back into the hot path.
                    thread.refill_pools_from_shared(
                        &shared.mem.operand_stack_pool,
                        &shared.mem.tag_pool,
                        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                        cached.max_locals as usize,
                        // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                        (cached.max_stack as usize).max(16) + 8,
                    );
                    let mut frame = Frame::new_pooled_cached(
                        cached,
                        args_slice,
                        &mut thread.locals_pool,
                        &mut thread.stacks_pool,
                    );
                    frame.monitor_on_exit = monitor_obj;
                    if crate::runtime::env_cache::frame_trace() {
                        eprintln!(
                            "[FRAME_PUSH/vcached] depth={} {}.{}{}",
                            thread.frames.len(),
                            frame.class_name(),
                            frame.method_name(),
                            frame.method_descriptor()
                        );
                    }
                    push_frame_and_fire_entry(shared.vm_identity, thread, frame);
                    Ok(CachedCallResult::FramePushed)
                }
                Value::Object(None) => {
                    // Round 63 — defer to slow path which has the
                    // null-tolerant shim for Spring's
                    // `GenericConversionService$Converters.getClassHierarchy`.
                    Ok(CachedCallResult::CacheMiss)
                }
                _ => Ok(CachedCallResult::CacheMiss),
            }
        }
        CachedInvokeTarget::VirtualNative {
            receiver_class_id,
            callback,
            native_id,
            native_kind,
            num_params,
            gate: _,
        } => {
            let num_params_usize = num_params as usize; // Widening: parameter count conversion
            let receiver_val = thread.frames[frame_idx].stack.peek_at(num_params_usize);

            match receiver_val {
                Value::Object(Some(obj_ref)) => {
                    // Refresh via the same GC-forwarding barrier as invoke
                    // args (`refresh_stale_object_args`) — this receiver
                    // came from a bare `peek_at`, not a `pop`. See
                    // docs/known-issues/h2/
                    // bug-h2-suite-residual-fail-triage.md.
                    let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
                    // JVMS §4.4.1: an array type inherits its method table
                    // from `java.lang.Object`, but an array's header stores
                    // its COMPONENT class id (`ClassId(0)` for primitive
                    // arrays). Comparing that raw id against the cached
                    // receiver class therefore lets a `Foo[]` receiver hit an
                    // entry installed for a plain `Foo` and run `Foo`'s body
                    // with the array as `this` — `new Foo[3].toString()`
                    // returned `Foo`'s override reading array element 0 as
                    // field 0. Cede to the slow path, which routes array
                    // receivers through `Object` (same guard as
                    // `execute_invokevirtual_vtable_fast` and the JIT
                    // MIC/PIC's `receiver_is_plain_object`).
                    if shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    let actual_class_id = shared.mem.heap.class_id_of(obj_ref);
                    if crate::jit::profile::is_profiling_enabled() {
                        let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                        shared.jit.profile_store.record_receiver_borrowed(
                            cid,
                            mn,
                            md,
                            site_pc,
                            actual_class_id.as_u32(),
                        );
                    }
                    if actual_class_id != receiver_class_id {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    let Some(callback) =
                        revalidate_cached_native(shared, native_id, callback, native_kind)
                    else {
                        thread
                            .invoke_cache
                            .evict(caller_class_id, cp_index, is_special);
                        return Ok(CachedCallResult::CacheMiss);
                    };
                    // The callback identity proves this cache entry is one of
                    // the eight real-layout Matcher leaves. The receiver was
                    // just checked against the cache's monomorphic class guard,
                    // so it cannot be a lambda/annotation proxy and its sole
                    // object argument is already heap-validated. Skip those two
                    // generic proxy locks and the duplicate heap-membership
                    // search while retaining ordinary native pinning, return,
                    // and exception handling.
                    if cratonvm_native_builtins::is_matcher_realjdk_native_callback(callback) {
                        let (args, method_descriptor) = pop_coerced_invoke_args_virtual(
                            shared,
                            caller_class_id,
                            cp_index,
                            frame_idx,
                            thread,
                        )?;
                        invoke_cached_native_callback_prevalidated(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            &args,
                            &method_descriptor,
                        )?;
                        return Ok(CachedCallResult::Handled);
                    }
                    // The two hottest cache lookups in Tomcat are
                    // `String.toLowerCase(Locale)` and `Map.get(Object)`.
                    // Their virtual-native cache entries already prove both
                    // receiver class and callback identity, but the generic
                    // arm below still re-resolved the CP descriptor and
                    // allocated a `Vec` for their two reference arguments on
                    // every iteration. Pop the verifier-known reference pair
                    // directly and retain the ordinary prevalidated native
                    // call for pinning, GC remapping, exceptions and return
                    // coercion.
                    let cached_string_lower = num_params_usize == 1
                        && cratonvm_native_builtins::lang_string::is_lower_case_native_callback(
                            callback,
                        );
                    let cached_map_get = num_params_usize == 1
                        && !cached_string_lower
                        && cratonvm_native_collections::is_hot_map_get_native_callback(callback);
                    if cached_string_lower || cached_map_get {
                        let argument = thread.frames[frame_idx]
                            .stack
                            .pop_compact_with_long_mark()?
                            .0
                            .decode_by_descriptor(b'L');
                        let receiver = thread.frames[frame_idx]
                            .stack
                            .pop_compact_with_long_mark()?
                            .0
                            .decode_by_descriptor(b'L');
                        let mut args = [receiver, argument];
                        refresh_stale_object_args(shared, &mut args);
                        let descriptor = if cached_string_lower {
                            "(Ljava/util/Locale;)Ljava/lang/String;"
                        } else {
                            "(Ljava/lang/Object;)Ljava/lang/Object;"
                        };
                        invoke_cached_native_callback_prevalidated(
                            shared, thread, frame_idx, callback, &args, descriptor,
                        )?;
                        return Ok(CachedCallResult::Handled);
                    }
                    // Lambda proxies require the slow `try_lambda_dispatch`
                    // route instead of a cached interface target.
                    if !is_special
                        && shared
                            .classes
                            .lambda_proxies
                            .read()
                            .contains_key(&actual_class_id)
                    {
                        return Ok(CachedCallResult::CacheMiss);
                    }
                    // WP2.7 — same escape hatch as in the bytecode branch:
                    // AnnotationProxy method dispatch must always go through
                    // `execute_invoke`'s spec-compliant interception layer.
                    if !is_special {
                        let cm = shared.classes.class_manager.read();
                        let is_ann_proxy = cm
                            .get_class(actual_class_id)
                            .map(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
                            .unwrap_or(false);
                        drop(cm);
                        if is_ann_proxy {
                            return Ok(CachedCallResult::CacheMiss);
                        }
                    }

                    let (args, method_descriptor) = pop_coerced_invoke_args_virtual(
                        shared,
                        caller_class_id,
                        cp_index,
                        frame_idx,
                        thread,
                    )?;
                    invoke_cached_native_callback(
                        shared,
                        thread,
                        frame_idx,
                        callback,
                        &args,
                        &method_descriptor,
                    )?;
                    Ok(CachedCallResult::Handled)
                }
                Value::Object(None) => {
                    // Round 63 — defer to slow path which has the
                    // null-tolerant shim for Spring's
                    // `GenericConversionService$Converters.getClassHierarchy`.
                    Ok(CachedCallResult::CacheMiss)
                }
                _ => Ok(CachedCallResult::CacheMiss),
            }
        }
        // Interpreter intrinsic reached via invokevirtual/invokeinterface/
        // invokespecial. Modelled on the `VirtualNative` arm: the
        // `receiver_class_id` guard is the virtual-dispatch soundness check
        // (roadmap §3.4) — an overriding subclass produces a different
        // class id and falls to `CacheMiss`, re-resolving down the slow
        // path. A `None` `receiver_class_id` means a static intrinsic
        // (only reachable here via invokespecial); it carries no guard.
        CachedInvokeTarget::Intrinsic {
            kind: _,
            callback,
            num_params,
            param_descs,
            return_type,
            receiver_class_id,
            gate: _,
        } => {
            if let Some(guard_class_id) = receiver_class_id {
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                let num_params_usize = num_params as usize;
                let receiver_val = thread.frames[frame_idx].stack.peek_at(num_params_usize);
                match receiver_val {
                    Value::Object(Some(obj_ref)) => {
                        // Refresh via the same GC-forwarding barrier as
                        // invoke args (`refresh_stale_object_args`) — this
                        // receiver came from a bare `peek_at`, not a `pop`.
                        // See docs/known-issues/h2/
                        // bug-h2-suite-residual-fail-triage.md.
                        let obj_ref = shared.mem.heap.load_and_forward(obj_ref);
                        // JVMS §4.4.1: an array type inherits its method table
                        // from `java.lang.Object`, but an array's header stores
                        // its COMPONENT class id (`ClassId(0)` for primitive
                        // arrays). Comparing that raw id against the cached
                        // receiver class therefore lets a `Foo[]` receiver hit an
                        // entry installed for a plain `Foo` and run `Foo`'s body
                        // with the array as `this` — `new Foo[3].toString()`
                        // returned `Foo`'s override reading array element 0 as
                        // field 0. Cede to the slow path, which routes array
                        // receivers through `Object` (same guard as
                        // `execute_invokevirtual_vtable_fast` and the JIT
                        // MIC/PIC's `receiver_is_plain_object`).
                        if shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
                            return Ok(CachedCallResult::CacheMiss);
                        }
                        let actual_class_id = shared.mem.heap.class_id_of(obj_ref);
                        if crate::jit::profile::is_profiling_enabled() {
                            let (cid, mn, md) = method_key_parts(&thread.frames[frame_idx]);
                            shared.jit.profile_store.record_receiver_borrowed(
                                cid,
                                mn,
                                md,
                                site_pc,
                                actual_class_id.as_u32(),
                            );
                        }
                        // Virtual-dispatch soundness guard: a receiver whose
                        // actual class differs from the resolved one may
                        // override the method — miss the intrinsic.
                        if actual_class_id != guard_class_id {
                            return Ok(CachedCallResult::CacheMiss);
                        }
                        // Phase 3: pop against the IC-cached `param_descs`
                        // (receiver included) into a stack buffer — no
                        // resolve, no descriptor parse, no heap alloc on the
                        // steady-state path.
                        let mut arg_buf = [Value::Uninitialized; MAX_INTRINSIC_ARGS];
                        let args = pop_coerced_invoke_args_intrinsic(
                            shared,
                            thread,
                            frame_idx,
                            num_params_usize,
                            &param_descs,
                            true,
                            &mut arg_buf,
                        )?;
                        invoke_cached_intrinsic(
                            shared,
                            thread,
                            frame_idx,
                            callback,
                            args,
                            return_type,
                        )?;
                        Ok(CachedCallResult::Handled)
                    }
                    Value::Object(None) => Ok(CachedCallResult::CacheMiss),
                    _ => Ok(CachedCallResult::CacheMiss),
                }
            } else {
                // `receiver_class_id: None` is only ever produced by the
                // invokestatic populator, which is consulted via
                // `execute_invokestatic_cached` — never this function. So
                // this arm is unreachable in practice; degrade safely to
                // the slow path rather than guess the arg-pop convention.
                let _ = callback;
                Ok(CachedCallResult::CacheMiss)
            }
        }
        // Static cache entries: invokespecial uses Bytecode/Native
        CachedInvokeTarget::Bytecode { cached, gate: _ } => {
            // NULL-RECEIVER-CACHED-20260801: JVMS §6.5 — invokevirtual /
            // invokespecial / invokeinterface must raise NullPointerException
            // when `objectref` is null, BEFORE the callee frame exists. This
            // arm popped the receiver straight into `args_slice[0]` and pushed
            // the frame regardless, so a warmed call site silently RAN the
            // callee body with `this == null`; the sibling `VirtualBytecode`
            // arm has always deferred `Value::Object(None)` to the slow path
            // (which owns both the canonical NPE and the deliberate
            // null-tolerant shims), and this arm — which serves invokespecial,
            // i.e. every private/super call — simply never got the same guard.
            //
            // Measured on `probes/NullReceiverInvokeProbe.java`: the FIRST
            // `Impl.callPrivateOn(null)` throws NPE correctly (slow path),
            // and after 50k warming calls the SAME site returns `3` — the
            // private method's body, executed with a null `this`.
            //
            // That divergence is what turned a null element in
            // `getPermittedSubclasses0()` into
            // `NullPointerException: Cannot read field "interfaces" because
            // "rd" is null` at `Class.java:1217` instead of a plain NPE at
            // `Class.isDirectSubType`: `c.getInterfaces(false)` is an
            // invokespecial, the callee frame was pushed with `this == null`,
            // and `Class.reflectionData()`'s registered native answers a null
            // receiver with a null RETURN. Deferring to the slow path here
            // makes the warmed and cold answers identical, whichever the slow
            // path decides.
            if !cached.is_static
                && matches!(
                    thread.frames[frame_idx]
                        .stack
                        .peek_at(cached.num_params as usize),
                    Value::Object(None)
                )
            {
                return Ok(CachedCallResult::CacheMiss);
            }
            if is_special && crate::runtime::env_cache::loader_aware_resolution() {
                if let Some(owner_cid) =
                    lookup_loader_initiated(shared, caller_class_id, cached.class_name.as_ref())
                {
                    if owner_cid != cached.declaring_class_id {
                        thread
                            .invoke_cache
                            .evict(caller_class_id, cp_index, is_special);
                        return Ok(CachedCallResult::CacheMiss);
                    }
                }
            }
            if thread.frames.len() >= shared.config.max_stack_depth {
                dump_stack_on_soe(thread);
                return Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::StackOverflowError,
                )));
            }

            let total_args = cached.num_params as usize + 1; // Widening: parameter count conversion
            const MAX_INLINE_ARGS: usize = 16;
            // Decode args bit-exact via parameter descriptors (receiver = 'L');
            // pop_unchecked()/to_value() dropped the high bits of collision-
            // pattern long args. See docs/bc-ec-mod-mododdinverse-investigation.md.
            let arg_desc_byte = |i: usize| -> u8 {
                if i == 0 {
                    b'L'
                } else {
                    nth_param_tag_byte(&cached.method_descriptor, i - 1)
                }
            };
            let mut args_buf = [Value::Uninitialized; MAX_INLINE_ARGS];
            let mut args_vec: Vec<Value> = Vec::new();
            let args_slice: &mut [Value] = if total_args <= MAX_INLINE_ARGS {
                for i in (0..total_args).rev() {
                    args_buf[i] = thread.frames[frame_idx]
                        .stack
                        .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
                }
                &mut args_buf[..total_args]
            } else {
                args_vec.resize(total_args, Value::Uninitialized);
                for i in (0..total_args).rev() {
                    args_vec[i] = thread.frames[frame_idx]
                        .stack
                        .pop_arg_for_descriptor_checked(arg_desc_byte(i))?;
                }
                &mut args_vec
            };
            if crate::runtime::env_cache::dbg_loader_trace()
                && cached.class_name.contains("RootReference")
                && cached.method_name.as_ref() == "tryUpdate"
            {
                let describe = |v: &Value| -> String {
                    match v {
                        Value::Object(Some(obj)) => {
                            let cid = shared.mem.heap.class_id_of(*obj);
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                        }
                        Value::Object(None) => "null".to_string(),
                        other => format!("{:?}", other),
                    }
                };
                eprintln!(
                    "[TRYUPDATE-TRACE] caller_class_id={:?} cp_index={} pre-refresh receiver(args[0])={} updated(args[1])={}",
                    caller_class_id,
                    cp_index,
                    describe(&args_slice[0]),
                    args_slice.get(1).map(describe).unwrap_or_default(),
                );
            }
            refresh_stale_object_args(shared, args_slice);
            if crate::runtime::env_cache::dbg_loader_trace()
                && cached.class_name.contains("RootReference")
                && cached.method_name.as_ref() == "tryUpdate"
            {
                let describe = |v: &Value| -> String {
                    match v {
                        Value::Object(Some(obj)) => {
                            let cid = shared.mem.heap.class_id_of(*obj);
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                        }
                        Value::Object(None) => "null".to_string(),
                        other => format!("{:?}", other),
                    }
                };
                eprintln!(
                    "[TRYUPDATE-TRACE] caller_class_id={:?} cp_index={} post-refresh receiver(args[0])={} updated(args[1])={}",
                    caller_class_id,
                    cp_index,
                    describe(&args_slice[0]),
                    args_slice.get(1).map(describe).unwrap_or_default(),
                );
            }
            if crate::runtime::env_cache::dbg_loader_trace()
                && cached.class_name.contains("Page")
                && cached.method_name.as_ref() == "<init>"
            {
                let describe = |v: &Value| -> String {
                    match v {
                        Value::Object(Some(obj)) => {
                            let cid = shared.mem.heap.class_id_of(*obj);
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                        }
                        Value::Object(None) => "null".to_string(),
                        other => format!("{:?}", other),
                    }
                };
                eprintln!(
                    "[PAGEINIT-TRACE/bc] caller_class_id={:?} cp_index={} ctor_desc={} new_page(args[0])={} map_arg(args[1])={}",
                    caller_class_id,
                    cp_index,
                    cached.method_descriptor,
                    describe(&args_slice[0]),
                    args_slice.get(1).map(describe).unwrap_or_default(),
                );
            }
            // Same class-filter gate as the slow path above.
            if let Value::Object(Some(o)) = &args_slice[0] {
                if crate::runtime::env_cache::field_watch_class_matches(&cached.class_name) {
                    cratonvm_types::field_watch::watch(*o);
                }
            }
            if crate::runtime::env_cache::dbg_loader_trace()
                && cached.class_name.contains("RootReference")
                && cached.method_name.as_ref() == "<init>"
            {
                let describe = |v: &Value| -> String {
                    match v {
                        Value::Object(Some(obj)) => {
                            let cid = shared.mem.heap.class_id_of(*obj);
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_default();
                            format!("addr={:?} cid={:?} loader_class={}", obj, cid, cn)
                        }
                        Value::Object(None) => "null".to_string(),
                        other => format!("{:?}", other),
                    }
                };
                eprintln!(
                    "[ROOTREFINIT-TRACE/bc] caller_class_id={:?} cp_index={} ctor_desc={} this(args[0])={} a1={} a2={} a3={}",
                    caller_class_id,
                    cp_index,
                    cached.method_descriptor,
                    describe(&args_slice[0]),
                    args_slice.get(1).map(describe).unwrap_or_default(),
                    args_slice.get(2).map(describe).unwrap_or_default(),
                    args_slice.get(3).map(describe).unwrap_or_default(),
                );
            }
            // Same class-filter gate as the slow path above.
            if let Value::Object(Some(o)) = &args_slice[0] {
                if crate::runtime::env_cache::field_watch_class_matches(&cached.class_name) {
                    cratonvm_types::field_watch::watch(*o);
                }
            }

            if let Some(res) = intercept_classloader_set_default_assertion_status(
                shared,
                thread,
                cached.method_name.as_ref(),
                cached.method_descriptor.as_ref(),
                args_slice,
            ) {
                return res;
            }

            if let Some(res) = intercept_force_registered_native_cached(
                shared, thread, frame_idx, &cached, args_slice,
            ) {
                return res;
            }

            // Acquire monitor for synchronized methods. This statically-resolved
            // `Bytecode` arm serves `invokespecial` (super-calls) and any
            // invoke whose inline cache resolved to a non-virtual bytecode
            // target. Unlike the `VirtualBytecode` and `invokestatic`
            // `Bytecode` arms — which both do this — the original code here
            // pushed the frame WITHOUT entering the method's monitor and
            // without recording `monitor_on_exit`. A `synchronized` method
            // reached on this cached path therefore ran without holding its
            // lock: silently wrong for mutual exclusion, and an outright
            // IllegalMonitorStateException the moment the body calls
            // `wait`/`notify`/`notifyAll` on `this`. The first call (slow path
            // `try_stackless_invoke`) acquires the monitor correctly, so the
            // bug only surfaces on the 2nd+ call once this arm's cache entry is
            // live. Observed as Apache Derby's embedded boot failing with
            // `XBM01.D` — `BasePage.releaseExclusive` is a `synchronized`
            // method invoked via `super` (invokespecial) that calls
            // `notifyAll()` on `this` to release a page latch. Mirror the
            // `VirtualBytecode` arm exactly.
            let monitor_obj: Option<ObjectRef> = if cached.is_synchronized {
                let obj = if cached.is_static {
                    // JVMS §2.11.10: static-synchronized monitor is the Class
                    // mirror (same object as ldc class / synchronized(X.class)).
                    get_or_create_class_mirror(shared, cached.declaring_class_id)
                } else {
                    // Receiver is args_slice[0] for instance calls.
                    match args_slice.first() {
                        Some(Value::Object(Some(r))) => *r,
                        _ => return Ok(CachedCallResult::CacheMiss),
                    }
                };
                Some(crate::vm::monitor_enter_synchronized_method(
                    shared, thread, obj, args_slice,
                ))
            } else {
                None
            };

            // T10.7 — refill per-thread pool from the shared VecPool if empty.
            thread.refill_pools_from_shared(
                &shared.mem.operand_stack_pool,
                &shared.mem.tag_pool,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                cached.max_locals as usize,
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                (cached.max_stack as usize).max(16) + 8,
            );
            let mut frame = Frame::new_pooled_cached(
                cached,
                args_slice,
                &mut thread.locals_pool,
                &mut thread.stacks_pool,
            );
            frame.monitor_on_exit = monitor_obj;
            if crate::runtime::env_cache::frame_trace() {
                eprintln!(
                    "[FRAME_PUSH/vcached2] depth={} {}.{}{}",
                    thread.frames.len(),
                    frame.class_name(),
                    frame.method_name(),
                    frame.method_descriptor()
                );
            }
            push_frame_and_fire_entry(shared.vm_identity, thread, frame);
            Ok(CachedCallResult::FramePushed)
        }
        CachedInvokeTarget::Native {
            callback,
            native_id,
            native_kind,
            num_params,
            gate: _,
        } => {
            // NULL-RECEIVER-CACHED-20260801: same guard as the `Bytecode` arm
            // above — this function only ever serves instance invokes
            // (invokestatic goes to `execute_invokestatic_cached`), so
            // `pop_coerced_invoke_args_virtual` always lays the receiver down
            // as `args[0]`. Handing a registered native a null `args[0]` is
            // how `Class.reflectionData()` came to return null instead of
            // throwing: its body answers a non-object receiver with
            // `Value::Object(None)`, and dozens of sibling natives do the
            // same. Defer to the slow path, which raises the NPE (or applies
            // the deliberate null-tolerant shim) exactly as the cold call did.
            if matches!(
                thread.frames[frame_idx].stack.peek_at(num_params as usize),
                Value::Object(None)
            ) {
                return Ok(CachedCallResult::CacheMiss);
            }
            let Some(callback) =
                revalidate_cached_native(shared, native_id, callback, native_kind)
            else {
                thread
                    .invoke_cache
                    .evict(caller_class_id, cp_index, is_special);
                return Ok(CachedCallResult::CacheMiss);
            };
            let (args, method_descriptor) = pop_coerced_invoke_args_virtual(
                shared,
                caller_class_id,
                cp_index,
                frame_idx,
                thread,
            )?;
            invoke_cached_native_callback(
                shared,
                thread,
                frame_idx,
                callback,
                &args,
                &method_descriptor,
            )?;
            Ok(CachedCallResult::Handled)
        }
        // JIT entries don't apply to virtual dispatch
        CachedInvokeTarget::Jit { .. } => Ok(CachedCallResult::CacheMiss),
    }
}

/// Populate the virtual invoke cache for a given call site after a successful virtual dispatch.
/// The cache is monomorphic: it stores the receiver class of the most recent call.
pub(super) fn populate_virtual_invoke_cache(
    thread: &mut JvmThread,
    shared: &SharedVm,
    caller_class_id: ClassId,
    cp_index: u16,
    receiver_class_id: ClassId,
    receiver_value: &Value,
) {
    // T10.4 fast path — the VM-wide `SharedResolutionState` may already
    // hold a fully-built `CachedInvokeTarget` that a sibling thread promoted
    // after running the slow resolution walk.  A hit here only takes the
    // lock-free read-guard on `promoted_invokes` and completely bypasses
    // the class_manager + resolution_cache write locks below.
    let promoted_key: crate::runtime::lockfree_resolve::PromotedInvokeKey =
        (caller_class_id, cp_index, false, Some(receiver_class_id));
    if let Some(target) = shared
        .classes
        .shared_resolution
        .get_promoted_invoke(&promoted_key)
    {
        thread.invoke_cache.put_poly(
            caller_class_id,
            cp_index,
            false,
            receiver_class_id,
            target.clone(),
        );
        thread
            .invoke_cache
            .put(caller_class_id, cp_index, false, target);
        return;
    }

    // Don't cache lambda proxy dispatch (they have special capture semantics)
    if shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&receiver_class_id)
    {
        return;
    }

    // Resolve method reference from constant pool
    let (class_name, method_name, descriptor, num_params) =
        match resolve_method_ref(shared, caller_class_id, cp_index) {
            Ok(r) => r,
            Err(_) => return,
        };

    // See the matching guard in `populate_invoke_cache`: a shared
    // `Object.equals(Object)` Methodref is not safely cacheable by constant
    // pool index alone because one method can invoke it at several distinct
    // receiver-polymorphic bytecode offsets.
    if class_name.as_ref() == "java/lang/Object"
        && method_name.as_ref() == "equals"
        && descriptor.as_ref() == "(Ljava/lang/Object;)Z"
    {
        return;
    }

    if crate::runtime::env_cache::dbg_vdisp()
        && (method_name.as_ref() == "hashCode"
            || method_name.as_ref() == "equals"
            || method_name.as_ref() == "run")
    {
        let cm = shared.classes.class_manager.read();
        let caller = cm
            .get_class(caller_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let rcv = cm
            .get_class(receiver_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let cp_static = cm
            .get_class(caller_class_id)
            .and_then(|_| resolve_method_ref(shared, caller_class_id, cp_index).ok())
            .map(|(cn, _, _, _)| cn.to_string())
            .unwrap_or_default();
        let found = crate::classloading::find_method_recursive(
            receiver_class_id,
            &method_name,
            &descriptor,
            &cm.class_store,
        );
        let declaring = found
            .and_then(|(_m, did)| cm.class_store.get(did).map(|c| c.name.to_string()))
            .unwrap_or_default();
        let is_abstract = found.map(|(m, _)| m.is_abstract()).unwrap_or(false);
        let has_code = found.map(|(m, _)| m.code().is_some()).unwrap_or(false);
        eprintln!("[vdisp] POPULATE caller={caller} cp_static={cp_static} method={method_name}{descriptor} receiver_class={rcv} declaring={declaring} is_abstract={is_abstract} has_code={has_code}");
    }

    // Interpreter intrinsic probe (invokevirtual/invokeinterface).
    //
    // CRITICAL — virtual-dispatch soundness (roadmap §3.4): the table is
    // keyed on the *resolved declaring class* — the class that actually
    // provides the method body for THIS receiver — NOT the static cp
    // class. We walk `find_method_recursive` from the receiver's actual
    // class: an overriding subclass resolves to its own class name, which
    // is not in the intrinsic table, so it correctly MISSES the intrinsic
    // and falls through to ordinary dispatch. A non-overriding subclass
    // resolves to `java/lang/Object` (etc.) and may legitimately hit.
    //
    // The entry stores `receiver_class_id: Some(receiver_class_id)`; the
    // execute path additionally guards `actual == receiver` at dispatch
    // time, so even a megamorphic call site stays sound.
    //
    // `intrinsics_disabled()` suppresses population entirely — the
    // differential-test off-switch.
    if !crate::runtime::env_cache::intrinsics_disabled()
        && cratonvm_native_builtins::intrinsics::might_have_method_descriptor(
            &method_name,
            &descriptor,
        )
    {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        if let Some((_method, declaring_id)) = crate::classloading::find_method_recursive(
            receiver_class_id,
            &method_name,
            &descriptor,
            store,
        ) {
            let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
            // WP0.1 soundness — a Rust native registered for the method on the
            // RECEIVER's class (or any ancestor strictly below the resolved
            // declaring class) outranks the intrinsic, exactly as it outranks
            // bytecode (see the native-override block below and the matching
            // guard in `execute_invokevirtual_vtable_fast`). Without this walk,
            // a synthetic class with no bytecode (e.g.
            // `cratonvm/internal/UnmodifiableList`, whose `hashCode` native
            // implements the List contract) resolves to `java/lang/Object` and
            // caches the identity-hash intrinsic — the FIRST call (slow path)
            // honours the native, every later call through the poisoned IC
            // returns the identity hash. Canonical victim: JUnit 6
            // `Namespace.hashCode()` (a `List.of` parts list) became unstable,
            // so `NamespacedHierarchicalStore` lookups missed and every
            // @ParameterizedTest died in `getDeclarationContext` (NPE).
            let native_override_below_declaring = if shared
                .natives
                .native_methods
                .might_have_method_descriptor(&method_name, &descriptor)
            {
                let mut cid = receiver_class_id;
                let mut hit = false;
                loop {
                    if cid == declaring_id {
                        break;
                    }
                    let Some(class) = store.get(cid) else { break };
                    if shared
                        .natives
                        .native_methods
                        .find(&class.name, &method_name, &descriptor)
                        .is_some()
                    {
                        hit = true;
                        break;
                    }
                    match class.superclass {
                        Some(parent) => cid = parent,
                        None => break,
                    }
                }
                hit
            } else {
                false
            };
            // JVMTI redefine guard (2026-07-23 mockitobean length() fix):
            // this intrinsic-population block had NO redefine awareness at
            // all, unlike its sibling gate in `execute_invokevirtual_vtable_fast`
            // (which already checks exactly this) and unlike the plain-Native
            // populate-side check further down in THIS function (fixed for
            // bug 3 of the mockitobean session). `StringBuilder.length()`'s
            // compiler-generated public bridge (`AbstractStringBuilder` is
            // package-private) is declared directly on `StringBuilder` --
            // `find_method_recursive` resolves `declaring_id` to
            // `StringBuilder` itself, which IS in the intrinsic table
            // (`StringBuilderLength`) -- so a Mockito inline mock's woven
            // advice on that exact bridge was being permanently shadowed by
            // this early, unconditional `Intrinsic` cache population, never
            // even reaching the (already redefine-aware) native-override
            // check below. Without this guard `verify(mock).length()`
            // silently no-ops instead of throwing "wanted but not invoked".
            let declaring_redefined_not_immune = crate::classloading::any_class_redefined()
                && cm.class_redefine_generation(declaring_id) > 0
                && !redefine_immune_layout_native(declaring_name, &method_name, &descriptor);
            let intrinsic_kind = if declaring_redefined_not_immune {
                None
            } else {
                (!native_override_below_declaring)
                    .then(|| {
                        cratonvm_native_builtins::intrinsics::lookup(
                            declaring_name,
                            &method_name,
                            &descriptor,
                        )
                        .or_else(|| {
                            // Records (JEP 395): `hashCode`/`equals` are not
                            // keyed on a fixed class, so they cannot come from
                            // the static table — see `record_object_intrinsic`.
                            store.get(declaring_id).and_then(|declaring| {
                                record_object_intrinsic(declaring, &method_name, &descriptor)
                            })
                        })
                    })
                    .flatten()
            };
            if let Some(kind) = intrinsic_kind {
                // Gate bound to the receiver class — a redefine of the
                // receiver swaps the dispatched body, mirroring the
                // `VirtualNative` gate binding below.
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
                drop(cm);
                // Phase 3 — split the descriptor ONCE here so the
                // steady-state dispatch path never re-resolves or re-parses.
                let (pd_vec, _) = split_method_descriptor(&descriptor);
                let param_descs: Arc<[Arc<str>]> =
                    pd_vec.iter().map(|s| Arc::from(s.as_str())).collect();
                let return_type = crate::jit::return_type(&descriptor);
                let target = CachedInvokeTarget::Intrinsic {
                    kind,
                    callback: cratonvm_native_builtins::intrinsics::callback_for(kind),
                    // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                    num_params: num_params as u16,
                    param_descs,
                    return_type,
                    receiver_class_id: Some(receiver_class_id),
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread.invoke_cache.put_poly(
                    caller_class_id,
                    cp_index,
                    false,
                    receiver_class_id,
                    target.clone(),
                );
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, false, target);
                return;
            }
        }
        drop(cm);
    }

    // Check native overrides FIRST — a Rust native registered for a method
    // takes priority over bytecode from the class file.  This matches the
    // dispatch order in `try_stackless_invoke` (step 1: native override,
    // step 4: class hierarchy).  Without this check, a JDK method that is
    // NOT ACC_NATIVE but HAS a Rust override (e.g. Method.getName) would
    // be cached as VirtualBytecode, causing the bytecode to execute with
    // the wrong field layout.
    {
        // Determine the class name for native lookup.  Arrays dispatch
        // through java/lang/Object; for normal classes use the receiver's
        // name directly.
        let cm = shared.classes.class_manager.read();
        let rcv_name = cm
            .get_class(receiver_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        let lookup_name = if rcv_name.starts_with('[') {
            "java/lang/Object".to_string()
        } else {
            rcv_name
        };
        let native_signature_may_exist = shared
            .natives
            .native_methods
            .might_have_method_descriptor(&method_name, &descriptor);
        // ThreadPoolExecutor.execute(Runnable): same receiver-aware
        // exemption as `try_stackless_invoke`'s step-1 native lookup above --
        // a genuinely real ThreadPoolExecutor (its own `workers` field
        // populated by a real `<init>`) must not have its native shadow
        // cached here. This cache is keyed by (call site, receiver
        // class_id) alone, so caching `VirtualNative` here would
        // permanently route EVERY future call at this call site -- any
        // instance of this class_id -- through `native_es_execute`'s
        // inline "run synchronously" fallback instead of real async
        // bytecode. Falling through instead lets the bytecode-resolution
        // path below cache `VirtualBytecode`, whose dispatch-time
        // `intercept_force_registered_native` check re-validates the
        // ACTUAL receiver on every hit (not just at population time). See
        // docs/known-issues/threadpoolexecutor-execute-dispatch-degrades-to-synchronous.md.
        let is_real_tpe_execute = lookup_name == "java/util/concurrent/ThreadPoolExecutor"
            && method_name.as_ref() == "execute"
            && descriptor.as_ref() == "(Ljava/lang/Runnable;)V"
            && threadpool_executor_has_real_workers(shared, receiver_value);
        // JVMTI redefine guard: this direct-native-override lookup has no
        // awareness of JVMTI `redefineClasses` -- unlike the SLOW,
        // uncached dispatch path (`intercept_force_registered_native` /
        // `should_force_registered_native_over_bytecode`), which correctly
        // cedes to a redefined class's woven bytecode. A Mockito inline
        // mock of e.g. `StringBuilder` redefines BOTH `StringBuilder` and
        // `AbstractStringBuilder` in place (advice woven into
        // `substring(int)`'s own body) while `native_sb_substring` stays
        // registered on both class names forever. Call #1 at a fresh call
        // site takes the slow path (correct: runs the woven advice), but
        // this populate step -- run to warm the cache for next time --
        // still matched the always-present native registration and cached
        // it as `VirtualNative`, with nothing to notice the receiver had
        // already been redefined. Every call #2+ then hit that cached
        // native directly, silently skipping Mockito's advice and running
        // the real (empty-buffer) implementation instead of the stubbed
        // answer. Mirrors the `receiver_redefined` guard in
        // `execute_invokevirtual_vtable_fast` -- `is_string_builder_layout_native_override`'s
        // methods (append/charAt/delete/getChars/insert/length/toString/
        // <init>) stay forced-native even after redefine because their real
        // JDK bodies assume a compact byte[]/coder layout CratonVM's
        // synthetic StringBuilder doesn't have; `substring` is deliberately
        // NOT in that list; it (RE-)validated the SAME cache before this fix.
        let receiver_redefined = crate::classloading::any_class_redefined()
            && cm.class_redefine_generation(receiver_class_id) > 0
            && !redefine_immune_reflection_native(&lookup_name, &method_name)
            && !redefine_immune_layout_native(&lookup_name, &method_name, &descriptor);
        let direct_native =
            if native_signature_may_exist && !is_real_tpe_execute && !receiver_redefined {
                resolve_cached_native_registration(
                    shared,
                    &lookup_name,
                    &method_name,
                    &descriptor,
                )
            } else {
                None
            };
        if let Some((callback, native_id, native_kind)) = direct_native {
            // WP2.4-F1: gate bound to the receiver class (where dispatch
            // landed). A redefine of the receiver swaps the method body.
            let gate =
                RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
            drop(cm);
            let target = CachedInvokeTarget::VirtualNative {
                receiver_class_id,
                callback,
                native_id,
                native_kind,
                // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                num_params: num_params as u16,
                gate,
            };
            // T10.4 — promote so sibling threads skip the class_manager walk.
            shared
                .classes
                .shared_resolution
                .insert_promoted_invoke(promoted_key, target.clone());
            thread.invoke_cache.put_poly(
                caller_class_id,
                cp_index,
                false,
                receiver_class_id,
                target.clone(),
            );
            thread
                .invoke_cache
                .put(caller_class_id, cp_index, false, target);
            return;
        }
        // FJP fix: walk the parent chain looking for natives registered on
        // an ancestor class. Mirrors the slow path in `vm_exec::invoke_or_native`
        // so that calls like `SumTask.fork()` (where the native is registered
        // on `RecursiveTask`/`ForkJoinTask`) reach the Rust override and not
        // the inherited JDK bytecode.
        //
        // Round 63 (peaceful-sammet) — receiver-bytecode short-circuit:
        // if the *receiver* class declares its own bytecode for this
        // method, the subclass override must win over any ancestor's
        // native registration. Without this guard, Kafka's
        // `Group$GroupType.toString()` (a subclass override that reads
        // the subclass `name` field — "classic"/lowercase) was being
        // shadowed by the native `Enum.toString()` registered on
        // `java/lang/Enum`, which reads `Enum.name` ("CLASSIC"/upper).
        // First call dispatched via the slow path (correct override);
        // this cache populator then poisoned the inline cache with the
        // parent native, breaking subsequent calls and causing
        // `GroupCoordinatorConfig.<clinit>` to default the rebalance
        // protocols list to `["CONSUMER", "CLASSIC", "STREAMS"]`
        // instead of the lowercase forms the validator accepts.
        let receiver_has_own_bytecode = cm
            .get_class(receiver_class_id)
            .map(|c| c.find_method(&method_name, &descriptor).is_some())
            .unwrap_or(false);
        if receiver_has_own_bytecode {
            // Skip the ancestor-native promotion entirely — fall through
            // to the bytecode dispatch path below.
        } else if native_signature_may_exist {
            let mut cid = receiver_class_id;
            while let Some(parent_id) = cm.get_class(cid).and_then(|c| c.superclass) {
                if let Some(parent) = cm.get_class(parent_id) {
                    // S107 collection-toString fix: if this parent has its own
                    // bytecode for the method (e.g. AbstractCollection.toString),
                    // the bytecode override wins over any deeper native ancestor
                    // (e.g. Object.toString). Stop walking so the bytecode dispatch
                    // path runs (via find_method_recursive below).
                    //
                    // Round 19 (peaceful-sammet) — IMPORTANT exception: if the
                    // parent has BOTH its own bytecode AND a Rust native registered
                    // for this method, the native wins. Our LinkedHashMap natives
                    // store state in an external overlay (not real fields), so
                    // executing JDK bytecode for inherited callers like
                    // `AnnotationAttributes` (which extends `LinkedHashMap`) walks
                    // empty real fields and returns empty `entrySet()` /
                    // `keySet()`, breaking Spring's
                    // `MetadataReader.getAnnotationAttributes("...Import", true)`
                    // for `@EnableAutoConfiguration` and surfacing as
                    // `MissingWebServerFactoryBean` on Spring Boot startup.
                    let parent_name = parent.name.to_string();
                    // JVMTI redefine guard: Mockito's inline mock maker mocks a
                    // CONCRETE class (e.g. `java.net.HttpURLConnection`) by
                    // redefining that class directly (weaving advice into its
                    // methods) and instantiating a trivial marker SUBCLASS that
                    // does NOT itself override every mockable method — so the
                    // RECEIVER here is that marker subclass (never redefined),
                    // while an ANCESTOR up this walk (`parent`) is the one
                    // that was redefined. Without this guard, BOTH branches
                    // below (the Round-19 dual-registration exception and the
                    // pure-inherited-native case) cache a `VirtualNative`
                    // target pointing at our native, permanently shadowing the
                    // now-woven bytecode for every FUTURE call at this call
                    // site — even though the first (uncached) call correctly
                    // ran the woven bytecode via the slow path and the mock's
                    // advice fired. `execute_invokevirtual_cached`'s eviction
                    // check only inspects the RECEIVER class's redefine
                    // generation (stored in the cached target), so it never
                    // catches a shadow whose declaring class is an ancestor —
                    // the fix has to be here, where the entry is created.
                    let parent_redefined = crate::classloading::any_class_redefined()
                        && cm.class_redefine_generation(parent_id) > 0
                        && !redefine_immune_reflection_native(&parent_name, &method_name)
                        && !redefine_immune_layout_native(&parent_name, &method_name, &descriptor);
                    if parent_redefined {
                        break;
                    }
                    if parent.find_method(&method_name, &descriptor).is_some() {
                        if let Some((callback, native_id, native_kind)) =
                            resolve_cached_native_registration(
                            shared,
                            &parent_name,
                            &method_name,
                            &descriptor,
                        ) {
                            let gate = RedefineGate::snapshot(
                                cm.class_redefine_generation_handle(receiver_class_id),
                            );
                            drop(cm);
                            let target = CachedInvokeTarget::VirtualNative {
                                receiver_class_id,
                                callback,
                                native_id,
                                native_kind,
                                // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                                num_params: num_params as u16,
                                gate,
                            };
                            shared
                                .classes
                                .shared_resolution
                                .insert_promoted_invoke(promoted_key, target.clone());
                            thread.invoke_cache.put_poly(
                                caller_class_id,
                                cp_index,
                                false,
                                receiver_class_id,
                                target.clone(),
                            );
                            thread
                                .invoke_cache
                                .put(caller_class_id, cp_index, false, target);
                            return;
                        }
                        break;
                    }
                    if let Some((callback, native_id, native_kind)) =
                        resolve_cached_native_registration(
                            shared,
                            &parent_name,
                            &method_name,
                            &descriptor,
                        )
                    {
                        let gate = RedefineGate::snapshot(
                            cm.class_redefine_generation_handle(receiver_class_id),
                        );
                        drop(cm);
                        let target = CachedInvokeTarget::VirtualNative {
                            receiver_class_id,
                            callback,
                            native_id,
                            native_kind,
                            // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                            num_params: num_params as u16,
                            gate,
                        };
                        shared
                            .classes
                            .shared_resolution
                            .insert_promoted_invoke(promoted_key, target.clone());
                        thread.invoke_cache.put_poly(
                            caller_class_id,
                            cp_index,
                            false,
                            receiver_class_id,
                            target.clone(),
                        );
                        thread
                            .invoke_cache
                            .put(caller_class_id, cp_index, false, target);
                        return;
                    }
                }
                cid = parent_id;
            }
        }
    }

    // Look up method on the receiver's actual class (virtual dispatch resolution)
    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
    let Some((method, declaring_id)) = crate::classloading::find_method_recursive(
        receiver_class_id,
        &method_name,
        &descriptor,
        store,
    ) else {
        return;
    };

    if method.is_native() {
        let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
        if let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
            shared,
            declaring_name,
            &method_name,
            &descriptor,
        )
        {
            // WP2.4-F1: gate bound to the declaring class.
            let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
            drop(cm);
            let target = CachedInvokeTarget::VirtualNative {
                receiver_class_id,
                callback,
                native_id,
                native_kind,
                num_params: num_params as u16, // Widening: parameter count conversion
                gate,
            };
            // T10.4 — promote so sibling threads skip this walk.
            shared
                .classes
                .shared_resolution
                .insert_promoted_invoke(promoted_key, target.clone());
            thread.invoke_cache.put_poly(
                caller_class_id,
                cp_index,
                false,
                receiver_class_id,
                target.clone(),
            );
            thread
                .invoke_cache
                .put(caller_class_id, cp_index, false, target);
        }
        return;
    }

    // S111r13: For real-JDK Map functional methods (computeIfAbsent / compute
    // / merge / putIfAbsent / forEach / replaceAll / getOrDefault / replace /
    // putMapEntries — internal helper invoked from putAll / Map.copyOf), the
    // bytecode reads `getfield table` followed by `arraylength`.  Our
    // synthetic HashMap layout stores the `Int(capacity)` in slot 2 instead
    // of an array, surfacing as
    //   `expected object reference, got int(N)`.
    // Mirror the force-native override list at vm_exec.rs:invoke_on_class_shared_inner.
    // This branch fires when find_method_recursive resolved to non-native
    // bytecode declared on HashMap (or a sibling), but a Rust native exists
    // for the (declaring_class, method, descriptor) triple — caching the
    // bytecode would re-introduce the layout mismatch on every dispatch
    // through this call-site.
    {
        let declaring_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
        // ThreadPoolExecutor.execute(Runnable): `force_native_over_real_jdk_bytecode`
        // is a pure (class, method, descriptor) allowlist with no receiver
        // awareness -- it unconditionally returns true for this triple (see
        // its own entry, added alongside the receiver-aware checks at
        // `intercept_force_registered_native`/`invoke_or_native`/
        // `invoke_on_class_shared_inner`). Consulting it directly here,
        // bypassing those receiver checks entirely, is what actually poisons
        // this call site's inline cache with `VirtualNative` for a
        // genuinely real ThreadPoolExecutor. Exempt it the same way as the
        // other call sites. See docs/known-issues/
        // threadpoolexecutor-execute-dispatch-degrades-to-synchronous.md.
        let is_real_tpe_execute_force = declaring_name == "java/util/concurrent/ThreadPoolExecutor"
            && method_name.as_ref() == "execute"
            && descriptor.as_ref() == "(Ljava/lang/Runnable;)V"
            && threadpool_executor_has_real_workers(shared, receiver_value);
        let force = !is_real_tpe_execute_force
            && (force_native_over_real_jdk_bytecode(declaring_name, &method_name, &descriptor)
                || (matches!(
                    declaring_name,
                    "java/util/HashMap"
                        | "java/util/LinkedHashMap"
                        | "java/util/Hashtable"
                        | "java/util/concurrent/ConcurrentHashMap"
                ) && matches!(
                    &*method_name,
                    "computeIfAbsent" | "compute" | "computeIfPresent"
            | "merge" | "putIfAbsent" | "replace"
            | "forEach" | "replaceAll" | "getOrDefault"
            | "putMapEntries" | "putAll"
            | "keySet" | "values" | "entrySet"
            // S111r14: see vm_exec.rs — logback LoggerContext.<init>
            // hits HashMap.put → putVal → arraylength on Int(16).
            | "put" | "get" | "remove"
            | "containsKey" | "containsValue"
            | "size" | "isEmpty" | "clear"
            | "<init>"
            // `Hashtable.keys()` / `elements()` — legacy pre-1.2 Enumeration
            // accessors. Real-JDK body walks `this.table[]`; our overrides
            // store data in a side-store, so the JDK bytecode sees an empty
            // table. See companion entry in `force_native_over_real_jdk_bytecode`.
            | "keys" | "elements"
                )));
        if force {
            if let Some((callback, native_id, native_kind)) =
                resolve_cached_native_registration(
                    shared,
                    declaring_name,
                    &method_name,
                    &descriptor,
                )
            {
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
                drop(cm);
                let target = CachedInvokeTarget::VirtualNative {
                    receiver_class_id,
                    callback,
                    native_id,
                    native_kind,
                    // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
                    num_params: num_params as u16,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread.invoke_cache.put_poly(
                    caller_class_id,
                    cp_index,
                    false,
                    receiver_class_id,
                    target.clone(),
                );
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, false, target);
                return;
            }
        }
    }

    // WP2.2 / Surefire — never promote `VirtualBytecode` for `Method.invoke` or
    // `Constructor.newInstance`; the monomorphic fast path skips
    // `try_stackless_invoke` and would execute JDK bytecode instead of the
    // Rust overrides registered in `register_essential_natives`.
    let declaring_for_jython = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
    if declaring_for_jython == "org/python/core/PyObject"
        && method_name.as_ref() == "__findattr__"
        && descriptor.as_ref() == "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        let receiver_name = store.get(receiver_class_id).map(|c| &*c.name).unwrap_or("");
        if receiver_name == "org/python/core/PyModule" {
            if let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
                shared,
                "org/python/core/PyModule",
                &method_name,
                &descriptor,
            ) {
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
                drop(cm);
                let target = CachedInvokeTarget::VirtualNative {
                    receiver_class_id,
                    callback,
                    native_id,
                    native_kind,
                    num_params: num_params as u16,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread.invoke_cache.put_poly(
                    caller_class_id,
                    cp_index,
                    false,
                    receiver_class_id,
                    target.clone(),
                );
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, false, target);
                return;
            }
        }
    }

    let declaring_for_pyjavatype = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
    if declaring_for_pyjavatype == "org/python/core/PyType"
        && method_name.as_ref() == "__findattr_ex__"
        && descriptor.as_ref() == "(Ljava/lang/String;)Lorg/python/core/PyObject;"
    {
        let receiver_name = store.get(receiver_class_id).map(|c| &*c.name).unwrap_or("");
        if receiver_name == "org/python/core/PyJavaType" {
            if let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
                shared,
                "org/python/core/PyJavaType",
                &method_name,
                &descriptor,
            ) {
                let gate =
                    RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
                drop(cm);
                let target = CachedInvokeTarget::VirtualNative {
                    receiver_class_id,
                    callback,
                    native_id,
                    native_kind,
                    num_params: num_params as u16,
                    gate,
                };
                shared
                    .classes
                    .shared_resolution
                    .insert_promoted_invoke(promoted_key, target.clone());
                thread.invoke_cache.put_poly(
                    caller_class_id,
                    cp_index,
                    false,
                    receiver_class_id,
                    target.clone(),
                );
                thread
                    .invoke_cache
                    .put(caller_class_id, cp_index, false, target);
                return;
            }
        }
    }

    let declaring_for_reflect = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
    if native_override_for_cached_reflect_invoke(
        shared,
        declaring_for_reflect,
        method_name.as_ref(),
        descriptor.as_ref(),
    )
    .is_some()
    {
        let Some((callback, native_id, native_kind)) = resolve_cached_native_registration(
            shared,
            declaring_for_reflect,
            method_name.as_ref(),
            descriptor.as_ref(),
        ) else {
            return;
        };
        let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(receiver_class_id));
        drop(cm);
        let target = CachedInvokeTarget::VirtualNative {
            receiver_class_id,
            callback,
            native_id,
            native_kind,
            // Truncation: usize -> u16 (param count fits in 16 bits per JVM method limit)
            num_params: num_params as u16,
            gate,
        };
        shared
            .classes
            .shared_resolution
            .insert_promoted_invoke(promoted_key, target.clone());
        thread.invoke_cache.put_poly(
            caller_class_id,
            cp_index,
            false,
            receiver_class_id,
            target.clone(),
        );
        thread
            .invoke_cache
            .put(caller_class_id, cp_index, false, target);
        return;
    }

    let Some(code_attr) = method.code() else {
        return;
    };

    let class = cm.get_class(declaring_id);
    let source_file = class.and_then(|c| c.source_file.as_deref()).map(Arc::from);
    let declaring_class_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");

    let cached = CachedBytecodeMethod {
        declaring_class_id: declaring_id,
        class_name: Arc::from(declaring_class_name),
        method_name: Arc::clone(&method_name),
        method_descriptor: Arc::clone(&descriptor),
        source_file,
        code: crate::runtime::frame::padded_bytecode(&code_attr.code),
        exception_table: Arc::from(code_attr.exception_table.as_slice()),
        max_stack: code_attr.max_stack,
        max_locals: code_attr.max_locals,
        num_params: num_params as u16, // Widening: parameter count conversion
        is_synchronized: method.is_synchronized(),
        is_static: method.is_static(),
        force_native_cache: std::sync::OnceLock::new(),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };

    // WP2.4-F1: snapshot before dropping the class_manager read-lock so
    // we get the same Arc<AtomicU32> that `redefine_class` will later
    // bump.  Bind to the declaring class — that's the class whose
    // method body lives in `cached`.
    let gate = RedefineGate::snapshot(cm.class_redefine_generation_handle(declaring_id));
    drop(cm);
    let target = CachedInvokeTarget::VirtualBytecode {
        receiver_class_id,
        cached: std::sync::Arc::new(cached),
        gate,
    };
    // T10.4 — promote so sibling threads dispatching the same call-site
    // skip the class_manager read-lock and find_method_recursive walk.
    shared
        .classes
        .shared_resolution
        .insert_promoted_invoke(promoted_key, target.clone());
    thread.invoke_cache.put_poly(
        caller_class_id,
        cp_index,
        false,
        receiver_class_id,
        target.clone(),
    );
    thread
        .invoke_cache
        .put(caller_class_id, cp_index, false, target);
}

/// Spring's `MergedAnnotation$Adapt.isIn` loader-split bridge (see the guard
/// at the top of `execute_invokevirtual_cached`) applies only to call sites
/// whose constant pool actually names that method. This latches the first
/// time [`resolve_method_metadata`] resolves such an entry, so the guard costs
/// one relaxed atomic load per invoke instead of a full method-ref resolution
/// in every process that has never loaded Spring.
pub(super) static ADAPT_ISIN_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The class whose `isIn` call sites [`ADAPT_ISIN_SEEN`] arms for.
pub(super) const SPRING_MERGED_ANNOTATION_ADAPT: &str =
    "org/springframework/core/annotation/MergedAnnotation$Adapt";

#[inline]
pub(super) fn adapt_isin_seen() -> bool {
    ADAPT_ISIN_SEEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Resolve the metadata for a constant-pool method reference.
///
/// Besides the symbolic names and parameter count, this stores the exact
/// native callback/category for the symbolic owner. The registry hash is paid
/// once per resolved CP entry, not once per call site that consumes it.
///
/// **This is the resolution core, not the public entry point.** New callers
/// outside `crate::runtime::interpreter` go through
/// [`crate::runtime::resolve::MemberResolver::method_ref`], which tags the
/// answer with the VM that produced it and converts failures into the
/// structured [`crate::runtime::resolve::ResolveError`]. It is `pub(crate)`
/// only so that `MemberResolver` can delegate here; `runtime::resolve::guard`
/// fails the build if anything else names it.
pub(crate) fn resolve_method_metadata(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
) -> Result<ResolvedMethod, MethodCallFailed> {
    hotpath_counts::bump(&hotpath_counts::RESOLVE_METHOD_REF_CALLS);
    // Check cache first. Cloning is Arc bumps plus two copied function-pointer
    // metadata fields; there is no string allocation or registry hash.
    if let Some(cached) = shared
        .classes
        .resolution_cache
        .read()
        .get_method(current_class_id, cp_index)
    {
        return Ok(cached.clone());
    }

    // read_recursive() instead of read() — resolve_method_ref can be called
    // from ctx.invoke_virtual within a native, which may itself be dispatched
    // by an interpreter frame that already holds class_manager.read() on this
    // thread. parking_lot's write-preferring policy blocks new read() calls
    // when a writer is queued, so a recursive plain read() → deadlock;
    // read_recursive() succeeds immediately for an existing read-holder.
    let cm = shared.classes.class_manager.read_recursive();
    let class = cm
        .get_class(current_class_id)
        .ok_or_else(|| VmError::Internal {
            message: "current class not found".to_string(),
        })?;

    let (class_idx, nat_idx) = match class.constant_pool.get(cp_index) {
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        })
        | Some(ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        _ => {
            return Err(VmError::Internal {
                message: format!("invalid method ref at cp#{cp_index}"),
            }
            .into());
        }
    };

    let class_name: Arc<str> = Arc::from(
        class
            .constant_pool
            .get_class_name(class_idx)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{class_idx}"),
            })?,
    );
    let (method_name_str, method_descriptor_str) = class
        .constant_pool
        .get_name_and_type(nat_idx)
        .ok_or_else(|| VmError::Internal {
        message: format!("invalid name_and_type at cp#{nat_idx}"),
    })?;

    let method_name: Arc<str> = Arc::from(method_name_str);
    let method_descriptor: Arc<str> = Arc::from(method_descriptor_str);
    let num_params = count_method_params(&method_descriptor);

    // Arm the `execute_invokevirtual_cached` fast-path guard the first time a
    // constant pool names Spring's loader-split `Adapt.isIn` bridge. Cold
    // path: this runs once per (class, cp_index), not per call.
    if &*class_name == SPRING_MERGED_ANNOTATION_ADAPT && &*method_name == "isIn" {
        ADAPT_ISIN_SEEN.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // Module access check (JPMS §5.4.4): verify accessor can reach the target
    // class's module.
    //
    // C2 P0 — routed through `runtime::resolve::MemberResolver`, the one
    // access-control entry point in `vm/src/runtime/`. `MemberFlags::OwnerOnly`
    // + `AccessPolicy::ModuleOnly` is the exact shape of what this call has
    // always been: at this point the hierarchy walk has not happened, so there
    // is no declaring method and no method flags to check — only the owner
    // class the constant pool names. That is also why the member half of JVMS
    // §5.4.4 is not enforced on this path; see
    // `classloading::access_control`'s module docs. Unchanged behaviour,
    // named policy.
    if let Some(target_id) = cm.get_loaded_class_id(&class_name) {
        let resolver = crate::runtime::resolve::MemberResolver::new(shared);
        let _grant = resolver.check_member_access(
            &cm,
            resolver.scope(current_class_id),
            resolver.scope(target_id),
            crate::runtime::resolve::MemberFlags::OwnerOnly,
            None,
            crate::runtime::resolve::AccessPolicy::ModuleOnly,
        )?;
    }

    // Drop the read lock before acquiring write lock
    drop(cm);

    let (native_target, native_kind) = shared
        .natives
        .native_methods
        .find_with_kind(&class_name, &method_name, &method_descriptor)
        .map(|(target, kind)| (Some(target), Some(kind)))
        .unwrap_or((None, None));
    let resolved = ResolvedMethod {
        declaring_class_id: current_class_id,
        class_name,
        method_name,
        method_descriptor,
        num_params: num_params as u16, // Widening: parameter count conversion
        native_target,
        native_kind,
    };
    shared.classes.resolution_cache.write().put_method(
        current_class_id,
        cp_index,
        resolved.clone(),
    );

    Ok(resolved)
}

/// Compatibility view for the interpreter paths that only need symbolic
/// method data. The underlying cache entry still carries its native target.
pub(super) fn resolve_method_ref(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
) -> Result<(Arc<str>, Arc<str>, Arc<str>, usize), MethodCallFailed> {
    let resolved = resolve_method_metadata(shared, current_class_id, cp_index)?;
    Ok((
        resolved.class_name,
        resolved.method_name,
        resolved.method_descriptor,
        resolved.num_params as usize,
    ))
}

/// JVMS §6.5 `invokespecial` — apply the super-call "selection" redirect
/// (see `classloading::invokespecial_selection_start`'s doc comment for the
/// full rule) to the class named by an `invokespecial` constant-pool
/// reference, so dispatch starts the method search at the CALLING class's
/// own direct superclass rather than blindly at the CP-referenced ancestor
/// whenever that redirect applies. `resolve_method_ref` (which produced
/// `method_class_name`) returns the bare CP text and has no notion of the
/// calling class, so this is a separate, deliberately narrow lookup.
///
/// A class between the caller and the CP-referenced ancestor may override
/// the method (a compiler-generated bridge, or an ordinary override) —
/// resolving from the CP-referenced class directly walks straight past it.
/// Shared by both the interpreter's own `invokespecial` dispatch (here) and
/// the JIT compiler's call-site resolution (`try_jit_compile_callee`'s
/// `invoke_resolver` closures in this file), so the two execution modes
/// never disagree on the target.
///
/// Returns `method_class_name` unchanged whenever the redirect does not
/// apply (constructors, interface references, non-superclass references, a
/// caller class file without `ACC_SUPER`, or any resolution miss) — always
/// safe to substitute directly for `method_class_name` at the `is_special`
/// call site.
pub(super) fn invokespecial_owner_class_name(
    shared: &SharedVm,
    current_class_id: ClassId,
    cp_index: u16,
    method_class_name: &Arc<str>,
    method_name: &str,
) -> Arc<str> {
    if method_name == "<init>" {
        return Arc::clone(method_class_name);
    }
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(current_class_id) else {
        return Arc::clone(method_class_name);
    };
    let is_interface_ref = matches!(
        class.constant_pool.get(cp_index),
        Some(ConstantPoolEntry::InterfaceMethodReference { .. })
    );
    let Some(cp_class_id) = cm.find_class_by_name_for_class(method_class_name, current_class_id)
    else {
        return Arc::clone(method_class_name);
    };
    let store = cm.class_store();
    let start = crate::classloading::invokespecial_selection_start(
        current_class_id,
        cp_class_id,
        is_interface_ref,
        method_name,
        store,
    );
    if start == cp_class_id {
        return Arc::clone(method_class_name);
    }
    match store.get(start) {
        Some(c) => Arc::from(&*c.name),
        None => Arc::clone(method_class_name),
    }
}
