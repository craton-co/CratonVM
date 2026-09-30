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

use super::site_cache::{
    cast_memo_epoch_in, site_stats, ArrayReceiverKey, CastSite, CastSiteCache, ClassSiteCache,
    ResolvedNewSite,
};
use super::*;
use crate::runtime::exceptions::helpful_npe::ArrayElemKind;

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
/// `checkcast`/`instanceof` target cache (and with it the `anewarray`
/// component lookup that shares the table, and the receiver memo).
///
/// Default-ON, and gated the same way `new_site_cache_enabled` is, including the
/// diagnostic exclusions: those tracers print the resolved class NAME on every
/// execution, and a cache hit never materializes one, so a hit would silence
/// them. A lever that quietly blinds a diagnostic is worse than one that costs
/// a lock.
pub(super) fn cast_site_cache_enabled() -> bool {
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
/// The first half of [`site_fill_admitted`], the admission test for
/// [`ClassSiteCache`] and the cast table. `true` means
/// `resolve_class_loader_aware` may consult the initiating-resolution memo or
/// drive a user `loadClass`, and the site is admitted only for an answer the
/// loader recorded (wave 24; refused outright before). `false` means the answer
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
pub(super) fn referencing_class_has_loader_namespace(shared: &SharedVm, class_id: ClassId) -> bool {
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

/// May a per-thread site table (`new`, `anewarray`, `checkcast` /
/// `instanceof`) of `referencing_class_id` remember that its constant `name`
/// resolved to `id`? Asked once, at the fill, after the resolution and the
/// §5.4.4 access check succeeded.
///
/// Interpreter round i1 wave 24 (lane L5), stage 1 of
/// `i1-L2-proposal-per-class-resolved-constant-pool`. Before it, a
/// referencing class with a loader namespace
/// ([`referencing_class_has_loader_namespace`]) was refused outright, so every
/// `new` / cast / `anewarray` in code defined by a user loader (webapps, H2 or
/// Spring under a URL loader, Groovy scripts) re-ran
/// `resolve_class_loader_aware` (two to four `class_manager` reads and the
/// initiating-memo probe) and the access check on EVERY execution.
///
/// Admitted now when the answer is one the entry's two epochs (`NameEpochs`:
/// the class-name generation and this VM's resolution epoch) cover:
///
/// * no loader namespace: the global name mapping, as before;
/// * a JDK-global name (`is_global_resolution_namespace`): the resolver answers
///   it from the same global mapping whatever the referencing loader, or (wave
///   27) from the loader's own definition of it / its memoised `loadClass`
///   answer after a global miss — a definition moves the name generation and
///   a memo write the resolution epoch, so the two epochs still cover it;
/// * any other non-array name: only when the loader's OWN record gives this
///   answer — the lookup `resolve_class_loader_aware` consults first
///   ([`loader_recorded_answer`]). That lookup changes only when the loader
///   defines the name itself (a second definition of a mapped name: the name
///   generation moves) or its initiating memo is written or swept
///   (`cache_loader_initiated` and the unload sweep bump the resolution
///   epoch). An answer that came from the global fallback after the loader
///   declined has no record and is never remembered, which is the concern
///   the proposal's wave-2 note raised.
///
/// Array names from a namespaced class stay refused (their resolution has its
/// own loader-faithful arm).
fn site_fill_admitted(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    id: ClassId,
) -> bool {
    if !referencing_class_has_loader_namespace(shared, referencing_class_id) {
        return true;
    }
    if name.starts_with('[') {
        return namespaced_array_answer_admitted(shared, thread, referencing_class_id, name, id);
    }
    if super::constants::is_global_resolution_namespace(name) {
        return true;
    }
    loader_recorded_answer(shared, thread, referencing_class_id, name) == Some(id)
}

/// `CRATONVM_JIT_CAST_SITE_NAMESPACED_ARRAYS` (default on; `=0` restores the
/// refusal): [`namespaced_array_answer_admitted`].
/// Read at a fill, for a covered answer only; never on a hit.
fn namespaced_array_sites_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CAST_SITE_NAMESPACED_ARRAYS")
}

/// An ARRAY name resolved from a loader-namespaced class (proposal
/// `interpreter-L5-proposal-a-loaders-array-class-answers-are-recorded-FIXED-20261007`, interpreter
/// round i1 wave 43, lane L5). Before it every such site was refused, so each
/// execution of `checkcast [Ljava/lang/String;` in a user loader's class
/// re-resolved the array and, for an element another loader defined, took the
/// class-manager WRITE lock (`load_array_class_for_loader`).
///
/// An array class is a function of its bottom element class (the array is
/// keyed by the element's defining loader, JVMS §5.3.3, and is never
/// replaced while the element lives), so the array answer is covered by the
/// entry's two epochs exactly when the element's answer is:
///
/// * a primitive bottom element: one class per VM;
/// * a `java/` element: no user loader can define one, so every loader's
///   answer is the one global class;
/// * any other element: only when it is the loader's OWN record of the name
///   ([`loader_recorded_answer`], the plain-name rule of
///   [`site_fill_admitted`]). The writers that change that record are the
///   ones that move the epochs: a definition of the name (the class-name
///   generation), the initiating memo's write -- its capped eviction included
///   -- and the unload sweep (`cache_loader_initiated`, `memory::gc`; the
///   resolution epoch). An element answered by the global fallback after the
///   loader declined has no record and is refused, as a plain name is.
///
/// The array `id` itself is checked against that element: its component
/// chain (`array_info`) must end at it.
fn namespaced_array_answer_admitted(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    id: ClassId,
) -> bool {
    // The switch is read last, so a shape that is refused anyway (and so
    // re-resolves on every execution) never reads it.
    namespaced_array_answer_covered(shared, thread, referencing_class_id, name, id)
        && namespaced_array_sites_enabled()
}

/// The coverage test of [`namespaced_array_answer_admitted`].
fn namespaced_array_answer_covered(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
    id: ClassId,
) -> bool {
    let element = name.trim_start_matches('[');
    let dims = name.len() - element.len();
    if element.len() == 1 {
        return true;
    }
    let Some(leaf) = element.strip_prefix('L').and_then(|e| e.strip_suffix(';')) else {
        return false;
    };
    let recorded = if leaf.starts_with("java/") {
        None
    } else {
        match loader_recorded_answer(shared, thread, referencing_class_id, leaf) {
            Some(recorded) => Some(recorded),
            None => return false,
        }
    };
    let cm = shared.classes.class_manager.read();
    let mut current = id;
    for _ in 0..dims {
        match cm.get_class(current).and_then(|c| c.array_info.as_ref()) {
            Some(info) => current = info.component_class_id,
            None => return false,
        }
    }
    match recorded {
        Some(recorded) => current == recorded,
        None => cm.get_class(current).is_some_and(|c| &*c.name == leaf),
    }
}

/// What `referencing_class_id`'s loader has itself recorded for `name`, by the
/// lookup `resolve_class_loader_aware`'s step (1) makes for a namespaced
/// class: exact definitions only for an isolated URL loader, else definitions
/// and the initiating memo. Read-only: no `loadClass` runs.
fn loader_recorded_answer(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    name: &str,
) -> Option<ClassId> {
    let isolated = cratonvm_native_builtins::classloader::defining_loader_for(
        shared.vm_identity,
        referencing_class_id.as_u32(),
    )
    .is_some()
        && super::constants::is_isolated_url_loader_definition(
            shared,
            thread,
            referencing_class_id,
        );
    if isolated {
        super::constants::lookup_loader_defined_exact(shared, referencing_class_id, name)
    } else {
        super::constants::lookup_loader_initiated(shared, referencing_class_id, name)
    }
}

/// `CRATONVM_DBG_IMSE` — the `athrow` read-hold autopsy
/// (`dump_imse_holdcount_state`). Read once: `athrow` used to call
/// `runtime_var_os` on EVERY throw, which is a declared-name set probe and,
/// for an undeclared name, a process-environment read — paid by every
/// exception a program throws, with the gate off.
fn dbg_imse_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IMSE").is_some())
}

/// JEP 358 action text for a `monitorexit` on null. `helpful_npe` carries only
/// the `monitorenter` spelling (`action_monitor`); HotSpot 25 prints the two
/// differently (`bytecodeUtils.cpp`, `print_NPE_failed_action`). Shared with
/// the compiled-code NPE builder in `jit_npe_message.rs`.
pub(super) fn monitorexit_npe_action() -> &'static str {
    "Cannot exit synchronized block"
}

/// An array index operand as a slot number, or the `ArrayIndexOutOfBounds`
/// a NEGATIVE one raises.
///
/// The heap's `get_array_element` / `set_array_element` take a `usize`, and
/// their `Err` channel saturates any index `>= i32::MAX` to `i32::MAX`
/// (`cratonvm_types::oob_index_code`). So `index as usize` on `-1` became
/// `usize::MAX`, came back as `i32::MAX`, and the exception read
/// `Index 2147483647 out of bounds for length N` where HotSpot 25 says
/// `Index -1 out of bounds for length N`. The raw-bytecode arms in
/// `interpreter.rs` test `index < 0` first; the decoded arms did not.
///
/// `length` is a closure because only the refusal needs it.
#[inline]
fn nonnegative_array_index(
    index: i32,
    length: impl FnOnce() -> i32,
) -> Result<usize, RuntimeError> {
    usize::try_from(index).map_err(|_| RuntimeError::aioobe(index, length()))
}

/// Which array opcode a null-array `NullPointerException` is being built for.
#[derive(Clone, Copy, Debug)]
enum NullArraySite {
    /// `*aload` — JEP 358 depth 1; legacy text `Cannot load from null array`.
    Load(ArrayElemKind),
    /// `*astore` — JEP 358 depth 2; legacy text `<op> in <class>.<method> pc=<pc>`.
    Store(ArrayElemKind, &'static str),
    /// `arraylength` — JEP 358 depth 0; legacy text
    /// `arraylength null (in <class>.<method><desc> pc=<pc>)`.
    Length,
}

/// The JEP 358 element kind an array load or store opcode names. `baload` /
/// `bastore` serve `byte[]` and `boolean[]` alike; `ArrayElemKind::Byte`
/// prints the combined `byte/boolean` spelling HotSpot uses.
fn array_elem_kind_of(instruction: &Instruction) -> ArrayElemKind {
    match instruction {
        Instruction::Iaload | Instruction::Iastore => ArrayElemKind::Int,
        Instruction::Laload | Instruction::Lastore => ArrayElemKind::Long,
        Instruction::Faload | Instruction::Fastore => ArrayElemKind::Float,
        Instruction::Daload | Instruction::Dastore => ArrayElemKind::Double,
        Instruction::Aaload | Instruction::Aastore => ArrayElemKind::Object,
        Instruction::Baload | Instruction::Bastore => ArrayElemKind::Byte,
        Instruction::Caload | Instruction::Castore => ArrayElemKind::Char,
        _ => ArrayElemKind::Short,
    }
}

/// Pop the array operand of an array load, store or `arraylength`.
///
/// A non-null reference — the case that matters — costs a peek and a pop.
/// Every other operand shape goes to [`array_operand_slow`], which is where
/// the NPE message context is captured. Until 2026-09-23 each array arm
/// captured that context inline, BEFORE learning whether the array was null:
/// three `Arc` clones (frame code, method name, method descriptor; six for
/// `arraylength`) per executed load or store, on refcounts that every thread
/// running the same method shares. `monitor_operand_slow` is the same split,
/// made for the same reason.
#[inline]
fn pop_array_operand(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    site: NullArraySite,
) -> Result<ObjectRef, MethodCallFailed> {
    // `plain_ref_on_top`, not a `Value` peek (wave 3, lane L1): the context-free
    // decode answers `long` for an `Object`-tagged payload with no recorded
    // provenance, which `array_operand_slow` then rejected as "expected object
    // reference, got long" — an uncatchable internal error for a live array.
    if let Some(array_ref) = plain_ref_on_top(&thread.frames[frame_idx].stack) {
        thread.frames[frame_idx].stack.pop()?;
        return Ok(array_ref);
    }
    array_operand_slow(shared, thread, frame_idx, site)
}

/// Every operand shape [`pop_array_operand`] does not take: null, an
/// `Uninitialized` slot, a `long`/`double` slot that `pop_object_ref_ctx_with`
/// may recognise as a smuggled `jobject`, or garbage. The messages are the ones
/// each arm used to build inline, byte for byte, with one deliberate
/// difference: the legacy (non-JEP-358) `*aload` arm used `pop_object_ref_ctx`,
/// which accepts an aligned `long`/`double` bit pattern as a reference WITHOUT
/// the heap-membership check (C7) that `pop_object_ref_ctx_with` applies. Every
/// arm now gets the checked form.
#[cold]
fn array_operand_slow(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    site: NullArraySite,
) -> Result<ObjectRef, MethodCallFailed> {
    use crate::runtime::exceptions::helpful_npe;
    let pc = thread.frames[frame_idx].pc;
    let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
    let npe_code = Arc::clone(&thread.frames[frame_idx].code);
    let npe_cid = thread.frames[frame_idx].class_id;
    let npe_cname = thread.frames[frame_idx].class_name_arc();
    let npe_mname = thread.frames[frame_idx].method_name_arc();
    let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
    let npe_bci = thread.frames[frame_idx].last_instr_pc;
    let popped = pop_object_ref_ctx_with(
        &mut thread.frames[frame_idx].stack,
        &shared.mem.heap,
        || {
            if jep358 {
                let (action, depth) = match site {
                    NullArraySite::Load(kind) => (helpful_npe::action_array_load(kind), 1),
                    NullArraySite::Store(kind, _) => (helpful_npe::action_array_store(kind), 2),
                    NullArraySite::Length => (helpful_npe::action_array_length(), 0),
                };
                helpful_npe_opcode_message_parts(
                    shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, depth,
                )
            } else {
                match site {
                    NullArraySite::Load(_) => "Cannot load from null array".to_string(),
                    NullArraySite::Store(_, op) => {
                        format!("{op} in {npe_cname}.{npe_mname} pc={pc}")
                    }
                    NullArraySite::Length => {
                        format!("arraylength null (in {npe_cname}.{npe_mname}{npe_mdesc} pc={pc})")
                    }
                }
            }
        },
    );
    // S111r14 diag: print the full Java stack on an `arraylength` failure.
    if popped.is_err()
        && matches!(site, NullArraySite::Length)
        && crate::runtime::env_cache::iae_trace_os()
    {
        eprintln!("[ARRAYLEN-DIAG] failure in {npe_cname}.{npe_mname}{npe_mdesc} pc={pc}");
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
    popped
}

/// `anewarray`'s component class.
///
/// A `CONSTANT_Class` resolution and nothing more — JVMS §6.5 `anewarray`
/// resolves its component, it does not initialize it — which is exactly what a
/// [`CastSiteCache`] entry records for the same (class, cp index): one
/// constant-pool entry has one resolution whichever opcode asks. So the cast
/// table serves this opcode too, under the same admission rule as the cast
/// arms' fill. Before 2026-09-23 every execution re-derived it: a
/// `class_manager` read, a fresh `String` for the name and a full
/// `resolve_class_loader_aware`, per array allocated.
fn anewarray_component(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    index: u16,
) -> Result<ClassId, MethodCallFailed> {
    let enabled = cast_site_cache_enabled();
    if enabled {
        if let Some(site) = thread.cast_sites.get(referencing_class_id, index) {
            return Ok(site.target);
        }
    }
    // Read BEFORE resolving; see `SiteCache::put`.
    let epochs_at_entry = enabled.then(|| CastSiteCache::epochs_for(shared));
    // Read BEFORE the pool read below: a failure is recorded against the
    // entry only while no redefinition of the class may have replaced that
    // pool (`record_resolution_failure_as_of`; i22-L5, wave 24).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let name = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(referencing_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;
        // The pool's interned `Arc<str>`, not a fresh `String` per miss.
        class
            .constant_pool
            .get_class_name_arc(index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{index}"),
            })?
    };
    if let Some(recorded) = recorded_resolution_failure(shared, thread, referencing_class_id, index)
    {
        return Err(recorded);
    }
    let id = resolve_class_loader_aware(shared, thread, referencing_class_id, &name)
        .map_err(|e| convert_class_not_found(shared, thread, &name, e))
        .map_err(|e| {
            record_resolution_failure_as_of(shared, referencing_class_id, index, e, fill_as_of)
        })?;
    // JVMS §5.4.3: a failure another thread recorded for the entry meanwhile
    // is its outcome (wave 38, lane L5; `--jdk-only`).
    if let Some(recorded) =
        recorded_resolution_failure_after_success(shared, thread, referencing_class_id, index)
    {
        return Err(recorded);
    }
    // JVMS §5.4.4 (i8-L2 / i9-L2): the component must be accessible, like
    // `new`'s class; before the fill below, which is what later hits skip.
    super::constants::check_class_constant_access_as_of(
        shared,
        thread,
        (referencing_class_id, index),
        &name,
        Some(id),
        "anewarray",
        fill_as_of,
    )?;
    // The cast arms' admission rule (`site_fill_admitted`; a loader-namespaced
    // referencing class only for an answer its loader recorded, wave 24). An
    // array name is admitted since i11-L2: it resolves through the same
    // `resolve_class_loader_aware`, to the class the cast arms record too.
    if let Some(epochs) = epochs_at_entry {
        if site_fill_admitted(shared, thread, referencing_class_id, &name, id) {
            thread
                .cast_sites
                .put(referencing_class_id, index, epochs, CastSite::resolved(id));
        }
    }
    Ok(id)
}

/// Can resolving this `checkcast` / `instanceof` target neither fail nor tell
/// the verdict anything it needs?
///
/// True for an array type whose bottom element is primitive (`[I`, `[[B`), and
/// for `java/lang/Object`, `java/lang/Cloneable` and `java/io/Serializable`
/// or an array of one of them: bootstrap-defined public classes that every
/// VM has. The array verdict answers all of these from the receiver's header
/// alone (`typecheck::array_cast_fast_verdict`), and access to them is never
/// refused, so a loader-namespaced referencing class — whose sites the cast
/// table does not admit — is spared a resolution per execution.
#[inline]
fn cast_target_resolution_is_trivial(name: &str) -> bool {
    let element = name.trim_start_matches('[');
    if element.len() != name.len() && element.len() == 1 {
        return true;
    }
    let element = if element.len() == name.len() {
        element
    } else {
        match element.strip_prefix('L').and_then(|e| e.strip_suffix(';')) {
            Some(e) => e,
            None => return false,
        }
    };
    matches!(
        element,
        "java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable"
    )
}

/// JVMS §6.5 `checkcast` / `instanceof` with a non-null receiver: "the named
/// class, array, or interface type is resolved (§5.4.3.1)" — whatever the
/// receiver turns out to be (i11-L2). Before i11-L2 only a non-array receiver
/// against a non-array target resolved, so an array on either side answered
/// `false` / `ClassCastException` where HotSpot throws `NoClassDefFoundError`
/// or `IllegalAccessError`, and the array verdict re-resolved component
/// names loader-blind.
///
/// `site` is `(referencing class, cp index)`. `Ok(Some(id))` is the resolved
/// class (for an array name, the array class, whose `array_info` names the
/// component its own loader produced). `Ok(None)` only when `needs_id` is
/// `false` and [`cast_target_resolution_is_trivial`] holds: nothing to decide
/// (the table is still filled then, [`fill_trivial_cast_target`]).
/// A recorded failure of the entry is rethrown and a new one recorded (JVMS
/// §5.4.3), then the §5.4.4 access check runs, then the cast-site table is
/// filled (`fill_epochs`, read before the probe that missed; a
/// loader-namespaced referencing class is refused as for every fill).
/// `cached` is the entry a probe already found: it is returned as is, since a
/// fill only ever follows a successful resolution and access check.
/// `fill_as_of` is `ResolutionCache::fill_snapshot`, taken by the caller
/// before it read `name` from the pool: failures are recorded under it
/// (i22-L5, wave 24).
///
/// GC-safety: resolution can load classes, run a user `loadClass` and so
/// collect. Every raw reference the caller holds must be pinned across it.
#[allow(clippy::too_many_arguments)]
fn resolve_cast_target(
    shared: &SharedVm,
    thread: &mut JvmThread,
    (referencing_class_id, index): (ClassId, u16),
    name: &str,
    needs_id: bool,
    cached: Option<ClassId>,
    fill_epochs: Option<(u64, u64)>,
    insn: &'static str,
    fill_as_of: u64,
) -> Result<Option<ClassId>, MethodCallFailed> {
    if let Some(id) = cached {
        return Ok(Some(id));
    }
    if !needs_id && cast_target_resolution_is_trivial(name) {
        if let Some(epochs_at_entry) = fill_epochs {
            fill_trivial_cast_target(
                shared,
                thread,
                (referencing_class_id, index),
                name,
                epochs_at_entry,
            );
        }
        return Ok(None);
    }
    if let Some(recorded) = recorded_resolution_failure(shared, thread, referencing_class_id, index)
    {
        return Err(recorded);
    }
    let id = resolve_class_loader_aware(shared, thread, referencing_class_id, name)
        .map_err(|e| convert_class_not_found(shared, thread, name, e))
        .map_err(|e| {
            record_resolution_failure_as_of(shared, referencing_class_id, index, e, fill_as_of)
        })?;
    // JVMS §5.4.3: a failure another thread recorded for the entry meanwhile
    // is its outcome (wave 38, lane L5; `--jdk-only`). The callers pin their
    // receiver across this function, as across the resolution.
    if let Some(recorded) =
        recorded_resolution_failure_after_success(shared, thread, referencing_class_id, index)
    {
        return Err(recorded);
    }
    // JVMS §5.4.4 (i9-L2), before the fill below, which is what later hits
    // skip. For an array name the bottom element class is checked.
    super::constants::check_class_constant_access_as_of(
        shared,
        thread,
        (referencing_class_id, index),
        name,
        Some(id),
        insn,
        fill_as_of,
    )?;
    if let Some(epochs_at_entry) = fill_epochs {
        if !site_fill_admitted(shared, thread, referencing_class_id, name, id) {
            site_stats::bump(site_stats::CAST_REJECT_LOADER);
        } else {
            thread.cast_sites.put(
                referencing_class_id,
                index,
                epochs_at_entry,
                CastSite::resolved(id),
            );
            site_stats::bump(site_stats::CAST_FILL);
        }
    }
    Ok(Some(id))
}

/// i16-L4: fill the cast table for a TRIVIAL target
/// ([`cast_target_resolution_is_trivial`]: `[I`, `java/lang/Object`,
/// `[Ljava/lang/Object;`, ...) that an array receiver reached without
/// resolving it.
///
/// Before i16-L4 such a site never got an entry, so an array receiver ran the
/// full path on every execution — a `class_manager` read for the name, then
/// the array verdict — and could never reach the array memo
/// ([`CastSite::positive_array`]), although `(T[]) x` erases to exactly such
/// a target (`checkcast [Ljava/lang/Object;`). The entry is the one any other
/// receiver fills for the same `(class, index)`: the resolution, under the
/// same admission rule, after the same access verdict (never a refusal for
/// these names). The caller still answers this execution with no id
/// (`Ok(None)`), exactly as before; later executions hit.
///
/// Screened lock-free first: a referencing class that may have a defining
/// loader (the table refuses it anyway) returns at once, so it pays one
/// bitmap load per execution and no lock. A resolution that fails —
/// impossible for these names on a VM with its bootstrap classes — fills
/// nothing and raises nothing.
///
/// GC-safety: resolution can load a class; this runs inside
/// [`resolve_cast_target`], across which both callers pin the receiver.
fn fill_trivial_cast_target(
    shared: &SharedVm,
    thread: &mut JvmThread,
    (referencing_class_id, index): (ClassId, u16),
    name: &str,
    epochs_at_entry: (u64, u64),
) {
    if cratonvm_native_builtins::classloader::class_may_have_defining_loader(
        referencing_class_id.as_u32(),
    ) {
        return;
    }
    let Ok(id) = resolve_class_loader_aware(shared, thread, referencing_class_id, name) else {
        return;
    };
    if !super::constants::class_constant_fill_admitted(shared, referencing_class_id, name, id) {
        return;
    }
    if referencing_class_has_loader_namespace(shared, referencing_class_id) {
        site_stats::bump(site_stats::CAST_REJECT_LOADER);
        return;
    }
    thread.cast_sites.put(
        referencing_class_id,
        index,
        epochs_at_entry,
        CastSite::resolved(id),
    );
    site_stats::bump(site_stats::CAST_FILL);
}

/// The key an ARRAY receiver's verdict is memoised under
/// ([`CastSite::positive_array`], i16-L4). Two header reads: no lock, no
/// safepoint, and a collection that moves the array keeps both.
#[inline]
fn array_receiver_key(shared: &SharedVm, obj_ref: ObjectRef) -> ArrayReceiverKey {
    (
        shared.mem.heap.element_type_of(obj_ref),
        shared.mem.heap.class_id_of(obj_ref),
    )
}

/// Can a cast site's ARRAY memo answer this receiver without a lock (i16-L4)?
/// Only an ARRAY receiver whose key is exactly the memoised one — the mirror
/// of [`cast_memo_answers`]'s exclusion: a plain object whose class id equals
/// a memoised component id is no array (a `String` is not a `String[]`).
#[inline]
fn array_cast_memo_answers(
    site: &CastSite,
    receiver_is_array: bool,
    key: ArrayReceiverKey,
) -> bool {
    receiver_is_array && site.positive_array == Some(key)
}

/// Record an ARRAY receiver's admission at a `checkcast` / `instanceof` site
/// whose full path just answered `true` for it (i16-L4).
///
/// The array verdict reads the receiver's header key, the site's resolved
/// target, the VM's mode and the class table — no instance data (an array is
/// never a proxy or a display stamp) — so, for a fixed target, it is a
/// function of the key while the entry's tags and the definition epoch hold.
/// Only into an entry that already exists for the SAME `target` (the one the
/// verdict was computed against): the resolution fill's admission rule is
/// inherited, not restated. `epochs` were read before the site was probed and
/// `memo_epoch` at the same point, so a class the verdict loaded (the
/// descriptor path can) retires the memo (`SiteCache::put`,
/// `CastSite::observed_at`).
fn fill_array_cast_memo(
    thread: &mut JvmThread,
    (referencing_class_id, index): (ClassId, u16),
    (epochs, memo_epoch): ((u64, u64), u64),
    target: ClassId,
    key: ArrayReceiverKey,
) {
    let Some(site) = thread
        .cast_sites
        .get(referencing_class_id, index)
        .copied()
        .map(|site| site.observed_at(memo_epoch))
    else {
        return;
    };
    if site.target != target || site.positive_array == Some(key) {
        return;
    }
    thread.cast_sites.put(
        referencing_class_id,
        index,
        epochs,
        CastSite {
            positive_array: Some(key),
            ..site
        },
    );
}

/// `CRATONVM_DBG_CAST_MEMO_CROSSCHECK`: re-derive an array-memo hit's
/// verdict and report a disagreement (i16-L4). Asks the non-loading form
/// (`array_receiver_cast_verdict_in(.., may_load = false)`), so it neither
/// safepoints nor moves `obj_ref`, which the caller still holds raw; a shape
/// only the descriptor path decides is not re-checked.
#[cold]
fn crosscheck_array_memo(
    shared: &SharedVm,
    referencing_class_id: ClassId,
    index: u16,
    target: ClassId,
    obj_ref: ObjectRef,
) {
    // Own statement: the verdict below takes the read lock itself.
    let name = shared
        .classes
        .class_manager
        .read()
        .get_class(referencing_class_id)
        .and_then(|c| c.constant_pool.get_class_name_arc(index));
    let Some(name) = name else {
        return;
    };
    if array_receiver_cast_verdict_in(shared, obj_ref, &name, Some(target), false) == Some(false) {
        let (element, component) = array_receiver_key(shared, obj_ref);
        eprintln!(
            "[CAST-MEMO-CROSSCHECK] array memo answered true but the full path refuses: \
             receiver element={element:?} class_id={component} target={name} \
             site=({referencing_class_id}, cp#{index})"
        );
    }
}

/// Can a cast site's receiver memo answer this receiver without a lock?
///
/// Only a NON-ARRAY receiver whose class is exactly the one `is_subclass_of`
/// last proved assignable at this site. The array exclusion is load-bearing,
/// not tidiness: on a reference array the header's class id is the
/// COMPONENT's (`typecheck::array_descriptor_of`), so a `String[]` carries
/// `String`'s id and would otherwise pass a `checkcast String` memoised for a
/// `String` receiver.
#[inline]
fn cast_memo_answers(site: &CastSite, receiver_is_array: bool, receiver_class: ClassId) -> bool {
    !receiver_is_array && site.positive_receiver == Some(receiver_class)
}

/// The `checkcast` / `instanceof` cast-site HIT: does the cached target admit
/// this receiver by class hierarchy alone?
///
/// `true` only when the answer is "assignable" and needed no instance data:
/// either the memo names this receiver's class (no lock at all), or
/// `is_subclass_of` says yes under one `class_manager` read, after which this
/// receiver's class becomes the memo. `false` sends the caller down the full
/// path, which owns every refusal and every instance-dependent admission —
/// exactly as before the memo existed.
///
/// Why a memo is sound here: `is_subclass_of(receiver, target)` is a property
/// of two `ClassId`s whose superclass and interface vectors are fixed at load
/// time. The ways they can change are the ones every `SiteCache` entry is
/// already tagged against — a redefinition (`any_class_redefined` latch) and
/// `upgrade_synthetic_class` / `recompute_subclass_layouts` (the resolution
/// epoch). `IfaceSelectSiteCache` rests on the same argument. The memo is
/// written through `put` with epochs read BEFORE the probe, so an epoch that
/// moves while it is being computed discards it; `site` must come from
/// `CastSite::observed` at the probe, which stamps the memo with the
/// definition epoch read before this verdict (i7-L2).
///
/// An ARRAY receiver is answered only by the site's array memo
/// ([`array_cast_memo_answers`], i16-L4), filled by the full path
/// ([`fill_array_cast_memo`]); anything else about an array is the full
/// path's.
///
/// GC-safety: header reads, one read lock and a Rust-heap slot write; nothing
/// allocates on the Java heap or safepoints, so `obj_ref` cannot move.
fn cast_site_hit_admits(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    index: u16,
    epochs_before: (u64, u64),
    site: CastSite,
    obj_ref: ObjectRef,
) -> bool {
    // `kind_of` rather than `array_descriptor_of`: the latter builds a
    // `String` descriptor for an array receiver, which this test would only
    // discard.
    let is_array = shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array;
    if is_array {
        let answered =
            array_cast_memo_answers(&site, is_array, array_receiver_key(shared, obj_ref));
        if answered {
            if cast_memo_crosscheck() {
                crosscheck_array_memo(shared, referencing_class_id, index, site.target, obj_ref);
            }
            site_stats::bump(site_stats::CAST_ARRAY_HIT);
        }
        return answered;
    }
    let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
    if cast_memo_answers(&site, is_array, obj_class_id) {
        return true;
    }
    // Lock-free once the receiver class's supers closure is published.
    let assignable = class_is_subtype(shared, obj_class_id, site.target);
    if assignable {
        thread.cast_sites.put(
            referencing_class_id,
            index,
            epochs_before,
            CastSite {
                positive_receiver: Some(obj_class_id),
                ..site
            },
        );
    }
    assignable
}

/// `CRATONVM_JIT_NO_NEGATIVE_CAST_MEMO=1` — withdraw the `instanceof` negative
/// receiver memo ([`CastSite::negative_receivers`]) so one binary can be A/B'd
/// against its own pre-change behaviour. Default-ON. Read once.
fn negative_cast_memo_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_NEGATIVE_CAST_MEMO").is_none()
    })
}

/// `CRATONVM_DBG_CAST_MEMO_CROSSCHECK=1` — on every negative-memo hit,
/// re-evaluate the full path's predicates and report a disagreement. The
/// failure mode of a wrong classification is a silent wrong `false`, so this
/// is the instrument that proves the memo on a workload. Since i16-L4 it also
/// re-derives every ARRAY-memo hit (`crosscheck_array_memo`). Read once.
fn cast_memo_crosscheck() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_CAST_MEMO_CROSSCHECK").is_some()
    })
}

/// Is `instanceof`'s full-path verdict for a NON-ARRAY receiver of this class
/// a function of the class alone?
///
/// The full path's disjunction splits in two. `is_subclass_of`,
/// `loader_aware_name_assignable` (its gate is read once) and
/// `synthetic_implements` (name tables) read only class ids, names and the
/// receiver's load-time hierarchy, which the cast site's epochs already cover.
/// The other four read something else, and each is excluded by the SAME screen
/// it applies itself:
///
/// * `lambda_proxy_satisfies` — per-proxy side tables: lambda-proxy ids (the
///   `0x8000_0000+` range, which also holds the autobox sentinel) and any id
///   registered in `lambda_proxies`;
/// * `proxy_instance_satisfies_target` — the interface slot of a
///   `Proxy$Instance` or a generated proxy class (`class_is_generated_proxy_or_shim`,
///   the screen it applies itself; a user class that merely extends
///   `java.lang.reflect.Proxy` is refused from its class alone, so it may be
///   memoised — round 13 wave 7, lane predicates);
/// * `annotation_proxy_satisfies_target` — slot 0 of an `AnnotationProxy`;
/// * `display_class_satisfies_target` — the slots of a
///   `cratonvm/internal/Unmodifiable*` stamp.
///
/// Asked once per negative FILL, never on a hit. The JIT's negative memo
/// (`jit_typecheck_resolve`'s `stable_negative`) asks it too, so the two tiers
/// exclude the same receivers.
pub(crate) fn receiver_class_determines_cast_verdict(shared: &SharedVm, class_id: ClassId) -> bool {
    if class_id.as_u32() >= 0x8000_0000 {
        return false;
    }
    if shared.classes.lambda_proxies.read().contains_key(&class_id) {
        return false;
    }
    let name = match shared.classes.class_manager.read().get_class(class_id) {
        Some(c) => Arc::clone(&c.name),
        None => return false,
    };
    !(&*name == "java/lang/reflect/Proxy$Instance"
        || &*name == "java/lang/annotation/AnnotationProxy"
        || name.starts_with("cratonvm/internal/Unmodifiable")
        || class_is_generated_proxy_or_shim(shared, class_id))
}

/// Can the site's negative memo answer `instanceof` for this receiver without
/// a lock? Only a NON-ARRAY receiver whose class is exactly one of the two
/// memoised ones (i20-L5: either slot answers) — the array exclusion is the
/// one `cast_memo_answers` documents. A site with no refusal reads no header.
#[inline]
fn negative_memo_answers(shared: &SharedVm, site: &CastSite, obj_ref: ObjectRef) -> bool {
    match site.negative_receivers {
        [None, None] => false,
        _ => {
            shared.mem.heap.kind_of(obj_ref) != cratonvm_types::ObjectKind::Array
                && site.refuses(shared.mem.heap.class_id_of(obj_ref))
        }
    }
}

/// `CRATONVM_DBG_CAST_MEMO_CROSSCHECK`: recompute every class- and
/// instance-level admission `op_instanceof`'s full path would have tried, and
/// report any that says `true` where the memo answered `false`.
#[cold]
fn crosscheck_negative_memo(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    index: u16,
    target: ClassId,
    mut obj_ref: ObjectRef,
) {
    let Some(name) = shared
        .classes
        .class_manager
        .read()
        .get_class(referencing_class_id)
        .and_then(|c| c.constant_pool.get_class_name(index).map(str::to_string))
    else {
        return;
    };
    let cid = shared.mem.heap.class_id_of(obj_ref);
    let by_hierarchy = shared
        .classes
        .class_manager
        .read()
        .is_subclass_of(cid, target);
    // The name rule is `--compatible`-only, as in `op_instanceof`.
    let admitted = by_hierarchy
        || (!shared.config.is_jdk_only()
            && loader_aware_name_assignable(shared, cid, target, &name))
        || lambda_proxy_satisfies(shared, cid, target)
        || synthetic_implements(shared, cid, &name)
        || proxy_instance_satisfies_resolved(shared, obj_ref, &name, target)
        || annotation_proxy_satisfies_target(shared, obj_ref, &name)
        || display_class_satisfies_target(shared, thread, &mut obj_ref, cid, target);
    if admitted {
        eprintln!(
            "[CAST-MEMO-CROSSCHECK] negative memo answered false but the full path admits: \
             receiver class_id={cid} target={name} site=({referencing_class_id}, cp#{index})"
        );
    }
}

/// Record `receiver`'s refusal at an `instanceof` site whose full path just
/// answered `false` for it.
///
/// Only into an entry that already exists for the same `target` — the
/// resolution fill's admission rule (no loader namespace, not an array name) is
/// therefore inherited rather than restated — and only for a receiver class
/// whose verdict is class-determined. `epochs` were read before the site was
/// probed, so an epoch that moved during the full path discards the memo
/// (`SiteCache::put`); `memo_epoch` is the definition epoch read at the same
/// point, which the memo carries (`CastSite::observed_at`, i7-L2).
fn fill_negative_cast_memo(
    shared: &SharedVm,
    thread: &mut JvmThread,
    (referencing_class_id, index): (ClassId, u16),
    (epochs, memo_epoch): ((u64, u64), u64),
    target: ClassId,
    receiver: ClassId,
) {
    let Some(site) = thread
        .cast_sites
        .get(referencing_class_id, index)
        .copied()
        .map(|site| site.observed_at(memo_epoch))
    else {
        return;
    };
    if site.target != target
        || site.refuses(receiver)
        || !receiver_class_determines_cast_verdict(shared, receiver)
    {
        return;
    }
    // i20-L5: newest into slot 0, slot 0 into slot 1. The slot kept was
    // proved under the same `memo_epoch` (`observed_at` above cleared both
    // otherwise), so each slot stays exactly as exact as the one memo was.
    thread.cast_sites.put(
        referencing_class_id,
        index,
        epochs,
        site.with_negative(receiver),
    );
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
        Instruction::Ldc2W(index) => execute_ldc2w(shared, thread, frame_idx, *index)
            .map_err(|e| convert_ldc_class_format_error(shared, thread, e))?,

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
            //
            // A plain `Object`-tagged local passes through as the reference
            // it is, as the fast path's `load_ref_local` pushes it (wave 3,
            // lane L1): the `get_local` decode nulls an `Object`-tagged
            // payload with no recorded `ObjectRef` provenance.
            let cv = thread.frames[frame_idx].get_local_compact(*idx);
            let v = match ref_local_object(cv) {
                Some(r) => Value::Object(Some(r)),
                None => coerce_value_for_return_validated(
                    shared,
                    thread.frames[frame_idx].get_local(*idx),
                    b'L',
                ),
            };
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
            // element kind comes from the opcode itself. The message is built
            // only when the array really is null — see `pop_array_operand`.
            let array_ref = pop_array_operand(
                shared,
                thread,
                frame_idx,
                NullArraySite::Load(array_elem_kind_of(instruction)),
            )?;
            // A NEGATIVE index must be refused here, before the `usize`
            // conversion: `-1 as usize` is `usize::MAX`, which the heap's
            // error channel saturates to `i32::MAX` (`oob_index_code`), so the
            // message read "Index 2147483647 out of bounds" where HotSpot says
            // "Index -1 out of bounds". The raw-bytecode arm has always made
            // this check; only this decoded twin lacked it.
            let array_len = || shared.mem.heap.array_length(array_ref) as i32;
            let slot = nonnegative_array_index(index, array_len)?;
            let value = shared
                .mem
                .heap
                .get_array_element(array_ref, slot)
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
                    RuntimeError::array_store_fault(
                        i,
                        shared.mem.heap.array_length(array_ref) as i32,
                    )
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
            // A plain reference is stored as the reference it is, as the fast
            // path's `store_ref_local` stores it (wave 3, lane L1).
            let plain = plain_ref_on_top(&thread.frames[frame_idx].stack);
            let v = thread.frames[frame_idx].stack.pop()?;
            let coerced = match plain {
                Some(r) => Value::Object(Some(r)),
                None => coerce_value_for_return_validated(shared, v, b'L'),
            };
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
            // the same validated normalization. A plain reference passes
            // through as itself (`plain_ref_on_top`; wave 3, lane L1).
            let plain = plain_ref_on_top(&thread.frames[frame_idx].stack);
            let popped = thread.frames[frame_idx].stack.pop()?;
            let raw_value = match plain {
                Some(r) => Value::Object(Some(r)),
                None => coerce_value_for_return_validated(shared, popped, b'L'),
            };
            // A refused wrapper allocation is an OutOfMemoryError (gc-common w5-c).
            let value = normalize_aastore_value(shared, raw_value)?;
            let index = thread.frames[frame_idx].stack.pop_int()?;
            // JEP 358 increment 2: `Cannot store to object array because
            // "<expr>" is null`. Source stack `[..., arrayref, index, value]`
            // → array at depth 2. See `pop_array_operand` for why nothing
            // about the message is captured on the non-null path.
            let array_ref = pop_array_operand(
                shared,
                thread,
                frame_idx,
                NullArraySite::Store(ArrayElemKind::Object, "aastore"),
            )?;
            // JVMS §6.5 aastore fixes the order of the three checks:
            // NullPointerException (done above, by `pop_array_operand`),
            // THEN ArrayIndexOutOfBoundsException, THEN ArrayStoreException.
            // The bounds test used to be nothing but `set_array_element`'s error
            // return, which runs AFTER the covariance block below — so an
            // out-of-range index with an incompatible value reported
            // `ArrayStoreException` where HotSpot reports
            // `ArrayIndexOutOfBoundsException` (measured: `RArrayStoreTiers` s15).
            // `jit_aastore` already had this order; the interpreter did not.
            // See docs/internal/jdk-only/W8-C10-1-typecheck-hatch-audit-and-aastore-precedence.md
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
                .mem
                .heap
                .set_array_element(array_ref, index as usize, value) // Widening: index conversion
                .map_err(|i| {
                    if aioobe_dbg() {
                        let alen = shared.mem.heap.array_length(array_ref);
                        let f = &thread.frames[frame_idx];
                        eprintln!(
                            "AIOOBE-AASTORE class={} method={} pc={} idx={i} len={alen}",
                            f.class_name(),
                            f.method_name(),
                            f.pc
                        );
                    }
                    RuntimeError::array_store_fault(
                        i,
                        shared.mem.heap.array_length(array_ref) as i32,
                    )
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
            // JEP 358 increment 2: array at depth 2 (`[..., arrayref, index,
            // value]`); element kind from the opcode.
            let array_ref = pop_array_operand(
                shared,
                thread,
                frame_idx,
                NullArraySite::Store(array_elem_kind_of(instruction), "Xastore"),
            )?;
            // bc math-ec 0x4 smear hunt — see the Lastore twin below.
            if arrstore_enabled() {
                arrstore_check(shared, thread, array_ref, index, "iastore");
            }
            // Negative index: see the `*aload` arm.
            let array_len = || shared.mem.heap.array_length(array_ref) as i32;
            let slot = nonnegative_array_index(index, array_len)?;
            shared
                .mem
                .heap
                .set_array_element(array_ref, slot, value)
                .map_err(|i| {
                    if aioobe_dbg() {
                        let alen = shared.mem.heap.array_length(array_ref);
                        let f = &thread.frames[frame_idx];
                        eprintln!(
                            "AIOOBE-XASTORE class={} method={} pc={} idx={i} len={alen}",
                            f.class_name(),
                            f.method_name(),
                            f.pc
                        );
                    }
                    RuntimeError::array_store_fault(
                        i,
                        shared.mem.heap.array_length(array_ref) as i32,
                    )
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
            // JEP 358 increment 2: long array store, array at depth 2.
            let array_ref = pop_array_operand(
                shared,
                thread,
                frame_idx,
                NullArraySite::Store(ArrayElemKind::Long, "lastore"),
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
            // Negative index: see the `*aload` arm.
            let array_len = || shared.mem.heap.array_length(array_ref) as i32;
            let slot = nonnegative_array_index(index, array_len)?;
            shared
                .mem
                .heap
                .set_array_element(array_ref, slot, Value::Long(v))
                .map_err(|i| {
                    RuntimeError::array_store_fault(
                        i,
                        shared.mem.heap.array_length(array_ref) as i32,
                    )
                })?;
            // Phase 10 #2 — see the `Iastore` arm.
            #[cfg(feature = "gpu-offload")]
            crate::runtime::offload::input_cache::invalidate(array_ref);
        }
        Instruction::Dastore => {
            let d = thread.frames[frame_idx].stack.pop_double()?;
            let index = thread.frames[frame_idx].stack.pop_int()?;
            // JEP 358 increment 2: double array store, array at depth 2.
            let array_ref = pop_array_operand(
                shared,
                thread,
                frame_idx,
                NullArraySite::Store(ArrayElemKind::Double, "dastore"),
            )?;
            // Negative index: see the `*aload` arm.
            let array_len = || shared.mem.heap.array_length(array_ref) as i32;
            let slot = nonnegative_array_index(index, array_len)?;
            shared
                .mem
                .heap
                .set_array_element(array_ref, slot, Value::Double(d))
                .map_err(|i| {
                    RuntimeError::array_store_fault(
                        i,
                        shared.mem.heap.array_length(array_ref) as i32,
                    )
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
            // See `ValueStack::dup2_x2_form` for why the form is read off the
            // stack rather than decided from the first two pops.
            let stack = &mut thread.frames[frame_idx].stack;
            let Some(form) = stack.dup2_x2_form() else {
                return Err(RuntimeError::IllegalStateException {
                    message: "dup2_x2 on an operand stack that presents no JVMS form".to_string(),
                }
                .into());
            };
            let mut held = [(CompactValue::int(0), 0u8); 4];
            for slot in held.iter_mut().take(form.consumed()) {
                *slot = stack.pop_with_kind()?;
                record_shuffle_push(slot.0, "dup2_x2");
            }
            for &depth in form.push_order() {
                let (cv, kind) = held[depth];
                thread.frames[frame_idx].stack.push_with_kind(cv, kind)?;
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
            // JVMS §2.11.10: a return with a block monitor still recorded
            // throws `IllegalMonitorStateException` here, before the value
            // is popped and before `MethodExit` (HotSpot's `remove_activation`).
            super::held_monitors::check_structured_return(shared, thread, frame_idx)?;
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
            // JVMS §2.11.10: a return with a block monitor still recorded
            // throws `IllegalMonitorStateException` here, before the value
            // is popped and before `MethodExit` (HotSpot's `remove_activation`).
            super::held_monitors::check_structured_return(shared, thread, frame_idx)?;
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
            // JVMS §2.11.10: a return with a block monitor still recorded
            // throws `IllegalMonitorStateException` here, before the value
            // is popped and before `MethodExit` (HotSpot's `remove_activation`).
            super::held_monitors::check_structured_return(shared, thread, frame_idx)?;
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
            // JVMS §2.11.10: a return with a block monitor still recorded
            // throws `IllegalMonitorStateException` here, before the value
            // is popped and before `MethodExit` (HotSpot's `remove_activation`).
            super::held_monitors::check_structured_return(shared, thread, frame_idx)?;
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
            // JVMS §2.11.10: a return with a block monitor still recorded
            // throws `IllegalMonitorStateException` here, before the value
            // is popped and before `MethodExit` (HotSpot's `remove_activation`).
            super::held_monitors::check_structured_return(shared, thread, frame_idx)?;
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
            // JVMS §2.11.10: a return with a block monitor still recorded
            // throws `IllegalMonitorStateException` here, before the value
            // is popped and before `MethodExit` (HotSpot's `remove_activation`).
            super::held_monitors::check_structured_return(shared, thread, frame_idx)?;
            // A plain reference passes through as itself, as the fast path's
            // `ref_operand_value` passes it (wave 3, lane L1).
            let plain = plain_ref_on_top(&thread.frames[frame_idx].stack);
            let v = thread.frames[frame_idx].stack.pop()?;
            let ret = crate::jit::return_type(thread.frames[frame_idx].method_descriptor());
            let v = match plain {
                Some(r) if matches!(ret, b'L' | b'[') => Value::Object(Some(r)),
                _ => coerce_value_for_return_validated(shared, v, ret),
            };
            // Popped, so pinned across the MethodExit callback (wave 4, lane
            // L1): the reference is in no frame slot until the dispatch loop
            // pushes it into the parent.
            let mut rv = Some(v);
            fire_method_exit_keeping_value(shared, thread, frame_idx, &mut rv);
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
                    match execute_invoke(
                        shared,
                        thread,
                        frame_idx,
                        *index,
                        is_special_invoke,
                        saved_pc,
                    )? {
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
                    match execute_invoke_kind(
                        shared, thread, frame_idx, *index, false, true, saved_pc,
                    )? {
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
        // One body each, shared with the raw-bytecode arms in
        // `execute_frame_from_index` (wave 3, lane L1).
        Instruction::Newarray(atype) => op_newarray(shared, thread, frame_idx, *atype)?,
        Instruction::Anewarray(index) => op_anewarray(shared, thread, frame_idx, *index)?,
        Instruction::Arraylength => {
            // JEP 358 increment 2: `Cannot read the array length because
            // "<expr>" is null`. The array ref is at the top of the operand
            // stack (depth 0). This handler IS the arraylength path under
            // `-Xverify:none`, which is why the message context — and the
            // S111r14 `[ARRAYLEN-DIAG]` stack dump — live in
            // `array_operand_slow` and cost nothing on a non-null array.
            let arr_ref = pop_array_operand(shared, thread, frame_idx, NullArraySite::Length)?;
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

            let mut popped = Vec::with_capacity(dims);
            for _ in 0..dims {
                popped.push(thread.frames[frame_idx].stack.pop_int()?);
            }
            // The operand stack holds the dimensions innermost-on-top, so the
            // pops come back inside-out.
            popped.reverse();
            // Check them OUTERMOST FIRST, and check them ALL before allocating
            // anything. The check used to ride the pop loop, which reported the
            // INNERMOST negative and stopped at the first one it met going
            // inward: `new byte[-1][4][-2]` threw `-2` here and `-1` on
            // HotSpot 25.0.3+9, and so did the JIT, whose helper always read
            // the dimensions outermost-first. MEASURED 2026-09-22 against HotSpot 25.0.3+9 and pinned by
            // `regression-suite/src/RJitMultiArrayDims.java`, whose e-rows also
            // cover the zero-length cases HotSpot still rejects
            // (`new byte[2][0][-1]` throws `-1` even though no inner array
            // would ever be allocated).
            // Resolution first, as `op_anewarray` explains: `new Missing[-1][2]`
            // is a NoClassDefFoundError on HotSpot.
            if let Some(&size) = popped.iter().find(|&&s| s < 0) {
                let class_id = thread.frames[frame_idx].class_id;
                anewarray_component(shared, thread, class_id, *index)?;
                return Err(RuntimeError::NegativeArraySizeException { size }.into());
            }
            // Widening: index conversion, every element non-negative above.
            let sizes: Vec<usize> = popped.into_iter().map(|s| s as usize).collect();

            // Descriptor parse, the JVMS §4.9.1 bracket-count guard, the
            // per-level component-class resolution and the allocation itself
            // all live in `multianewarray_alloc`, which the JIT's
            // `jit_multianewarray_n` helper calls too. Keeping one body is the
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
        // One body, shared with the raw-bytecode `athrow` arm in
        // `execute_frame_from_index` (wave 3, lane L1). It never succeeds.
        Instruction::Athrow => match op_athrow(shared, thread, frame_idx) {
            Ok(never) => match never {},
            Err(e) => return Err(e),
        },

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

/// `newarray` — allocate a primitive array of the popped length.
///
/// Moved out of `execute_instruction`'s match arm (wave 3, lane L1) so the
/// raw-bytecode dispatch loop and the decoded path share one body. The length
/// is an int, so nothing reference-typed is held across the allocation (a
/// safepoint); the new array is on the operand stack before `maybe_gc`.
pub(super) fn op_newarray(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    atype: u8,
) -> Result<(), MethodCallFailed> {
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
    Ok(())
}

/// `anewarray` — allocate a reference array of the popped length whose
/// component is constant-pool class `index`.
///
/// Shared by both dispatch paths, like [`op_newarray`]. Component resolution
/// (which may load classes and run Java) happens after the int length is
/// popped and before the allocation, exactly as the decoded arm ordered it.
///
/// Resolution also comes BEFORE the negative-length check: JVMS §6.5 lists
/// resolution failures as linking exceptions and `NegativeArraySizeException`
/// as the run-time exception of an instruction whose resolution succeeded
/// ("Otherwise, if count is less than zero ..."), and HotSpot's
/// `InterpreterRuntime::anewarray` resolves (`klass_at`) before
/// `new_objArray` checks the length — so `new Missing[-1]` throws
/// `NoClassDefFoundError`, not `NegativeArraySizeException`.
pub(super) fn op_anewarray(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
) -> Result<(), MethodCallFailed> {
    let length = thread.frames[frame_idx].stack.pop_int()?;
    let referencing_class_id = thread.frames[frame_idx].class_id;
    let component_class_id = anewarray_component(shared, thread, referencing_class_id, index)?;
    if length < 0 {
        return Err(RuntimeError::NegativeArraySizeException { size: length }.into());
    }
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
    Ok(())
}

/// `athrow` — raise the throwable on top of the operand stack.
///
/// Moved out of `execute_instruction`'s match arm (wave 3, lane L1) so the
/// raw-bytecode dispatch loop and the decoded path share ONE body, with every
/// diagnostic it carries (`CRATONVM_DBG_IMSE`, the IAE trace, `athrow_dbg`,
/// the charset-NPE dump, the JFR `JavaErrorThrow` event). The `Ok` type is
/// uninhabited: every path answers `Err` — the throwable itself as
/// `ExceptionThrown`, an NPE for a null operand, or an internal error for a
/// non-reference.
///
/// The operand is reference-typed, so a plain `Object`-tagged slot is thrown
/// as the reference it holds (`plain_ref_on_top`): the `Value` decode alone
/// answered `long` for a payload with no recorded `ObjectRef` provenance, and
/// the arm then reported "athrow: not an object reference" for a live
/// throwable.
pub(super) fn op_athrow(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Result<std::convert::Infallible, MethodCallFailed> {
    let plain = plain_ref_on_top(&thread.frames[frame_idx].stack);
    let exc_value = thread.frames[frame_idx].stack.pop()?;
    let exc_value = match plain {
        Some(r) => Value::Object(Some(r)),
        None => exc_value,
    };
    match exc_value {
        Value::Object(Some(obj_ref)) => {
            // DBG (CRATONVM_DBG_IMSE): autopsy for the RRWL "attempt
            // to unlock read lock" hold-count loss (ES testAllEqual
            // face). At the throw site, dump the Sync's complete
            // read-hold bookkeeping so the broken invariant is named
            // directly: firstReader identity, the cached hold
            // counter, and the current thread's readHolds
            // ThreadLocalMap entry.
            if dbg_imse_enabled() {
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
                        Value::Object(Some(msg_ref)) => read_java_string(&shared.mem.heap, msg_ref)
                            .unwrap_or_else(|| "<non-string>".to_string()),
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
                        eprintln!("IAE-ATHROW-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
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
                    if let Value::Object(Some(msg_ref)) = shared.mem.heap.get_field(obj_ref, fi) {
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
                    if let Value::Object(Some(msg_ref)) = shared.mem.heap.get_field(obj_ref, fi) {
                        if let Some(s) = read_java_string(&shared.mem.heap, msg_ref) {
                            if !s.is_empty() {
                                detail = s;
                                break;
                            }
                        }
                    }
                }
                if exc_class_name == "java/lang/NullPointerException" && detail == "charset" {
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
                    "java/lang/NoClassDefFoundError" => Some("java.lang.NoClassDefFoundError"),
                    "java/lang/NoSuchFieldError" => Some("java.lang.NoSuchFieldError"),
                    "java/lang/NoSuchMethodError" => Some("java.lang.NoSuchMethodError"),
                    "java/lang/AbstractMethodError" => Some("java.lang.AbstractMethodError"),
                    "java/lang/IncompatibleClassChangeError" => {
                        Some("java.lang.IncompatibleClassChangeError")
                    }
                    "java/lang/LinkageError" => Some("java.lang.LinkageError"),
                    "java/lang/VerifyError" => Some("java.lang.VerifyError"),
                    "java/lang/ClassFormatError" => Some("java.lang.ClassFormatError"),
                    "java/lang/UnsatisfiedLinkError" => Some("java.lang.UnsatisfiedLinkError"),
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
                    static EMPTY_MSG: std::sync::OnceLock<Arc<str>> = std::sync::OnceLock::new();
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
    let obj_ref = match thread.frames[frame_idx].stack.peek_checked() {
        Ok(Value::Object(Some(obj_ref)))
            if crate::threading::thread_registry::monitor_fastpath_enabled() =>
        {
            thread.frames[frame_idx].stack.pop()?;
            obj_ref
        }
        _ => monitor_operand_slow(shared, thread, frame_idx, false)?,
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
    // Structured locking (JVMS §2.11.10, wave 23 lane L7): the release goes
    // through THIS frame's record of what its own `monitorenter`s took. A
    // monitor it entered is one compare and a pop, then the table release
    // (`monitor_exit_and_retract_jmx`, release + JMX retract as ONE call —
    // see there for why the pairing is a function). Anything else — a
    // monitor the thread holds through a CALLER's frame, or not at all — is
    // `held_monitors::monitorexit_unrecorded`'s cold decision, which throws
    // HotSpot's null-message `IllegalMonitorStateException` unless the table
    // shows an acquisition no frame recorded (a frame resumed from compiled
    // code).
    let tid = thread.thread_id;
    if thread.frames[frame_idx]
        .held_monitors
        .remove_newest(obj_ref)
    {
        return super::held_monitors::monitorexit_recorded(shared, tid, obj_ref);
    }
    super::held_monitors::monitorexit_unrecorded(shared, thread, frame_idx, obj_ref)
}

/// Pop the `monitorenter` / `monitorexit` operand for every operand shape the
/// fast path in those two handlers does not take: a null reference, an
/// `Uninitialized` slot, or a `long` / `double` slot carrying a smuggled
/// `jobject`. `pop_object_ref_ctx_with` is what classifies those, and the
/// JEP 358 message it may need is what makes the path expensive.
///
/// Split out so the common case costs nothing. Building the message context
/// requires three `Arc` clones (frame code, method name, method descriptor)
/// because the formatting closure cannot borrow the frame while the stack is
/// borrowed mutably, and until 2026-09-08 both handlers paid for them on every
/// single acquire and release. `perf` on `probes/SyncCost.java`'s
/// `synchronized`-block loop put the two prologues at 16.7% of the loop against
/// 3.8% for `MonitorTable::{enter_or_contend,exit}` -- four times the cost of
/// the locking they introduce.
///
/// Behaviour here is verbatim what those handlers used to do inline, so every
/// non-fast operand shape still produces the identical exception and message.
#[cold]
fn monitor_operand_slow(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    entering: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let pc_snap = thread.frames[frame_idx].pc;
    let jep358 = crate::runtime::env_cache::helpful_npe_opcodes();
    let npe_code = Arc::clone(&thread.frames[frame_idx].code);
    let npe_cid = thread.frames[frame_idx].class_id;
    let npe_mname = thread.frames[frame_idx].method_name_arc();
    let npe_mdesc = thread.frames[frame_idx].method_descriptor_arc();
    let npe_bci = thread.frames[frame_idx].last_instr_pc;
    let frame_ref = &thread.frames[frame_idx];
    // Cast: reinterpret pointer/address to typed pointer
    let cls_ptr = frame_ref.class_name() as *const str;
    // Cast: reinterpret pointer/address to typed pointer
    let mth_ptr = frame_ref.method_name() as *const str;
    let stack = &mut thread.frames[frame_idx].stack;
    pop_object_ref_ctx_with(stack, &shared.mem.heap, || {
        if jep358 {
            // HotSpot names the two opcodes apart (`bytecodeUtils.cpp`,
            // `print_NPE_failed_action`): `monitorexit` on null is "Cannot
            // exit synchronized block", not the `monitorenter` text this arm
            // used to print for both.
            let action = if entering {
                crate::runtime::exceptions::helpful_npe::action_monitor()
            } else {
                monitorexit_npe_action().to_string()
            };
            helpful_npe_opcode_message_parts(
                shared, npe_cid, &npe_code, &npe_mname, &npe_mdesc, npe_bci, &action, 0,
            )
        } else {
            // SAFETY: cls/mth originate from `frame_ref.inner`, which is not
            // mutated by the stack ops `pop_object_ref_ctx_with` performs; the
            // pointers are valid for the duration of the closure call.
            let cls = unsafe { &*cls_ptr };
            let mth = unsafe { &*mth_ptr };
            let op = if entering {
                "monitorenter"
            } else {
                "monitorexit"
            };
            format!("{op} in {cls}.{mth} pc={pc_snap}")
        }
    })
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
    let obj_ref = match thread.frames[frame_idx].stack.peek_checked() {
        Ok(Value::Object(Some(obj_ref)))
            if crate::threading::thread_registry::monitor_fastpath_enabled() =>
        {
            thread.frames[frame_idx].stack.pop()?;
            obj_ref
        }
        _ => monitor_operand_slow(shared, thread, frame_idx, true)?,
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
    // re-reads it from that fixed local.
    //
    // gen r4w3/rooting: the contended wait is a GC point, so the
    // pre-block `obj_ref` may name a vacated from-space address.
    // Shadow it with the returned (possibly relocated) ref: the JFR
    // event and `set_jfr_enter_recorded` below key on it.
    let obj_ref = crate::vm::monitor_enter_blocking(shared, thread, obj_ref);
    // The frame's structured-locking record (JVMS §2.11.10, wave 23 lane L7;
    // `interpreter::held_monitors`). Recorded with the returned — possibly
    // relocated — reference, before anything below can collect; from here on
    // the record is a GC root like `monitor_on_exit`. Inline for the first two
    // entries, so this is a store and a length bump.
    thread.frames[frame_idx].held_monitors.push(obj_ref);
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

/// `instanceof`'s assignability test for a NON-ARRAY receiver against an
/// already-resolved non-array target: the class hierarchy, then every
/// fail-open fallback the opcode consults, in the opcode's order.
///
/// Shared with `invokedynamic`'s `typeSwitch` type labels (`case Foo f` is
/// JLS `instanceof`), which used to answer with a bare `is_subclass_of` and so
/// could take a different branch than an `instanceof` over the same object.
/// One predicate, two callers.
///
/// `obj_ref` must be rooted by the caller or be re-read by it from this
/// `&mut` afterwards: the display-class arm can load a class (a safepoint) and
/// writes the forwarded reference back.
pub(crate) fn object_is_instance_of_resolved(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: &mut ObjectRef,
    obj_class_id: ClassId,
    target_class_id: ClassId,
    target_class_name: &str,
) -> bool {
    // `is_subclass_of` is bound in a statement OF ITS OWN, and that is
    // load-bearing: a `class_manager.read()` temporary lives to the end of the
    // statement that creates it, and every predicate after it takes the same
    // lock again, and `display_class_satisfies_target` loads a class (the
    // proxy predicate took the WRITE lock too until wave 17; its resolved
    // form below takes none).
    // `parking_lot::RwLock` is task-fair, so even a nested READ blocks behind
    // a queued writer. Until 2026-09-23 the guard spanned the five predicates
    // after it (it was dropped before the display arm only). Same trap as
    // `resolve_component` in `typecheck::array_is_assignable_to_impl`.
    // (`class_is_subtype` takes no lock at all once the closure is published.)
    let by_hierarchy = class_is_subtype(shared, obj_class_id, target_class_id);
    let assignable = by_hierarchy
        // Same-name-other-loader is assignable in the default mode only. Under
        // `--jdk-only` two loaders' `p.X` are two classes, as on HotSpot; the
        // JIT's compile-time resolvers stopped handing fork code an ancestor's
        // copy (`class_resolved_without_loading`), which is what this rule was
        // hiding.
        || (!shared.config.is_jdk_only()
            && loader_aware_name_assignable(
                shared,
                obj_class_id,
                target_class_id,
                target_class_name,
            ))
        || lambda_proxy_satisfies(shared, obj_class_id, target_class_id)
        || synthetic_implements(shared, obj_class_id, target_class_name)
        || proxy_instance_satisfies_resolved(shared, *obj_ref, target_class_name, target_class_id)
        || annotation_proxy_satisfies_target(shared, *obj_ref, target_class_name);
    // LAST, after every cheap predicate: the receiver's `getClass()` display
    // class. `Class.isInstance` has consulted it since the Spring
    // `GenericConversionService` fix and the opcodes never did, so the two
    // doors disagreed about one object at one instant — MEASURED on seven of
    // seven immutable/unmodifiable receivers. H18-1.
    assignable
        || display_class_satisfies_target(shared, thread, obj_ref, obj_class_id, target_class_id)
}

/// `instanceof` — the non-throwing sibling of `checkcast`.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
///
/// Wave 27 (lane L7): this is the pop, the cast-site probe and its answers;
/// everything a probe cannot answer is [`instanceof_full_path`], out of line.
/// `#[inline(never)]` (it was `#[inline]`): the dispatch loop's `0xc1` arm is
/// then the same call in every build, whatever fat LTO's inliner decides for
/// this body as the code around it changes.
#[inline(never)]
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
        Value::Object(Some(obj_ref)) => {
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
            // `cached`: the entry's resolution on a hit neither memo could
            // answer, which the full path reuses instead of resolving again.
            // `cast_epochs`: the resolution fill's epochs (a miss).
            // `memo_epochs`: the negative memo's (a miss, or a hit the
            // positive memo could not use).
            let (cached, cast_epochs, memo_epochs) = if cast_site_cache_enabled() {
                // Read BEFORE the probe and BEFORE resolving; see
                // `SiteCache::put` and `cast_site_hit_admits`. The memo
                // epoch, too: every verdict memoised below is computed
                // after it (`CastSite::observed_at`).
                let epochs_before = CastSiteCache::epochs_for(shared);
                let memo_epoch_before = cast_memo_epoch_in(shared);
                match thread
                    .cast_sites
                    .get(referencing_class_id, *index)
                    .copied()
                    .map(|site| site.observed_at(memo_epoch_before))
                {
                    Some(site) => {
                        // Negative memo: this receiver's class was refused
                        // here by the full path, and the refusal depended
                        // on nothing but the class. No lock, no name.
                        if negative_cast_memo_enabled()
                            && negative_memo_answers(shared, &site, obj_ref)
                        {
                            if cast_memo_crosscheck() {
                                crosscheck_negative_memo(
                                    shared,
                                    thread,
                                    referencing_class_id,
                                    *index,
                                    site.target,
                                    obj_ref,
                                );
                            }
                            site_stats::bump(site_stats::CAST_NEG_HIT);
                            thread.frames[frame_idx].stack.push(Value::Int(0))?;
                            return Ok(());
                        }
                        // GC-safety: safepoint-free — see
                        // `cast_site_hit_admits`. The slow path below still
                        // pins, because loader-aware resolution there
                        // genuinely can safepoint.
                        if cast_site_hit_admits(
                            shared,
                            thread,
                            referencing_class_id,
                            *index,
                            epochs_before,
                            site,
                            obj_ref,
                        ) {
                            site_stats::bump(site_stats::CAST_HIT);
                            thread.frames[frame_idx].stack.push(Value::Int(1))?;
                            return Ok(());
                        }
                        site_stats::bump(site_stats::CAST_UNUSABLE);
                        (
                            Some(site.target),
                            None,
                            Some((epochs_before, memo_epoch_before)),
                        )
                    }
                    None => {
                        site_stats::bump(site_stats::CAST_MISS);
                        (
                            None,
                            Some(epochs_before),
                            Some((epochs_before, memo_epoch_before)),
                        )
                    }
                }
            } else {
                (None, None, None)
            };
            return instanceof_full_path(
                shared,
                thread,
                frame_idx,
                *index,
                obj_ref,
                referencing_class_id,
                (cached, cast_epochs, memo_epochs),
            );
        }
        _ => thread.frames[frame_idx].stack.push(Value::Int(0))?,
    }
    Ok(())
}

/// The full `instanceof` path (resolution, the array rule, the fail-open
/// admissions, the negative memo's fill) for a receiver the cast-site memos
/// above could not answer. `probe` is what the probe learnt:
/// `(cached, cast_epochs, memo_epochs)` as [`op_instanceof`] names them.
///
/// Out of line (interpreter round i1 wave 27, lane L7), moved verbatim: a
/// memo HIT -- every execution of a warm type-check site -- runs only the
/// probe above, so the dispatch loop's call reaches a small function whose
/// hot path fat LTO lays out and register-allocates on its own, instead of
/// the probe sharing one frame, prologue and inlining budget with this
/// resolution and verdict code (which grew and shrank the whole function,
/// and with it the probe's code, as unrelated code changed).
#[inline(never)]
fn instanceof_full_path(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
    mut obj_ref: ObjectRef,
    referencing_class_id: ClassId,
    probe: CastProbeOutcome,
) -> Result<(), MethodCallFailed> {
    let (cached, cast_epochs, memo_epochs) = probe;
    // `index` is rebound as a reference so the moved body's `*index` reads
    // unchanged.
    let index = &index;
    // BEFORE the pool read: `resolve_cast_target` records a failure
    // under it (i22-L5, wave 24).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let target_class_name = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(referencing_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;
        // The pool's interned `Arc<str>`: a refcount bump, where
        // `.to_string()` allocated a copy on every miss execution.
        class
            .constant_pool
            .get_class_name_arc(*index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{index}"),
            })?
    };
    let receiver_is_array = shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array;
    let target_is_array = target_class_name.starts_with('[');
    // JVMS §6.5 / §5.4.3.1: resolve the target (and check access,
    // and fill the cast table) whatever either side is (i11-L2).
    // GC-safety: this object has been popped from the operand stack
    // and is only rooted by `native_pin_roots` while resolution can
    // load a class.
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(obj_ref);
    let resolved = resolve_cast_target(
        shared,
        thread,
        (referencing_class_id, *index),
        &target_class_name,
        !receiver_is_array && !target_is_array,
        cached,
        cast_epochs,
        "instanceof",
        fill_as_of,
    );
    obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
    thread.native_pin_roots.truncate(pin);
    let target_id = resolved?;
    let result = if receiver_is_array {
        // One array rule for both opcodes (SBR-03: a genuine
        // `Object[]` is not an instance of an unrelated `T[]`); the
        // resolved target lets it compare component ids. The name
        // path inside can load, so the receiver stays pinned.
        // The memo key is read first; a move keeps it.
        let key = array_receiver_key(shared, obj_ref);
        thread.native_pin_roots.push(obj_ref);
        let ok = array_receiver_cast_verdict(shared, obj_ref, &target_class_name, target_id);
        thread.native_pin_roots.truncate(pin);
        // i16-L4: the next array of this key is answered by the hit.
        if let (true, Some(epochs), Some(target)) = (ok, memo_epochs, target_id) {
            fill_array_cast_memo(thread, (referencing_class_id, *index), epochs, target, key);
        }
        i32::from(ok)
    } else if target_is_array {
        // Non-array object is not instanceof any array type.
        0
    } else {
        let target_class_id = target_id.ok_or_else(|| VmError::Internal {
            message: format!("instanceof cp#{index}: class target left unresolved"),
        })?;
        let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
        let admitted = object_is_instance_of_resolved(
            shared,
            thread,
            &mut obj_ref,
            obj_class_id,
            target_class_id,
            &target_class_name,
        );
        if !admitted && negative_cast_memo_enabled() {
            if let Some(epochs) = memo_epochs {
                fill_negative_cast_memo(
                    shared,
                    thread,
                    (referencing_class_id, *index),
                    epochs,
                    target_class_id,
                    obj_class_id,
                );
            }
        }
        i32::from(admitted)
    };
    thread.frames[frame_idx].stack.push(Value::Int(result))?;
    Ok(())
}

/// What the cast-site probe of [`op_instanceof`] / [`op_checkcast`] hands its
/// full path: the entry's resolved target when the memos could not use it,
/// the resolution fill's epochs (a miss), and the memos' epochs.
type CastProbeOutcome = (Option<ClassId>, Option<(u64, u64)>, Option<((u64, u64), u64)>);

/// `checkcast` — JVMS 6.5 assignability, or `ClassCastException`.
///
/// Moved verbatim out of `execute_instruction`'s match arm so the
/// raw-bytecode fast path in `execute_frame_from_index` can call the SAME
/// implementation instead of carrying a second copy. Two copies of an
/// opcode is the shape `difftest`'s `interp-decoded` axis exists to catch;
/// one implementation with two callers cannot drift.
///
/// Wave 27 (lane L7): this is the pop, the cast-site probe and its hit;
/// everything a probe cannot answer is [`checkcast_full_path`], out of line,
/// and this function is `#[inline(never)]` (it was `#[inline]`), for the
/// reasons [`op_instanceof`] gives.
#[inline(never)]
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
        Value::Object(Some(obj_ref)) => {
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
            // `cached`: the entry's resolution on a hit the memo could not
            // answer, which the full path reuses (i11-L2).
            // `cast_epochs`: the resolution fill's epochs (a miss).
            // `memo_epochs`: the array memo's (i16-L4; a miss, or a hit the
            // memos could not use).
            let (cached, cast_epochs, memo_epochs) = if cast_site_cache_enabled() {
                // Read BEFORE the probe and BEFORE resolving; see
                // `SiteCache::put` and `cast_site_hit_admits`. The memo
                // epoch, too: every verdict memoised below is computed
                // after it (`CastSite::observed_at`).
                let epochs_before = CastSiteCache::epochs_for(shared);
                let memo_epoch_before = cast_memo_epoch_in(shared);
                match thread
                    .cast_sites
                    .get(referencing_class_id, *index)
                    .copied()
                    .map(|site| site.observed_at(memo_epoch_before))
                {
                    Some(site) => {
                        // GC-safety: safepoint-free — see
                        // `cast_site_hit_admits`. The slow path below still
                        // pins, because loader-aware resolution there
                        // genuinely can safepoint.
                        if cast_site_hit_admits(
                            shared,
                            thread,
                            referencing_class_id,
                            *index,
                            epochs_before,
                            site,
                            obj_ref,
                        ) {
                            site_stats::bump(site_stats::CAST_HIT);
                            thread.frames[frame_idx]
                                .stack
                                .push(Value::Object(Some(obj_ref)))?;
                            return Ok(());
                        }
                        site_stats::bump(site_stats::CAST_UNUSABLE);
                        (
                            Some(site.target),
                            None,
                            Some((epochs_before, memo_epoch_before)),
                        )
                    }
                    None => {
                        site_stats::bump(site_stats::CAST_MISS);
                        (
                            None,
                            Some(epochs_before),
                            Some((epochs_before, memo_epoch_before)),
                        )
                    }
                }
            } else {
                (None, None, None)
            };
            return checkcast_full_path(
                shared,
                thread,
                frame_idx,
                *index,
                obj_ref,
                referencing_class_id,
                (cached, cast_epochs, memo_epochs),
            );
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

/// The full `checkcast` path (resolution, the array rule, the fail-open
/// admissions, the `ClassCastException` and its forensics) for a receiver the
/// cast-site memos of [`op_checkcast`] could not answer. `probe` is what the
/// probe learnt (see [`CastProbeOutcome`]).
///
/// Out of line (interpreter round i1 wave 27, lane L7), moved verbatim; see
/// [`instanceof_full_path`] for why.
#[inline(never)]
fn checkcast_full_path(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    index: u16,
    mut obj_ref: ObjectRef,
    referencing_class_id: ClassId,
    probe: CastProbeOutcome,
) -> Result<(), MethodCallFailed> {
    let (cached, cast_epochs, memo_epochs) = probe;
    // `index` is rebound as a reference so the moved body's `*index` reads
    // unchanged.
    let index = &index;
    // BEFORE the pool read: `resolve_cast_target` records a failure
    // under it (i22-L5, wave 24).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let target_class_name = {
        let cm = shared.classes.class_manager.read();
        let class =
            cm.get_class(referencing_class_id)
                .ok_or_else(|| VmError::Internal {
                    message: "current class not found".to_string(),
                })?;
        // The pool's interned `Arc<str>`: a refcount bump, where
        // `.to_string()` allocated a copy on every miss execution.
        class
            .constant_pool
            .get_class_name_arc(*index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{index}"),
            })?
    };
    let receiver_is_array =
        shared.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array;
    let target_is_array = target_class_name.starts_with('[');
    // GC-safety: `obj_ref` has been popped off the operand stack,
    // so keep it in a remappable native root until this opcode either
    // pushes it back or decides to throw. Array assignability and
    // loader-aware resolution can both take paths that may safepoint.
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(obj_ref);
    // JVMS §6.5 / §5.4.3.1: resolve the target (and check access,
    // and fill the cast table) whatever either side is (i11-L2); see
    // the `instanceof` twin.
    let resolved = resolve_cast_target(
        shared,
        thread,
        (referencing_class_id, *index),
        &target_class_name,
        !receiver_is_array && !target_is_array,
        cached,
        cast_epochs,
        "checkcast",
        fill_as_of,
    );
    // For the failure message's loader clauses (`--jdk-only`, wave 32).
    let noted_target = resolved.as_ref().ok().copied().flatten();
    let cast_ok = if receiver_is_array {
        // One array rule for both opcodes (e.g. `int[]` is not an
        // `Object[]`); the resolved target lets it compare component
        // ids. Still pinned: its name path can load a class.
        let ok = match resolved {
            Ok(target_id) => {
                let receiver = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
                // The memo key is read first; a move keeps it.
                let key = array_receiver_key(shared, receiver);
                let ok = array_receiver_cast_verdict(
                    shared,
                    receiver,
                    &target_class_name,
                    target_id,
                );
                // i16-L4: the next array of this key is answered by
                // the hit.
                if let (true, Some(epochs), Some(target)) = (ok, memo_epochs, target_id) {
                    fill_array_cast_memo(
                        thread,
                        (referencing_class_id, *index),
                        epochs,
                        target,
                        key,
                    );
                }
                Ok(ok)
            }
            Err(e) => Err(e),
        };
        obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
        thread.native_pin_roots.truncate(pin);
        ok?
    } else if target_is_array {
        // Non-array object cannot be cast to an array type.
        obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
        thread.native_pin_roots.truncate(pin);
        resolved?;
        false
    } else {
        obj_ref = thread.native_pin_roots.get(pin).copied().unwrap_or(obj_ref);
        thread.native_pin_roots.truncate(pin);
        let target_class_id = resolved?.ok_or_else(|| VmError::Internal {
            message: format!("checkcast cp#{index}: class target left unresolved"),
        })?;
        let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
        // `is_subclass_of` in a statement of its own, so its read
        // guard is dropped before the predicates after it take the
        // lock again (the display arm can load a class). See the twin
        // in `op_instanceof`. Lock-free once the closure is published.
        let by_hierarchy = class_is_subtype(shared, obj_class_id, target_class_id);
        let assignable = by_hierarchy
            // Same-name-other-loader is assignable in the default
            // mode only. Under `--jdk-only` two loaders' `p.X` are
            // two classes, as on HotSpot; the JIT's compile-time
            // resolvers stopped handing fork code an ancestor's copy
            // (`class_resolved_without_loading`), which is what this
            // rule was hiding.
            || (!shared.config.is_jdk_only()
                && loader_aware_name_assignable(
                    shared,
                    obj_class_id,
                    target_class_id,
                    &target_class_name,
                ))
            || lambda_proxy_satisfies(shared, obj_class_id, target_class_id)
            || synthetic_implements(shared, obj_class_id, &target_class_name)
            || proxy_instance_satisfies_resolved(
                shared,
                obj_ref,
                &target_class_name,
                target_class_id,
            )
            || annotation_proxy_satisfies_target(shared, obj_ref, &target_class_name);
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
        // No value rewriting here: the three caller-name hatches that
        // used to turn a failed cast into a converted value (two Spring
        // annotation shapes, a log4j app-loader stand-in) were deleted
        // after a census of both modes read zero uses (interpreter
        // round i1, wave 10; `checkcast_has_no_caller_name_hatches`).
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
        let obj_binary =
            cce_display_class_name(shared, obj_ref, &obj_class_name).replace('/', ".");
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
                eprintln!("  CCE-BT-STK[{i}] {}.{} pc={}", cn, f.method_name(), f.pc);
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
                for (e, moved_to, mlen, as_dest) in crate::memory::gc::gcpart_probe(addr) {
                    eprintln!(
                                "  CCE-BT-GCPART epoch={e} map_len={mlen} moved_to={moved_to:x?} appears_as_dest={as_dest}"
                            );
                }
                for (ago, site) in push_prov_find(addr) {
                    eprintln!("  CCE-BT-PUSHPROV pushed {ago} pushes ago at {site}");
                }
                for (age, site, tag, s, l) in cratonvm_gc::zero_forensics::probe(addr) {
                    eprintln!(
                        "  CCE-BT-ZEROED age={age} site={} tag={tag} range=0x{s:x}+0x{l:x}",
                        if site == 1 {
                            "sweep-span"
                        } else {
                            "fromspace-reset"
                        },
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
        // old-sweep-liveness.md section 7.
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
        // Round 9 wave 8b (vmrt8 R1): the reclaimed-memory forensics below
        // scan the GC reclamation rings linearly (up to 1 M x 32 B). A
        // failing cast is ordinary Java (a type test), so pay for them once
        // per distinct (receiver class, target) pair, and always for a
        // `ClassId(0)` receiver -- see `cast_refusal_forensics_admitted`.
        if crate::runtime::exceptions::cast_refusal_forensics_admitted(
            shared.mem.heap.class_id_of(obj_ref).as_u32(),
            &target_binary,
        ) {
            let reclaimed = crate::memory::reclaim_guard::report_reclaimed_receiver_forced(
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
            //
            // GATED on the free-list verdict the line above returns.
            // Unconditional was wrong: a failing `checkcast` is ORDINARY
            // Java — `catch (ClassCastException)` is control flow in
            // `equals`, in Jackson's and Spring's type probes — and
            // `report_root_slice_provenance` ends in an unconditional
            // `tracing::error!`. With no collection yet performed the
            // published snapshot is trivially empty, so every such cast
            // printed `in_published_snapshot=false … a root COLLECTION
            // gap` at ERROR level. Six lines of Java were enough:
            //
            //     Object o = "s";
            //     try { Integer i = (Integer) o; }
            //     catch (ClassCastException e) {}
            //
            // `vm_exec.rs`'s copy of this pair already gated on the same
            // verdict; only this one did not, and the divergence is what
            // made the guard cry wolf on healthy runs. A stale reference
            // reaching a cast still reports in full — `_forced` returns
            // `true` exactly when the address really is reclaimed memory,
            // which is the only case this provenance walk can explain.
            //
            // The free-list verdict is not the only evidence: an address
            // whose block has already been handed out again is gone from
            // the free list but still in the old-gen freed ledger, which
            // the block below queries for its own report. Ask both, so
            // the re-served face — the one this site exists for — keeps
            // its provenance walk.
            if reclaimed
                || cratonvm_gc::gen_heap::old_freed_lookup_covering(
                    obj_ref.as_ptr() as usize
                )
                .is_some()
            {
                crate::memory::reclaim_guard::report_root_slice_provenance(
                    shared,
                    thread,
                    obj_ref.as_ptr() as usize,
                    "checkcast",
                );
            }
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
        }
        if obj_class_name == "java/lang/Object" && &*target_class_name != "java/lang/Object"
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
            if let Some((what, span, size)) = shared.mem.heap.reclaimed_hole_at(addr) {
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
            for (age, site, tag, s, l) in cratonvm_gc::zero_forensics::probe(addr) {
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
        if let Some(target) = noted_target {
            crate::runtime::exceptions::note_cast_operands(thread, actual_class_id, target);
        }
        return Err(RuntimeError::ClassCastException {
            message: format!("{obj_binary} cannot be cast to {target_binary}"),
        }
        .into());
    }
    thread.frames[frame_idx]
        .stack
        .push(Value::Object(Some(obj_ref)))?;
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
        Some(ClassSiteCache::epochs_for(shared))
    } else {
        None
    };
    // BEFORE the pool read: a failure is recorded under it (i22-L5, wave 24).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    // The pool's interned `Arc<str>` (a refcount bump), not a fresh `String`
    // per miss execution — the `checkcast` twin's form.
    let class_name_arc = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(referencing_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;
        class
            .constant_pool
            .get_class_name_arc(*index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{index}"),
            })?
    };
    let class_name: &str = &class_name_arc;

    // JVMS §5.4.3: a `new` whose class resolution already failed fails the
    // same way again. Initialization failures are NOT this record: an
    // erroneous class answers `NoClassDefFoundError` from its init state.
    if let Some(recorded) =
        recorded_resolution_failure(shared, thread, referencing_class_id, *index)
    {
        return Err(recorded);
    }
    let target_class_id =
        resolve_class_loader_aware(shared, thread, referencing_class_id, class_name)
            .map_err(|e| convert_class_not_found(shared, thread, class_name, e))
            .map_err(|e| {
                record_resolution_failure_as_of(
                    shared,
                    referencing_class_id,
                    *index,
                    e,
                    fill_as_of,
                )
            })?;
    // JVMS §5.4.3: a failure another thread recorded for this entry while
    // this one resolved is the entry's outcome, as on HotSpot (interpreter
    // round i1 wave 38, lane L5; `L5W37RacingEntryOutcome`, `fail-first`;
    // `--jdk-only`). Before the site-cache fill below.
    if let Some(recorded) =
        recorded_resolution_failure_after_success(shared, thread, referencing_class_id, *index)
    {
        return Err(recorded);
    }

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

    if crate::runtime::env_cache::dbg_loader_trace() && class_name.contains("RootReference") {
        let cm = shared.classes.class_manager.read();
        let ref_loader = cm.get_loader_id(referencing_class_id);
        let target_loader = cm.get_loader_id(target_class_id);
        eprintln!(
                    "[LOADER-TRACE] new thread={:?} class_name={class_name} referencing_class_id={referencing_class_id:?} referencing_loader={ref_loader:?} target_class_id={target_class_id:?} target_loader={target_loader:?}",
                    thread.thread_id
                );
        if matches!(ref_loader, Some(cratonvm_types::ClassLoaderId::Application)) {
            eprintln!("[LOADER-TRACE-STACK] full Java stack for this Application-context 'new':");
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
    //
    // Every mode, as always; through the one class-constant verdict the other
    // five instructions and the JIT's CP helper share (i9-L2), so `new` gets
    // the same HotSpot message, the hidden-class runtime package and the
    // serialization-accessor rule they do.
    let denial = {
        let cm = shared.classes.class_manager.read();
        super::constants::class_constant_access_denial_in(
            shared,
            &cm,
            referencing_class_id,
            class_name,
            Some(target_class_id),
        )
    };
    if let Some(message) = denial {
        return Err(LinkageError::IllegalAccessError { message }.into());
    }

    // JVMS 6.5 `new`: an interface or abstract class answers
    // `InstantiationError` after resolution and before initialization, as
    // HotSpot's `InterpreterRuntime::_new` orders it. Not a resolution
    // failure, so nothing is recorded; the site is never filled (below), so
    // every execution asks again. Wave 16.
    let refusal = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(target_class_id)
            .and_then(super::gc_and_alloc::new_instantiation_refusal)
    };
    if let Some((error_class, message)) = refusal {
        let exc = crate::runtime::exceptions::create_exception_object(
            shared,
            thread,
            error_class,
            Some(&message),
        )?;
        return Err(MethodCallFailed::ExceptionThrown(exc));
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
    // class with a loader namespace unless its loader recorded this
    // answer (`site_fill_admitted`, wave 24), and for an array name — `new` on
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
        } else if class_name.starts_with('[')
            || !site_fill_admitted(
                shared,
                thread,
                referencing_class_id,
                class_name,
                target_class_id,
            )
        {
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
    if class_name == "java/util/NoSuchElementException" && crate::runtime::env_cache::nsee_trace() {
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
///
/// `#[inline(never)]` since interpreter round i1 wave 27 (lane L7; it was
/// `#[inline]`): the dispatch loop reaches it only when the quickened
/// `field_fast::putfield_fast` declined, and a ~420-line body must not be
/// inlined into that loop on the whim of fat LTO's cost model (see
/// `op_instanceof`).
#[inline(never)]
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
    let ff_epochs = super::field_fast::fill_epochs(shared);
    let mut field = resolve_field_ref_loader_aware(shared, thread, current_class_id, *index)?;
    // JVMS §6.5 putfield: a static field is an IncompatibleClassChangeError.
    check_field_staticness(shared, current_class_id, *index, &field, false)?;
    // JVMS §6.5 putfield: a final field only from its class's `<init>`.
    check_final_field_put(
        shared,
        current_class_id,
        thread.frames[frame_idx].method_name(),
        *index,
        &field,
    )?;
    // JDWP FieldModification (interpreter round i1 wave 10, lane L1): before
    // the pops, so the receiver and the new value stay on the operand stack
    // (GC roots) if the event parks the thread.
    if crate::runtime::jvmti::any_field_watchpoint_active() {
        super::deliver_field_watch_if_armed(shared, thread, frame_idx, &field, true);
    }
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
            // Kinds-aware bit-exact pop — see `pop_static_field_value`'s
            // D arm, which this mirrors for putfield.
            if thread.frames[frame_idx].stack.peek_kind_is_double() {
                let cv = thread.frames[frame_idx].stack.pop_compact_checked()?;
                Value::Double(f64::from_bits(cv.raw_bits()))
            } else {
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
        diag_putfield_consolidated(
            shared,
            thread,
            frame_idx,
            current_class_id,
            *index,
            obj_ref,
            &field,
            value,
        )?;
    } // end `if any_field_diag()` — consolidated putfield diagnostics
      // T17.Δ.4 — JVMTI FieldModification watchpoint, scoped to this VM.
    if crate::runtime::jvmti::any_field_watchpoint_active() {
        let method_id = jvmti_method_id(shared.vm_identity, &thread.frames[frame_idx]);
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
    // address watch list). See bug-h2-suite-residual-fail-triage-FIXED.md.
    if crate::runtime::env_cache::dbg_field_watch() {
        diag_putfield_watch(
            shared,
            thread,
            frame_idx,
            current_class_id,
            *index,
            obj_ref,
            &field,
            value,
            old_value,
        )?;
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
    super::field_fast::fill_site(
        shared,
        thread,
        current_class_id,
        *index,
        obj_ref,
        &field,
        ff_epochs,
        true,
    );
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
///
/// `#[inline(never)]` since interpreter round i1 wave 27 (lane L7; it was
/// `#[inline]`): the dispatch loop reaches it only when the quickened
/// `field_fast::getfield_fast` declined (or on a heap without the quickened
/// arms), and a ~360-line body must not be inlined into that loop on the whim
/// of fat LTO's cost model (see `op_instanceof`).
#[inline(never)]
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
    //
    // Resolve FIRST, while the receiver is still on the operand stack, as
    // `op_putfield` already does. JVMS §6.5 (and HotSpot) raise the linking
    // exceptions -- NoSuchFieldError, the IncompatibleClassChangeError below
    // -- before the null check, and a cold resolution can load classes and
    // allocate: with the receiver still in its stack slot it is a root for
    // that whole window instead of a bare Rust local.
    let ff_epochs = super::field_fast::fill_epochs(shared);
    let mut field = resolve_field_ref_loader_aware(shared, thread, current_class_id, *index)?;
    // JVMS §6.5 getfield: a static field is an IncompatibleClassChangeError.
    check_field_staticness(shared, current_class_id, *index, &field, false)?;
    // JDWP FieldAccess (interpreter round i1 wave 10, lane L1): before the
    // pop, so the receiver stays on the operand stack if the event parks.
    if crate::runtime::jvmti::any_field_watchpoint_active() {
        super::deliver_field_watch_if_armed(shared, thread, frame_idx, &field, false);
    }
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
    // (Round i1 moved the resolution above the pop, so that window is now
    // closed at the source; the heal is kept as the cheap backstop it is.)
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
    // CRATONVM_DBG_STRAYSTACK, the getfield door -- the READ twin of
    // the putfield dump above. Deliberately outside `any_field_diag()`,
    // exactly like its putfield sibling, so the two doors are armed by
    // one variable and cannot drift.
    if straystack_enabled() {
        diag_getfield_straystack(
            shared,
            thread,
            frame_idx,
            current_class_id,
            *index,
            obj_ref,
            &field,
        )?;
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
        diag_getfield_consolidated(
            shared,
            thread,
            frame_idx,
            current_class_id,
            *index,
            obj_ref,
            &field,
        )?;
    } // end `if any_field_diag()` — consolidated getfield diagnostics
      // Read side of the [PUTFIELD-WATCH] ledger further down: with both
      // halves on one filter a "the constructor stored it but the reader
      // sees null" question is answerable from a single log, without
      // guessing which of the two sides is wrong. Same class filter
      // (`CRATONVM_DBG_FIELD_WATCH=<substr>[,<substr>…]`).
    if crate::runtime::env_cache::dbg_field_watch() {
        diag_getfield_watch(
            shared,
            thread,
            frame_idx,
            current_class_id,
            *index,
            obj_ref,
            &field,
        )?;
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
    if crate::runtime::jvmti::any_field_watchpoint_active() {
        let method_id = jvmti_method_id(shared.vm_identity, &thread.frames[frame_idx]);
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
    super::field_fast::fill_site(
        shared,
        thread,
        current_class_id,
        *index,
        obj_ref,
        &field,
        ff_epochs,
        false,
    );
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
    // Quickened site (wave 5): resolved, initialized, non-final; shares
    // `op_getstatic`'s table (see `field_fast::putstatic_site_hit`).
    if super::field_fast::putstatic_site_hit(shared, thread, frame_idx, index)? {
        return Ok(());
    }
    // `index` is rebound as a reference so the moved body's `*index`
    // reads unchanged — this is a pure move, not a rewrite.
    let index = &index;
    let current_class_id = thread.frames[frame_idx].class_id;
    let field = {
        let res = resolve_field_ref_loader_aware(shared, thread, current_class_id, *index);
        match res {
            Ok(f) => f,
            Err(e) => {
                let name =
                    field_ref_class_name(shared, current_class_id, *index).unwrap_or_default();
                return Err(convert_class_not_found(shared, thread, &name, e));
            }
        }
    };
    // JVMS §6.5 putstatic: an instance field is an IncompatibleClassChangeError,
    // raised at resolution -- before the holder is initialised.
    check_field_staticness(shared, current_class_id, *index, &field, true)?;
    // JVMS §6.5 putstatic: a final field only from its class's `<clinit>`.
    check_final_field_put(
        shared,
        current_class_id,
        thread.frames[frame_idx].method_name(),
        *index,
        &field,
    )?;
    ensure_class_initialized_shared(shared, thread, field.declaring_class_id)?;
    // T17.Δ.4 — JVMTI FieldModification watchpoint, scoped to this VM. After
    // the holder's initialization, as HotSpot posts it (its `putstatic`
    // template resolves -- which initializes -- and only then posts): a
    // `<clinit>` that throws produces no event.
    if crate::runtime::jvmti::any_field_watchpoint_active() {
        let method_id = jvmti_method_id(shared.vm_identity, &thread.frames[frame_idx]);
        crate::runtime::jvmti::fire_field_modification_if_watched_for_vm(
            shared.vm_identity,
            thread.thread_id.0,
            method_id,
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            field.declaring_class_id.as_u32() as u64,
            field.field_index,
        );
        // JDWP FieldModification (wave 10, lane L1), before the pop.
        super::deliver_field_watch_if_armed(shared, thread, frame_idx, &field, true);
    }
    // T10.9.D K3 — Pop via the descriptor-aware path so category-2
    // primitives (J/D) keep their exact 64-bit payload.  The naïve
    // `pop()?` decodes untagged long bits as Value::Double, which
    // then re-encodes as a double on the next push — silently
    // corrupting every J/D static.
    let desc_byte = Some(field.desc_byte);
    let value = pop_static_field_value(&mut thread.frames[frame_idx].stack, desc_byte)?;
    // The SATB pre-barrier on the overwritten reference is taken inside
    // `set_static_shared` (both its lock-free and its locked path), so the
    // separate read of the old value that used to sit here is gone.
    // Volatile static fields: emit memory fence before write
    if field.is_volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    set_static_shared(shared, field.declaring_class_id, field.field_index, value);
    if field.is_volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    // The entry also serves `getstatic`, so it must meet that opcode's
    // admission too: never a reference static the `Boolean` repair could
    // still serve (`fill_static_site` screens the rest, `System` included).
    let repairable =
        field.is_reference && static_may_be_boolean_constant(shared, field.declaring_class_id);
    if !repairable {
        super::field_fast::fill_static_site(shared, thread, frame_idx, *index, &field);
    }
    Ok(())
}

/// Could `class_id` be `java/lang/Boolean`, whose `TRUE`/`FALSE` the
/// synthetic-`Boolean` repair in [`op_getstatic`] serves?
///
/// One relaxed load and a compare once this VM has prepared `Boolean`
/// (`ClassRealm::boolean_class_id`). `true` before that, which sends the caller
/// to the name comparison it always made, so a VM whose `Boolean` never passes
/// through `prepare_class_shared` keeps the old behaviour instead of losing
/// the repair.
#[inline]
fn static_may_be_boolean_constant(shared: &SharedVm, class_id: ClassId) -> bool {
    let known = shared
        .classes
        .boolean_class_id
        .load(std::sync::atomic::Ordering::Relaxed);
    known == u32::MAX || known == class_id.as_u32()
}

/// Synthetic-`Boolean` repair for a reference static that read back `null`:
/// `TRUE`/`FALSE` of a `Boolean` without a `<clinit>` are materialised,
/// stored back, and returned; any other static returns `value` unchanged.
///
/// Screened by class IDENTITY before any lock: this used to take the
/// `class_manager` read lock and compare the class name on EVERY `getstatic`
/// that read `Object(None)` — every legitimately null reference static (lazy
/// singletons, `if (INSTANCE == null)`), and zero-initialized primitive
/// statics too, which can decode as `Object(None)` through the `Value`
/// boundary. The name path remains only while this VM has not yet prepared
/// `Boolean`.
///
/// Shared by `op_getstatic` and, since i1 wave 5, the JIT's `jit_getstatic`
/// helper, which returned the `null` instead: a compiled method whose first
/// `Boolean.TRUE` read ran compiled saw `null` where the interpreter (and
/// HotSpot, whose `Boolean` always has its `<clinit>`) sees `Boolean.TRUE`.
/// `field_index` is the static-field index `get_static_shared` takes.
pub(crate) fn repair_boolean_constant_static(
    shared: &SharedVm,
    thread: &mut JvmThread,
    declaring_class_id: ClassId,
    field_index: usize,
    value: Value,
) -> Result<Value, MethodCallFailed> {
    if !matches!(value, Value::Object(None))
        || !static_may_be_boolean_constant(shared, declaring_class_id)
    {
        return Ok(value);
    }
    let boolean_const = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(declaring_class_id)
            .filter(|c| &*c.name == "java/lang/Boolean")
            .and_then(|c| {
                let mut static_idx = 0usize;
                for f in &c.fields {
                    if f.is_static() {
                        if static_idx == field_index {
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
    let Some(b) = boolean_const else {
        return Ok(value);
    };
    let obj = gc_alloc_object(shared, thread, declaring_class_id, 1)?;
    shared.mem.heap.set_field(obj, 0, Value::Int(i32::from(b)));
    let repaired = Value::Object(Some(obj));
    set_static_shared(shared, declaring_class_id, field_index, repaired);
    Ok(repaired)
}

/// Could `class_id` be THIS VM's `java/lang/System`, whose `out`/`err`/`in`
/// the bootstrap intercept in [`op_getstatic`] serves?
///
/// One relaxed load and a compare once the VM knows System's id
/// (`ClassRealm::system_class_id`, recorded by `prepare_class_shared` or
/// learned here). Until then, one locked name lookup per call, which records
/// the id as soon as `System` is defined in this VM.
///
/// i1 wave 5, lane L4: this replaces `classloading::class_is_java_lang_system`,
/// a process-global latch holding the id of whichever VM defined `System`
/// last. A second VM in the process therefore screened its `getstatic`s
/// against another VM's id, and — because the arm it gated did not re-check
/// the class name — a class of that VM whose id collided and which declared a
/// static named `out`, `err` or `in` was served the `System` stream. The JIT's
/// `jit_getstatic` asks the same screen (it replaced that helper's
/// first-VM-owned `system_class_memo`).
#[inline]
pub(crate) fn static_may_be_system(shared: &SharedVm, class_id: ClassId) -> bool {
    let known = shared
        .classes
        .system_class_id
        .load(std::sync::atomic::Ordering::Relaxed);
    if known != u32::MAX {
        return known == class_id.as_u32();
    }
    learn_system_class_id(shared, class_id)
}

/// Cold half of [`static_may_be_system`]: find `java/lang/System` by name (a
/// `java.*` name only the boot loader can define) and record its id for this
/// VM. `false` while it is not defined, which is exact: a field resolved to a
/// class that does not exist yet cannot be one of `System`'s.
#[cold]
#[inline(never)]
fn learn_system_class_id(shared: &SharedVm, class_id: ClassId) -> bool {
    let found = shared
        .classes
        .class_manager
        .read()
        .find_bootstrap_class_by_name("java/lang/System");
    match found {
        Some(id) => {
            shared
                .classes
                .system_class_id
                .store(id.as_u32(), std::sync::atomic::Ordering::Relaxed);
            id == class_id
        }
        None => false,
    }
}

/// A `java/lang/System` static the bootstrap intercept in [`op_getstatic`] and
/// the JIT's `jit_getstatic` helper serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemStreamField {
    Out,
    Err,
    In,
}

/// Is the `static_index`-th static field of `class_id` one of this VM's
/// `System.out` / `err` / `in`? Takes the `class_manager` read lock; callers
/// screen with [`static_may_be_system`] first.
///
/// `static_index` is a static `ResolvedField::field_index`, an index among the
/// class's STATIC fields (`get_static_shared`'s index space), so the field is
/// found by counting statics, as `repair_boolean_constant_static` does.
/// Until i1 wave 6 both intercepts indexed `Class::fields` (every field) with
/// it, which names the wrong field once an instance field precedes the
/// streams; JDK `System` declares none, so this was latent.
pub(crate) fn system_stream_field(
    shared: &SharedVm,
    class_id: ClassId,
    static_index: usize,
) -> Option<SystemStreamField> {
    let cm = shared.classes.class_manager.read();
    let class = cm
        .get_class(class_id)
        .filter(|c| &*c.name == "java/lang/System")?;
    let field = class
        .fields
        .iter()
        .filter(|f| f.is_static())
        .nth(static_index)?;
    match &*field.name {
        "out" => Some(SystemStreamField::Out),
        "err" => Some(SystemStreamField::Err),
        "in" => Some(SystemStreamField::In),
        _ => None,
    }
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
    // Quickened site: resolved, initialized, plain (see `fill_static_site`).
    if super::field_fast::getstatic_site_hit(shared, thread, frame_idx, index)? {
        return Ok(());
    }
    // `index` is rebound as a reference so the moved body's `*index`
    // reads unchanged — this is a pure move, not a rewrite.
    let index = &index;
    let current_class_id = thread.frames[frame_idx].class_id;
    let field = {
        let res = resolve_field_ref_loader_aware(shared, thread, current_class_id, *index);
        match res {
            Ok(f) => f,
            Err(e) => {
                let name =
                    field_ref_class_name(shared, current_class_id, *index).unwrap_or_default();
                return Err(convert_class_not_found(shared, thread, &name, e));
            }
        }
    };
    // JVMS §6.5 getstatic: an instance field is an IncompatibleClassChangeError,
    // raised at resolution -- before the holder is initialised.
    check_field_staticness(shared, current_class_id, *index, &field, true)?;
    // T17.Δ.4 — JVMTI FieldAccess watchpoint.  Consults *this VM's*
    // watchpoint row; a single HashMap read + branch on the no-watch
    // path. The process-wide `any_field_watchpoint_active()` union
    // gate inside the callee stays as the cheap pre-filter — it may
    // say "yes" because another VM is watching, and the per-VM lookup
    // then answers exactly. Fired after the holder's initialization on the
    // ordinary arm below, as HotSpot posts it (a `<clinit>` that throws
    // produces no event); the `System` stream arm initializes nothing.
    let fire_access_watch = |thread: &mut JvmThread| {
        if crate::runtime::jvmti::any_field_watchpoint_active() {
            let method_id = jvmti_method_id(shared.vm_identity, &thread.frames[frame_idx]);
            crate::runtime::jvmti::fire_field_access_if_watched_for_vm(
                shared.vm_identity,
                thread.thread_id.0,
                method_id,
                // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
                field.declaring_class_id.as_u32() as u64,
                field.field_index,
            );
            // JDWP FieldAccess (wave 10, lane L1), before the push.
            super::deliver_field_watch_if_armed(shared, thread, frame_idx, &field, false);
        }
    };

    // Bootstrap intercept: System.out / System.err / System.in
    //
    // The real JDK's `System.<clinit>` depends on a complex
    // initialization chain (SecurityManager, Charset, etc.)
    // that isn't fully bootable yet. To allow real JDK classes
    // to call `System.out.println`, we intercept the getstatic
    // on the three standard streams and return our pre-built
    // synthetic PrintStream objects. `System.in` uses the same
    // early pinning strategy via [`ensure_system_stdin_object`].
    //
    // The screen is a one-integer compare, NOT a class-manager lookup.
    //
    // This used to take a `class_manager` **read lock**, call `get_class`, and
    // compare the class's name against the literal `"java/lang/System"` — on
    // every `getstatic` in the program — to decide whether this was one of
    // three bootstrap fields. Measured 2026-09-05, a `getstatic` + `putstatic`
    // pair cost 86.4 ns against HotSpot's 3.45 (25x).
    //
    // `static_may_be_system` answers from this VM's recorded `System` id (one
    // relaxed load and a compare; wave 5 replaced a process-global latch that
    // a second VM overwrote). The lock and the `to_string` now happen only for
    // a field of `java/lang/System` itself, which is where they were always
    // going.
    //
    // `CRATONVM_JIT_NO_SYSTEM_CLASS_LATCH=1` (`no_system_class_latch`) keeps the
    // pre-2026-09-05 shape -- the locked name check on every `getstatic` -- so
    // the two arms are comparable inside one binary. Otherwise the name is
    // re-checked under the lock only behind the id screen: an id match is only
    // ever as good as the id recorded, and this arm hands out streams. Either
    // way no `String` is built (the old arm allocated the field name).
    let stream_field = if crate::runtime::env_cache::no_system_class_latch()
        || static_may_be_system(shared, field.declaring_class_id)
    {
        system_stream_field(shared, field.declaring_class_id, field.field_index)
    } else {
        None
    };
    // T10.9.D K3 — Resolve the field's declared descriptor byte so
    // category-2 primitives (J/D) get pushed with the correct
    // CompactValue tag instead of round-tripping through `Value`.
    // The Value boundary would encode `Value::Long(x)` as untagged
    // raw bits; a later `to_value()` decodes those bits as
    // `Value::Double`, silently corrupting the long on every read.
    let desc_byte = Some(field.desc_byte);
    if let Some(stream_field) = stream_field {
        fire_access_watch(&mut *thread);
        if stream_field != SystemStreamField::In {
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
            let overridden =
                match get_static_shared(shared, field.declaring_class_id, field.field_index) {
                    Value::Object(Some(s)) => Some(s),
                    _ => None,
                };
            let stream = match overridden {
                Some(s) => s,
                None => {
                    let (out, err) = shared.ensure_system_streams();
                    if stream_field == SystemStreamField::Out {
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
        } else {
            // Honor `System.setIn`, exactly as the `out`/`err` arm above honors
            // `setOut`/`setErr`: read the STATIC FIELD first and fall back to
            // the canonical stdin only when it is absent.
            //
            // This arm used to go straight to `ensure_system_stdin_object`, so
            // the field write that `System.setIn` performs was never read back
            // and `setIn` could not be observed AT ALL -- the field held the
            // caller's stream and every `getstatic System.in` answered the
            // original. A test harness feeding stdin through `System.setIn` --
            // which is the ordinary way to do it -- silently read the real
            // process stdin instead. MEASURED by
            // `apps/probes/SystemRuntimeObjectSweep.java`: `System.in ==
            // replacement` was false while reflection on the field said true.
            //
            // The asymmetry is the point: two arms of one `if`/`else if` chain,
            // for three fields with identical semantics, and only one of them
            // consulted the field.
            let overridden =
                match get_static_shared(shared, field.declaring_class_id, field.field_index) {
                    Value::Object(Some(s)) => Some(s),
                    _ => None,
                };
            let stdin = match overridden {
                Some(s) => s,
                None => ensure_system_stdin_object(shared, thread)?,
            };
            if remap_trace_on() {
                push_prov_record(stdin.as_ptr() as usize, "getstatic-stream");
            }
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(stdin)))?;
        }
    } else {
        // Every other static, `System`'s other fields included (the site fill
        // below refuses a `System` field).
        ensure_class_initialized_shared(shared, thread, field.declaring_class_id)?;
        fire_access_watch(&mut *thread);
        let mut value = get_static_shared(shared, field.declaring_class_id, field.field_index);
        if field.is_reference && matches!(value, Value::Object(None)) {
            value = repair_boolean_constant_static(
                shared,
                thread,
                field.declaring_class_id,
                field.field_index,
                value,
            )?;
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
        // A reference static the `Boolean` repair above could still serve must
        // keep coming through here; every other plain site may be quickened.
        let repairable =
            field.is_reference && static_may_be_boolean_constant(shared, field.declaring_class_id);
        if !repairable {
            super::field_fast::fill_static_site(shared, thread, frame_idx, *index, &field);
        }
    }
    Ok(())
}

/// What the RECEIVER of a watched field access actually is, at the moment of
/// the access.
///
/// The ledger used to print only the receiver's ADDRESS, and that is not
/// enough to answer the one question a "the constructor stored it but the
/// reader sees null" log is opened to answer: is this the same object the
/// constructor wrote to, moved, or a DIFFERENT object now living at that
/// address? The address alone cannot tell those apart — a young semispace is
/// re-served from the same base every cycle and the old gen allocates out of a
/// free list, so one address names many objects over a run.
///
/// So: the receiver's own class (not the field's declaring class, which is
/// already printed and is a static property of the bytecode), its `num_slots`,
/// its `gc_flags` (`0x…` — the compact bit lives there), and whether the heap
/// still recognises the address as an object start at all.
fn watch_receiver_note(shared: &SharedVm, obj_ref: ObjectRef, field_index: usize) -> String {
    let recognised = shared
        .mem
        .heap
        .is_object_address(obj_ref.as_ptr() as usize)
        .is_some();
    let header = shared.mem.heap.get_header(obj_ref);
    let recv_cid = header.class_id.as_u32();
    let recv_name = cratonvm_gc::gc::resolve_class_info(recv_cid)
        .map(|(n, _)| n)
        .unwrap_or_else(|| "<unresolved>".to_string());
    // The BYTE ADDRESS of this field's storage, which is what a write-watch
    // (`CRATONVM_DBG_WATCH_CELL`) has to be aimed at. It is not derivable from
    // the object address and the field index by the reader: a compact object
    // packs its fields at per-class offsets, so the legacy
    // `base + HEADER_SIZE + index * SLOT_SIZE` formula names the wrong cell for
    // exactly the objects this ledger is usually opened on.
    let slot_addr = if cratonvm_types::is_compact_object(header) {
        cratonvm_types::with_class_layout(recv_cid, header.num_slots(), |layout| {
            layout.field_disp(field_index)
        })
        .flatten()
        .map(|disp| obj_ref.as_ptr() as usize + disp as usize)
    } else {
        Some(
            obj_ref.as_ptr() as usize
                + cratonvm_types::HEADER_SIZE
                + field_index * cratonvm_types::SLOT_SIZE,
        )
    };
    format!(
        "recv_class={recv_name} recv_class_id={recv_cid} recv_num_slots={} \
         recv_gc_flags=0x{:x} recv_kind={} object_start={recognised} slot_addr={}",
        header.num_slots(),
        header.gc_flags(),
        cratonvm_gc::heap::ObjectHeader::kind_tag(
            header.mark_word.load(std::sync::atomic::Ordering::Relaxed)
        ),
        match slot_addr {
            Some(a) => format!("0x{a:x}"),
            None => "<no-layout>".to_string(),
        },
    )
}

/// Cold half of `op_putfield`: the diagnostics behind `crate::runtime::env_cache::dbg_field_watch()`,
/// moved out of the handler body verbatim (2026-09-02) so the unarmed
/// path keeps one gate load and none of the code. Takes the handler
/// locals the block read; a `return Err` inside the block
/// becomes an `Err` the handler propagates with `?`.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn diag_getfield_watch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    current_class_id: ClassId,
    index: u16,
    obj_ref: ObjectRef,
    field: &ResolvedField,
) -> Result<(), MethodCallFailed> {
    let index = &index;
    let _ = (
        frame_idx,
        thread.thread_id,
        current_class_id,
        obj_ref,
        field.field_index,
    );
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
            let recv = watch_receiver_note(shared, obj_ref, field.field_index);
            let fr = &thread.frames[frame_idx];
            eprintln!(
                        "[GETFIELD-WATCH] obj={:p} decl_class={} field={:?} field_index={} value={:?} {recv} in {}.{} pc={} thread={}",
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
    Ok(())
}

/// Cold half of `op_putfield`: the diagnostics behind `crate::runtime::env_cache::any_field_diag()`,
/// moved out of the handler body verbatim (2026-09-02) so the unarmed
/// path keeps one gate load and none of the code. Takes the handler
/// locals the block read; a `return Err` inside the block
/// becomes an `Err` the handler propagates with `?`.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn diag_getfield_consolidated(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    current_class_id: ClassId,
    index: u16,
    obj_ref: ObjectRef,
    field: &ResolvedField,
) -> Result<(), MethodCallFailed> {
    let index = &index;
    let _ = (
        frame_idx,
        thread.thread_id,
        current_class_id,
        obj_ref,
        field.field_index,
    );
    if crate::runtime::env_cache::any_field_diag() {
        if crate::runtime::env_cache::field_addr_dbg() {
            let field_name = resolve_field_name(shared, current_class_id, *index);
            if let Some(fname) = field_name.as_deref() {
                if matches!(
                    fname,
                    "unsharedLongs" | "threadFactory" | "runningThreads" | "submittedTaskCounter"
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
                        let field_name = resolve_field_name(shared, current_class_id, *index);
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
    }
    Ok(())
}

/// Cold half of `op_putfield`: the diagnostics behind `straystack_enabled()`,
/// moved out of the handler body verbatim (2026-09-02) so the unarmed
/// path keeps one gate load and none of the code. Takes the handler
/// locals the block read; a `return Err` inside the block
/// becomes an `Err` the handler propagates with `?`.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn diag_getfield_straystack(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    current_class_id: ClassId,
    index: u16,
    obj_ref: ObjectRef,
    field: &ResolvedField,
) -> Result<(), MethodCallFailed> {
    let index = &index;
    let _ = (
        frame_idx,
        thread.thread_id,
        current_class_id,
        obj_ref,
        field.field_index,
    );
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
    Ok(())
}

/// Cold half of `op_putfield`: the diagnostics behind `crate::runtime::env_cache::dbg_field_watch()`,
/// moved out of the handler body verbatim (2026-09-02) so the unarmed
/// path keeps one gate load and none of the code. Takes the handler
/// locals the block read and the value being stored; a `return Err` inside the block
/// becomes an `Err` the handler propagates with `?`.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn diag_putfield_watch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    current_class_id: ClassId,
    index: u16,
    obj_ref: ObjectRef,
    field: &ResolvedField,
    value: Value,
    old_value: Value,
) -> Result<(), MethodCallFailed> {
    let index = &index;
    let _ = (
        frame_idx,
        thread.thread_id,
        current_class_id,
        obj_ref,
        field.field_index,
    );
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
            let recv = watch_receiver_note(shared, obj_ref, field.field_index);
            let fr = &thread.frames[frame_idx];
            eprintln!(
                        "[PUTFIELD-WATCH] obj={:p} decl_class={} field={:?} field_index={} old={:?} new={:?} {recv} in {}.{} pc={} thread={}",
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
    Ok(())
}

/// Cold half of `op_putfield`: the diagnostics behind `crate::runtime::env_cache::any_field_diag()`,
/// moved out of the handler body verbatim (2026-09-02) so the unarmed
/// path keeps one gate load and none of the code. Takes the handler
/// locals the block read and the value being stored; a `return Err` inside the block
/// becomes an `Err` the handler propagates with `?`.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn diag_putfield_consolidated(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    current_class_id: ClassId,
    index: u16,
    obj_ref: ObjectRef,
    field: &ResolvedField,
    value: Value,
) -> Result<(), MethodCallFailed> {
    let index = &index;
    let _ = (
        frame_idx,
        thread.thread_id,
        current_class_id,
        obj_ref,
        field.field_index,
    );
    if crate::runtime::env_cache::any_field_diag() {
        // Gated diagnostic (CRATONVM_DBG_FIELDADDR): trace put for specific
        // fields — object address + resolved slot — to localize a write
        // that doesn't reach the read site.
        if crate::runtime::env_cache::field_addr_dbg() {
            let field_name = resolve_field_name(shared, current_class_id, *index);
            if let Some(fname) = field_name.as_deref() {
                if matches!(
                    fname,
                    "unsharedLongs" | "threadFactory" | "runningThreads" | "submittedTaskCounter"
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
    }
    Ok(())
}

/// Interpreter round i1, lane L2: the decoded arms' array-index, null-array and
/// cast-site-memo rules, pinned.
///
/// The end-to-end rows drive `execute_instruction` itself on a bare frame, which
/// is the handler the `interp-decoded` difftest axis runs and the one the
/// raw-bytecode loop falls back to.
#[cfg(test)]
mod l2_decoded_opcode_tests {
    use super::*;
    use crate::threading::jvm_thread::ThreadId;

    fn vm() -> Arc<SharedVm> {
        Arc::new(SharedVm::new(crate::config::VmConfig::default()))
    }

    fn thread_with_frame() -> JvmThread {
        let mut thread = JvmThread::new(ThreadId(4242), "l2-decoded");
        thread.frames.push(Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        thread
    }

    fn runtime_error(outcome: Result<InstructionResult, MethodCallFailed>) -> RuntimeError {
        match outcome {
            Err(MethodCallFailed::InternalError(VmError::Runtime(e))) => e,
            Err(other) => panic!("expected a Java runtime error, got {other:?}"),
            Ok(_) => panic!("expected a Java runtime error, the instruction completed"),
        }
    }

    fn message(err: &RuntimeError) -> Option<String> {
        err.as_java_throwable()
            .and_then(|(_, m)| m)
            .map(|m| m.into_owned())
    }

    #[test]
    fn a_negative_index_is_named_as_itself_not_as_i32_max() {
        let refused = nonnegative_array_index(-1, || 3).unwrap_err();
        assert_eq!(
            message(&refused).as_deref(),
            Some("Index -1 out of bounds for length 3"),
            "HotSpot 25 names the negative index verbatim"
        );
        // The conversion this replaced: `-1 as usize` reaches the heap as
        // `usize::MAX`, and the heap's error channel saturates that.
        assert_eq!(cratonvm_types::oob_index_code((-1i32) as usize), i32::MAX);
        // A non-negative index never reads the length.
        let never = || -> i32 { panic!("length read on the accept path") };
        assert_eq!(nonnegative_array_index(2, never).ok(), Some(2));
    }

    /// Every decoded array arm, not just the helper: a helper nobody calls
    /// would pass the row above.
    #[test]
    fn decoded_array_arms_report_a_negative_index_like_hotspot() {
        let shared = vm();
        let cases: [(Instruction, ArrayElementType); 4] = [
            (Instruction::Iaload, ArrayElementType::Int),
            (Instruction::Iastore, ArrayElementType::Int),
            (Instruction::Lastore, ArrayElementType::Long),
            (Instruction::Dastore, ArrayElementType::Double),
        ];
        for (insn, et) in cases {
            let mut thread = thread_with_frame();
            let arr = shared.mem.heap.alloc_array(ClassId::new(0), et, 3);
            let stack = &mut thread.frames[0].stack;
            stack.push(Value::Object(Some(arr))).unwrap();
            stack.push_int(-1).unwrap();
            match insn {
                Instruction::Iastore => stack.push_int(7).unwrap(),
                Instruction::Lastore => stack.push_long(7).unwrap(),
                Instruction::Dastore => stack.push_double(7.0).unwrap(),
                _ => {}
            }
            let err = runtime_error(execute_instruction(&shared, &mut thread, 0, &insn, 0));
            assert!(
                matches!(
                    err,
                    RuntimeError::ArrayIndexOutOfBoundsException { index: -1, .. }
                ),
                "{insn:?}: expected AIOOBE for index -1, got {err:?}"
            );
            assert_eq!(
                message(&err).as_deref(),
                Some("Index -1 out of bounds for length 3"),
                "{insn:?}"
            );
        }
    }

    /// The null-array path now lives in `array_operand_slow`; it must still be
    /// a `NullPointerException`, and a non-null array must still load.
    #[test]
    fn decoded_array_load_still_raises_npe_on_null_and_loads_otherwise() {
        let shared = vm();
        let mut thread = thread_with_frame();
        thread.frames[0].stack.push(Value::Object(None)).unwrap();
        thread.frames[0].stack.push_int(0).unwrap();
        let err = runtime_error(execute_instruction(
            &shared,
            &mut thread,
            0,
            &Instruction::Iaload,
            0,
        ));
        assert!(
            matches!(err, RuntimeError::NullPointerException { .. }),
            "null array: expected NPE, got {err:?}"
        );

        let heap = &shared.mem.heap;
        let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
        heap.set_array_element(arr, 2, Value::Int(41)).unwrap();

        let mut thread = thread_with_frame();
        let stack = &mut thread.frames[0].stack;
        stack.push(Value::Object(Some(arr))).unwrap();
        stack.push_int(2).unwrap();
        let outcome = execute_instruction(&shared, &mut thread, 0, &Instruction::Iaload, 0);
        assert!(outcome.is_ok());
        assert_eq!(thread.frames[0].stack.pop_int().unwrap(), 41);

        let mut thread = thread_with_frame();
        let stack = &mut thread.frames[0].stack;
        stack.push(Value::Object(Some(arr))).unwrap();
        let outcome = execute_instruction(&shared, &mut thread, 0, &Instruction::Arraylength, 0);
        assert!(outcome.is_ok());
        assert_eq!(thread.frames[0].stack.pop_int().unwrap(), 3);
    }

    /// The array exclusion is the soundness condition of the receiver memo: a
    /// reference array's header carries its COMPONENT's class id, so without
    /// it a `String[]` would pass a `checkcast String` memoised for a `String`.
    #[test]
    fn the_cast_memo_answers_only_its_own_non_array_receiver() {
        let receiver = ClassId::new(17);
        let memo = CastSite {
            target: ClassId::new(3),
            positive_receiver: Some(receiver),
            negative_receivers: [None; 2],
            negative_catch: None,
            positive_array: None,
            memo_epoch: 0,
        };
        assert!(cast_memo_answers(&memo, false, receiver));
        assert!(
            !cast_memo_answers(&memo, true, receiver),
            "an array whose component id equals the memo must not be answered"
        );
        assert!(!cast_memo_answers(&memo, false, ClassId::new(18)));
        assert!(
            !cast_memo_answers(&CastSite::resolved(ClassId::new(3)), false, receiver),
            "a freshly resolved site carries no verdict"
        );

        // i16-L4, the array half: the array memo answers only an ARRAY
        // receiver of exactly its (element, header id) key, and a class memo
        // never answers an array (nor the array memo a plain object).
        let key = (ArrayElementType::Reference, receiver);
        let array_memo = CastSite {
            positive_array: Some(key),
            ..memo
        };
        assert!(array_cast_memo_answers(&array_memo, true, key));
        assert!(
            !array_cast_memo_answers(&array_memo, false, key),
            "a plain object whose class id equals the memoised component is no array"
        );
        let other_component = (ArrayElementType::Reference, ClassId::new(18));
        assert!(
            !array_cast_memo_answers(&array_memo, true, other_component),
            "another component"
        );
        assert!(
            !array_cast_memo_answers(&array_memo, true, (ArrayElementType::Int, receiver)),
            "another element kind"
        );
        assert!(
            !array_cast_memo_answers(&memo, true, key),
            "no array memo, no answer"
        );
        assert!(
            !cast_memo_answers(&array_memo, true, receiver),
            "the class memo still refuses an array whatever the array memo holds"
        );
    }

    /// i16-L4: the array memo goes where the other memos go when the
    /// definition epoch moves, and `resolved` starts without one.
    #[test]
    fn i16_l4_the_array_memo_follows_the_definition_epoch() {
        let key = (ArrayElementType::Reference, ClassId::new(21));
        let proved = CastSite {
            positive_array: Some(key),
            memo_epoch: 7,
            ..CastSite::resolved(ClassId::new(3))
        };
        assert_eq!(CastSite::resolved(ClassId::new(3)).positive_array, None);
        assert_eq!(proved.observed_at(7).positive_array, Some(key));
        let moved = proved.observed_at(8);
        assert_eq!(moved.positive_array, None);
        assert_eq!(moved.target, ClassId::new(3));
        assert!(!array_cast_memo_answers(&moved, true, key));
    }

    /// i16-L4, end to end through `execute_instruction`: a `String[]` cast to
    /// `[Ljava/lang/Object;` (the erasure of `(T[]) x`) fills the site on the
    /// first execution, memoises the array verdict on the second and answers
    /// the third from the memo. An `int[]` at the same site is not answered by
    /// the `String[]` memo (it runs the full path and is refused), and the
    /// `String[]` receiver is still admitted afterwards.
    #[test]
    fn i16_l4_an_array_cast_site_is_answered_from_its_array_memo() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

        if !cast_site_cache_enabled() {
            return; // the lever is armed in this environment
        }
        let shared = vm();
        let holder = {
            let mut cm = shared.classes.class_manager.write();
            let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l4/ArrayMemoHolder", 0)
            else {
                return; // --jdk-only policy in the environment: nothing to fabricate
            };
            let Some(class) = cm.class_store.get_mut(id) else {
                return;
            };
            class.constant_pool = ConstantPool::new(vec![
                E::Tombstone,
                E::Utf8(Arc::from("[Ljava/lang/Object;")),
                E::ClassReference { name_index: 1 },
            ]);
            id
        };
        let string_id = {
            let cm = shared.classes.class_manager.read();
            match cm.find_unique_class_by_name("java/lang/String") {
                Some(id) => id,
                None => return, // stripped VM without `String`
            }
        };
        let mut thread = JvmThread::new(ThreadId(4245), "l4-array-memo");
        thread.frames.push(Frame::new(
            holder,
            "cratonvm/test/l4/ArrayMemoHolder".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        let strings = shared
            .mem
            .heap
            .alloc_array(string_id, ArrayElementType::Reference, 1);
        let ints = shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
        let key = (ArrayElementType::Reference, string_id);
        let memo_of = |thread: &mut JvmThread| {
            thread
                .cast_sites
                .get(holder, 2)
                .copied()
                .map(|site| site.observed_in(&shared))
                .and_then(|site| site.positive_array)
        };
        let run = |thread: &mut JvmThread, receiver: ObjectRef, insn: Instruction| {
            thread.frames[0]
                .stack
                .push(Value::Object(Some(receiver)))
                .unwrap();
            let outcome = execute_instruction(&shared, thread, 0, &insn, 0);
            let answer = match insn {
                Instruction::Instanceof(_) => {
                    outcome.is_ok() && thread.frames[0].stack.pop_int().unwrap() == 1
                }
                _ => outcome.is_ok(),
            };
            thread.frames[0].stack.clear();
            answer
        };
        for round in 0..3 {
            assert!(
                run(&mut thread, strings, Instruction::Checkcast(2)),
                "round {round}: String[] is an Object[]"
            );
        }
        // Epochs are process-wide and other tests define classes
        // concurrently, so the memo is asserted only when nothing moved.
        let stable = memo_of(&mut thread);
        if stable.is_some() {
            assert_eq!(stable, Some(key), "the memo holds the receiver's key");
        }
        assert!(
            !run(&mut thread, ints, Instruction::Instanceof(2)),
            "int[] is no Object[]: the String[] memo must not answer it"
        );
        assert!(
            !run(&mut thread, ints, Instruction::Checkcast(2)),
            "int[] cannot be cast to Object[]"
        );
        assert!(run(&mut thread, strings, Instruction::Instanceof(2)));
    }

    /// i7-L2: the cast-site entry is tagged with the NAME generation, so its
    /// receiver memos carry the definition epoch themselves. A memo proved
    /// under another definition epoch is invisible once observed; one proved
    /// under the observing epoch survives; the target always survives; and
    /// the observed site is stamped with the observing epoch, so a memo
    /// written from it (`..site`) carries the epoch read before its verdict.
    #[test]
    fn i7_l2_cast_memos_are_dropped_when_their_definition_epoch_moves() {
        let receiver = ClassId::new(17);
        let proved = CastSite {
            target: ClassId::new(3),
            positive_receiver: Some(receiver),
            negative_receivers: [Some(ClassId::new(18)), Some(ClassId::new(22))],
            negative_catch: Some(ClassId::new(19)),
            positive_array: Some((ArrayElementType::Reference, ClassId::new(20))),
            memo_epoch: 40,
        };
        let same = proved.observed_at(40);
        assert_eq!(same, proved, "same epoch: every memo kept");
        let moved = proved.observed_at(41);
        assert_eq!(moved.target, ClassId::new(3), "the resolution survives");
        assert_eq!(moved.memo_epoch, 41);
        assert_eq!(
            (
                moved.positive_receiver,
                moved.negative_receivers,
                moved.negative_catch,
                moved.positive_array
            ),
            (None, [None; 2], None, None),
            "a verdict proved under another definition epoch is not answered"
        );
        assert!(!cast_memo_answers(&moved, false, receiver));
        let refilled = CastSite {
            positive_receiver: Some(receiver),
            ..moved
        };
        assert_eq!(refilled.observed_at(41), refilled);
    }

    #[test]
    fn monitorexit_and_monitorenter_have_distinct_npe_actions() {
        assert_eq!(monitorexit_npe_action(), "Cannot exit synchronized block");
        assert_ne!(
            crate::runtime::exceptions::helpful_npe::action_monitor(),
            monitorexit_npe_action()
        );
    }

    /// The negative memo answers only a NON-ARRAY receiver of exactly a
    /// memoised class: a reference array's header carries its component's id.
    /// i20-L5: either of the two slots answers, and nothing else does.
    #[test]
    fn the_negative_cast_memo_answers_only_its_own_non_array_receiver() {
        let shared = vm();
        let refused = ClassId::new(0);
        let site = CastSite {
            target: ClassId::new(3),
            positive_receiver: None,
            negative_receivers: [Some(refused), None],
            negative_catch: None,
            positive_array: None,
            memo_epoch: 0,
        };
        let plain = shared.mem.heap.alloc_object(refused, 0);
        assert!(negative_memo_answers(&shared, &site, plain));
        let array = shared
            .mem
            .heap
            .alloc_array(refused, ArrayElementType::Reference, 1);
        assert!(
            !negative_memo_answers(&shared, &site, array),
            "an array whose component id equals the memo must not be answered"
        );
        assert!(!negative_memo_answers(
            &shared,
            &CastSite::resolved(ClassId::new(3)),
            plain
        ));

        // The older slot answers too, and the array exclusion holds for it.
        let other = ClassId::new(5);
        let older = CastSite {
            negative_receivers: [Some(other), Some(refused)],
            ..site
        };
        assert!(negative_memo_answers(&shared, &older, plain));
        assert!(
            !negative_memo_answers(&shared, &older, array),
            "slot 1 must not answer an array either"
        );
        let unrelated = shared.mem.heap.alloc_object(ClassId::new(6), 0);
        assert!(
            !negative_memo_answers(&shared, &older, unrelated),
            "a class in neither slot runs the full path"
        );
    }

    /// i20-L5: a fill pushes the newest refusal into slot 0 and slot 0 into
    /// slot 1, so a rung refusing two alternating receivers keeps both; a
    /// class already in either slot leaves the order alone; a third class
    /// evicts the oldest. The epoch clears both slots at once.
    #[test]
    fn i20_l5_the_negative_memo_keeps_the_two_newest_refusals() {
        let (a, b, c) = (ClassId::new(30), ClassId::new(31), ClassId::new(32));
        let site = CastSite::resolved(ClassId::new(3)).with_negative(a);
        assert_eq!(site.negative_receivers, [Some(a), None]);
        let site = site.with_negative(b);
        assert_eq!(site.negative_receivers, [Some(b), Some(a)]);
        assert!(site.refuses(a) && site.refuses(b) && !site.refuses(c));
        assert_eq!(site.with_negative(a), site, "a known refusal is a no-op");
        assert_eq!(site.with_negative(b), site);
        // Alternating a, b, a, b ... never evicts either.
        let mut rotating = site;
        for receiver in [a, b, a, b, a] {
            if !rotating.refuses(receiver) {
                rotating = rotating.with_negative(receiver);
            }
        }
        assert_eq!(rotating, site);
        let site = site.with_negative(c);
        assert_eq!(site.negative_receivers, [Some(c), Some(b)]);
        assert!(!site.refuses(a), "the oldest refusal is dropped");
        // The other memos are carried through untouched.
        let with_positive = CastSite {
            positive_receiver: Some(ClassId::new(40)),
            negative_catch: Some(ClassId::new(41)),
            ..site
        };
        let pushed = with_positive.with_negative(a);
        assert_eq!(pushed.positive_receiver, Some(ClassId::new(40)));
        assert_eq!(pushed.negative_catch, Some(ClassId::new(41)));
        assert_eq!(pushed.target, ClassId::new(3));
        let stamped = CastSite {
            memo_epoch: 9,
            ..pushed
        };
        assert_eq!(stamped.observed_at(10).negative_receivers, [None; 2]);
        assert_eq!(
            stamped.observed_at(9).negative_receivers,
            [Some(a), Some(c)]
        );
    }

    /// i20-L5, end to end through `execute_instruction`: one `instanceof`
    /// site refusing receivers that alternate between two unrelated classes
    /// memoises BOTH refusals, keeps answering `false` for both, and still
    /// answers `true` for a subclass of the target.
    #[test]
    fn i20_l5_an_alternating_refusal_is_answered_from_either_negative_slot() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

        if !cast_site_cache_enabled() || !negative_cast_memo_enabled() {
            return; // a lever is armed in this environment
        }
        let shared = vm();
        let ids = {
            let mut cm = shared.classes.class_manager.write();
            let mut ids = Vec::new();
            for name in [
                "cratonvm/test/l5/NegHolder",
                "cratonvm/test/l5/NegTarget",
                "cratonvm/test/l5/NegA",
                "cratonvm/test/l5/NegB",
                "cratonvm/test/l5/NegSub",
            ] {
                match cm.try_ensure_synthetic_class(name, 0) {
                    Ok(id) => ids.push(id),
                    Err(_) => return, // --jdk-only policy: nothing to fabricate
                }
            }
            ids
        };
        let (holder, target, a, b, sub) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
        {
            let mut cm = shared.classes.class_manager.write();
            cm.set_superclass(sub, Some(target));
            let Some(class) = cm.class_store.get_mut(holder) else {
                return;
            };
            class.constant_pool = ConstantPool::new(vec![
                E::Tombstone,
                E::Utf8(Arc::from("cratonvm/test/l5/NegTarget")),
                E::ClassReference { name_index: 1 },
            ]);
        }
        let mut thread = JvmThread::new(ThreadId(4246), "l5-neg-memo");
        thread.frames.push(Frame::new(
            holder,
            "cratonvm/test/l5/NegHolder".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        let objects = [
            shared.mem.heap.alloc_object(a, 0),
            shared.mem.heap.alloc_object(b, 0),
            shared.mem.heap.alloc_object(sub, 0),
        ];
        // `None`: the resolution failed in this stripped VM.
        let run = |thread: &mut JvmThread, receiver: ObjectRef| -> Option<bool> {
            thread.frames[0]
                .stack
                .push(Value::Object(Some(receiver)))
                .unwrap();
            let outcome = execute_instruction(&shared, thread, 0, &Instruction::Instanceof(2), 0);
            let answer = outcome
                .ok()
                .map(|_| thread.frames[0].stack.pop_int().unwrap() == 1);
            thread.frames[0].stack.clear();
            answer
        };
        for round in 0..6 {
            let receiver = objects[round % 2];
            match run(&mut thread, receiver) {
                None => return,
                Some(answer) => assert!(!answer, "round {round}: no subtype of the target"),
            }
        }
        // Epochs are process-wide and other tests define classes
        // concurrently, so the memo is asserted only when nothing moved.
        let memo = thread
            .cast_sites
            .get(holder, 2)
            .copied()
            .map(|site| site.observed_in(&shared))
            .map(|site| site.negative_receivers);
        if let Some(slots) = memo.filter(|slots| slots.iter().all(Option::is_some)) {
            assert!(slots.contains(&Some(a)) && slots.contains(&Some(b)));
        }
        assert_eq!(
            run(&mut thread, objects[2]),
            Some(true),
            "NegSub is a NegTarget"
        );
        assert_eq!(run(&mut thread, objects[0]), Some(false));
        assert_eq!(run(&mut thread, objects[1]), Some(false));
    }

    /// Lambda-proxy ids (and the autobox sentinel above them) read per-proxy
    /// side tables, so their refusals are never memoised; an id the class
    /// store does not know is not memoised either.
    #[test]
    fn instance_dependent_receivers_are_not_class_determined() {
        let shared = vm();
        assert!(!receiver_class_determines_cast_verdict(
            &shared,
            ClassId::new(0x8000_0001)
        ));
        assert!(!receiver_class_determines_cast_verdict(
            &shared,
            ClassId::new(u32::MAX)
        ));
        assert!(!receiver_class_determines_cast_verdict(
            &shared,
            ClassId::new(0x7fff_fff0)
        ));
    }

    /// The synthetic-`Boolean` screen is an identity test once the id is
    /// known, and falls back to "ask by name" (answer `true`) before that.
    #[test]
    fn the_boolean_static_screen_is_by_identity_once_known() {
        use std::sync::atomic::Ordering::Relaxed;
        let shared = vm();
        let unknown = shared.classes.boolean_class_id.load(Relaxed);
        if unknown == u32::MAX {
            assert!(static_may_be_boolean_constant(&shared, ClassId::new(5)));
        }
        shared.classes.boolean_class_id.store(5, Relaxed);
        assert!(static_may_be_boolean_constant(&shared, ClassId::new(5)));
        assert!(!static_may_be_boolean_constant(&shared, ClassId::new(6)));
    }

    /// JVMS §6.5 `anewarray`: the component is resolved before the count is
    /// checked, so a missing component with a negative count raises the
    /// linkage error (HotSpot: `NoClassDefFoundError`), not
    /// `NegativeArraySizeException`.
    #[test]
    fn anewarray_resolves_its_component_before_checking_the_count() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

        let shared = vm();
        let holder = {
            let mut cm = shared.classes.class_manager.write();
            let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l2/AnewarrayHolder", 0)
            else {
                return; // --jdk-only policy in the environment: nothing to fabricate
            };
            let Some(class) = cm.class_store.get_mut(id) else {
                return;
            };
            class.constant_pool = ConstantPool::new(vec![
                E::Tombstone,
                E::Utf8(Arc::from("cratonvm/test/l2/NoSuchComponent")),
                E::ClassReference { name_index: 1 },
                E::Utf8(Arc::from("[[Lcratonvm/test/l2/NoSuchComponent;")),
                E::ClassReference { name_index: 3 },
            ]);
            id
        };
        let is_nase = |outcome: &Result<InstructionResult, MethodCallFailed>| {
            matches!(
                outcome,
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::NegativeArraySizeException { .. }
                )))
            )
        };
        let mut thread = JvmThread::new(ThreadId(4243), "l2-anewarray");
        thread.frames.push(Frame::new(
            holder,
            "cratonvm/test/l2/AnewarrayHolder".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        thread.frames[0].stack.push_int(-1).unwrap();
        let outcome = execute_instruction(&shared, &mut thread, 0, &Instruction::Anewarray(2), 0);
        assert!(outcome.is_err(), "a missing component cannot allocate");
        assert!(
            !is_nase(&outcome),
            "anewarray: resolution precedes the count check"
        );

        // `multianewarray`, outermost count negative: same rule.
        thread.frames[0].stack.push_int(-1).unwrap();
        thread.frames[0].stack.push_int(2).unwrap();
        let insn = Instruction::Multianewarray {
            index: 4,
            dimensions: 2,
        };
        let outcome = execute_instruction(&shared, &mut thread, 0, &insn, 0);
        assert!(outcome.is_err(), "a missing component cannot allocate");
        assert!(
            !is_nase(&outcome),
            "multianewarray: resolution precedes the count check"
        );
    }

    /// i11-L2: JVMS §6.5 resolves the `checkcast` / `instanceof` target for
    /// every non-null receiver. Before, an ARRAY on either side answered from
    /// names — `false` / `ClassCastException` — where HotSpot throws
    /// `NoClassDefFoundError` for a class missing at run time.
    #[test]
    fn array_casts_resolve_their_class_constant() {
        use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

        let shared = vm();
        let holder = {
            let mut cm = shared.classes.class_manager.write();
            let Ok(id) = cm.try_ensure_synthetic_class("cratonvm/test/l2/CastHolder", 0) else {
                return; // --jdk-only policy in the environment: nothing to fabricate
            };
            let Some(class) = cm.class_store.get_mut(id) else {
                return;
            };
            class.constant_pool = ConstantPool::new(vec![
                E::Tombstone,
                E::Utf8(Arc::from("cratonvm/test/l2/NoSuchCastTarget")),
                E::ClassReference { name_index: 1 },
                E::Utf8(Arc::from("[Lcratonvm/test/l2/NoSuchCastTarget;")),
                E::ClassReference { name_index: 3 },
                E::Utf8(Arc::from("[I")),
                E::ClassReference { name_index: 5 },
            ]);
            id
        };
        let mut thread = JvmThread::new(ThreadId(4244), "l2-array-cast");
        thread.frames.push(Frame::new(
            holder,
            "cratonvm/test/l2/CastHolder".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        let is_cce = |outcome: &Result<InstructionResult, MethodCallFailed>| {
            matches!(
                outcome,
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::ClassCastException { .. }
                )))
            )
        };
        let ints = shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
        let plain = shared.mem.heap.alloc_object(holder, 0);
        let cases = [
            (ints, 2u16, "int[] against a missing class"),
            (plain, 4, "an object against a missing class's array"),
            (ints, 4, "int[] against a missing class's array"),
        ];
        for (receiver, index, label) in cases {
            for insn in [
                Instruction::Instanceof(index),
                Instruction::Checkcast(index),
            ] {
                thread.frames[0]
                    .stack
                    .push(Value::Object(Some(receiver)))
                    .unwrap();
                let outcome = execute_instruction(&shared, &mut thread, 0, &insn, 0);
                assert!(outcome.is_err(), "{insn:?}, {label}: must not answer");
                assert!(
                    !is_cce(&outcome),
                    "{insn:?}, {label}: resolution fails before the type test"
                );
                thread.frames[0].stack.clear();
            }
        }

        // Control: a primitive-array target resolves trivially and answers.
        for (receiver, expected) in [(ints, 1), (plain, 0)] {
            thread.frames[0]
                .stack
                .push(Value::Object(Some(receiver)))
                .unwrap();
            let outcome =
                execute_instruction(&shared, &mut thread, 0, &Instruction::Instanceof(6), 0);
            assert!(outcome.is_ok());
            assert_eq!(thread.frames[0].stack.pop_int().unwrap(), expected);
        }
        // A null receiver resolves nothing (JVMS §6.5).
        thread.frames[0].stack.push(Value::Object(None)).unwrap();
        let outcome = execute_instruction(&shared, &mut thread, 0, &Instruction::Instanceof(2), 0);
        assert!(outcome.is_ok());
        assert_eq!(thread.frames[0].stack.pop_int().unwrap(), 0);
    }

    /// i11-L2: which targets are resolved for an array receiver or an
    /// array-type target. A primitive-element array and the three array
    /// supertypes cannot fail to resolve and decide nothing the verdict needs.
    #[test]
    fn trivial_cast_targets_are_exactly_the_unfailing_ones() {
        for name in [
            "[I",
            "[[B",
            "java/lang/Object",
            "java/io/Serializable",
            "[Ljava/lang/Cloneable;",
            "[[Ljava/lang/Object;",
        ] {
            assert!(cast_target_resolution_is_trivial(name), "{name}");
        }
        for name in [
            "p/Missing",
            "[Lp/Missing;",
            "[[Lp/Missing;",
            "java/lang/String",
            "[Ljava/lang/String;",
            "I",
            "[Ljava/lang/Object",
        ] {
            assert!(!cast_target_resolution_is_trivial(name), "{name}");
        }
    }

    #[test]
    fn array_opcodes_map_to_their_jep358_element_kinds() {
        assert_eq!(
            array_elem_kind_of(&Instruction::Baload),
            ArrayElemKind::Byte
        );
        assert_eq!(
            array_elem_kind_of(&Instruction::Bastore),
            ArrayElemKind::Byte
        );
        assert_eq!(
            array_elem_kind_of(&Instruction::Aastore),
            ArrayElemKind::Object
        );
        assert_eq!(
            array_elem_kind_of(&Instruction::Saload),
            ArrayElemKind::Short
        );
        assert_eq!(
            array_elem_kind_of(&Instruction::Caload),
            ArrayElemKind::Char
        );
        assert_eq!(
            array_elem_kind_of(&Instruction::Laload),
            ArrayElemKind::Long
        );
    }

    /// i10-L2: `checkcast`'s failure path throws; it no longer rewrites the
    /// value for a caller it recognises by NAME (Spring's
    /// `ConfigurationClassParser` / `ServletComponentHandler`, the log4j
    /// app-loader stand-in). Those hatches were deleted after a census of
    /// both modes read zero uses. A source witness: the hatches needed a
    /// Spring or log4j class tree to trigger, which the unit VM does not have.
    #[test]
    fn checkcast_has_no_caller_name_hatches() {
        let src = std::fs::read_to_string(format!(
            "{}/src/runtime/interpreter/opcodes.rs",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("read opcodes.rs");
        let start = src
            .find("pub(super) fn op_checkcast(")
            .expect("op_checkcast must exist");
        let end = start
            + src[start..]
                .find("pub(super) fn op_new(")
                .expect("op_new follows op_checkcast");
        let body = &src[start..end];
        for needle in [
            "ConfigurationClassParser",
            "ServletComponentHandler",
            "peek_app_loader",
            "convert_class_values_to_strings",
            "AnnotationAttributes",
        ] {
            assert!(
                !body.contains(needle),
                "op_checkcast names `{needle}` again: fix the producer of the \
                 value, do not rewrite it at the cast"
            );
        }
    }
}
