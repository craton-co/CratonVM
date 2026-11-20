#[allow(unused_imports)]
use super::*;

/// One interpreter frame that compiled code is running right now.
#[derive(Clone, Copy)]
pub(in crate::runtime::interpreter::jit_bridge) struct OsrContinuation {
    /// `thread.frames.len()` at the moment of entry -- deliberately the SAME
    /// number `JitEntryGuard::enter_with_compiled_at` records as the chain
    /// entry's `interp_depth`, so a consumer holding one of
    /// `active_compiled_frames`' `(depth, label, class_id, cm_ptr)` tuples can
    /// compare directly instead of inventing a second convention. The frame
    /// itself is `frames[interp_depth - 1]`.
    pub(in crate::runtime::interpreter::jit_bridge) interp_depth: u32,
    /// The artifact running this activation, as `Arc::as_ptr(..) as usize`.
    /// Bit-identical to the `cm_ptr` that tuple carries: both are the address
    /// of the payload of the same `Arc<CompiledMethod>`.
    pub(in crate::runtime::interpreter::jit_bridge) cm_ptr: usize,
}

thread_local! {
    /// This thread's live OSR continuations, outermost first.
    ///
    /// A `Vec` rather than one slot because an OSR'd body can call a method
    /// that itself OSRs; each is a separate activation at a different depth.
    /// Written once per OSR ENTRY -- never per back-edge -- and read only by a
    /// stack capture.
    pub(in crate::runtime::interpreter::jit_bridge) static OSR_CONTINUATIONS: std::cell::RefCell<Vec<OsrContinuation>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Publishes one `OsrContinuation` for exactly as long as the artifact is on
/// the stack, and withdraws it however control leaves -- normal return, OSR
/// exit, routed exception, or a panic unwinding through `catch_unwind`.
pub(in crate::runtime::interpreter::jit_bridge) struct OsrContinuationGuard {
    /// The registry length before this guard pushed. `Drop` truncates back to
    /// it rather than popping once, for the same reason `JitEntryGuard::drop`
    /// restores its depth first: a non-local exit out of a NESTED OSR entry
    /// could otherwise strand that descendant's record above ours, and a stale
    /// record names a frame depth that by then belongs to a different
    /// activation.
    pub(in crate::runtime::interpreter::jit_bridge) depth_at_push: usize,
    /// `false` when the kill switch is set or the registry was already
    /// borrowed; `Drop` must then truncate nothing.
    pub(in crate::runtime::interpreter::jit_bridge) armed: bool,
}

impl OsrContinuationGuard {
    pub(in crate::runtime::interpreter::jit_bridge) fn publish(interp_depth: usize, cm_ptr: usize) -> Self {
        if !osr_pc_refresh_enabled() {
            return Self {
                depth_at_push: 0,
                armed: false,
            };
        }
        OSR_CONTINUATIONS.with(|c| match c.try_borrow_mut() {
            Ok(mut v) => {
                let depth_at_push = v.len();
                v.push(OsrContinuation {
                    // Saturate rather than panic: a depth this record cannot
                    // represent must degrade to "no information", never take
                    // the process down on a path that only feeds a diagnostic.
                    interp_depth: u32::try_from(interp_depth).unwrap_or(u32::MAX),
                    cm_ptr,
                });
                Self {
                    depth_at_push,
                    armed: true,
                }
            }
            // Not reachable today -- the only reader holds the borrow for the
            // length of one lookup and cannot re-enter OSR from inside it --
            // but refusing to publish is the safe direction: the consumer then
            // sees exactly what it saw before this registry existed.
            Err(_) => Self {
                depth_at_push: 0,
                armed: false,
            },
        })
    }
}

impl Drop for OsrContinuationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        OSR_CONTINUATIONS.with(|c| {
            if let Ok(mut v) = c.try_borrow_mut() {
                v.truncate(self.depth_at_push);
            }
        });
    }
}

/// The artifact currently running `thread.frames[frame_index]` as an OSR
/// continuation, as a `*const cratonvm_jit::CompiledMethod` cast to `usize` --
/// the same encoding `conservative_roots::active_compiled_frames` uses for the
/// `cm_ptr` in its tuples, so the two compare with `==`.
///
/// `None` means "no information", not "this frame is interpreted": it is also
/// what the kill switch and a contended borrow report. A caller must fall back
/// to whatever it did before rather than read it as a negative claim.
///
/// The consumer is `runtime::stackwalker`; the exact call site is written out
/// in `.agent-requests/A7-wiring.txt`. It lives here because the OSR entry is
/// the only place that knows the pairing -- nothing else on the thread can tell
/// an OSR continuation apart from an interpreted frame that merely happens to
/// be parked on a back-edge the artifact could have been entered at.
#[allow(dead_code)] // wired up from `runtime::stackwalker`; see A7-wiring.txt
pub(crate) fn live_osr_continuation_artifact(frame_index: usize) -> Option<usize> {
    if !osr_pc_refresh_enabled() {
        return None;
    }
    let depth = u32::try_from(frame_index.checked_add(1)?).ok()?;
    OSR_CONTINUATIONS.with(|c| -> Option<usize> {
        let live = c.try_borrow().ok()?;
        live.iter()
            .rev()
            .find(|r| r.interp_depth == depth)
            .map(|r| r.cm_ptr)
    })
}

/// t1 wave 5 lane E2 (stage 2 of
/// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`):
/// the calling thread's OSR-continuation records, moved out for a continuation
/// slice run on this OS thread below its blocked carrier's compiled frames.
/// The records name frame counts of the CARRIER's interpreter stack, which the
/// continuation's own frames could equal (a slice frame would be labelled as
/// the carrier's OSR artifact). Opaque outside this module.
pub(crate) struct OsrContinuationsAside(pub(in crate::runtime::interpreter::jit_bridge) Vec<OsrContinuation>);

/// Move the calling thread's OSR-continuation records out
/// ([`OsrContinuationsAside`]); the slice starts with none, as on a runner.
pub(crate) fn take_osr_continuations_aside() -> OsrContinuationsAside {
    OsrContinuationsAside(
        OSR_CONTINUATIONS
            .try_with(|c| c.try_borrow_mut().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default())
            .unwrap_or_default(),
    )
}

/// Put back what [`take_osr_continuations_aside`] moved out. The slice's own
/// records were withdrawn by their guards (each truncates to the length it
/// found); any left over are dropped with the slice.
pub(crate) fn restore_osr_continuations_aside(aside: OsrContinuationsAside) {
    let _ = OSR_CONTINUATIONS.try_with(|c| {
        if let Ok(mut v) = c.try_borrow_mut() {
            *v = aside.0;
        }
    });
}

/// The address of the calling thread's OSR-continuation registry, stable for
/// the thread's life: another thread may read it through
/// [`osr_continuation_artifact_of_held_thread`] while this thread is held in
/// a native (interpreter round i2 wave 60, lane L6,
/// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED`).
pub(crate) fn osr_continuations_cell_addr() -> usize {
    OSR_CONTINUATIONS.with(|c| c as *const std::cell::RefCell<Vec<OsrContinuation>> as usize)
}

/// [`live_osr_continuation_artifact`] of ANOTHER thread, through the
/// registry address it published ([`osr_continuations_cell_addr`]). `None`
/// as there: no information.
///
/// # Safety
///
/// `cell` is 0 or that address, and the thread is held where it runs no
/// Java and no compiled code (so it neither publishes nor withdraws an OSR
/// continuation) until this returns: a thread excused by a handshake pause
/// in a native.
pub(crate) unsafe fn osr_continuation_artifact_of_held_thread(
    cell: usize,
    frame_index: usize,
) -> Option<usize> {
    if cell == 0 || !osr_pc_refresh_enabled() {
        return None;
    }
    let depth = u32::try_from(frame_index.checked_add(1)?).ok()?;
    // SAFETY: see the contract above.
    let cell = unsafe { &*(cell as *const std::cell::RefCell<Vec<OsrContinuation>>) };
    let live = unsafe { cell.try_borrow_unguarded() }.ok()?;
    live.iter()
        .rev()
        .find(|r| r.interp_depth == depth)
        .map(|r| r.cm_ptr)
}

/// The `int`-family value a compiled method returned, narrowed to its
/// descriptor's type.
///
/// JVMS §6.5 `ireturn`: a method whose return type is `boolean`, `byte`,
/// `char` or `short` returns its value truncated to that type (`boolean` to
/// bit 0). The interpreter narrows at `ireturn`, and so do both JIT tiers now:
/// the single-pass backend (`x64/op_control.rs`, `emit_narrow_int_return`,
/// also at a splice's return in `x64/inlining.rs`) and the IR builder
/// (`ir.rs`, `narrow_int_return`). Before they did, a compiled body fed
/// non-javac bytecode (`iconst_2; ireturn` from a `Z` method) handed the
/// interpreter a 2.
///
/// So this is a belt, not the fix: it keeps the value that re-enters
/// interpreted code correct should any compiled return path miss the
/// narrowing, at the cost of one sign/zero extension on a path that already
/// dispatches on the return tag.
pub(in crate::runtime::interpreter::jit_bridge) fn narrow_int_return(ret_type: u8, raw: i64) -> i32 {
    // Casts: the JIT ABI returns every int-family value in the i64 register.
    match ret_type {
        b'Z' => (raw & 1) as i32,
        b'B' => i32::from(raw as i8),
        b'C' => i32::from(raw as u16),
        b'S' => i32::from(raw as i16),
        _ => raw as i32,
    }
}

/// Review #80: the name of the class that DECLARES the method the `Methodref`
/// at `cp_idx` in `holder`'s constant pool resolves to. It backs the
/// `cp_invoke_declaring_class_resolver` each
/// `try_compile_with_invokespecial_resolver` door hands the JIT, which admits a
/// `Counter extends AtomicInteger` site to the `Atomic*` intrinsics only when
/// this names the JDK class.
///
/// The constant-pool class is resolved in `holder`'s loader, and the method is
/// found with `find_method_recursive`, the walk `try_jit_compile_callee_slow`
/// uses to find a callee's declaring class. An `InterfaceMethodref` is not
/// answered: the intrinsics this feeds are class methods.
///
/// Takes its own class-manager read, like the other resolver closures handed
/// to `try_compile_with_invokespecial_resolver`. It must NOT be called from
/// `compile_osr_artifact`'s invoke loop, which already holds the guard; that
/// door resolves through `cm_lock` instead.
pub(in crate::runtime::interpreter::jit_bridge) fn cp_method_ref_declaring_class_name(
    shared: &SharedVm,
    holder: ClassId,
    cp_idx: u16,
) -> Option<String> {
    cp_method_ref_declaring_class(shared, holder, cp_idx)
        .map(|(_, declaring_class)| declaring_class)
}

/// The id of the class [`cp_method_ref_declaring_class_name`] names, from the
/// same walk. It backs `CompileRequest::cp_invoke_declaring_class_id_resolver`,
/// which the bind ladders ask for an inherited-name site's recursion-cycle
/// checks, so a cycle through one loader's `A.f` keeps no call into another
/// loader's same-named `A.f` on dispatch (interpreter round i1 wave 17, L3).
/// Same lock rules as the name resolver.
pub(in crate::runtime::interpreter::jit_bridge) fn cp_method_ref_declaring_class_id(
    shared: &SharedVm,
    holder: ClassId,
    cp_idx: u16,
) -> Option<u32> {
    cp_method_ref_declaring_class(shared, holder, cp_idx).map(|(id, _)| id.as_u32())
}

/// `(declaring class id, declaring class name)` of the `Methodref` at `cp_idx`
/// in `holder`'s constant pool; the body of the two resolvers above.
pub(in crate::runtime::interpreter::jit_bridge) fn cp_method_ref_declaring_class(
    shared: &SharedVm,
    holder: ClassId,
    cp_idx: u16,
) -> Option<(ClassId, String)> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(holder)?;
    let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
            ..
        }) => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let target_class = class.constant_pool.get_class_name(class_idx)?;
    let (method_name, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
    site_class_and_declaring_class(
        shared,
        &cm,
        holder,
        (target_class, Some(class_idx)),
        method_name,
        descriptor,
    )
    .map(|(_, declaring_id, declaring_class)| (declaring_id, declaring_class))
}

/// What a `CONSTANT_Class` reference to `name` in `holder`'s constant pool
/// resolves to, as far as the compiler may know it without running a class
/// loader: [`class_resolved_without_loading`], in `find_class_by_name_for_class`'s
/// argument order so every compile-time resolver in this file reads the same.
///
/// Every class-name lookup a compile door makes on `holder`'s behalf goes
/// through here -- a `new` site, a type-check target, an invoke owner, a
/// direct-call or inlining candidate. `holder` must be the class whose
/// constant pool the name came from: for a call inside a spliced body that is
/// the body's declaring class, not the compile's root (i11-L0; see
/// `InlineDirectBind`). It is both the loader asked and the §5.4.4 accessor,
/// so the wrong one resolves another loader's copy or refuses a class the
/// callee may use. A `None` is "not yet known through
/// `holder`'s loader", and each caller already has a conservative arm for it
/// (a `Deferred` site, a by-name helper call, no bind).
///
/// A class `holder` may not access (JVMS §5.4.4; for an array name, its
/// element class) is answered `None` as well, in every mode (i9-L2): a baked
/// id is exactly what lets compiled code skip the access check the
/// interpreter runs at the same site. The deferred arms re-resolve at run time
/// through `helpers::jit_resolve_cp_class`, which runs that check when the
/// interpreter would (`new` always, `anewarray` / `ldc` under `--real-jdk`);
/// where the interpreter does not check, deferring costs a bind and changes
/// no answer.
///
/// Only for a name the constant pool holds: a declaring class or selection
/// start the compiler SUBSTITUTED for it is bound by id, without this check
/// (`direct_bind_callee_class`, i12-L2).
///
/// The `CONSTANT_Class` entry's recorded resolution answers first
/// ([`jit_recorded_class_entry_in`]; JIT round 16, lane classsite, R2 of
/// `r15w5-classsite2-review-findings-FIXED-20261001.md`): the interpreter and the
/// compiled class helpers answer an isolated loader's entry with its FIRST
/// resolution, where the initiating memo below keeps the LAST drive's answer.
/// A door that holds the entry's index passes it through [`jit_known_class_at`].
#[inline]
pub(in crate::runtime::interpreter) fn jit_known_class(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    name: &str,
    holder: ClassId,
) -> Option<ClassId> {
    jit_known_class_at(shared, cm, name, holder, None)
}

/// `CRATONVM_JIT_CLASS_ENTRY_DOORS` (default on; `0` / `false` / `off` / `no`
/// turns it off): the compile doors and the compiled code's run-time bridges
/// answer a `CONSTANT_Class` reference of a class whose entries are recorded
/// (an isolated URL loader's, `constants::class_entry_record_applies`) from
/// the entry's record, as the interpreter does. JIT round 16, lane classsite
/// (`r15w5-classsite2-review-findings-FIXED-20261001.md` R2, proposals CS2-1 and
/// CS2-2). Read only past [`jit_recorded_class_entry_in`]'s cheap gates, so a
/// VM in which no entry was ever recorded never reads it.
pub(crate) fn jit_class_entry_doors_enabled() -> bool {
    true
}

/// The recorded resolution (`constants::recorded_class_entry`) of the
/// `CONSTANT_Class` entry of `holder`'s pool that names `name`: entry
/// `class_index` when the caller holds it (and the pool, as `cm` holds it,
/// still names `name` there), else the first entry naming `name` that has a
/// record (a pool search; javac writes one entry per name). `None` -- and no
/// pool read -- unless `holder` has a defining loader (one lock-free side-table
/// probe) and some entry is recorded at all in this VM (one read guard), and
/// with [`jit_class_entry_doors_enabled`] off. Takes the caller's
/// class-manager guard and the resolution cache's read lock, in the order
/// `resolve_jit_new_site` takes them. JIT round 16, lane classsite.
pub(crate) fn jit_recorded_class_entry_in(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    class_index: Option<u16>,
    name: &str,
) -> Option<ClassId> {
    cratonvm_native_builtins::classloader::defining_loader_for(
        &shared.loader_identity,
        holder.as_u32(),
    )?;
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    if !resolver.any_class_entry_recorded() || !jit_class_entry_doors_enabled() {
        return None;
    }
    let pool = &cm.get_class(holder)?.constant_pool;
    match class_index {
        Some(index) => {
            if pool.get_class_name(index) != Some(name) {
                return None;
            }
            resolver.probe_class_entry(resolver.scope(holder), index)
        }
        None => {
            let len = u16::try_from(pool.len()).unwrap_or(u16::MAX);
            (1..len)
                .filter(|&index| pool.get_class_name(index) == Some(name))
                .find_map(|index| resolver.probe_class_entry(resolver.scope(holder), index))
        }
    }
}

/// A recorded entry's class `id` for a compile door, or `None` (defer) where
/// `holder` may not access it (JVMS §5.4.4): a record is filed before the
/// interpreter's access check runs, so a denied entry can hold one.
pub(in crate::runtime::interpreter::jit_bridge) fn recorded_entry_if_accessible(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    name: &str,
    id: ClassId,
) -> Option<ClassId> {
    if crate::vm::class_path_package_refused_at_compile(shared, cm, id) {
        return None;
    }
    crate::runtime::interpreter::constants::class_constant_access_denial_in(
        shared,
        cm,
        holder,
        name,
        Some(id),
    )
    .is_none()
    .then_some(id)
}

/// [`jit_known_class`] for a door that holds the `CONSTANT_Class` entry's
/// index (`Some`), or searches the pool for it (`None`): the entry's record
/// first, refused (`None`, the door defers) where `holder` may not access it,
/// as the flat answer is; else the flat answer. JIT round 16, lane classsite.
pub(in crate::runtime::interpreter) fn jit_known_class_at(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    name: &str,
    holder: ClassId,
    class_index: Option<u16>,
) -> Option<ClassId> {
    if let Some(id) = jit_recorded_class_entry_in(shared, cm, holder, class_index, name) {
        // The positive control of R2: a compile door the record answered
        // differently from the memo (`R16ClasssiteLateFlip`).
        if crate::runtime::env_cache::dbg_jitc() {
            let flat = class_resolved_without_loading(shared, cm, holder, name);
            if flat != Some(id) {
                eprintln!(
                    "[cratonvm-jitc] class-entry record answers {name} for cid={} \
                     entry={class_index:?}: record={} memo={flat:?}",
                    holder.as_u32(),
                    id.as_u32(),
                );
            }
        }
        return recorded_entry_if_accessible(shared, cm, holder, name, id);
    }
    let id = class_resolved_without_loading(shared, cm, holder, name)?;
    // The deferred-site count of `i26-L5-a-user-loaders-class-under-a-jdk-name-
    // is-resolved-to-the-jdks-class` (interpreter round i1 wave 37, lane L2):
    // one cached flag load when the diagnostic is off.
    if crate::runtime::env_cache::dbg_isolated_cnf() {
        note_jit_global_name_answer(shared, cm, holder, name, id);
    }
    // An unlinked application class whose `(package, code source)` pair a
    // door had the JDK refuse (sealing) is not bound: the deferred site's
    // run-time helper runs the package step and throws HotSpot's
    // `SecurityException` (interpreter round i4 wave 96, lane L5;
    // `i95-L5-define-door-loose-ends` item 2). One lock and one `is_empty`
    // while nothing was refused.
    if crate::vm::class_path_package_refused_at_compile(shared, cm, id) {
        return None;
    }
    if crate::runtime::interpreter::constants::class_constant_access_denial_in(
        shared,
        cm,
        holder,
        name,
        Some(id),
    )
    .is_some()
    {
        return None;
    }
    Some(id)
}

/// `CRATONVM_JIT_INDY_ADMIT_BY_ID` (JIT round 21 lane indylate2, default ON;
/// `=0` restores the by-name-only admission): may an `invokedynamic` whose
/// bound direct target's class NAME is not known from the caller (a class
/// of a user-defined loader naming a class its parent defined, which
/// [`jit_known_class_at`] answers only from the loader's own definitions,
/// direct supertypes and initiating memo) still be planned as a plain call,
/// admitted by the class id the handle names? Page
/// `jr21-indylate2-indy-target-refused-for-a-child-loader-caller-20261014.md`.
/// Read per compile (no per-call read, no static).
pub(crate) fn jit_indy_admit_by_id_enabled() -> bool {
    true
}

/// The class half of an `invokedynamic` target's admission (JIT round 21
/// lane indylate2): `by_name` is what the target's class name answers from
/// the caller ([`jit_known_class_at`], as a raw id), `target` the class id
/// the bound handle names.
///
/// * `by_name == Some(target)`: `Ok("by-name")`, the rule since round 16.
/// * `by_name == Some(other)`: refused -- the caller's own name for that
///   class is another class, so a by-name question of the planned call from
///   the caller would bind the wrong one.
/// * `by_name == None` (the caller's loader has not been seen to answer the
///   name): `Ok("by-id")` when `by_id_on` and `by_id_terms()` hold. The
///   handle names the exact method: the link-time `MethodHandles.Lookup`
///   already did the access check, as on HotSpot, whose compiler inlines
///   through the handle's method and asks no name of the caller. The plan
///   resolves the call in the TARGET class's context (its `class_context_at`),
///   never the caller's, and `by_id_terms` checks that context answers it.
pub(crate) fn indy_target_class_admission(
    by_name: Option<u32>,
    target: u32,
    by_id_on: bool,
    by_id_terms: impl FnOnce() -> bool,
) -> Result<&'static str, &'static str> {
    match by_name {
        Some(found) if found == target => Ok("by-name"),
        Some(_) => Err("the name answers another class from this caller"),
        None if !by_id_on => Err("the name is not known from this caller (by-id off)"),
        None if by_id_terms() => Ok("by-id"),
        None => Err("the name is not known from this caller; by-id terms refused"),
    }
}

/// Where the JIT's compile-time answer for `holder`'s reference to `name`
/// would change under the recommended fix of
/// `docs/feature-designs/real-jdk-mode.md`,
/// and why; `None` where it would not (interpreter round i1 wave 37, lane L2).
///
/// Today [`class_resolved_without_loading`] answers a JDK-global name for a
/// user-defined loader with `find_class_by_name_for_class`: the loader's own
/// definition, then a recorded user ancestor's, then the built-in chain — the
/// JDK's class, which is wrong for a loader that would define its own copy
/// when asked (the interpreter asks it: `drive_loader_for_global_name`, wave
/// 28). The recommendation, `--real-jdk`, for a name a user loader may define
/// (`is_user_loader_global_name`): keep a user loader's class (own or
/// ancestor's), then the loader's initiating memo, then the built-in answer
/// only for a loader `ClassRealm::loader_global_name_transparency` records as
/// transparent; otherwise nothing (the compile defers the site). The answer
/// is `(recommended, why)`: `"memo"` (the memo names another class),
/// `"transparency-unknown"` (the interpreter has not classified the loader
/// yet) or `"not-transparent"` (it overrides `loadClass` and has not answered
/// this name). [`note_jit_global_name_answer`] prints every answer it would
/// change at any compile-time lookup. Applied (wave 37, after the host run of
/// `L2W37JdkNameColdSite` showed a compiled cold site erring 5000 times out of
/// 5000) only through [`loader_faithful_global_name_answer`], at the doors
/// the interpreter itself drives the loader for, so that a deferral costs
/// only a site the interpreter never ran; see that function.
pub(in crate::runtime::interpreter::jit_bridge) fn jit_user_loader_global_name_change(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    name: &str,
    current: ClassId,
) -> Option<(Option<ClassId>, &'static str)> {
    use cratonvm_types::ClassLoaderId;
    if !super::constants::loader_global_name_ask_enabled(shared, name) {
        return None;
    }
    let loader = match cm.get_loader_id(holder) {
        Some(l @ ClassLoaderId::UserDefined(_)) => l,
        _ => return None,
    };
    if matches!(
        cm.get_loader_id(current),
        Some(ClassLoaderId::UserDefined(_))
    ) {
        return None;
    }
    // A DIRECT supertype of `holder` was resolved through `holder`'s own
    // loader when it was linked (JVMS §5.3.5): the loader's answer, whoever
    // defined it (a constructor's `super.<init>` keeps its direct bind).
    if cm
        .get_class(holder)
        .is_some_and(|c| c.superclass == Some(current) || c.interfaces.contains(&current))
    {
        return None;
    }
    let memo =
        crate::runtime::resolve::initiating_records::loader_initiated_answer(shared, loader, name);
    if let Some(memo) = memo {
        return (memo != current).then_some((Some(memo), "memo"));
    }
    let transparent = shared
        .classes
        .loader_global_name_transparency
        .read()
        .get(&loader)
        .copied();
    match transparent {
        Some(true) => None,
        Some(false) => Some((None, "not-transparent")),
        None => Some((None, "transparency-unknown")),
    }
}

/// The class a compile door, or a compiled site's run-time fast path, may
/// take for `holder`'s CONSTANT-POOL reference to `name` when the flat lookup
/// answered `current` — `None` to defer (the compile leaves the site to its
/// run-time helper; the helper then resolves through the loader,
/// `resolve_class_loader_aware`, which asks it as the interpreter does)
/// (interpreter round i1 wave 37, lane L2; `i26-L5-a-user-loaders-class-under-
/// a-jdk-name-is-resolved-to-the-jdks-class`).
///
/// For a JDK-global name (`javax/`, `jdk/`, `sun/`, `com/sun/`) of a
/// user-defined loader under `--real-jdk`, the flat answer is the JDK's class
/// whenever the loader has not defined its own copy YET; a child-first loader
/// defines it on request, and the compiled site bound the JDK's class and
/// failed (`L2W37JdkNameColdSite`: `NoSuchMethodError`-shaped errors on every
/// call). Answered with [`jit_user_loader_global_name_change`]'s
/// recommendation. Used only where the interpreter drives the loader for the
/// same reference (`new`, a statically bound call's owner, the owner of a
/// constant-pool splice): an executed site has filled the loader's memo, so
/// only a site the interpreter never ran is deferred. The receiver doors
/// (`invokevirtual` / `invokeinterface` owners, final devirtualisation,
/// receiver inlining) are not asked: the interpreter never drives their owner,
/// so they would defer on every compile of a fat-jar loader's class.
pub(crate) fn loader_faithful_global_name_answer(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    name: &str,
    current: ClassId,
) -> Option<ClassId> {
    match jit_user_loader_global_name_change(shared, cm, holder, name, current) {
        None => Some(current),
        Some((recommended, _)) => recommended,
    }
}

/// [`jit_known_class`] for a constant-pool reference the interpreter drives
/// the loader for: the answer passes [`loader_faithful_global_name_answer`].
pub(in crate::runtime::interpreter) fn jit_known_class_constant(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    name: &str,
    holder: ClassId,
) -> Option<ClassId> {
    jit_known_class_constant_at(shared, cm, name, holder, None)
}

/// [`jit_known_class_constant`] with the entry's index when the door holds
/// it ([`jit_known_class_at`]). A recorded entry is the loader's answer
/// already (the interpreter drove it), so it is not re-judged by
/// [`loader_faithful_global_name_answer`], whose memo may name a later answer.
/// JIT round 16, lane classsite.
pub(crate) fn jit_known_class_constant_at(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    name: &str,
    holder: ClassId,
    class_index: Option<u16>,
) -> Option<ClassId> {
    if let Some(id) = jit_recorded_class_entry_in(shared, cm, holder, class_index, name) {
        return recorded_entry_if_accessible(shared, cm, holder, name, id);
    }
    let id = jit_known_class_at(shared, cm, name, holder, class_index)?;
    loader_faithful_global_name_answer(shared, cm, holder, name, id)
}

/// `CRATONVM_DBG_ISOLATED_CNF`: one `[ISOLATED-CNF] jit global-name` line per
/// compile-time answer [`jit_user_loader_global_name_change`] would change.
/// `would-defer` lines are the deferred-site count the page asks for
/// (`sort -u` them: a compile may ask a site more than once).
pub(in crate::runtime::interpreter::jit_bridge) fn note_jit_global_name_answer(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    name: &str,
    current: ClassId,
) {
    let Some((recommended, why)) =
        jit_user_loader_global_name_change(shared, cm, holder, name, current)
    else {
        return;
    };
    let holder_name = cm
        .get_class(holder)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    let verb = if recommended.is_some() {
        "would-rebind"
    } else {
        "would-defer"
    };
    eprintln!(
        "[ISOLATED-CNF] jit global-name {verb} holder={holder_name} name={name} loader={:?} \
         today={} recommended={recommended:?} why={why}",
        cm.get_loader_id(holder),
        current.as_u32(),
    );
}

/// Why a compile door must not compile an `invokespecial` site of `caller`
/// naming `cp_class_name.name desc` the way it would, or `None` when it may
/// (interpreter round i1 wave 37, lane L2; the last half of item 1 of
/// `i29-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot`).
///
/// The interpreter selects through `invoke::invokespecial_owner_class_name`:
/// under `--real-jdk` JVMS §6.5's `selection::select_special`, whose abstract
/// selected method or conflicting defaults is an ERROR; otherwise (and for
/// every shape `select_special` leaves alone) the lenient walk from
/// `invokespecial_selection_start`. Wave 36 keeps a caller whose site raised
/// that error interpreted, but only once the interpreter has raised it: a
/// caller compiled before its erring site first runs (a cold branch, an OSR
/// loop's last iteration, a body spliced into a hot caller) bound the lenient
/// target and ran it.
///
/// * `"invokespecial-selection-error"` (`--real-jdk`): the selection is an
///   error. No door may compile the site; the interpreter raises it.
/// * `"invokespecial-selection-start"`: `cp_owner_binds` is `true` (the door
///   binds the site by its constant-pool class: the single-pass OSR door, the
///   eager first-call door, a spliced body), the interpreter's owner is not
///   the constant-pool class, and the two walks find different bodies — a
///   super call naming an ancestor above an override, which javac never
///   emits. The method-entry doors pass `false`: their owner resolver
///   (`CpResolvers::invokespecial_owner`) substitutes the interpreter's owner.
/// * `"invokespecial-interface-receiver-unchecked"` (both modes, wave 133 lane
///   L4, `CRATONVM_INTERFACE_SPECIAL_RECEIVER_CHECK`): `caller` is an interface
///   and some `invokespecial` of the member in it does not pass its method's
///   own `this` ([`interface_special_receivers_proven`]); the interpreter makes
///   HotSpot's receiver check there, and no compiled route does.
///
/// `None` for a constructor and for a constant-pool class not yet known
/// through `caller`'s loader (see [`special_site_cp_class`]). Reads
/// through the caller's class-manager guard; takes no lock. Compile time only.
///
/// Interpreter round i6 wave 118, lane L2 (the count
/// `i37-L2-proposal-one-invoke-site-classifier-for-every-compile-door` gates
/// its performance half on): every answer is counted
/// ([`invokespecial_selection_census`]; a relaxed add on a compile-time
/// path), split into "the door binds by its constant-pool class" (the three
/// doors that refuse: the single-pass OSR door, the eager door, a splice) and
/// "the door substitutes the owner" (the method-entry doors), and printed at
/// exit under `CRATONVM_DBG_JITC`.
pub(in crate::runtime::interpreter) fn special_site_bind_refusal(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    cp_class_name: &str,
    is_interface_ref: bool,
    name: &str,
    desc: &str,
    cp_owner_binds: bool,
) -> Option<&'static str> {
    let why = special_site_bind_refusal_uncounted(
        shared,
        cm,
        caller,
        cp_class_name,
        is_interface_ref,
        name,
        desc,
        cp_owner_binds,
    );
    // Wave 119 follow-up (L9 review): counted only when the census can be
    // printed (`CRATONVM_DBG_JITC`, the exit line in `interp_census.rs`), so
    // the default path touches no shared counter.
    if !crate::runtime::env_cache::dbg_jitc() {
        return why;
    }
    use std::sync::atomic::Ordering::Relaxed;
    let row = match (why, cp_owner_binds) {
        (None, _) => &INVOKESPECIAL_SELECTION_CENSUS[0],
        (Some("invokespecial-selection-start"), true) => &INVOKESPECIAL_SELECTION_CENSUS[1],
        (Some(_), true) => &INVOKESPECIAL_SELECTION_CENSUS[2],
        (Some(_), false) => &INVOKESPECIAL_SELECTION_CENSUS[3],
    };
    row.fetch_add(1, Relaxed);
    why
}

/// [`special_site_bind_refusal`]'s answers this process: `[admitted,
/// start-refused at a constant-pool-binding door, error-refused at one,
/// refused at an owner-substituting door]`. Diagnostic only.
pub(in crate::runtime::interpreter::jit_bridge) static INVOKESPECIAL_SELECTION_CENSUS: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// `(admitted, start_refused_cp_door, error_refused_cp_door,
/// refused_owner_door)`: the i37-L2 count (interpreter round i6 wave 118,
/// lane L2). `start_refused_cp_door` is the number of binds a shared
/// classifier would turn from a refusal (an OSR denial, a sealed eager
/// compile, a refused splice) into a substitution.
pub fn invokespecial_selection_census() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        INVOKESPECIAL_SELECTION_CENSUS[0].load(Relaxed),
        INVOKESPECIAL_SELECTION_CENSUS[1].load(Relaxed),
        INVOKESPECIAL_SELECTION_CENSUS[2].load(Relaxed),
        INVOKESPECIAL_SELECTION_CENSUS[3].load(Relaxed),
    )
}

/// How a compile door binds an `invokespecial` site, decided once
/// ([`classify_invoke_site`]; interpreter round i6 wave 119, lane L2, the
/// first slice of `i37-L2-proposal-one-invoke-site-classifier-for-every-compile-door`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::runtime::interpreter) enum SiteBinding {
    /// Bind the constant-pool class: it is the interpreter's selection.
    ConstantPoolClass,
    /// Bind this owner instead (its name and id): the interpreter's JVMS §6.5
    /// selection start, whose body differs from the constant-pool class's (a
    /// super call naming an ancestor above an override). A door that can
    /// carry an owner substitutes it rather than refusing the site.
    Owner(Arc<str>, ClassId),
    /// No door may bind the site (`invokespecial-selection-error`: the
    /// selection is an error the interpreter raises).
    Refused(&'static str),
}

/// `CRATONVM_JIT_INVOKE_SITE_CLASSIFIER` (performance, default ON since the i6 wave-126 flip, `=0` off; token
/// `jit-invoke-site-classifier`; interpreter round i6 wave 119, lane L2): the
/// single-pass OSR door binds an `invokespecial` whose §6.5 selection start
/// differs from its constant-pool class to that owner ([`SiteBinding::Owner`])
/// instead of denying the loop. Read per such site (compile time only).
pub(crate) fn invoke_site_classifier_enabled() -> bool {
    true
}

/// The binding of `caller`'s `invokespecial` of `cp_class_name.name desc`
/// ([`SiteBinding`]): [`special_site_bind_refusal`]'s question, answered with
/// the owner where that function can only refuse. Same reads (the caller's
/// class-manager guard, no lock of its own), same counting
/// ([`invokespecial_selection_census`]: a substitution counts as the
/// start-refusal it replaces, so the census line keeps reading "binds a
/// shared classifier turns from refusals into substitutions").
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn classify_invoke_site(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    cp_class_name: &str,
    is_interface_ref: bool,
    name: &str,
    desc: &str,
) -> SiteBinding {
    match special_site_bind_refusal(
        shared,
        cm,
        caller,
        cp_class_name,
        is_interface_ref,
        name,
        desc,
        true,
    ) {
        None => SiteBinding::ConstantPoolClass,
        Some("invokespecial-selection-start") => {
            let Some(cp_class_id) = special_site_cp_class(shared, cm, caller, cp_class_name) else {
                return SiteBinding::Refused("invokespecial-selection-start");
            };
            let store = cm.class_store();
            let mut owner = None;
            if shared.classes.selection_checks {
                use crate::runtime::resolve::selection::{select_special, SpecialSelection};
                if let SpecialSelection::Owner(id) =
                    select_special(store, caller, cp_class_id, is_interface_ref, name, desc)
                {
                    owner = Some(id);
                }
            }
            let owner = owner.unwrap_or_else(|| {
                crate::classloading::invokespecial_selection_start(
                    caller,
                    cp_class_id,
                    is_interface_ref,
                    name,
                    store,
                )
            });
            match store.get(owner) {
                Some(class) => SiteBinding::Owner(Arc::clone(&class.name), owner),
                None => SiteBinding::Refused("invokespecial-selection-start"),
            }
        }
        Some(why) => SiteBinding::Refused(why),
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn special_site_bind_refusal_uncounted(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    cp_class_name: &str,
    is_interface_ref: bool,
    name: &str,
    desc: &str,
    cp_owner_binds: bool,
) -> Option<&'static str> {
    if name == "<init>" || name == "<clinit>" {
        return None;
    }
    // Interpreter round i6 wave 133, lane L4 (i132-L6): an interface sender's
    // `invokespecial` owes HotSpot's receiver subtype check, which the
    // interpreter makes (`invoke::interface_special_receiver_refusal`) and no
    // compiled route does. Bound only where every such site of the interface
    // passes its own `this`.
    // Interpreter round i7 wave 136, lane L2
    // (`CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, default OFF): a
    // method-entry door (`cp_owner_binds == false`, `CpResolvers::invoke`)
    // binds the unproven site through `jit_invoke_dispatch`, which makes the
    // check (`helpers::iface_special_receiver_guard`); its direct binders and
    // splices decline the target ([`iface_special_guarded_target`]). The
    // doors that bind by the constant-pool class themselves keep refusing.
    if crate::runtime::interpreter::invoke::interface_special_receiver_check_enabled(shared)
        && (cp_owner_binds)
        && !interface_special_receivers_proven(cm, caller, cp_class_name, name, desc)
    {
        return Some("invokespecial-interface-receiver-unchecked");
    }
    let cp_class_id = special_site_cp_class(shared, cm, caller, cp_class_name)?;
    let store = cm.class_store();
    let mut owner = None;
    if shared.classes.selection_checks {
        use crate::runtime::resolve::selection::{select_special, SpecialSelection};
        match select_special(store, caller, cp_class_id, is_interface_ref, name, desc) {
            SpecialSelection::Unchanged => {}
            SpecialSelection::Owner(id) => owner = Some(id),
            SpecialSelection::AbstractMethod(_) | SpecialSelection::IncompatibleClassChange(_) => {
                return Some("invokespecial-selection-error");
            }
        }
    }
    if !cp_owner_binds {
        return None;
    }
    let owner = owner.unwrap_or_else(|| {
        crate::classloading::invokespecial_selection_start(
            caller,
            cp_class_id,
            is_interface_ref,
            name,
            store,
        )
    });
    if owner == cp_class_id {
        return None;
    }
    let body = |from: ClassId| {
        crate::classloading::find_method_recursive(from, name, desc, store).map(|(_, id)| id)
    };
    (body(owner) != body(cp_class_id)).then_some("invokespecial-selection-start")
}

/// Interpreter round i6 wave 133, lane L4 (page
/// `docs/feature-designs/real-jdk-mode.md`):
/// may a compiled route bind an `invokespecial` of `cp_class_name.name desc`
/// held by `caller` without HotSpot's interface-sender receiver check? Always
/// for a class sender (the verifier proves the receiver). For an interface
/// sender, only when every `invokespecial` naming that member, in every method
/// of the interface, pushes the method's unstored `this` as the receiver
/// (`private_iface_this::every_special_site_receiver_is_this`): javac's
/// `I.super.m()` and pre-JDK-11 private interface calls. The question names
/// the member, not one site, so it holds for whichever site is being bound.
/// Under the caller's class-manager guard; compile time only (and, under
/// `CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, from the dispatch helper's
/// receiver guard for an interface sender's statically bound site).
pub(crate) fn interface_special_receivers_proven(
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    cp_class_name: &str,
    name: &str,
    desc: &str,
) -> bool {
    let store = cm.class_store();
    let Some(class) = store.get(caller) else {
        return false;
    };
    if !class.is_interface() {
        return true;
    }
    let pool = &class.constant_pool;
    let names_target = |cp: u16| {
        let (class_index, nat_index) = match pool.get(cp) {
            Some(ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
                ..
            })
            | Some(ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
                ..
            }) => (*class_index, *name_and_type_index),
            _ => return false,
        };
        pool.get_class_name(class_index) == Some(cp_class_name)
            && pool.get_name_and_type(nat_index) == Some((name, desc))
    };
    class.methods.iter().all(|method| {
        let Some(code_attr) = method.code() else {
            return true;
        };
        let handlers: Vec<u16> = code_attr
            .exception_table
            .iter()
            .map(|entry| entry.handler_pc)
            .collect();
        private_iface_this::every_special_site_receiver_is_this(
            code_attr.code.as_bytes(),
            &handlers,
            method.is_static(),
            desc,
            names_target,
        )
    })
}

/// Interpreter round i7 wave 136, lane L2
/// (`CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, performance, default OFF;
/// page `i135-L6-trait-static-accessors-run-uncached-and-uncompiled`): is
/// `callee_class.callee_method callee_desc`, named by a site of `holder`, the
/// target of an interface sender's `invokespecial` whose receiver is not
/// proven to be `this` ([`interface_special_receivers_proven`])? With the
/// switch on, a method-entry door compiles such a site
/// ([`special_site_bind_refusal`] admits it there) on the condition that the
/// call goes through `jit_invoke_dispatch`, whose receiver guard
/// (`helpers::iface_special_receiver_guard`) raises HotSpot's
/// `IllegalAccessError`. So no direct bind ([`direct_bind_name_refusal`]) and
/// no splice ([`resolve_inline_site_from`]) may serve the target from that
/// holder: both would skip the helper. The question names the member, not one
/// site, so a non-special site of the same member in the interface is declined
/// too (it only loses a direct bind). `false` with the switch off, for a class
/// holder, and for a constructor. Takes a short recursive class-manager read
/// guard. Compile time only.
pub(crate) fn iface_special_guarded_target(
    shared: &SharedVm,
    holder: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> bool {
    if !crate::runtime::interpreter::invoke::interface_special_receiver_check_enabled(shared)
        || callee_method == "<init>"
        || callee_method == "<clinit>"
    {
        return false;
    }
    // `read_recursive`: a door may ask with its own read guard live (a binder
    // closure inside a compile that reads the pool), and a plain `read` behind
    // a queued writer would then wait on itself.
    let cm = shared.classes.class_manager.read_recursive();
    !interface_special_receivers_proven(&cm, holder, callee_class, callee_method, callee_desc)
}

/// The class an `invokespecial` of `caller` naming `cp_class_name` refers to,
/// at compile time: [`jit_known_class`], else the class of that name on
/// `caller`'s superclass chain.
///
/// `jit_known_class` answers a user loader's reference only from what the
/// loader defined or initiated (`class_resolved_without_loading`): a DIRECT
/// supertype, or a name some executed site drove. A super call naming a
/// GRANDPARENT (`invokespecial GpA.m` from below `GpB extends GpA`) at a site
/// the interpreter never ran is neither, so the owner resolver and
/// [`special_site_bind_refusal`] answered nothing, and every door bound the
/// constant-pool class by name: `GpA.m` where JVMS §6.5 selects `GpB.m`
/// (host run of `L2W37SpecialSelectionColdSite`, row `osr-grandparent-named`,
/// wave 37). A class on the superclass chain was resolved through the chain's
/// own loaders when `caller` was linked; it is the class the reference means
/// whenever the reference is a super call at all, which is the only case the
/// JVMS §6.5 redirect (and this answer) is about. Interpreter round i1 wave
/// 37, lane L2.
pub(in crate::runtime::interpreter::jit_bridge) fn special_site_cp_class(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    cp_class_name: &str,
) -> Option<ClassId> {
    if let Some(id) = jit_known_class(shared, cm, cp_class_name, caller) {
        return Some(id);
    }
    let mut cur = cm.get_class(caller).and_then(|c| c.superclass);
    let mut steps = 0usize;
    while let Some(id) = cur {
        steps += 1;
        if steps > 256 {
            return None;
        }
        let class = cm.get_class(id)?;
        if &*class.name == cp_class_name {
            return Some(id);
        }
        cur = class.superclass;
    }
    None
}

/// Does an `invokeinterface` of `holder` naming `cp_class_name.name desc`
/// resolve to a PRIVATE interface method, which the compile doors pin as a
/// statically bound call (`CpResolvers::invokespecial_owner`'s `0xb9` arm)?
/// The same question, through the same lookups, as that arm.
pub(in crate::runtime::interpreter::jit_bridge) fn private_interface_method_pinned(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    cp_class_name: &str,
    name: &str,
    desc: &str,
) -> bool {
    let Some(cp_class_id) = jit_known_class(shared, cm, cp_class_name, holder) else {
        return false;
    };
    let store = cm.class_store();
    store.get(cp_class_id).is_some_and(|c| c.is_interface())
        && crate::classloading::invokevirtual_private_declaring_class(
            cp_class_id,
            name,
            desc,
            store,
        )
        .is_some()
}


/// Interpreter round i6 wave 132, lane L6: stage 1 of page
/// `i40-L2-proposal-guard-a-pinned-private-interface-call-instead-of-refusing-the-method`
/// (`CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN`, default OFF, performance only).
/// May the doors pin the `invokeinterface` sites naming `cp_idx` (the one at
/// `only_pc`, when given) of `holder.method_name method_descriptor` without
/// the receiver check no compiled route makes? Yes when `holder` is an
/// interface, the private target is declared by `holder` itself, the method is
/// an instance method whose local 0 is never stored, and each site's receiver
/// is pushed by `aload_0` (`private_iface_this::every_site_receiver_is_this`):
/// that receiver implements `holder`, so JVMS §6.5's
/// `IncompatibleClassChangeError` cannot arise there. `compiled_code`, when
/// given, is the bytecode the door compiles; a body that does not start with
/// the class's current code (a redefinition in between) is not proven.
/// Under the caller's class-manager guard; compile time only.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn private_iface_this_pin_proven(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    method_name: &str,
    method_descriptor: &str,
    compiled_code: Option<&[u8]>,
    cp_class_name: &str,
    cp_idx: u16,
    only_pc: Option<usize>,
    callee_desc: &str,
) -> bool {
    if !private_iface_this::private_iface_this_pin_enabled() {
        return false;
    }
    if jit_known_class(shared, cm, cp_class_name, holder) != Some(holder) {
        return false;
    }
    let store = cm.class_store();
    let Some(class) = store.get(holder) else {
        return false;
    };
    if !class.is_interface() {
        return false;
    }
    let Some(method) = class.find_method(method_name, method_descriptor) else {
        return false;
    };
    if method.is_static() {
        return false;
    }
    let Some(code_attr) = method.code() else {
        return false;
    };
    let code = code_attr.code.as_bytes();
    if compiled_code.is_some_and(|compiled| !compiled.starts_with(code)) {
        return false;
    }
    let handlers: Vec<u16> = code_attr
        .exception_table
        .iter()
        .map(|entry| entry.handler_pc)
        .collect();
    private_iface_this::every_site_receiver_is_this(code, &handlers, cp_idx, only_pc, callee_desc)
}

/// JIT round 20 wave 7 (lane huntDisp): does the `opcode` invoke of the
/// method reference at `cp_idx` of `referencing`'s pool (naming
/// `cp_class_name.name desc`) end in the linkage error the interpreter raises
/// on every execution -- the reference's tag against its class
/// (`invoke::methodref_kind_mismatch`: `Found class C, but interface was
/// expected` and its kin, `--real-jdk` / selection checks), then the static
/// flag (`selection::static_flag_mismatch`: `Expecting non-static method` for
/// `invokevirtual` / `invokespecial` / `invokeinterface` of a static method,
/// `Expected static method` for `invokestatic` of an instance method)? The
/// same two questions, in the same order, as `invoke.rs`'s instance arm and
/// `dispatch_static.rs`'s static arm. A compiled site asks neither: the
/// dispatch helpers resolve the call by name, so a method warmed on a cold
/// branch ran the callee where HotSpot throws (`R20HuntDispLink`
/// `cold.SiteVirtStatic` / `cold.SiteStaticInst` / `cold.SiteIfaceOnClass`).
/// `false` when the owner is not known to `referencing` (the run-time
/// resolution decides), for a constructor or initializer, and for an array
/// owner. Under the caller's class-manager guard.
pub(in crate::runtime::interpreter::jit_bridge) fn invoke_site_linkage_error(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    referencing: ClassId,
    cp_idx: u16,
    cp_class_name: &str,
    name: &str,
    desc: &str,
    opcode: u8,
) -> bool {
    use super::invoke::MethodrefUse;
    if name.starts_with('<') || cp_class_name.starts_with('[') {
        return false;
    }
    let use_ = match opcode {
        0xb6 => MethodrefUse::Virtual,
        0xb7 => MethodrefUse::Special,
        0xb8 => MethodrefUse::Static,
        0xb9 => MethodrefUse::Interface,
        _ => return false,
    };
    let Some(owner) = jit_known_class(shared, cm, cp_class_name, referencing) else {
        return false;
    };
    super::invoke::methodref_kind_mismatch(shared, cm, referencing, cp_idx, owner, use_, name, desc)
        .is_some()
        || crate::runtime::resolve::selection::static_flag_mismatch(
            cm.class_store(),
            owner,
            name,
            desc,
            opcode == 0xb8,
        )
        .is_some()
        // Interpreter round i5 wave 100, lane L2 (`L2W100UnresolvedSiteWarm`
        // v2/vp/i2 JIT arms): a reference that resolves to nothing at all
        // (separate compilation removed the method) is the interpreter's
        // `NoSuchMethodError` on every execution too
        // (`invoke::method_ref_resolution_fails`). Compiled, the site bound
        // by the receiver's class and ran a method the receiver declares.
        || super::invoke::method_ref_resolution_fails(
            shared,
            cm,
            referencing,
            cp_idx,
            owner,
            cp_class_name,
            name,
            desc,
        )
}

/// `CRATONVM_DBG_JITC=1`: the line a door prints when
/// [`special_site_bind_refusal`] refuses a site (the positive control of
/// `L2W37SpecialSelectionColdSite.java`).
pub(in crate::runtime::interpreter) fn dbg_special_site_refusal(
    door: &str,
    caller: &str,
    method: &str,
    descriptor: &str,
    site: (&str, &str, &str),
    why: &str,
) {
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] invokespecial-selection REFUSED {door} {caller}.{method}{descriptor} \
             site {}.{}{}: {why}",
            site.0, site.1, site.2
        );
    }
}

/// For an invoke site in `holder` that names `cp_class`, returns two things.
/// The first is the id of `cp_class` resolved in `holder`'s loader, which is
/// the class an exact receiver guard compares against. The second is the name
/// of the class that declares `name descriptor` as resolved from `cp_class`.
///
/// Reads through a class-manager guard the caller already holds and takes no
/// lock of its own. That is what lets `compile_osr_artifact`'s invoke loop,
/// which holds `cm_lock` for its whole body, ask it without a recursive read.
pub(in crate::runtime::interpreter::jit_bridge) fn site_class_and_declaring_class_name(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    cp_class: &str,
    name: &str,
    descriptor: &str,
) -> Option<(u32, String)> {
    site_class_and_declaring_class(shared, cm, holder, (cp_class, None), name, descriptor)
        .map(|(cp_class_id, _, declaring_class)| (cp_class_id, declaring_class))
}

/// [`site_class_and_declaring_class_name`] with the declaring class's id
/// beside its name: one hierarchy walk answers both, so the name and the
/// identity a compile names a call's declaring class by cannot disagree
/// (interpreter round i1 wave 17, L3). `cp_class` carries the
/// `CONSTANT_Class` entry's index when the caller read the name through it
/// ([`jit_known_class_at`]; JIT round 16, lane classsite).
pub(in crate::runtime::interpreter::jit_bridge) fn site_class_and_declaring_class(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    holder: ClassId,
    (cp_class, class_index): (&str, Option<u16>),
    name: &str,
    descriptor: &str,
) -> Option<(u32, ClassId, String)> {
    let cp_class_id = jit_known_class_at(shared, cm, cp_class, holder, class_index)?;
    let store = cm.class_store();
    let (_, declaring_id) =
        crate::classloading::find_method_recursive(cp_class_id, name, descriptor, store)?;
    let declaring_class = store.get(declaring_id)?.name.to_string();
    Some((cp_class_id.as_u32(), declaring_id, declaring_class))
}

/// Whether a cached OSR artifact must be recompiled rather than reused
/// because it still carries a speculative guard this VM has de-speculated
/// since the artifact was compiled (round 9 wave 10, lane `call10`;
/// `evicted-callee-stays-reachable-through-a-compiled-callers-binding-FIXED-20260918.md`).
///
/// `compile_osr_artifact` reuses a cached OSR body whenever it can enter at
/// the pc. The per-bci de-spec (`DespecRegistry`) is honoured by every NEW
/// compile, but an OSR body compiled before the insert kept its failing guard
/// for good: `TryLoop.sumThrow`'s loop-header speculative-BCE guard OSR-exited
/// at once on every entry, each exit was charged, and the method reached
/// `MakeNotCompilable` long before anything recompiled it.
///
/// All of these must hold, so the answer is `false` (reuse, as before) in
/// every ordinary case:
///
/// * `deopt_real_enabled()` -- it stamps `compilation_epoch` on the artifact;
/// * the artifact is OLDER than the method's live compilation epoch (a deopt
///   was recorded since it was compiled). A recompile is stamped with the live
///   epoch, so a guard the recompile still emits does not recompile again
///   until the next charged deopt: the loop is bounded by the deopt ledger;
/// * one of its speculative deopt points (anything but an `OsrExit` map, which
///   every OSR body carries at its loop boundaries and no de-spec removes) is
///   now de-speculated -- `contains_speculation`, so a wildcard entry matches;
/// * the method is not on the not-compilable skip list: the recompile would be
///   refused there, and a stale body is still better than none.
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn cached_osr_body_is_despec_stale(
    shared: &SharedVm,
    cached: &crate::jit::CompiledMethod,
    class_name: &Arc<str>,
    method_name: &Arc<str>,
    descriptor: &Arc<str>,
) -> bool {
    if !cratonvm_jit::deopt_real_enabled()
        || (cached.deopt_points.is_empty() && cached._deopt_point_boxes.is_empty())
    {
        return false;
    }
    let method_key = format!("{class_name}.{method_name}:{descriptor}");
    if cached.compilation_epoch >= shared.compilation_epoch_for(&method_key) {
        return false;
    }
    let registry = &shared.jit.despec_registry;
    if registry.count_for(&method_key) == 0 {
        return false;
    }
    let keeps_withdrawn_guard = cached
        .deopt_points
        .iter()
        .chain(cached._deopt_point_boxes.iter().map(|b| &**b))
        .any(|p| {
            if p.reason == cratonvm_jit::deopt::DeoptReason::OsrExit {
                return false;
            }
            let check_bci = if p.frame_state.caller.is_some() {
                let mut f = &p.frame_state;
                while let Some(ref c) = f.caller {
                    f = c;
                }
                f.bci
            } else {
                p.bci
            };
            registry.contains_speculation(&method_key, check_bci, p.speculation_id)
                || (p.frame_state.caller.is_some()
                    && !p.frame_state.method_key.is_empty()
                    && p.bci != u32::MAX
                    && registry.contains_speculation(
                        &method_key,
                        check_bci,
                        cratonvm_jit::deopt::chain_predicate_speculation_id(
                            &p.frame_state.method_key,
                            p.bci,
                            cratonvm_jit::deopt::PredicateKind::UnstableIf,
                        ),
                    ))
        });
    if !keeps_withdrawn_guard {
        return false;
    }
    let skip_key = (
        Arc::clone(class_name),
        Arc::clone(method_name),
        Arc::clone(descriptor),
    );
    !shared.jit.jit_skip_set.read().contains(&skip_key)
}



/// The speculative guard an OSR body left through at `bci`, when the exit can
/// be attributed to exactly one (round 9 wave 11, lane `osr11`).
///
/// The reconstructed frame carries a bci and no reason, and a loop header
/// carries TWO points: the speculative-BCE guard (`BoundsCheck`) and the
/// OSR-exit map (`OsrExit`). `osr_exit::deopt_reason_at_bci` sees both and
/// answers `Ambiguous`; the first-in-list pick the first-call tier-up sink
/// uses answers `OsrExit`. Neither is what fired. An `OsrExit` map is never a
/// failed speculation: its stub (reason 7) is jumped to by the test triggers
/// (`CRATONVM_OSR_EXIT_TEST` / `CRATONVM_OSR_EXIT_AFTER`), which
/// [`charge_osr_guard_exit`] refuses to run under, and since wave 12 by an OSR
/// body's back-edge poll when the interpreter is required
/// ([`osr_exit_left_for_the_interpreter`]). So the `OsrExit` maps are
/// skipped here, as are the exceptional (`RETHROW`) frames, which go to a
/// different stash.
///
/// Only the speculations the single-pass emitter drops once de-spec'd are
/// answered: `BoundsCheck` (the loop-header BCE guard, `x64/driver.rs`) and
/// `ReceiverTypeChanged` (the call-site type guards, `x64/op_invoke.rs`). A
/// guard the recompile would re-emit whatever the registry says must not be
/// charged per exit, or the charges alone would retire the method. Anything
/// else -- the invokedynamic trap's `UnreachedCode` above all, whose policy is
/// "give up on the first" -- keeps the old uncharged behaviour.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_guard_exit_reason(
    deopt_points: &[cratonvm_jit::deopt::DeoptimizationPoint],
    bci: u32,
) -> Option<cratonvm_jit::deopt::DeoptReason> {
    use cratonvm_jit::deopt::DeoptReason;
    unique_guard_reason_at_bci(deopt_points, bci).filter(|r| {
        matches!(
            r,
            DeoptReason::BoundsCheck | DeoptReason::ReceiverTypeChanged
        )
    })
}

/// The one reason the non-`OsrExit`, non-`RETHROW` points at `bci` agree on,
/// or `None` when there is none or they disagree. See
/// [`osr_guard_exit_reason`] for why the `OsrExit` maps are skipped.
pub(in crate::runtime::interpreter::jit_bridge) fn unique_guard_reason_at_bci(
    deopt_points: &[cratonvm_jit::deopt::DeoptimizationPoint],
    bci: u32,
) -> Option<cratonvm_jit::deopt::DeoptReason> {
    use cratonvm_jit::deopt::DeoptReason;
    let mut found: Option<DeoptReason> = None;
    for p in deopt_points {
        if p.bci != bci || p.semantics.rethrow_exception || p.reason == DeoptReason::OsrExit {
            continue;
        }
        match found {
            None => found = Some(p.reason),
            Some(r) if r == p.reason => {}
            Some(_) => return None,
        }
    }
    found
}

/// Charge a speculative-guard exit out of an OSR body, and let the per-bci
/// de-spec withdraw the guard (round 9 wave 11, lane `osr11`;
/// `evicted-callee-stays-reachable-through-a-compiled-callers-binding-FIXED-20260918.md`).
///
/// The OSR-exit TRANSFER in [`try_osr`] used to charge NOTHING. So an OSR body
/// whose loop-header speculative-BCE guard fails on every entry
/// (`TryLoop.sumThrow(a, a.length + 5)`: `n > a.length`, so the guard exits at
/// once) was reused forever: 5 entries per call, each an immediate exit, and
/// the rest of the 65 541-iteration loop interpreted -- 2 550 `OSR-reuse` +
/// `OSR-exit TRANSFER` pairs on `TryLoopRefl` (8.5 s against HotSpot's 16 ms),
/// and the same ~500 calls before `TryLoop throw` finally de-spec'd through its
/// method-entry trap (the remaining 2.4 s). Wave 10's
/// [`cached_osr_body_is_despec_stale`] could never fire: the ledger never
/// heard of these exits, so neither the epoch nor the registry moved.
///
/// Now the exit goes through the same policy a trapped method-entry body gets
/// (`helpers::despeculate_trapped_method`): charged under the guard's own
/// reason at its bci (evicting the body on the count-based schedule), and
/// after `PER_BCI_DESPEC_LIMIT` (4) charges at that bci the speculation is
/// recorded in the de-spec registry, so the next OSR compile (and the next
/// method-entry compile) drops the guard and keeps per-access bounds checks.
///
/// Called only AFTER a successful transfer: the frame is already parked at its
/// resume bci, and nothing here touches it. An inlined caller chain is
/// charged at its call site but answers `false` (round 14 wave 3). Not run
/// for the re-run sentinel, or under the
/// OSR-exit / eager-deopt TEST triggers, whose exits come from `OsrExit` maps
/// or synthetic reason-2 jumps rather than from a failed speculation.
///
/// Returns whether the exit was charged. A charged exit is bounded by the
/// guard's own despeculation, so the caller does not ALSO charge it against the
/// per-pc OSR rejection budget (see `try_osr`'s TRANSFER arm).
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn charge_osr_guard_exit(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    compiled: &crate::jit::CompiledMethod,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> bool {
    if rframe.bci == u32::MAX
        || cratonvm_jit::osr_exit_test_enabled()
        || cratonvm_jit::osr_exit_after().is_some()
        || cratonvm_jit::deopt_eager_enabled()
        || cratonvm_jit::deopt_eager_bci_override().is_some()
    {
        return false;
    }
    // Round 14 wave 3 (lane resume; CH3-4): an inlined chain's bci is a
    // callee's, so it is charged at its call site instead, and never reported
    // as charged: the spliced guard survives that de-spec, so the rejection
    // budget stays its brake (`deopt_resume::charge_osr_chain_guard_exit`).
    if !rframe.caller_frames.is_empty() {
        // JIT round 23 wave 4 (lane L1): a spliced loop's entry predicate
        // (`deopt_resume::charge_osr_chain_predicate_exit`) is charged like
        // the flat predicate below -- under its own chain id at the call site,
        // splice kept -- and, being bounded by that withdrawal, reported
        // charged.
        if super::deopt_resume::charge_osr_chain_predicate_exit(
                shared,
                class_id,
                class_name,
                method_name,
                descriptor,
                rframe,
                cause,
            )
        {
            return true;
        }
        return super::deopt_resume::charge_osr_chain_guard_exit(
            shared,
            class_id,
            class_name,
            method_name,
            descriptor,
            rframe,
            cause,
        );
    }
    // JIT round 23 wave 3 review (`CRATONVM_JIT_OSR_PREDICATE_EXIT_CHARGE`): a
    // loop-entry predicate's exit is charged as the method-entry sink charges
    // the same guard's trap, so the next OSR compile drops a predicate that
    // keeps failing instead of the rejection budget retiring the header.
    if osr_predicate_exit_charge(
            compiled
                .deopt_points
                .iter()
                .chain(compiled._deopt_point_boxes.iter().map(|b| &**b)),
            &compiled.ir_osr_opaque_entry_bcis,
            rframe.bci,
            !rframe.caller_frames.is_empty(),
            cause,
        )
    {
        if dbg_deopt_enabled() {
            eprintln!(
                "[cratonvm-deopt] OSR predicate exit charged: {class_name}.{method_name}{descriptor} \
                 bci={} id={:#x}",
                rframe.bci,
                cause.map_or(0, |c| c.speculation_id)
            );
        }
        crate::jit::helpers::despeculate_osr_predicate_exit(
            shared,
            class_name,
            method_name,
            descriptor,
            class_id,
            rframe,
            cause,
        );
        return true;
    }
    // The stash's own cause first (round 11 wave 5), when this artifact has
    // that point at this bci; it is held to the same two charged reasons.
    let from_cause = cause
        .filter(|c| {
            compiled.deopt_points.iter().any(|p| {
                p.bci == rframe.bci && p.reason == c.reason && !p.semantics.rethrow_exception
            })
        })
        .map(|c| c.reason);
    let Some(reason) =
        from_cause.or_else(|| osr_guard_exit_reason(&compiled.deopt_points, rframe.bci))
    else {
        return false;
    };
    // A cause is held to the same two charged reasons `osr_guard_exit_reason`
    // filters to.
    if !matches!(
        reason,
        cratonvm_jit::deopt::DeoptReason::BoundsCheck
            | cratonvm_jit::deopt::DeoptReason::ReceiverTypeChanged
    ) {
        return false;
    }
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR guard exit charged: {class_name}.{method_name}{descriptor} \
             reason={reason:?} bci={}",
            rframe.bci
        );
    }
    crate::jit::helpers::despeculate_trapped_method(
        shared,
        class_name,
        method_name,
        descriptor,
        class_id,
        reason,
        rframe.bci,
    );
    true
}



/// Is this OSR guard exit a loop-entry predicate's, to be charged by
/// `helpers::despeculate_osr_predicate_exit` (JIT round 23 wave 3 review,
/// `docs/feature-designs/deopt-osr.md`)?
///
/// A predicate guard (range, clamp, length, mask, null-base, ...) is an
/// `UncommonTrap` at its loop header's bci. Before this fix, `charge_osr_guard_exit`
/// charged only `BoundsCheck` / `ReceiverTypeChanged`. A predicate that failed
/// on every entry therefore left uncharged, and every recompile planted it
/// again until the per-pc rejection budget retired the header. The loop then
/// ran interpreted.
///
/// `true` for a flat frame at a known bci whose stashed cause is an
/// `UncommonTrap` of one of this artifact's points at that bci (`points`), and
/// for which either holds:
/// - the cause names a planter
///   (`cratonvm_jit::deopt::is_predicate_speculation_id`, set under
///   `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`): the planter's own id is
///   withdrawn;
/// - the bci is an OPAQUE OSR entry header of the body (`opaque_entry_bcis`,
///   sorted): every `UncommonTrap` guard there is a loop-entry speculation (a
///   predicate, or the arm's exact / profiled local checks), and every one of
///   them is withdrawn by the wildcard at the header that the charge records
///   (`deopt::predicate_withdrawn`, and `lib.rs`'s `arm_checks`).
///
/// Any other `UncommonTrap` keeps the old uncharged exit. That includes the
/// invokedynamic trap, whose policy is to give up on the first.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_predicate_exit_charge<'p>(
    mut points: impl Iterator<Item = &'p cratonvm_jit::deopt::DeoptimizationPoint>,
    opaque_entry_bcis: &[u32],
    frame_bci: u32,
    is_chain: bool,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> bool {
    use cratonvm_jit::deopt::DeoptReason;
    let Some(c) = cause else {
        return false;
    };
    if is_chain || frame_bci == u32::MAX || c.reason != DeoptReason::UncommonTrap {
        return false;
    }
    let own_point = points.any(|p| {
        p.bci == frame_bci && p.reason == DeoptReason::UncommonTrap && !p.semantics.rethrow_exception
    });
    own_point
        && (cratonvm_jit::deopt::is_predicate_speculation_id(frame_bci, c.speculation_id)
            || opaque_entry_bcis.binary_search(&frame_bci).is_ok())
}

/// Did the OSR body just transferred into `frame_idx` leave through an
/// `OsrExit` map because the interpreter is now required (interpreter round
/// i1 wave 12, lane L4)? The back-edge poll's verdict
/// (`jvmti_events::compiled_loop_must_leave`) is not carried to the sink, so
/// the same question is asked again: an `OsrExit` cause while the frame's
/// method must run interpreted. The only other `OsrExit` exits are the test
/// triggers, which do not coincide with an agent's request.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_exit_left_for_the_interpreter(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> bool {
    if !matches!(
        cause,
        Some(c) if c.reason == cratonvm_jit::deopt::DeoptReason::OsrExit
    ) {
        return false;
    }
    thread.frames.get(frame_idx).is_some_and(|frame| {
        super::jvmti_requires_interpreter_for_method(
            shared,
            frame.class_id,
            frame.method_name(),
            frame.method_descriptor(),
        )
    })
}

/// Did the single-pass OSR body `compiled` just leave through an `OsrExit` map
/// for PROFIT (JIT round 23 wave 1, lane L1; THROWS22-1,
/// `CRATONVM_JIT_OSR_PROFIT_EXIT`)? The body is armed
/// ([`arm_single_pass_osr_profit_exit`]) and the redefinition does not own
/// its exits (`JitCache::osr_profit_exit_may_leave`, the question the poll's
/// verdict asked). Its only other `OsrExit` exits are a JVMTI interpreter-only
/// request (answered by [`osr_exit_left_for_the_interpreter`] the same way)
/// and the test triggers, which do not run with a profit exit in flight. A
/// `true` answer is counted and, under `CRATONVM_DBG_JITC`, said.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_profit_exit_left(
    shared: &SharedVm,
    compiled: &cratonvm_jit::CompiledMethod,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
) -> bool {
    if !matches!(
        cause,
        Some(c) if c.reason == cratonvm_jit::deopt::DeoptReason::OsrExit
    ) || !compiled.osr_profit_exit_armed()
        || !shared.jit.jit_cache.osr_profit_exit_may_leave(compiled)
    {
        return false;
    }
    let n = shared.jit.jit_cache.note_osr_profit_exit_taken();
    if crate::runtime::env_cache::dbg_jitc() {
        let (armed, verdicts, taken) = shared.jit.jit_cache.osr_profit_exit_counts();
        eprintln!(
            "[cratonvm-jitc] osr profit exit TAKEN #{n}: {} (armed={armed} verdicts={verdicts} \
             taken={taken})",
            compiled.method_label
        );
    }
    true
}

/// JIT round 23 wave 1, lane L1 (THROWS22-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`,
/// `CRATONVM_JIT_OSR_PROFIT_EXIT`, default ON): an optimizing OSR body of
/// `names` was just remembered; when one can now be entered from the door's
/// cache at EVERY loop header of `code`
/// (`cratonvm_jit::tiered::osr_profit_exit_headers`, each asked through
/// [`osr_optimizing_cached_peek`], the door's own question without its side
/// effects), arm the method's
/// published SINGLE-PASS OSR body for a profit exit
/// (`JitCache::arm_osr_profit_exit`).
///
/// Before, a frame already running the single-pass body -- entered while the
/// worker still owed its optimizing verdict (tier proposal 29's arm), or
/// before a transient refusal cleared -- ran it until the activation
/// returned, and a `main`-shaped activation never returns. Armed, its next
/// back-edge poll leaves for the interpreter through the poll's `OsrExit`
/// map (the exit a JVMTI interpreter-only request takes), the door's sink
/// commits it ([`osr_profit_exit_left`]), and the interpreter's next back
/// edge enters the optimizing body.
///
/// Every header, not only `entry_pc`: an armed body's every exit poll is
/// forced, so a frame inside another loop of the method leaves too, and must
/// find an optimizing body where its next back edge lands. A header the
/// optimizing body cannot be entered at keeps the body unarmed. The door
/// never re-enters or reuses an armed body (the re-entry memo filter, the
/// `profit_armed` reuse refusal in [`compile_osr_artifact`]), so a frame the
/// optimizing entry refuses after all (a slot-kind disagreement) gets a fresh
/// single-pass body, not a ping-pong. Nothing here runs when no single-pass
/// body is published, or it is an IR body, or a redefinition owns its exits.
pub(in crate::runtime::interpreter::jit_bridge) fn arm_single_pass_osr_profit_exit(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    code: &Arc<[u8]>,
    door: &str,
) {
    let (class_name, method_name, method_descriptor) = names;
    if !shared.jit.jit_cache.osr_profit_exit_enabled() {
        return;
    }
    let Some(single_pass) = shared.jit.jit_cache.read().get_osr(
        class_name,
        method_name,
        method_descriptor,
        class_id,
    ) else {
        return;
    };
    if !single_pass.compiled_via_osr
        || single_pass.used_ir_backend
        || single_pass.osr_profit_exit_armed()
    {
        return;
    }
    let headers = cratonvm_jit::tiered::osr_profit_exit_headers(code);
    let uncovered = headers.iter().copied().find(|&pc| {
        let key = osr_optimizing_refusal_key(
            shared.vm_identity,
            class_id,
            method_name,
            method_descriptor,
            pc,
        );
        !osr_optimizing_cached_peek(shared, class_id, names, key, pc, code)
    });
    let dbg = crate::runtime::env_cache::dbg_jitc();
    if headers.is_empty() || uncovered.is_some() {
        if dbg {
            eprintln!(
                "[cratonvm-jitc] osr profit exit{door} {class_name}.{method_name} NOT armed: \
                 no optimizing entry at header pc={:?} (headers={headers:?})",
                uncovered
            );
        }
        return;
    }
    let armed = shared.jit.jit_cache.arm_osr_profit_exit(&single_pass);
    // t1 lane jitea (L1W1-2, `CRATONVM_JIT_OSR_PROFIT_EXIT_SUPERSEDED`, default ON, `=0` kills):
    // a frame still running an OLDER single-pass OSR body of this method (one a
    // de-spec-stale or late-bind recompile replaced in the OSR map) is armed
    // too, so it leaves at its next back edge instead of running the old body
    // for the rest of its activation. Same verdict, same refusals.
    if armed.is_ok() {
        for old in shared
            .jit
            .jit_cache
            .superseded_single_pass_osr_bodies(&single_pass)
        {
            let r = shared.jit.jit_cache.arm_osr_profit_exit(&old);
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] osr profit exit{door} superseded {class_name}.{method_name}\
                     {method_descriptor} id={}: {r:?}",
                    old.compile_id
                );
            }
        }
    }
    if dbg {
        let (n, verdicts, taken) = shared.jit.jit_cache.osr_profit_exit_counts();
        match armed {
            Ok(sites) => eprintln!(
                "[cratonvm-jitc] osr profit exit{door} ARMED {class_name}.{method_name}\
                 {method_descriptor} id={} polls-forced={sites} headers={headers:?} \
                 (armed={n} verdicts={verdicts} taken={taken})",
                single_pass.compile_id
            ),
            Err(why) => eprintln!(
                "[cratonvm-jitc] osr profit exit{door} {class_name}.{method_name} NOT armed: \
                 {why}"
            ),
        }
    }
}



/// A rebuilt optimizing OSR body of the method was just remembered at every
/// header it was built for: arm the bodies a late bind superseded
/// ([`JitBridgeState::late_bind_osr_superseded`]) so a frame still running one
/// leaves for the interpreter at its next back edge, where the door enters
/// the rebuilt body. `main`-shaped activations never return, so without this
/// a class first used after the OSR compile kept the old body's deferred
/// `new` and by-name `<init>` helper for the life of the frame
/// (`osr-body-keeps-deferred-new-and-dispatch-helper-...-20261025.md`, 320 ns
/// against 3.5 ns per `new P(i, 2)`).
///
/// The arming is the OSR profit exit's (`JitCache::arm_osr_profit_exit`, the
/// route a single-pass body takes when an optimizing one is ready): the
/// poll's verdict names the armed body, the door commits the exit
/// ([`osr_profit_exit_left`]) and never re-enters an armed body. Asked only
/// when every header of the method has an enterable body
/// ([`osr_optimizing_cached_peek`]), as the single-pass arming asks; rows of a
/// method that is not covered yet stay for the next rebuild while a frame
/// still holds their body (weak rows, t1 wave 9 lane JITB).
pub(in crate::runtime::interpreter::jit_bridge) fn arm_superseded_optimizing_osr_exits(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    code: &Arc<[u8]>,
) {
    let (class_name, method_name, method_descriptor) = names;
    let mine: Vec<SupersededOsrBody> = {
        let Ok(mut superseded) = shared.jit.bridge.late_bind_osr_superseded.lock() else {
            return;
        };
        if superseded.is_empty() {
            return;
        }
        // t1 wave 9, lane JITB: a row whose body no frame runs any more (its
        // weak no longer upgrades) is dropped here, whichever method it names.
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *superseded)
            .into_iter()
            .filter(SupersededOsrBody::live)
            .partition(|row| {
                row.class_id == class_id.as_u32()
                    && row.method_name == method_name
                    && row.descriptor == method_descriptor
            });
        *superseded = rest;
        mine
    };
    if mine.is_empty() {
        return;
    }
    let dbg = crate::runtime::env_cache::dbg_jitc();
    let headers = cratonvm_jit::tiered::osr_profit_exit_headers(code);
    let uncovered = headers.iter().copied().find(|&pc| {
        let key = osr_optimizing_refusal_key(
            shared.vm_identity,
            class_id,
            method_name,
            method_descriptor,
            pc,
        );
        !osr_optimizing_cached_peek(shared, class_id, names, key, pc, code)
    });
    if headers.is_empty() || uncovered.is_some() {
        if dbg {
            eprintln!(
                "[cratonvm-jitc] late-bind OSR exit NOT armed {class_name}.{method_name}: no optimizing entry at header pc={uncovered:?} (headers={headers:?})"
            );
        }
        // Weak rows: re-pushing keeps no body alive, and a row whose frame
        // left meanwhile is dropped by the push.
        if let Ok(mut superseded) = shared.jit.bridge.late_bind_osr_superseded.lock() {
            for row in mine {
                push_superseded_osr_row(&mut superseded, row);
            }
        }
        return;
    }
    for row in mine {
        // Upgraded into a `RetainedCode`: if the running frame left between
        // the upgrade and here, this handle is the last one, and its release
        // must go through the retirement queue.
        let Some(body) = row.body.upgrade().map(cratonvm_jit::RetainedCode::new) else {
            continue;
        };
        let armed = shared.jit.jit_cache.arm_osr_profit_exit(body.arc());
        if dbg {
            match armed {
                Ok(sites) => eprintln!(
                    "[cratonvm-jitc] late-bind OSR exit ARMED {class_name}.{method_name}{method_descriptor} id={} polls-forced={sites} headers={headers:?}",
                    body.compile_id
                ),
                Err(why) => eprintln!(
                    "[cratonvm-jitc] late-bind OSR exit {class_name}.{method_name} NOT armed: {why}"
                ),
            }
        }
    }
}

/// A background optimizing OSR task left bodies of the method waiting for
/// their exit ([`JitBridgeState::late_bind_osr_superseded`]). The arming wants
/// every header enterable, and a header the rebuilt body was not built for
/// answers only after its first door lookup (`poly_decline_unasked`), which a
/// frame that never leaves the old body never makes: the fill loop's body of
/// a method whose second loop makes the calls. Ask each other header's door
/// here, then arm again. Cold: once per such task; one lock when no body
/// waits.
pub(in crate::runtime::interpreter::jit_bridge) fn cover_headers_for_superseded_osr_exits(
    shared: &SharedVm,
    key: &crate::jit::tiered::MethodKey,
    osr_bci: usize,
) {
    let Some((class_id, code, _max_locals)) = fetch_osr_compile_inputs(
        shared,
        key.class_id,
        &key.class_name,
        &key.method_name,
        &key.descriptor,
    ) else {
        return;
    };
    let names: (&str, &str, &str) = (&key.class_name, &key.method_name, &key.descriptor);
    let waits = shared.jit.bridge.late_bind_osr_superseded.lock().is_ok_and(|rows| {
        rows.iter().any(|row| {
            row.class_id == class_id.as_u32()
                && row.method_name == names.1
                && row.descriptor == names.2
        })
    });
    if !waits {
        return;
    }
    for pc in cratonvm_jit::tiered::osr_profit_exit_headers(&code) {
        if pc != osr_bci {
            let opt_key =
                osr_optimizing_refusal_key(shared.vm_identity, class_id, names.1, names.2, pc);
            // For its side effect: the header's one-time decline question.
            let _ = osr_optimizing_cached(shared, class_id, names, opt_key, pc, &code);
        }
    }
    arm_superseded_optimizing_osr_exits(shared, class_id, names, &code);
}

/// `CRATONVM_JIT_OSR_PROFIT_EXIT_SUPERSEDED_OPTIMIZING` (t1 wave 8, lane
/// L1OSR; L1W1-4 of `docs/feature-designs/jit-ir-optimizations.md`;
/// perf, default ON since j27 2026-10-28, `=0` kills): an optimizing OSR body
/// the memo stops serving because a
/// newer body of the same method replaced it at every header it was
/// remembered under (an acceptance re-offer, a de-spec rebuild) is queued for
/// the profit exit ([`queue_superseded_optimizing_osr_bodies`]), so a frame
/// still running it -- a `main`-shaped activation that never returns -- leaves
/// at its next back-edge poll and re-enters through the door, which serves the
/// newer body. A late-bind rebuild already takes this route
/// (`late_bind_osr_superseded`). Read once per optimizing OSR build.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_profit_exit_superseded_optimizing_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The distinct artifacts the optimizing OSR memo remembers for `names`'s
/// method of `class_id`, under any header, other than `fresh`. One lock of
/// the memo; a cold path (once per optimizing OSR build, behind
/// [`osr_profit_exit_superseded_optimizing_enabled`]).
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_remembered_other_artifacts(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    fresh: Option<&Arc<cratonvm_jit::CompiledMethod>>,
) -> Vec<cratonvm_jit::RetainedCode> {
    let method = osr_optimizing_refusal_key(shared.vm_identity, class_id, names.1, names.2, 0);
    let Ok(memo) = shared.jit.bridge.osr_optimizing_accepted.lock() else {
        return Vec::new();
    };
    let mut out: Vec<cratonvm_jit::RetainedCode> = Vec::new();
    for (key, entry) in memo.iter() {
        let body = entry.artifact.arc();
        if key.0 == method.0
            && key.1 == method.1
            && !fresh.is_some_and(|f| Arc::ptr_eq(f, body))
            && !out.iter().any(|o| Arc::ptr_eq(o.arc(), body))
        {
            out.push(entry.artifact.clone());
        }
    }
    out
}

/// L1W1-4 (`CRATONVM_JIT_OSR_PROFIT_EXIT_SUPERSEDED_OPTIMIZING`): of the
/// artifacts `before` the memo served for this method ahead of the build,
/// queue every one the memo no longer remembers under ANY header as a
/// superseded row ([`JitBridgeState::late_bind_osr_superseded`]), so
/// [`arm_superseded_optimizing_osr_exits`], which the build calls next, arms
/// it once every header of the method has an enterable body. An artifact
/// still remembered under some header (one the new body could not cover, a
/// handler-closed header) is not queued: arming it would make the door refuse
/// it there (the memo's armed-hit filter) and rebuild. A frame that runs no
/// queued body loses nothing: the arm forces polls nobody reaches, and the
/// row is dropped once armed. Bounded by [`LATE_BIND_CALLEE_CAP`].
pub(in crate::runtime::interpreter::jit_bridge) fn queue_superseded_optimizing_osr_bodies(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    before: Vec<cratonvm_jit::RetainedCode>,
) {
    if before.is_empty() {
        return;
    }
    let still = osr_optimizing_remembered_other_artifacts(shared, class_id, names, None);
    let (kept, gone): (Vec<_>, Vec<_>) = before
        .into_iter()
        .partition(|old| still.iter().any(|s| Arc::ptr_eq(s.arc(), old.arc())));
    drop(kept);
    drop(still);
    let dbg = crate::runtime::env_cache::dbg_jitc();
    if let Ok(mut superseded) = shared.jit.bridge.late_bind_osr_superseded.lock() {
        for old in &gone {
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] osr-optimizing superseded QUEUED {}.{}{} id={}",
                    names.0,
                    names.1,
                    names.2,
                    old.arc().compile_id
                );
            }
            push_superseded_osr_row(
                &mut superseded,
                SupersededOsrBody::new(class_id, names, old.arc()),
            );
        }
    }
    // The rows are weak (t1 wave 9, lane JITB): the strong handles are
    // released here, outside the lock, through the retirement queue. A body
    // no frame runs dies now instead of waiting for a covering rebuild.
    drop(gone);
}

/// Would [`osr_optimizing_cached`] serve a body for `entry_pc` now, asked
/// WITHOUT its side effects (JIT round 23 wave 1, lane L1; item 3 of
/// `docs/feature-designs/deopt-osr.md`): no
/// late-bind stale mark is consumed, no entry is forgotten or rewritten, no
/// dependency check is stored. Every condition under which the door would
/// forget the entry or decline to serve it answers `false`, and so does an
/// entry whose per-header polymorphic decline is still to be asked
/// (`poly_decline_unasked`): the door may decline it there. The OSR profit
/// exit's arming asks this at every header
/// ([`arm_single_pass_osr_profit_exit`]); a `false` keeps the body unarmed.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_optimizing_cached_peek(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    key: (u32, u64, usize),
    entry_pc: usize,
    code: &Arc<[u8]>,
) -> bool {
    use std::sync::atomic::Ordering;
    if osr_optimizing_cache_disabled() {
        return false;
    }
    let bridge = &shared.jit.bridge;
    // A pending late-bind mark: the door forgets this body at its next ask.
    if bridge
        .late_bind_osr_optimizing_stale_count
        .load(Ordering::Acquire)
        != 0
        && bridge
            .late_bind_osr_optimizing_stale
            .lock()
            .map_or(true, |stale| stale.contains(&key))
    {
        return false;
    }
    let generation = osr_redefine_generation(shared, class_id);
    let Ok(accepted) = bridge.osr_optimizing_accepted.lock() else {
        return false;
    };
    let Some(entry) = accepted.get(&key) else {
        return false;
    };
    if entry.withdrawn
        || entry.redefine_generation != generation
        || !same_bytecode(&entry.code, code)
        || (entry.poly_decline_unasked && cratonvm_jit::osr_poly_decline_per_header_enabled())
        || some_first_call_cell_fired(&entry.first_call_cells_at_build)
        || static_init_pending_any_initialized(shared, &entry.static_init_pending_at_build)
        || osr_optimizing_artifact_is_stale(shared, entry.code_state_epoch)
    {
        return false;
    }
    let cached = Arc::clone(entry.artifact.arc());
    let exits_admitted = entry.exits_admitted;
    let checked_through = entry.dependencies_checked_through.load(Ordering::Acquire);
    drop(accepted);
    if shared.jit.jit_cache.body_withdrawn_by_redefinition(&cached) {
        return false;
    }
    if checked_through < cratonvm_jit::jit_invalidation_epoch()
        && !osr_optimizing_installed_dependencies_current(
            shared,
            class_id,
            names,
            &cached,
            checked_through,
        )
    {
        return false;
    }
    // Cast: a bci fits u32 (`IR_MAX_BYTECODE_SIZE` is far below it).
    cached.ir_osr_entry_addr(entry_pc as u32).is_some()
        && (cached.ir_osr_sentinel_free || exits_admitted)
}

/// OSR exits that left compiled code with no frame of this method's to resume
/// from, but with the deopt flag raised: a frameless uncommon trap, a callee's
/// bare trap propagated through a post-invoke check, or a stale flag misread at
/// a `J`/`D` call site. See [`note_osr_unresumable_exit`].
pub(in crate::runtime::interpreter::jit_bridge) static OSR_EXIT_FRAMELESS_REPLAYS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// OSR exits whose stashed frame named ANOTHER method: a compiled callee's
/// frame no call-site service claimed. See [`note_osr_unresumable_exit`].
pub(in crate::runtime::interpreter::jit_bridge) static OSR_EXIT_FOREIGN_STASH_REPLAYS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Optimizing OSR bodies `try_osr` built on the mutator while background
/// compilation was on, and their total wall time in microseconds. Process-wide
/// diagnostics beside the two replay counters above, for the same reason (the
/// jit crate's `OSR_EVENTS` is not this lane's to extend); they decide nothing.
pub(in crate::runtime::interpreter::jit_bridge) static OSR_OPTIMIZING_MUTATOR_BUILDS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(in crate::runtime::interpreter::jit_bridge) static OSR_OPTIMIZING_MUTATOR_BUILD_MICROS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Count one optimizing OSR build run on the mutator under background
/// compilation (see [`osr_optimizing_mutator_builds`]).
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_optimizing_mutator_build(took: std::time::Duration) {
    OSR_OPTIMIZING_MUTATOR_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Saturating: a `u128` of microseconds does not fit a `u64` only after
    // half a million years of compiling.
    let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
    OSR_OPTIMIZING_MUTATOR_BUILD_MICROS.fetch_add(micros, std::sync::atomic::Ordering::Relaxed);
}

/// `(builds, total_micros)`: optimizing OSR bodies built on the mutator while
/// `CRATONVM_BG_COMPILE` was on. Since JIT round 11 (waves 9-10) the worker
/// builds them (`background_osr_optimizing_body`, tier proposal 29's arm in
/// `try_osr`), so this stays at zero unless `CRATONVM_JIT_OSR_OPTIMIZING_BG=0`
/// or `CRATONVM_JIT_OSR_OPTIMIZING_CACHE=0` restores the mutator build; a
/// non-zero count in the default configuration is a regression. See
/// `docs/feature-designs/deopt-osr.md`.
/// Printed on the `CRATONVM_DBG_JITC` exit census.
pub fn osr_optimizing_mutator_builds() -> (u64, u64) {
    (
        OSR_OPTIMIZING_MUTATOR_BUILDS.load(std::sync::atomic::Ordering::Relaxed),
        OSR_OPTIMIZING_MUTATOR_BUILD_MICROS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `(frameless, foreign_stash)` OSR exits since start; expected to stay at
/// zero. Each one left the body with no state to transfer. Since round 11 wave
/// 19 it then answers through `osr_refuse_replay` (`InternalError` for a
/// side-effecting body); with `CRATONVM_JIT_OSR_LOUD_UNRESUMABLE=0` it resumes
/// the interpreter at the OSR entry pc with the frame's pre-entry state, i.e.
/// replays whatever the body had run.
pub fn osr_unresumable_exits() -> (u64, u64) {
    (
        OSR_EXIT_FRAMELESS_REPLAYS.load(std::sync::atomic::Ordering::Relaxed),
        OSR_EXIT_FOREIGN_STASH_REPLAYS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Count an OSR exit `try_osr` can only safe-reject, and deny OSR for the
/// method so the same cause replays at most once (interpreter round i1 wave 4,
/// L7; `interpreter-L7-osr-safe-reject-arms-replay-committed-iterations-RETIRED`,
/// retired in wave 10 on a zero census -- this counter is its tripwire).
///
/// Neither arm has a state to resume from: the live frame was never advanced,
/// so continuing at the entry pc re-executes everything the body ran between
/// the entry and the exit. That is a replay even at the FIRST iteration unless
/// the exit is the loop header itself, which is why the deny is not reserved
/// for "a committed iteration" (which nothing here can observe). The rejection
/// budget alone let one cause replay up to `OSR_MAX_ATTEMPTS` times per pc per
/// activation. The denial is per method and lasts until the install epoch moves
/// (`TieredCompilationManager::mark_osr_denied`), so a recompile gets its chance.
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_unresumable_exit(
    shared: &SharedVm,
    counter: &std::sync::atomic::AtomicU64,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    shared
        .jit
        .tiered_manager
        .mark_osr_denied(crate::jit::tiered::MethodKey::with_class_id(
            class_id,
            class_name,
            method_name,
            descriptor,
        ));
    if dbg_deopt_enabled() {
        eprintln!(
            "[cratonvm-deopt] OSR denied for {class_name}.{method_name}{descriptor} after an \
             unresumable OSR exit (replayed from the entry pc)"
        );
    }
}

/// `CRATONVM_JIT_OSR_LOUD_UNRESUMABLE`, **default ON** (`=0` restores the
/// counted replay): whether `try_osr`'s two counted post-entry arms (a callee's
/// stashed frame, a frameless deopt sentinel) answer through
/// [`osr_refuse_replay`] (a silent replay only for a side-effect-free body,
/// `InternalError` otherwise) instead of replaying the loop from the entry pc
/// once per install epoch. Round 11 wave 19, tier proposal W17-5, after the
/// wave-18 census read `frameless=0 foreign_stash=0`
/// (r11-tier-osr-post-entry-refusals-replay-loop-iterations). Not memoised:
/// both arms are argued unreachable, so the read is off every hot path, and a
/// memo would be one more process static.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_loud_unresumable_exits() -> bool {
    true
}



/// The last single-pass OSR artifact this thread obtained from
/// [`compile_osr_artifact`], and what its answer depended on (round 9 wave 11,
/// lane `osr11`; `perf-implicit-exceptions-in-compiled-code-cost-microseconds-each-FIXED-20260920.md`).
///
/// # Why
///
/// A caught exception in an OSR'd loop is an exception-exit to the
/// interpreter's handler, then a back edge, then a fresh `try_osr` -- one
/// full trip through the door per exception (`ExcLoop.local`: 198 502 OSR
/// entries for 198 500 exceptions). The trip redid, every time, three
/// `String` and three `Arc<str>` allocations, the optimizing tier's
/// mutex-guarded refusal probe, the synchronized-method scan under the
/// class-manager lock, the native-registry probes, the OSR denial probe,
/// `JitCache::get_osr`, and the de-spec staleness check (a `format!` and two
/// map reads) -- to arrive at the artifact it had just left. Sampled on the
/// w10 binary (`ExcLocal`, fast throw on): `try_osr` self 9.7 %,
/// `compile_osr_artifact` 4.1 %, `HashMap<str>::contains_key` 3.4 %,
/// `get_osr` 2.2 %, `compilation_epoch_for` 1.2 %, `mi_malloc` 5.2 %.
///
/// # Why it is exact
///
/// The door's answer for `(this VM, this bytecode, this pc)` is a function of:
///
/// * static facts -- `CRATONVM_DISABLE_JIT` / the bisect levers, the method's
///   `ACC_SYNCHRONIZED` bit, a registered native shadow, the gpu-offload gate:
///   none can change for a method whose loop is running as bytecode;
/// * the JIT cache's contents (`get_osr`): every `put`, `put_osr` and removing
///   invalidation advances [`cratonvm_jit::JitCache::generation`], which is
///   stamped here, and the artifact's own `retired` flag is asked too;
/// * the deopt ledger (the de-spec staleness check reads the method's epoch,
///   the de-spec registry and the not-compilable set): all three move only
///   inside `DeoptimizationController::deoptimize`, which records a deopt
///   first, so the ledger's lifetime `total_deopts` is stamped;
/// * the OSR denial set: asked again on every hit (one read-locked probe);
/// * the optimizing tier's door in `try_osr`: an entry is memoised only when
///   that door answered `None`. Usually it recorded a permanent refusal for
///   the pc (or the tier is off), so it would answer `None` again. The one
///   exception is tier proposal 29's arm, which answers `None` while the
///   WORKER builds the optimizing body; its entry carries
///   `optimizing_pending`, and a hit on it first asks the accepted-body cache
///   ([`osr_reentry_memo_defers_to_optimizing`], round 11 wave 11). Without
///   that, the single-pass body pre-empted the worker's body at every
///   re-entry for as long as nothing else moved the stamp.
///
/// A hit therefore returns the artifact the full trip would have returned.
/// Held as a `Weak`: the memo must not keep a withdrawn body's code alive.
/// One entry per thread -- the hot loop -- keyed by the VM's identity, the
/// frame's bytecode identity, the class id, the method name/descriptor and the
/// pc.
///
/// `vm` is `SharedVm::vm_identity` (a monotonic counter), not the `SharedVm`'s
/// raw address. A destroyed VM's allocation can be reused by a later one on
/// the same thread -- `vm_init.rs`'s own history names the shape exactly:
/// "the two VMs traded heap objects and the reader segfaulted" was an
/// address-keyed side table, fixed by moving it to `vm_identity`. This memo
/// would have the same failure with the address: a fresh VM B started on a
/// thread that had memoised an entry for a destroyed VM A, running the same
/// class (very plausible -- a host that destroys and recreates an identical
/// embedded VM), could match `code`/`class_id`/`method_name`/`descriptor`/
/// `entry_pc` on coincidence and `generation`/`deopts` on coincidence (both
/// start near zero for a fresh VM), and hand B's OSR entry a `CompiledMethod`
/// built against A's heap and class layout. `Weak::upgrade` rules out a
/// dangling pointer, not a live artifact that answers for the wrong VM.
pub(in crate::runtime::interpreter::jit_bridge) struct OsrReentryMemo {
    pub(in crate::runtime::interpreter::jit_bridge) vm: usize,
    pub(in crate::runtime::interpreter::jit_bridge) code: usize,
    pub(in crate::runtime::interpreter::jit_bridge) class_id: ClassId,
    pub(in crate::runtime::interpreter::jit_bridge) method_name: Arc<str>,
    pub(in crate::runtime::interpreter::jit_bridge) descriptor: Arc<str>,
    pub(in crate::runtime::interpreter::jit_bridge) entry_pc: usize,
    pub(in crate::runtime::interpreter::jit_bridge) generation: u64,
    pub(in crate::runtime::interpreter::jit_bridge) deopts: u64,
    pub(in crate::runtime::interpreter::jit_bridge) artifact: std::sync::Weak<cratonvm_jit::CompiledMethod>,
    /// Put while the optimizing route's verdict for this pc was still with
    /// the worker (tier proposal 29's arm), so not yet exact. Cleared once
    /// the verdict is a refusal.
    pub(in crate::runtime::interpreter::jit_bridge) optimizing_pending: bool,
    /// Pending hits of this entry so far (j27 lane ONESHOT's re-ask cadence,
    /// `osr_reentry_pending_reask`); a new entry starts at 0. Kept here, not
    /// in a thread-local of its own, so it lives and dies with the entry.
    pub(in crate::runtime::interpreter::jit_bridge) pending_hits: u32,
}

thread_local! {
    /// See [`OsrReentryMemo`].
    pub(in crate::runtime::interpreter::jit_bridge) static OSR_REENTRY_MEMO: std::cell::RefCell<Option<OsrReentryMemo>> =
        const { std::cell::RefCell::new(None) };
}

/// What an [`OsrReentryMemo`] entry is stamped with: the JIT cache's
/// generation and the deopt ledger's lifetime count, read BEFORE the lookup the
/// entry memoises, so a change racing the lookup can only cause a miss.
/// `None` when the memo is off.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_reentry_stamp(shared: &SharedVm) -> Option<(u64, u64)> {
    let generation = shared.jit.jit_cache.generation();
    // Lock-free (interpreter round i1 wave 5, L7): this used to take the
    // VM-wide `deopt_log` mutex on every OSR entry just to read a count.
    let deopts = shared.jit.bridge.deopt_events();
    Some((generation, deopts))
}

/// The memoised artifact for this entry, when every stamp still holds.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_reentry_memo_get(
    shared: &SharedVm,
    stamp: (u64, u64),
    code: usize,
    class_id: ClassId,
    class_name: &Arc<str>,
    method_name: &Arc<str>,
    descriptor: &Arc<str>,
    entry_pc: usize,
) -> Option<Arc<cratonvm_jit::CompiledMethod>> {
    let vm = shared.vm_identity;
    let artifact = OSR_REENTRY_MEMO.with(|m| -> Option<Arc<cratonvm_jit::CompiledMethod>> {
        let guard = m.try_borrow().ok()?;
        let memo = guard.as_ref()?;
        if memo.vm != vm
            || memo.code != code
            || memo.class_id != class_id
            || memo.entry_pc != entry_pc
            || (memo.generation, memo.deopts) != stamp
            || *memo.method_name != **method_name
            || *memo.descriptor != **descriptor
        {
            return None;
        }
        memo.artifact.upgrade()
    })?;
    // j27 wave 7 (lane T1LAM3): the `Weak` upgrade may be the last owner (a
    // RETIRED body, the first refusal below), so every refusal releases it
    // through the retirement queue.
    let artifact = cratonvm_jit::RetainedCode::new(artifact);
    if artifact.retired.load(std::sync::atomic::Ordering::Acquire)
        || !artifact.can_osr_enter(entry_pc)
    {
        return None;
    }
    let osr_key = crate::jit::tiered::MethodKey::with_class_id(
        class_id,
        Arc::clone(class_name),
        Arc::clone(method_name),
        Arc::clone(descriptor),
    );
    if shared.jit.tiered_manager.is_osr_denied(&osr_key) {
        return None;
    }
    Some(Arc::clone(artifact.arc()))
}

/// Remember the single-pass artifact the door just answered with. Only a body
/// the door would REUSE next time is kept (an OSR-produced body that can enter
/// here); anything else empties the memo.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_reentry_memo_put(
    shared: &SharedVm,
    stamp: (u64, u64),
    code: usize,
    class_id: ClassId,
    method_name: &Arc<str>,
    descriptor: &Arc<str>,
    entry_pc: usize,
    artifact: &Arc<cratonvm_jit::CompiledMethod>,
) {
    let entry = if artifact.compiled_via_osr && artifact.can_osr_enter(entry_pc) {
        Some(OsrReentryMemo {
            vm: shared.vm_identity,
            code,
            class_id,
            method_name: Arc::clone(method_name),
            descriptor: Arc::clone(descriptor),
            entry_pc,
            generation: stamp.0,
            deopts: stamp.1,
            artifact: Arc::downgrade(artifact),
            optimizing_pending: false,
            pending_hits: 0,
        })
    } else {
        None
    };
    OSR_REENTRY_MEMO.with(|m| {
        if let Ok(mut slot) = m.try_borrow_mut() {
            *slot = entry;
        }
    });
}

/// Counts the optimizing OSR entries `try_osr` declined because a seeded
/// local's interpreter tag disagreed with the kind the entry stub reads it as
/// (`CompiledMethod::ir_osr_entry_kind_mismatch`). Diagnostic only; printed on
/// the `CRATONVM_DBG_JITC` exit census. Expected to stay at zero: a non-zero
/// count means the optimizing tier planned a local the interpreter does not
/// hold, which is a builder defect this check is the safety net for.
pub(in crate::runtime::interpreter::jit_bridge) static OSR_OPTIMIZING_KIND_REFUSALS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Optimizing OSR entries declined on a slot-kind disagreement since start.
pub fn osr_optimizing_kind_refusals() -> u64 {
    OSR_OPTIMIZING_KIND_REFUSALS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Count, and under `CRATONVM_DBG_JITC` name, an optimizing OSR entry declined
/// on a slot-kind disagreement. Out of line and cold: `try_osr`'s frame must
/// not carry the formatting temporaries.
#[inline(never)]
#[cold]
pub(in crate::runtime::interpreter::jit_bridge) fn dbg_osr_optimizing_kind_refusal(
    class_name: &str,
    method_name: &str,
    entry_pc: usize,
    local: usize,
) {
    OSR_OPTIMIZING_KIND_REFUSALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr optimizing {class_name}.{method_name} pc={entry_pc}: \
             not entered -- local {local}'s interpreter tag disagrees with the stub's kind"
        );
    }
}

/// `CRATONVM_DBG_JITC`: name a memo hit (the full trip prints `OSR-reuse`).
/// Out of line and cold, like `dbg_osr_recompile_reason`: `try_osr`'s frame
/// must not carry the formatting temporaries.
#[inline(never)]
#[cold]
pub(in crate::runtime::interpreter::jit_bridge) fn dbg_osr_reentry_memo_hit(
    c: &cratonvm_jit::CompiledMethod,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
    entry_pc: usize,
) {
    eprintln!(
        "[cratonvm-jitc] OSR-reuse(memo) {class_name}.{method_name}{method_descriptor} \
         entry_pc={entry_pc} entry={:p} len={}",
        c.entry_ptr(),
        c.code_bytes().len()
    );
}

/// Forget this thread's [`OsrReentryMemo`] entry -- the optimizing tier's door
/// answered instead, and a later hit must not route around it.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_reentry_memo_clear() {
    OSR_REENTRY_MEMO.with(|m| {
        if let Ok(mut slot) = m.try_borrow_mut() {
            *slot = None;
        }
    });
}

/// Set or clear the current entry's `optimizing_pending` (no-op without one).
pub(in crate::runtime::interpreter::jit_bridge) fn osr_reentry_memo_set_optimizing_pending(pending: bool) {
    OSR_REENTRY_MEMO.with(|m| {
        if let Ok(mut slot) = m.try_borrow_mut() {
            if let Some(entry) = slot.as_mut() {
                entry.optimizing_pending = pending;
            }
        }
    });
}

/// Count one pending hit of the current entry; its ordinal (1-based), or 0
/// without an entry.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_reentry_memo_note_pending_hit() -> u32 {
    OSR_REENTRY_MEMO.with(|m| match m.try_borrow_mut() {
        Ok(mut slot) => slot.as_mut().map_or(0, |entry| {
            entry.pending_hits = entry.pending_hits.saturating_add(1);
            entry.pending_hits
        }),
        Err(_) => 0,
    })
}

/// The current entry's `optimizing_pending`; `false` without one.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_reentry_memo_optimizing_pending() -> bool {
    OSR_REENTRY_MEMO.with(|m| match m.try_borrow() {
        Ok(guard) => matches!(&*guard, Some(entry) if entry.optimizing_pending),
        Err(_) => false,
    })
}

/// Must a re-entry memo hit yield to the optimizing body (round 11 wave 11,
/// lane tier, r11w8-tier-optimizing-osr-route-unreachable-behind-single-pass)?
///
/// Asked right after [`osr_reentry_memo_get`] hits, of that same entry. An
/// entry put while the worker still owed its optimizing verdict for this pc
/// (tier proposal 29's arm in `try_osr`) is not exact: once the worker caches
/// an accepted body here, the full trip enters it, and nothing the stamp
/// covers moves (the body is never published to `JitCache`). `true` then, so
/// the caller treats the hit as a miss. A verdict that came back a refusal
/// makes the entry exact again, and the flag is cleared so later hits skip
/// these probes. With neither yet, the single-pass hit stands.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_reentry_memo_defers_to_optimizing(
    shared: &SharedVm,
    class_id: ClassId,
    names: (&str, &str, &str),
    key: (u32, u64, usize),
    entry_pc: usize,
    code: &Arc<[u8]>,
) -> bool {
    if !osr_reentry_memo_optimizing_pending() {
        return false;
    }
    if osr_optimizing_cached(shared, class_id, names, key, entry_pc, code).is_some() {
        return true;
    }
    if osr_optimizing_already_refused(shared, key)
        || osr_optimizing_already_refused(shared, osr_optimizing_method_wide_key(key))
    {
        osr_reentry_memo_set_optimizing_pending(false);
        return false;
    }
    // j27 lane ONESHOT (`CRATONVM_JIT_OSR_REENTRY_PENDING_REASK`, opt-in): a
    // verdict that came back transient is never owed again; re-ask it.
    let _ =
        osr_reentry_pending_reask::reask_owed_optimizing_verdict(shared, class_id, names, entry_pc);
    false
}

/// A static OSR gate that refused, named for `osr_stage` and the refusal census.
pub(in crate::runtime::interpreter::jit_bridge) struct OsrGateRefusal {
    pub(in crate::runtime::interpreter::jit_bridge) stage: &'static str,
    pub(in crate::runtime::interpreter::jit_bridge) census: &'static str,
    /// The verdict is fixed for the method's current bytecode, so the caller
    /// may memoise it as an OSR denial (which expires with the install epoch).
    pub(in crate::runtime::interpreter::jit_bridge) permanent: bool,
}

/// The whole-method OSR gates, asked by BOTH OSR routes (round 11 wave 2,
/// r11-tier-osr-optimizing-route-bypasses-static-gates). They used to be
/// inline in [`compile_osr_artifact`] only, while the optimizing route went to
/// `compile_optimizing_artifact`, which carries no admission policy.
///
/// * `compiled_execution_forbidden` — `CRATONVM_DISABLE_JIT` and the bisect
///   levers (`CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY`).
/// * the GPU-offload admission gate — OSR-compiling a hot loop that holds an
///   offload-eligible `invokestatic` would silently end GPU dispatch there.
/// * a registered native that would run (S111r15; `registered_native_will_run`
///   rather than `find(..).is_some()`, see its doc).
///
/// `ACC_SYNCHRONIZED` is NOT a gate (interpreter round i1 wave 10, patch P1 of
/// `osr-optimizing-door-bypasses-osr-admission`, item 1). A compiled
/// body takes no method monitor, and an OSR body never needs one: it runs
/// inside the interpreter frame that already owns it (`monitor_on_exit`, set
/// before the frame was pushed), and returns, throws or transfers through
/// that frame, whose pop releases it. The refusal cost `StringBuffer` /
/// `Vector` / `Hashtable`-style loops their OSR. The OSR body is published
/// only in the OSR slot (`put_osr` / `get_osr`), which no raw-entry dispatch
/// door reads.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_static_gate_refusal(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    method_descriptor: &str,
) -> Option<OsrGateRefusal> {
    // Every gate here is a verdict nothing but a redefinition (or the install
    // epoch moving) can change: the levers are fixed for the process, the
    // native registration is a property of the loaded method, and
    // `offload_jit_gate::caller_blocks_jit` memoises its answer per
    // `(VM, class, method)` and never re-computes it. So each is `permanent`:
    // the frame's backoff (five offers per activation) and the background door
    // stop at `is_osr_denied` instead of re-running these gates and their
    // census `format!` on every offer
    // (`interpreter-L7-osr-refusals-reoffered-per-activation-FIXED`, fix 5).
    if cratonvm_jit::compile_gate::compiled_execution_forbidden(class_name, method_name) {
        return Some(OsrGateRefusal {
            stage: "gate:compiled-execution-forbidden",
            census: "compiled-execution-forbidden",
            permanent: true,
        });
    }
    if osr_gpu_offload_blocks(shared, class_id, method_name, method_descriptor) {
        return Some(OsrGateRefusal {
            stage: "gate:gpu-offload",
            census: "gpu-offload",
            permanent: true,
        });
    }
    // Interpreter round i6 wave 119, lane L2 (`CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`,
    // page `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`,
    // design item 2): a shadowed method marked re-entry-only runs its own
    // bytecode only in a native's bytecode-only re-entry, so a loop in that
    // frame may OSR like any other. An OSR body is entered only from the
    // frame that runs the loop, never bound by a caller.
    if registered_native_will_run(shared, class_name, method_name, method_descriptor)
        && !shared
            .jit
            .jit_cache
            .is_reentry_only_key(class_name, method_name, method_descriptor)
    {
        return Some(OsrGateRefusal {
            stage: "gate:registered-native",
            census: "registered-native",
            permanent: true,
        });
    }
    None
}

pub(in crate::runtime::interpreter) fn compile_osr_artifact(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: String,
    method_name: String,
    method_descriptor: String,
    // The bytecode to compile when no reusable body is cached: read inside
    // the compile's epoch witness by the caller, or the entering frame's,
    // adopted here (interpreter round i1 wave 43, lane L2; `CompileInputs`).
    source: OsrCodeSource<'_>,
    entry_pc: usize,
) -> Option<Arc<crate::jit::CompiledMethod>> {
    // x86-64 ONLY: this door calls `x64::compile_with_param_slots` directly,
    // and no other backend publishes OSR entry points. On any other target it
    // would publish x86-64 bytes. `cfg!` keeps the body type-checked there.
    if cfg!(not(target_arch = "x86_64")) {
        return None;
    }
    // Phase 5 WP3: no OSR body in a boot image's capture run.
    if compile_admission_closed_for_capture() {
        return None;
    }
    // Loader-aware, and asked of this VM's manager: OSR denials belong to a
    // class identity and expire when the install epoch moves.
    let osr_key = crate::jit::tiered::MethodKey::with_class_id(
        class_id,
        class_name.as_str(),
        method_name.as_str(),
        method_descriptor.as_str(),
    );
    osr_stage("entry");
    cratonvm_types::osr_refusal_census::note_attempt();
    if shared.jit.tiered_manager.is_osr_denied(&osr_key) {
        return None;
    }
    // The two whole-method vetoes that must also stop a CACHED artifact from
    // being reused, not merely stop a new compile:
    //
    //   * `CRATONVM_DISABLE_JIT=1` forces interpreter-only execution. OSR is a
    //     JIT entry point distinct from `try_jit_compile_callee` /
    //     `try_jit_upgrade_with_gate`, so the user-facing flag has to be asked
    //     here for it to disable all three.
    //   * the BISECT levers. `CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY`
    //     were applied only inside `cratonvm_jit::try_compile`, and this
    //     function reaches `x64::compile_with_param_slots` directly, so an OSR
    //     body could be force-interpreted by neither. A bisect step that cannot
    //     actually stop the compile reads as an exoneration — see
    //     `cratonvm_jit::jit_force_interpret`.
    //
    // Both now come from `compile_gate`, so this door and `try_compile` cannot
    // disagree about them. The rest of the admission chain (the permanent
    // bail-list, the code-cache cap, the compile-epoch witness) is asked at the
    // compile itself, further down — refusing to *reuse* a body that is already
    // committed on either of those grounds would cost throughput and buy
    // nothing.
    // Each early gate names itself before returning. Without this they all
    // report `stage=entry` and a real refusal looks like a method that was
    // never considered. See `osr_refusal_census`. The gates themselves live in
    // `osr_static_gate_refusal`, which the optimizing route asks too.
    if let Some(gate) = osr_static_gate_refusal(
        shared,
        class_id,
        &class_name,
        &method_name,
        &method_descriptor,
    ) {
        osr_stage(gate.stage);
        cratonvm_types::osr_refusal_census::note_refusal(
            gate.census,
            &format!("{class_name}.{method_name}"),
        );
        // A verdict the current bytecode cannot change is memoised, so the
        // next back edge stops at `is_osr_denied` above instead of re-taking
        // the class-manager lock for the same linear method scan.
        if gate.permanent {
            shared.jit.tiered_manager.mark_osr_denied(osr_key);
        }
        return None;
    }

    osr_stage("past-early-gates");
    // Check if already compiled
    let class_name_arc: Arc<str> = Arc::from(class_name.as_str());
    let method_name_arc: Arc<str> = Arc::from(method_name.as_str());
    let descriptor_arc: Arc<str> = Arc::from(method_descriptor.as_str());

    // RBC.2 — reuse a cached artifact when it can OSR-enter at this pc.
    // The historical "always recompile in OSR" policy (kept because an
    // early-compile artifact may lack direct-call wiring for callees
    // compiled later) re-ran the FULL x64 pipeline on every OSR trigger of
    // the same method: BC's `SecP521R1Curve$1.lookup` under DualECDRBG was
    // recompiled 2,610× in one crypto-prng suite run (its interface call
    // sites never promote it to method-entry JIT, so every call re-trips
    // the back-edge threshold). A cached artifact exposing an OSR entry for
    // this pc is at worst missing newer direct-call wiring — a throughput
    // nuance, not correctness — so prefer it. Artifacts without an entry
    // here (or no cached artifact) recompile exactly as before.
    let cached_osr = {
        let jit_cache = shared.jit.jit_cache.read();
        jit_cache.get_osr(&class_name_arc, &method_name_arc, &descriptor_arc, class_id)
    };
    // Only reuse artifacts the OSR path itself produced: those carry the
    // eager invokestatic callee wiring (direct calls). A first-call/upgrade
    // artifact can OSR-enter too, but pinning it into a hot loop forever
    // routes its callees through the slow dispatch helper — reusing those
    // regressed the DEFAULT (ban-on) BC suites ~2×. Such artifacts get one
    // fresh OSR recompile below (replacing them in the cache), after which
    // reuse kicks in.
    //
    // Round 9 wave 10 (`call10`): NOT a cached body that still carries a
    // speculative guard this VM has since de-speculated
    // ([`cached_osr_body_is_despec_stale`]). Reusing one re-entered the
    // failing guard on every OSR trigger -- `TryLoop.sumThrow`'s loop-header
    // BCE guard, an immediate OSR exit each time, until the aggregate count
    // made the method not-compilable and every later call interpreted its
    // 65 541-iteration loop (8 s against HotSpot's 16 ms).
    let despec_stale = matches!(&cached_osr, Some(c) if c.compiled_via_osr
    && c.can_osr_enter(entry_pc)
    && cached_osr_body_is_despec_stale(
        shared,
        c,
        &class_name_arc,
        &method_name_arc,
        &descriptor_arc,
    ));
    // JIT round 23 wave 1, lane L1 (`CRATONVM_JIT_OSR_PROFIT_EXIT`): nor a
    // body armed for a profit exit. Its polls are forced, so a frame entered
    // into it would be told to leave at its first back edge, re-offered here
    // and entered again. Reached only when the optimizing body this door
    // prefers could not be entered at this pc (a slot-kind refusal, a memo
    // that forgot it); the fresh body published below is not armed.
    let profit_armed = matches!(&cached_osr, Some(c) if c.osr_profit_exit_armed());
    let osr_reused = !despec_stale
        && !profit_armed
        && matches!(&cached_osr, Some(c) if c.compiled_via_osr && c.can_osr_enter(entry_pc));
    // EAGER19-4 (JIT round 19 proposal wave 7, lane interp7;
    // `CRATONVM_JIT_LATE_BIND_OSR`): a late bind asked this method's cached
    // OSR body replaced, because a callee one of its sites left on the
    // dispatch helper is compiled now (`late_bind_osr_key_served`). Only the
    // BACKGROUND compile (`OsrCodeSource::Read`, the task that request
    // queued) takes the mark; the mutator's reuse door keeps entering the
    // cached body meanwhile, as before. The compile below then publishes over
    // it (`put_osr`), exactly as the de-spec-stale recompile above does.
    // The body the mark replaces, kept to arm it for a profit exit once the
    // fresh one is published below: a frame already inside it never returns
    // to this door by itself.
    let mut late_replaced: Option<Arc<cratonvm_jit::CompiledMethod>> = None;
    let osr_reused = if osr_reused
        && matches!(source, OsrCodeSource::Read(_))
        && take_late_bind_osr_stale(shared, class_id, &method_name_arc, &descriptor_arc)
    {
        late_replaced = cached_osr.clone();
        false
    } else {
        osr_reused
    };
    // DBG: name WHY a cached artifact was not reused — see
    // `dbg_osr_recompile_reason`. MUST stay an `#[inline(never)]` call: this
    // function runs on the mutator's stack and is already ~1100 lines, and
    // `main-vm`'s remaining headroom here is thin enough that inlining even a
    // cold `eprintln!`'s formatting temporaries into this frame overflowed the
    // stack outright (`TestDefaultServlet`/`TestStandardWrapper`/`TestTomcat`
    // all died with "thread 'main-vm' has overflowed its stack"; the same
    // binaries pass with the diagnostic out of line). The branch never
    // executes without the env var, but the frame reservation is unconditional.
    if !osr_reused {
        dbg_osr_recompile_reason(
            cached_osr.as_deref(),
            despec_stale,
            &class_name,
            &method_name,
            &method_descriptor,
            entry_pc,
        );
    }
    let compiled = if osr_reused {
        cached_osr
    } else {
        // The frame's bytecode is adopted only now, when a compile is needed
        // (a reused body above takes no guard).
        let adopted;
        let inputs = match source {
            OsrCodeSource::Read(inputs) => inputs,
            OsrCodeSource::Frame { code, max_locals } => {
                let Some(frame_inputs) = CompileInputs::adopt_frame_code(
                    shared,
                    class_id,
                    &method_name,
                    &method_descriptor,
                    code,
                    max_locals,
                ) else {
                    osr_stage("bytecode-not-current");
                    return None;
                };
                adopted = frame_inputs;
                &adopted
            }
        };
        // Stage: compile the OSR body. See `compile_osr_body`.
        compile_osr_body(
            shared,
            class_id,
            &class_name,
            &method_name,
            &method_descriptor,
            inputs,
            entry_pc,
            &class_name_arc,
            &method_name_arc,
            &descriptor_arc,
            method_name.as_str(),
            &osr_key,
        )
    };

    let compiled = match compiled {
        Some(c) => c,
        None => return None,
    };
    osr_stage("published");

    // The compile succeeded but the body may still refuse to enter at the PC it
    // was compiled for: no published native offset (the codegen writes -1 for a
    // pc strictly inside a LICM-hoisted loop body, whose preheader an OSR entry
    // would skip, and RBC.6 withdraws an `athrow` method's entries inside a
    // protected range — `withdraw_osr_entries_inside_protected_ranges`), or —
    // only under `CRATONVM_JIT_OSR_DEAD_LOCALS=0` — a non-zero
    // `osr_dead_mask[entry_pc]`. Memo that so the next trip over this
    // back-edge does not re-run the whole pipeline to the same conclusion; the
    // artifact stays cached and other PCs are unaffected.
    if !osr_reused && !compiled.can_osr_enter(entry_pc) {
        cratonvm_jit::metrics::record_osr_event("osr_refused_entry");
        jit_verdicts(shared).mark_osr_entry_rejected(
            class_id,
            &class_name,
            &method_name,
            &method_descriptor,
            entry_pc,
        );
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] OSR-reject {}.{}{} entry_pc={} (no enterable native offset\
                 : LICM-hoisted body or RBC.6 athrow entry inside a protected range\
                 ; or dead_mask non-zero with CRATONVM_JIT_OSR_DEAD_LOCALS=0; memoed)",
                &*class_name_arc, &*method_name_arc, &*descriptor_arc, entry_pc
            );
        }
        return None;
    }

    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] OSR-{} {}.{}{} entry_pc={} entry={:p} len={}",
            if osr_reused { "reuse" } else { "compile" },
            &*class_name_arc,
            &*method_name_arc,
            &*descriptor_arc,
            entry_pc,
            compiled.entry_ptr(),
            compiled.code_bytes().len()
        );
    }
    // EAGER19-4 follow-up: the single-pass body a late bind replaced may have
    // a frame running it (a `main`-shaped activation); arm it to leave at its
    // next back-edge poll, where the interpreter enters the fresh body.
    if let Some(old) = late_replaced {
        if !Arc::ptr_eq(&old, &compiled) && late_bind_osr_enabled() {
            let armed = shared.jit.jit_cache.arm_osr_profit_exit(&old);
            if crate::runtime::env_cache::dbg_jitc() {
                match armed {
                    Ok(sites) => eprintln!(
                        "[cratonvm-jitc] late-bind OSR exit ARMED (single-pass) {}.{}{} id={} polls-forced={sites}",
                        &*class_name_arc, &*method_name_arc, &*descriptor_arc, old.compile_id
                    ),
                    Err(why) => eprintln!(
                        "[cratonvm-jitc] late-bind OSR exit (single-pass) {}.{}{} NOT armed: {why}",
                        &*class_name_arc, &*method_name_arc, &*descriptor_arc
                    ),
                }
            }
        }
    }
    if !osr_reused {
        crate::jit::disasm::maybe_dump(
            // The DOOR and the BACKEND are different questions and only the
            // first was ever printed. `used_ir_backend` has recorded the
            // second all along; a reader chasing a miscompiled body needs both
            // to know which emitter to go and read.
            if compiled.used_ir_backend {
                "osr/ir"
            } else {
                "osr/sp"
            },
            &class_name_arc,
            &method_name_arc,
            &descriptor_arc,
            compiled.entry_ptr(),
            compiled.code_bytes(),
        );
    }
    Some(compiled)
}

/// Deny OSR for a method whose `ldc` at `pc` loads a constant kind the OSR
/// body cannot materialise (a wide or unrepresentable string, a
/// `MethodHandle`, a `MethodType`, a dynamic constant).
///
/// That is a function of the constant pool, so the refusal is remembered
/// (`mark_osr_denied`, until the install epoch moves) like the
/// `CRATONVM_JIT_OSR_ATHROW=0` deny: a plain refusal was re-offered up to five
/// times per activation by the frame's backoff, and each attempt re-ran
/// admission, the scan and every resolution before this one. Method-entry
/// compilation is unaffected.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn deny_osr_for_constant_kind(
    shared: &SharedVm,
    osr_key: &crate::jit::tiered::MethodKey,
    pc: usize,
) {
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr-DENY (unsupported ldc constant kind at pc={pc}) {}.{}{}",
            osr_key.class_name, osr_key.method_name, osr_key.descriptor
        );
    }
    shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
}

/// The GPU-offload JIT admission gate, as both OSR tiers ask it (through
/// [`osr_static_gate_refusal`]): does compiling `class_id.method_name` end GPU
/// dispatch at an offload-eligible call site in it? Always `false` without the
/// `gpu-offload` feature, and one `bool` read when offload is off
/// (`caller_blocks_jit_by_name`).
///
/// The optimizing arm of `try_osr` runs BEFORE `compile_osr_artifact`, so with
/// that arm live (`CRATONVM_BG_COMPILE=0`) an offload caller's hot loop got an
/// optimizing OSR body unless the arm asks this gate too -- the exact case the
/// gate exists for.
#[cfg(feature = "gpu-offload")]
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_gpu_offload_blocks(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
) -> bool {
    crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
        shared,
        class_id,
        method_name,
        method_descriptor,
    )
}

/// Without the `gpu-offload` feature there is no gate to ask.
#[cfg(not(feature = "gpu-offload"))]
#[inline(always)]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_gpu_offload_blocks(
    _shared: &SharedVm,
    _class_id: ClassId,
    _method_name: &str,
    _method_descriptor: &str,
) -> bool {
    false
}

/// Deny OSR for a method whose field site at `pc` resolved to a field the
/// compiled site cannot serve: the wrong kind (`getfield` of a static field
/// or the reverse), a `putfield`/`putstatic` of a `final` field outside its
/// initializer, or a slot whose declared type disagrees with the constant
/// pool's descriptor (`jit_field_tag_agrees`).
///
/// All three are functions of the bytecode and the RESOLVED field -- a field
/// that failed to resolve returns earlier, undenied -- so, like
/// [`deny_osr_for_constant_kind`], the verdict is remembered until the install
/// epoch moves instead of being re-derived by every offer after admission, the
/// scan and every resolution before this site
/// (`interpreter-L7-osr-refusals-reoffered-per-activation-FIXED`, the
/// field-tag item). The
/// interpreter serves the site (and throws the linkage error where one is
/// due); method-entry compilation is unaffected.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn deny_osr_for_field_site(
    shared: &SharedVm,
    osr_key: &crate::jit::tiered::MethodKey,
    pc: usize,
    why: &str,
) {
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] osr-DENY (field site {why} at pc={pc}) {}.{}{}",
            osr_key.class_name, osr_key.method_name, osr_key.descriptor
        );
    }
    shared.jit.tiered_manager.mark_osr_denied(osr_key.clone());
}

// RBC.6's residual refusal, applied to a finished single-pass OSR artifact:
// withdraw (`-1`) every OSR entry at a bytecode pc that some row of
// `exception_table` covers (`start_pc <= pc < end_pc`). Returns how many
// enterable entries were withdrawn.
//
// Only for a method that has an `athrow` and a non-empty table — see the
// RBC.6 gate in `compile_osr_body` for why this is belt and braces rather
// than a known hazard. Entries outside every range are untouched, which is
// what admits the javac `synchronized`-block loop (its back-edge lies outside
// both of the block's ranges).
//
// `osr_pc_to_native` is indexed by interpreter bci — the same space as the
// classfile table, even under the bytecode loop rewriter
// (`osr_coords::BciIndexed`) — and a row past the table's end is clamped,
// never indexed.
// Kept for RBC.6's own tests (`r11w8_rbc6_narrowing_tests`); NOT `#[cfg(test)]`,
// which would cut the production source the text pins read at this line.
#[cfg_attr(not(test), allow(dead_code))]
pub(in crate::runtime::interpreter::jit_bridge) fn withdraw_osr_entries_inside_protected_ranges(
    osr_pc_to_native: &mut [i32],
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> usize {
    withdraw_osr_entries_inside_protected_ranges_except(osr_pc_to_native, exception_table, |_| {
        false
    })
}

/// [`withdraw_osr_entries_inside_protected_ranges`], skipping every row whose
/// index `keep_row` answers `true` for (MON10-1: javac's monitor-release
/// catch-any rows, [`is_javac_monitor_release_row`]). An entry another row
/// covers is still withdrawn.
///
/// Why a header inside a `synchronized` block can be entered (lane mon11,
/// wave 11, `CRATONVM_JIT_OSR_SYNC_BLOCK_ENTRY`, opt-in). The block's lock is
/// then held by the INTERPRETER frame at the entry, and the body's own
/// `monitorexit` releases it, which the machinery already describes:
///
/// * the entry contract lists the lock as taken (`relock == false`) and
///   `validate_osr_entry` refuses only an elided one (`jit/src/osr_entry.rs`);
/// * `jit_monitor_exit` releases by thread ownership and retracts the JMX
///   publish (`monitor_exit_and_retract_jmx`), so a lock the interpreter took
///   is released exactly once, inline or through the helper;
/// * a return or a throw out of the body empties the frame's `held_monitors`
///   record (`try_osr_offer`), an OSR exit or handler entry rewrites it from
///   the exit frame (`CompiledLocksOfAStash`), and an interpreted
///   `monitorexit` that misses the record is the unrecorded acquisition
///   `held_monitors` releases;
/// * the throw routing is per THROW bci (RBC.6b, `route_osr_exception_out_of_artifact`),
///   never the entry pc: the unwind pc is `OSR_FRAME_DECLINED_TO_CATCH`.
///
/// What stays withdrawn: an entry any OTHER row covers (`try { for .. }`).
pub(in crate::runtime::interpreter::jit_bridge) fn withdraw_osr_entries_inside_protected_ranges_except(
    osr_pc_to_native: &mut [i32],
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    keep_row: impl Fn(usize) -> bool,
) -> usize {
    let mut withdrawn = 0usize;
    for (index, row) in exception_table.iter().enumerate() {
        if keep_row(index) {
            continue;
        }
        // Widening: classfile pcs are u16.
        let start = row.start_pc as usize;
        let end = row.end_pc as usize;
        for off in osr_pc_to_native.iter_mut().take(end).skip(start) {
            if *off >= 0 {
                *off = -1;
                withdrawn += 1;
            }
        }
    }
    withdrawn
}

/// MON10-1 (lane mon11, wave 11): is `row` one of the two catch-any rows javac
/// emits for `synchronized (x) { .. }`? Judged on the classfile bytecode
/// `code`:
///
/// * `catch_type == 0`;
/// * the handler is `astore t; aload l; monitorexit; aload t; athrow`
///   (javac and ecj; the one- and two-byte `astore` / `aload` forms);
/// * the row is the block's body row -- it starts right after
///   `astore l; monitorenter` storing the SAME local `l` the handler releases
///   -- or the handler's own row (`start_pc == handler_pc`).
///
/// Anything else (a `wide` local, a hand-written table, Kotlin's shapes)
/// answers `false`, which keeps RBC.6's withdrawal for that row.
pub(in crate::runtime::interpreter::jit_bridge) fn is_javac_monitor_release_row(
    code: &[u8],
    row: &cratonvm_reader::attribute::ExceptionTableEntry,
) -> bool {
    /// `(local, length)` of a `astore` (`store == true`) or `aload` at `pc`.
    fn local_op(code: &[u8], pc: usize, store: bool) -> Option<(u16, usize)> {
        let (wide_op, short_base) = if store { (0x3a, 0x4b) } else { (0x19, 0x2a) };
        let op = *code.get(pc)?;
        if op == wide_op {
            return Some((u16::from(*code.get(pc + 1)?), 2));
        }
        // `astore_0..3` / `aload_0..3`.
        (short_base..short_base + 4)
            .contains(&op)
            .then(|| (u16::from(op - short_base), 1))
    }
    const MONITORENTER: u8 = 0xc2;
    const MONITOREXIT: u8 = 0xc3;
    const ATHROW: u8 = 0xbf;
    if row.catch_type != 0 {
        return false;
    }
    let handler = row.handler_pc as usize;
    let Some((thrown, n1)) = local_op(code, handler, true) else {
        return false;
    };
    let Some((lock, n2)) = local_op(code, handler + n1, false) else {
        return false;
    };
    let at = handler + n1 + n2;
    if code.get(at) != Some(&MONITOREXIT) {
        return false;
    }
    let Some((rethrown, n3)) = local_op(code, at + 1, false) else {
        return false;
    };
    if rethrown != thrown || code.get(at + 1 + n3) != Some(&ATHROW) {
        return false;
    }
    let start = row.start_pc as usize;
    if start == handler {
        return true;
    }
    // The body row: `.. astore l; monitorenter` ends right at `start_pc`.
    if start == 0 || code.get(start - 1) != Some(&MONITORENTER) {
        return false;
    }
    let stored_by = |len: usize| {
        start
            .checked_sub(1 + len)
            .and_then(|pc| local_op(code, pc, true))
            .is_some_and(|(local, l)| l == len && local == lock)
    };
    stored_by(1) || stored_by(2)
}

/// Record one `JitInvokeInfo` row of an OSR body for the call site at `pc`.
///
/// The info borrows its three names as `&'static str`, so they are boxed into
/// `owned_strings` first: a `Box<str>`'s buffer does not move when the `Vec`
/// holding it grows, and `compile_osr_body`'s owned vectors outlive the info
/// and the code compiled against it. The info itself is boxed into
/// `owned_infos` for the same reason, and its address recorded for `pc`.
/// `compile_osr_body` spelled this out by hand at seven sites
/// (`i1-L7-proposal-jit-bridge-decomposition`, stage 4).
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn push_osr_invoke_info(
    _owned_strings: &mut Vec<Box<str>>,
    owned_infos: &mut Vec<Box<crate::jit::JitInvokeInfo>>,
    invoke_info: &mut Vec<(usize, *const crate::jit::JitInvokeInfo)>,
    pc: usize,
    (class_box, method_box, desc_box): (Box<str>, Box<str>, Box<str>),
    num_jit_args: usize,
    return_type: u8,
    invoke_kind: u8,
    declaring_class_id: u32,
) {
    let info = Box::new(crate::jit::JitInvokeInfo {
        class_name: cratonvm_jit::intern_jit_invoke_str(&class_box),
        method_name: cratonvm_jit::intern_jit_invoke_str(&method_box),
        descriptor: cratonvm_jit::intern_jit_invoke_str(&desc_box),
        num_jit_args,
        return_type,
        invoke_kind,
        declaring_class_id,
        owner_class_id: 0,
    });
    let info_ptr: *const crate::jit::JitInvokeInfo = &*info;
    owned_infos.push(info);
    invoke_info.push((pc, info_ptr));
}





/// `CRATONVM_JIT_OSR_CALLEE_C1_FIRST` (W4C, **default ON**, `=0` off): the
/// single-pass OSR door compiles each direct-call callee IN PLACE, on the
/// mutator (the body is built once and keeps what it binds, so the callee door
/// may not defer it, [`CalleeDoorMiss::CompileInPlace`]). It asked for the
/// optimizing tier, so the app thread blocked for a full C2-class compile
/// (tens to ~100 ms, run through `ir_tier`) per cold callee of every hot loop,
/// while the background workers sat idle. Under the background compiler the
/// callee is now compiled single-pass (a few ms) and reported to the tiered
/// manager as C1 ([`note_osr_c1_callee_publish`]), which upgrades it on the
/// worker once hot; the C2 supersede forwards the baked CALL through the
/// not-entrant stub, exactly as for the ordinary eager chain's thin-profile
/// callees ([`eager_callee_compile_optimize`]). Without the background
/// compiler nothing would upgrade a C1 body, so the optimizing tier stays.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_callee_compile_optimize() -> bool {
    osr_callee_optimize_for(
        crate::runtime::env_cache::bg_compile(),
        true,
    )
}

/// [`osr_callee_compile_optimize`]'s rule.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_callee_optimize_for(bg_compile: bool, c1_first: bool) -> bool {
    !(bg_compile && c1_first)
}

/// Tell the tiered manager about a single-pass callee body an OSR compile just
/// bound, so its C1 -> C2 upgrade is scheduled (nothing else would: a callee
/// reached only from compiled code is never counted by the interpreter). A
/// no-op for an optimizing body, and when the callee class is not resolvable
/// through `holder`'s loader.
pub(in crate::runtime::interpreter::jit_bridge) fn note_osr_c1_callee_publish(
    shared: &SharedVm,
    holder: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
    found: &(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool),
) {
    if found.0.used_ir_backend {
        return;
    }
    let callee_id = shared
        .classes
        .class_manager
        .read()
        .find_class_by_name_for_class(callee_class, holder);
    if let Some(id) = callee_id {
        note_callee_publish_to_tiered_manager(
            shared,
            id,
            callee_class,
            callee_method,
            callee_desc,
            &found.0,
        );
    }
}

/// The direct bind a call inside an OSR-door splice gets: the callee compiled
/// through `holder`'s loader (the class whose constant pool names it), refused
/// on exactly the gates the door's own bind loop applies to a top-level site
/// (another loader's copy, an indy-trap body, a recursion cycle that must
/// dispatch, a callee the OSR door may not CALL directly). The `Arc` pins the
/// body until the caller publishes.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_splice_direct_bind(
    shared: &SharedVm,
    holder: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    if crate::jit::helpers::jit_callee_name_is_another_loaders_copy(shared, holder, callee_class) {
        return None;
    }
    // In place, never handed to the worker (wave 93, lane L7): this OSR body
    // is built once.
    let found = try_jit_compile_callee_from_site_in_place(
        shared,
        holder.as_u32(),
        callee_class,
        callee_method,
        callee_desc,
        osr_callee_compile_optimize(),
    )?;
    note_osr_c1_callee_publish(shared, holder, callee_class, callee_method, callee_desc, &found);
    let (pin, entry, needs_ctx) = found;
    if pin.has_indy_trap {
        return None;
    }
    let requires_dispatch = jit_verdicts(shared).direct_call_requires_dispatch_of(
        callee_class,
        callee_method,
        callee_desc,
        || {
            let cm = shared.classes.class_manager.read();
            jit_known_class(shared, &cm, callee_class, holder)
        },
    );
    if requires_dispatch
        || osr_callee_bars_direct_call(shared, holder, callee_class, callee_method, callee_desc)
            .is_some()
    {
        return None;
    }
    Some((pin, entry, needs_ctx))
}

/// `CRATONVM_JIT_OSR_REAL_NEW_SITE_FLAGS`, **default ON** (round 15 wave 5,
/// lane osrtier; `=0` bakes the old `(true, true)` row): does the single-pass
/// OSR door ([`compile_osr_body`]) bake a resolved `new` site's real
/// `(needs_post_alloc_init, has_finalizer)` ([`jit_new_site_flags`]) as the
/// method-entry door does? With the literal row no OSR loop's `new` could take
/// the inline TLAB bump: `R15GcpageYoungPremium.allocDead`'s single-pass OSR
/// body paid a full `jit_new_object` call, plus a shadow push and pop, per
/// iteration (`r15w5-osrtier-accepted-optimizing-osr-body-never-entered`).
/// Not memoised (`per_vm_state_statics_ratchet`): asked once per resolved
/// `new` site of an OSR compile. `pub(in crate::runtime::interpreter)` for the interpreter's first-call
/// door (`r15w5-osrtier-first-call-door-new-site-flags-patch-FIXED-20260930.md`).
pub(in crate::runtime::interpreter) fn osr_real_new_site_flags_enabled() -> bool {
    true
}

