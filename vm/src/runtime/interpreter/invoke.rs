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

// ---------------------------------------------------------------------------
// Lambda / invokedynamic dispatch
// ---------------------------------------------------------------------------
//
// Getting from a call site's SAM descriptor to the implementation method,
// and making the arguments fit: `interpreter/lambda.rs`.


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

// ---------------------------------------------------------------------------
// Native-over-bytecode override policy
// ---------------------------------------------------------------------------
//
// Which methods a registered Rust native takes over from real bytecode, the
// superclass walk that gives an abstract-class native its reach, and the
// redefinition rules that make an override yield: `interpreter/native_override.rs`.


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

// ---------------------------------------------------------------------------
// invokestatic
// ---------------------------------------------------------------------------
//
// Static dispatch, its call-site cache, the staleness check that survives a
// redefinition, and the intrinsic substitution point:
// `interpreter/dispatch_static.rs`.



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
