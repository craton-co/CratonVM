#[allow(unused_imports)]
use super::*;

/// The callee pcs of `candidates` (`(callee pc, the callee's declaring
/// class)`) that become never-run exits: a statically bound row (`invoke_kind`
/// 3) with no `direct_entry`, not spliced in turn (`nested`), not already an
/// unloaded exit, whose callee `has_run` denies. Pure, for the tests.
pub(in crate::runtime::interpreter::jit_bridge) fn never_run_static_exit_pcs(
    invoke_targets: &[(usize, cratonvm_jit::InlineInvokeTarget)],
    nested: &[cratonvm_jit::NestedInlineSite],
    already: &[usize],
    candidates: &[(usize, ClassId)],
    has_run: impl Fn(ClassId, &str, &str) -> bool,
) -> Vec<usize> {
    let mut out = Vec::new();
    for &(pc, decl) in candidates {
        if already.contains(&pc) || out.contains(&pc) || nested.iter().any(|n| n.callee_pc == pc) {
            continue;
        }
        let Some((_, t)) = invoke_targets.iter().find(|(p, _)| *p == pc) else {
            continue;
        };
        if t.invoke_kind == 3
            && t.direct_entry.is_none()
            && !has_run(decl, &t.method_name, &t.descriptor)
        {
            out.push(pc);
        }
    }
    out
}

/// `CRATONVM_JIT_IR_SPLICE_NEVER_RUN_PRIVATE_EXIT=1` (JIT round 21 wave 1,
/// lane updaters, UPDATERS21-3; opt-in, default OFF): in an optimizing-tier
/// splice, a PRIVATE instance call the binds left without an entry only
/// because its callee has never run becomes the precise exit
/// [`splice_never_run_static_exit_enabled`] gives a static one (the builder's
/// invoke arms plant an always-failing guard resumed through the chain
/// snapshot at that call; a trap that fires runs the callee in the
/// interpreter, which counts it, and the recompile binds it). Before, the row
/// refused its whole body (`ir-splice-unbindable-surviving-call`):
/// `Atomic*FieldUpdaterImpl.accessCheck` -> `throwAccessCheckException`. Read
/// per resolution.
pub(in crate::runtime::interpreter) fn splice_never_run_private_exit_enabled() -> bool {
    // The builder's half reads the same switch (one reader, two crates).
    cratonvm_jit::ir::ir_splice_never_run_private_exit_enabled()
}

/// The callee pcs that become never-run PRIVATE exits
/// ([`splice_never_run_private_exit_enabled`]): a statically bound row
/// (`invoke_kind` 1) with no `direct_entry`, not spliced in turn, not already
/// an exit, not a constructor, that is EITHER a private `invokevirtual` /
/// `invokeinterface` the resolver pinned to its declaring class
/// (`owner_class_id != 0`, the callee's class) OR an `invokespecial`
/// (`special_pcs`) naming this body's own class (`body_class_name`: a private
/// method of it), and whose callee `has_run` denies. Pure, for the tests.
pub(in crate::runtime::interpreter::jit_bridge) fn never_run_private_exit_pcs(
    invoke_targets: &[(usize, cratonvm_jit::InlineInvokeTarget)],
    nested: &[cratonvm_jit::NestedInlineSite],
    already: &[usize],
    special_pcs: &[usize],
    body_class_name: Option<&str>,
    has_run: impl Fn(ClassId, &str, &str) -> bool,
) -> Vec<usize> {
    let mut out = Vec::new();
    for (pc, t) in invoke_targets {
        if already.contains(pc)
            || out.contains(pc)
            || nested.iter().any(|n| n.callee_pc == *pc)
            || t.invoke_kind != 1
            || t.direct_entry.is_some()
            || t.method_name == "<init>"
        {
            continue;
        }
        let decl = if t.owner_class_id != 0 {
            ClassId::new(t.owner_class_id)
        } else if special_pcs.contains(pc) && body_class_name == Some(t.class_name.as_str()) {
            ClassId::new(t.declaring_class_id)
        } else {
            continue;
        };
        if !has_run(decl, &t.method_name, &t.descriptor) {
            out.push(*pc);
        }
    }
    out
}

/// `CRATONVM_JIT_LATE_BIND_UNINIT_CTOR` (JIT round 18 wave 9, lane eagercold;
/// **default OFF**, perf, opt-in until measured): a constructor splice that
/// [`resolve_inline_site_from`] refuses ONLY because the constructor's class is
/// not initialized yet (not for a finalizer) registers the method being
/// compiled as a late-bind waiter on that constructor
/// ([`note_uninit_ctor_late_bind_waiter`]). The first time a compiled body's
/// dispatch helper serves the constructor from a compiled body
/// ([`late_bind_site_served`]) -- by then the class is initialized, because
/// the site's own `new` ran first -- the method gets the one late-bind
/// recompile, which splices the constructor, and the displaced body is made
/// not entrant so baked callers forward. Needs `CRATONVM_JIT_LATE_BIND_RECOMPILE`
/// (default ON). HotSpot's C2 traps at such a `new` and recompiles. Read on the
/// refusal only (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn late_bind_uninit_ctor_enabled() -> bool {
    false
}

/// The constructor splice of `ctor_class.<init>ctor_desc` was refused because
/// its class is not initialized yet: remember the method on this thread's
/// compile stack as waiting on it ([`late_bind_uninit_ctor_enabled`]). Only
/// when the compile stack's top is a method of `requesting_class_id` (the class
/// whose code holds the site at splice depth 0, or a spliced body's class
/// deeper), so a door that compiles without the stack guard never names the
/// wrong method. Bounded by the late-bind map's own caps; idempotent.
pub(in crate::runtime::interpreter::jit_bridge) fn note_uninit_ctor_late_bind_waiter(
    shared: &SharedVm,
    requesting_class_id: ClassId,
    nest_depth: usize,
    ctor_class: &str,
    ctor_desc: &str,
) {
    if !late_bind_uninit_ctor_enabled() || !late_bind_recompile_enabled(shared) {
        return;
    }
    let Some((class_id, class_name, method_name, descriptor)) =
        cratonvm_jit::jit_active_compile_top()
    else {
        return;
    };
    if class_id.as_u32() == 0 || (nest_depth == 0 && class_id != requesting_class_id) {
        return;
    }
    let waiting = LateBindWaiter {
        class_id: class_id.as_u32(),
        class_name: Arc::from(class_name.as_str()),
        method_name: Arc::from(method_name.as_str()),
        descriptor: Arc::from(descriptor.as_str()),
    };
    note_late_bind_waiter_for(
        shared,
        cratonvm_jit::DirectBindRefusal::DeclaringClassNotInitialized,
        ctor_class,
        "<init>",
        ctor_desc,
        waiting,
    );
}



/// `CRATONVM_JIT_OPTIMIZING_DOOR_OWN_VERDICT` (default ON; round 15 wave 2,
/// lane calls2, CR15-4 of `jit-r15-calls-proposals-RETIRED-20261003.md`): the acceptance
/// verdict travels as a value on this door. [`compile_optimizing_artifact`]
/// takes its own compile's verdict off the thread-local slot and returns it in
/// [`PinnedArtifact::verdict`], and its in-place callee compile hands the
/// enclosing compile's pending verdict back instead of replacing it with (and
/// leaving behind) its own. `0` restores the slot as the only channel.
pub(in crate::runtime::interpreter::jit_bridge) fn optimizing_door_own_verdict_enabled() -> bool {
    true
}

/// A compiled but UNPUBLISHED artifact, with the owners of every callee entry
/// its code CALLs directly.
///
/// A raw entry address baked into machine code is only as meaningful as the
/// artifact that owns it. `JitCache::prepare_for_publication` roots the callees
/// of a published body for that body's life; until then — and for an artifact
/// that is never published through `JitCache`, such as an accepted optimizing
/// OSR body — `callee_pins` is what keeps them mapped. Drop it only after the
/// `put`, or together with the last reference to the artifact.
pub(in crate::runtime::interpreter) struct PinnedArtifact {
    pub(in crate::runtime::interpreter) compiled: cratonvm_jit::CompiledMethod,
    pub(in crate::runtime::interpreter) callee_pins: Vec<cratonvm_jit::RetainedCode>,
    /// This compile's own acceptance verdict (`Some(true)` accepted,
    /// `Some(false)` refused, `None` never judged), taken off the thread-local
    /// slot right after the compile; always `None` with
    /// `CRATONVM_JIT_OPTIMIZING_DOOR_OWN_VERDICT=0`, where the slot is left for
    /// the caller as before (`optimizing_door_own_verdict_enabled`).
    pub(in crate::runtime::interpreter) verdict: Option<bool>,
}

/// WP2.4-F1: variant of [`try_jit_upgrade`] that takes an explicit
/// [`RedefineGate`] so the JIT entry inherits the same staleness binding
/// as the bytecode entry it's replacing.  Saves one
/// `class_manager.read()` round-trip on the hot promotion path.
pub(in crate::runtime::interpreter) fn try_jit_upgrade_with_gate(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    gate: RedefineGate,
) -> Option<CachedInvokeTarget> {
    // Interpreter round i4 wave 91, lane L4: the finder census's JIT door
    // time (`CRATONVM_DBG=mh-finder-census`; one flag read otherwise).
    let _census_door = cratonvm_native_builtins::mh_finder_census::work_clock(
        cratonvm_native_builtins::mh_finder_census::Work::JitDoor,
    );
    // Panic containment for the inline upgrade door; see `try_jit_compile_callee`.
    match cratonvm_jit::tiered::contain_compile_panic(|| {
        try_jit_upgrade_with_gate_uncontained(shared, cached, gate)
    }) {
        Ok(target) => target,
        Err(payload) => {
            note_contained_mutator_compile_panic(
                shared,
                cached.declaring_class_id,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
                &*payload,
            );
            None
        }
    }
}

/// Is `cached` the body its declaring class holds now: the same bytecode,
/// exception table and frame sizes as the class's method of that name and
/// descriptor? Asked by the upgrade door for a class redefined at least once
/// (interpreter round i1 wave 38, lane L2), where an invoke-cache or
/// lambda-bridge entry may still hold a body a redefinition replaced. Equal
/// code bytes compile to the same body whichever copy is handed over, because
/// every constant-pool read goes to the class's current pool. The cached copy
/// is padded by two bytes (`frame::padded_bytecode`). Takes the class-manager
/// read lock.
pub(in crate::runtime::interpreter::jit_bridge) fn cached_is_current_body(shared: &SharedVm, cached: &CachedBytecodeMethod) -> bool {
    let cm = shared.classes.class_manager.read();
    cached_matches_class_body(&cm, cached)
}

/// Is `code` (unpadded) still the bytecode of `class_id`'s method
/// `method_name descriptor`? The eager first-call door asks it once its
/// admission holds the compile's epoch witness (interpreter round i1 wave 42,
/// lane L2): `execute` copied the bytecode under a guard it dropped long
/// before, so a redefinition that installed in between would otherwise be
/// compiled around -- a body of the old bytecode stamped with epochs newer
/// than the redefinition's fence, which `JitCache::put` publishes. Asked only
/// in a VM that redefined something. Takes the class-manager read lock.
pub(in crate::runtime::interpreter) fn method_code_is_current(
    shared: &SharedVm,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    code: &[u8],
) -> bool {
    let cm = shared.classes.class_manager.read();
    cm.get_class(class_id)
        .and_then(|class| class.find_method(method_name, descriptor))
        .and_then(|method| method.code())
        .is_some_and(|current| current.code[..] == *code)
}

/// Interpreter round i1 wave 43, lane L2
/// (`interpreter-L2-proposal-compile-inputs-are-read-inside-the-compile-witness-FIXED-20261007`):
/// the bytecode a compile uses, held together with the compile-epoch witness
/// ([`cratonvm_jit::open_compile_epoch_witness`]) that was open when the
/// bytecode was read.
///
/// A compiled body is judged at publication by the epochs its compile's
/// witness recorded. A door that read the bytecode BEFORE the witness opened
/// (at `compile_gate::admit`) could compile the old bytecode of a class a
/// redefinition replaced in between, stamp it with epochs newer than the
/// redefinition's fence, and have `JitCache::put` publish it after
/// `redefineClasses` returned (wave 42's tear). Every compile door now builds
/// one of these first, so the order is a property of the type:
///
/// * [`CompileInputs::read`] opens the witness, then runs the door's read,
///   which copies the bytecode under a class-manager guard of its own;
/// * [`BytecodeReadWitness::open`] / [`BytecodeReadWitness::inputs`] is the
///   same for a door that interleaves its own verdicts with the read
///   (`try_jit_compile_callee_slow`, the direct-bind callee compile);
/// * the `adopt*` constructors take bytecode the door already holds (an
///   invoke-cache entry's snapshot, the entering frame's code): they open the
///   witness and THEN compare the bytecode with the class's current method
///   under the class-manager guard, answering `None` on a mismatch -- one
///   rule for the upgrade door's gate, the OSR doors' `same_bytecode` and the
///   eager door's re-check.
///
/// The compile entry points take one: `CpResolvers::with_compile_request`
/// (every `CompileRequest` a VM door builds), `compile_optimizing_artifact`
/// and `compile_osr_body`. `i43_l2_compile_inputs_tests` pins the doors.
///
/// The witness is a thread-local scope that restores the outer one on drop,
/// so a `CompileInputs` lives on the stack of the door that built it and is
/// dropped before any witness opened before it.
pub(in crate::runtime::interpreter) struct CompileInputs<T = Arc<CachedBytecodeMethod>> {
    pub(in crate::runtime::interpreter::jit_bridge) inputs: T,
    pub(in crate::runtime::interpreter::jit_bridge) _witness: cratonvm_jit::CompileEpochWitness,
    /// al16 (2026-10-05): the loop header an optimizing OSR build was asked
    /// to enter ([`Self::with_osr_entry_pc`]); `None` for every other door.
    pub(in crate::runtime::interpreter::jit_bridge) osr_entry_pc: Option<usize>,
}

/// A compile-epoch witness opened ahead of a bytecode read that the door
/// performs itself, step by step, under its own class-manager guard (see
/// [`CompileInputs`]). Open it while holding that guard, or before taking it,
/// and before the bytecode is copied; [`Self::inputs`] then pairs it with
/// what was copied.
pub(in crate::runtime::interpreter) struct BytecodeReadWitness(pub(in crate::runtime::interpreter::jit_bridge) cratonvm_jit::CompileEpochWitness);

impl BytecodeReadWitness {
    /// Open the witness (two thread-local accesses and three atomic loads).
    #[inline]
    pub(in crate::runtime::interpreter) fn open() -> Self {
        Self(cratonvm_jit::open_compile_epoch_witness())
    }

    /// The inputs copied while this witness was open.
    #[inline]
    pub(in crate::runtime::interpreter) fn inputs<T>(self, inputs: T) -> CompileInputs<T> {
        CompileInputs {
            inputs,
            _witness: self.0,
            osr_entry_pc: None,
        }
    }
}

impl<T> CompileInputs<T> {
    /// Open the witness FIRST, then run `read`, which copies the method's
    /// bytecode under a class-manager guard it takes itself. `None` when
    /// `read` finds nothing to compile.
    pub(in crate::runtime::interpreter) fn read(read: impl FnOnce() -> Option<T>) -> Option<Self> {
        let witness = BytecodeReadWitness::open();
        let inputs = read()?;
        Some(witness.inputs(inputs))
    }

    /// What was read.
    #[inline]
    pub(in crate::runtime::interpreter) fn get(&self) -> &T {
        &self.inputs
    }

    /// The same inputs, for an optimizing OSR build asked to enter `pc`
    /// (`CompileRequest::osr_requested_bci`).
    pub(in crate::runtime::interpreter) fn with_osr_entry_pc(mut self, pc: usize) -> Self {
        self.osr_entry_pc = Some(pc);
        self
    }
}

impl CompileInputs<Arc<CachedBytecodeMethod>> {
    /// The method a `CompileRequest` compiles.
    #[inline]
    pub(in crate::runtime::interpreter) fn cached(&self) -> &Arc<CachedBytecodeMethod> {
        &self.inputs
    }

    /// A body the door already holds (an invoke-cache entry's snapshot, a
    /// frame's cached method): open the witness, then check under the
    /// class-manager guard that it is still its class's current method
    /// ([`cached_matches_class_body`]). `None` when a redefinition replaced
    /// it. Nothing but one relaxed load while no class of the process was
    /// ever redefined, and one guard and a lookup while this class never was.
    pub(in crate::runtime::interpreter) fn adopt(shared: &SharedVm, cached: Arc<CachedBytecodeMethod>) -> Option<Self> {
        let witness = BytecodeReadWitness::open();
        if crate::classloading::any_class_redefined() {
            let cm = shared.classes.class_manager.read();
            if cm.class_redefine_generation(cached.declaring_class_id) > 0
                && !cached_matches_class_body(&cm, &cached)
            {
                drop(cm);
                dbg_compile_inputs_refused(
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                    "adopt",
                );
                return None;
            }
        }
        Some(witness.inputs(cached))
    }
}

/// The inputs of a single-pass OSR compile ([`compile_osr_body`]): the
/// declaring class, its padded bytecode (`frame::padded_bytecode`, the
/// layout of `Frame::code`) and `max_locals`.
pub(in crate::runtime::interpreter) struct OsrBytecode {
    pub(in crate::runtime::interpreter) class_id: ClassId,
    pub(in crate::runtime::interpreter) code: Arc<[u8]>,
    pub(in crate::runtime::interpreter) max_locals: usize,
}

/// Where [`compile_osr_artifact`] takes the bytecode it compiles from.
pub(in crate::runtime::interpreter) enum OsrCodeSource<'a> {
    /// Read inside the witness by the caller (the background worker,
    /// [`CompileInputs::read`] over [`fetch_osr_compile_inputs`]).
    Read(&'a CompileInputs<OsrBytecode>),
    /// The entering frame's padded bytecode (`try_osr`), adopted
    /// ([`CompileInputs::adopt_frame_code`]) only when a compile is needed: a
    /// reused OSR artifact takes no guard.
    Frame {
        code: &'a Arc<[u8]>,
        max_locals: usize,
    },
}

impl CompileInputs<OsrBytecode> {
    /// A frame's padded bytecode, adopted as [`CompileInputs::adopt`] adopts
    /// a cached method: `None` when it is not `class_id`'s current
    /// `method_name descriptor` any more.
    pub(in crate::runtime::interpreter) fn adopt_frame_code(
        shared: &SharedVm,
        class_id: ClassId,
        method_name: &str,
        descriptor: &str,
        code: &Arc<[u8]>,
        max_locals: usize,
    ) -> Option<Self> {
        let witness = BytecodeReadWitness::open();
        if crate::classloading::any_class_redefined() {
            let cm = shared.classes.class_manager.read();
            if cm.class_redefine_generation(class_id) > 0
                && !padded_code_matches_class_body(&cm, class_id, method_name, descriptor, code)
            {
                let class_name = cm
                    .get_class(class_id)
                    .map_or_else(String::new, |c| c.name.to_string());
                drop(cm);
                dbg_compile_inputs_refused(&class_name, method_name, descriptor, "adopt-frame");
                return None;
            }
        }
        Some(witness.inputs(OsrBytecode {
            class_id,
            code: Arc::clone(code),
            max_locals,
        }))
    }
}

impl CompileInputs<Arc<[u8]>> {
    /// The eager first-call door's bytecode (`execute`, unpadded, copied at
    /// the call's entry), adopted as [`CompileInputs::adopt`] adopts a cached
    /// method, and padded (`frame::padded_bytecode`) for the backend.
    pub(in crate::runtime::interpreter) fn adopt_code(
        shared: &SharedVm,
        class_id: ClassId,
        method_name: &str,
        descriptor: &str,
        code: &[u8],
    ) -> Option<Self> {
        let witness = BytecodeReadWitness::open();
        if crate::classloading::any_class_redefined()
            && !method_code_is_current(shared, class_id, method_name, descriptor, code)
        {
            return None;
        }
        Some(witness.inputs(crate::runtime::frame::padded_bytecode(code)))
    }
}

/// Is `cached` (padded, as every cached method's code is) the body `cm`
/// holds for its method now: the same bytecode, exception table and sizes?
pub(in crate::runtime::interpreter::jit_bridge) fn cached_matches_class_body(
    cm: &crate::classloading::ClassManager,
    cached: &CachedBytecodeMethod,
) -> bool {
    let Some(code) = cm
        .get_class(cached.declaring_class_id)
        .and_then(|class| class.find_method(&cached.method_name, &cached.method_descriptor))
        .and_then(|method| method.code())
    else {
        return false;
    };
    let current: &[u8] = &code.code;
    let n = current.len();
    (cached.code.len() == n || cached.code.len() == n + 2)
        && cached.code[..n] == *current
        && cached.exception_table[..] == code.exception_table[..]
        && cached.max_locals == code.max_locals
        && cached.max_stack == code.max_stack
}

/// Is `padded` (a frame's code: the bytecode plus the two-byte tail) the
/// bytecode `cm` holds for `class_id`'s `method_name descriptor` now?
pub(in crate::runtime::interpreter::jit_bridge) fn padded_code_matches_class_body(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    padded: &[u8],
) -> bool {
    cm.get_class(class_id)
        .and_then(|class| class.find_method(method_name, descriptor))
        .and_then(|method| method.code())
        .is_some_and(|code| {
            let current: &[u8] = &code.code;
            let n = current.len();
            (padded.len() == n || padded.len() == n + 2) && padded[..n] == *current
        })
}

/// `CRATONVM_DBG_JITC`: a compile door's bytecode was not its class's current
/// method once the witness was open, so nothing was compiled (the positive
/// control of [`CompileInputs`]'s `adopt*`).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter::jit_bridge) fn dbg_compile_inputs_refused(class_name: &str, method_name: &str, descriptor: &str, how: &str) {
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] compile inputs refused ({how}) {class_name}.{method_name}{descriptor}: \
             the bytecode is not the class's current body"
        );
    }
}

/// Has the upgrade door already found `cached` calling a native-shadowed
/// target, since this VM's last redefinition? See
/// `JitBridgeState::upgrade_native_shadow_callers`. One lock and one probe;
/// the key is two reference-count bumps.
pub(in crate::runtime::interpreter::jit_bridge) fn upgrade_native_shadow_caller_known(shared: &SharedVm, cached: &CachedBytecodeMethod) -> bool {
    let epoch = shared.jit.jit_cache.redefine_epoch();
    let key = (
        cached.declaring_class_id.as_u32(),
        Arc::clone(&cached.method_name),
        Arc::clone(&cached.method_descriptor),
    );
    shared
        .jit
        .bridge
        .upgrade_native_shadow_callers
        .lock()
        .map(|m| m.get(&key) == Some(&epoch))
        .unwrap_or(false)
}

/// Remember the upgrade door's native-shadow verdict for `cached` (wave 38,
/// lane L2). `CRATONVM_DBG_JITC` names it: the positive control of
/// `L2W38UpgradeDoorNativeShadowOsr`.
pub(in crate::runtime::interpreter::jit_bridge) fn note_upgrade_native_shadow_caller(shared: &SharedVm, cached: &CachedBytecodeMethod) {
    let epoch = shared.jit.jit_cache.redefine_epoch();
    if let Ok(mut m) = shared.jit.bridge.upgrade_native_shadow_callers.lock() {
        m.insert(
            (
                cached.declaring_class_id.as_u32(),
                Arc::clone(&cached.method_name),
                Arc::clone(&cached.method_descriptor),
            ),
            epoch,
        );
    }
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] upgrade refused (calls a native-shadowed method; not bail-listed) {}.{}{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        );
    }
}

/// [`try_jit_upgrade_with_gate`] without panic containment.
pub(in crate::runtime::interpreter::jit_bridge) fn try_jit_upgrade_with_gate_uncontained(
    shared: &SharedVm,
    cached: &Arc<CachedBytecodeMethod>,
    gate: RedefineGate,
) -> Option<CachedInvokeTarget> {
    // Kill-switch: CRATONVM_DISABLE_JIT=1 forces interpreter-only execution.
    // Mirrors the gate in `try_jit_compile_callee` so the user-facing
    // CRATONVM_DISABLE_JIT flag actually disables BOTH JIT entry points
    // (the caller-method counter path here, and the dispatcher path there).
    // Useful for bisecting JIT-vs-interpreter bugs during bootstrap crashes.
    if crate::runtime::env_cache::disable_jit() || compile_admission_closed_for_capture() {
        return None;
    }
    // A redefined class compiles here like any other (interpreter round i1
    // wave 38, lane L2; item 5 of the wave-37 compile-door review). This door
    // refused every class redefined even once (`gate.generation > 0`), while
    // the eager first-call door, the callee door and `try_osr` compiled it,
    // so which methods of a mocked or retransformed class were compiled
    // depended on which door asked first. What the refusal protected is kept
    // by the checks it stood for: the body compiled must be the class's
    // CURRENT one. `cached` is the invoke-cache entry's, and a gate
    // snapshotted by the caller (`try_jit_upgrade`, the lambda nominations)
    // is fresh even when `cached` predates the last redefinition, so for a
    // redefined class the body is compared with the class's own
    // (`cached_is_current_body`); after the compile the gate is asked again,
    // and a redefinition racing the compile makes it stale (the compile-epoch
    // witness refuses the publish as well).
    if gate.is_stale() || (gate.generation > 0 && !cached_is_current_body(shared, cached)) {
        return None;
    }
    // The compiled body carries no ACC_SYNCHRONIZED monitor prologue/epilogue —
    // the *caller* supplies it. Both interpreter entry points into compiled code
    // (`execute_jit_call` and `execute_jit_call_decoded`) already wrap the call
    // in a `JitSynchronizedMonitorGuard`, which acquires the receiver's (or the
    // class mirror's) monitor for the whole native activation and releases it on
    // every Rust return path. A `CachedInvokeTarget::Jit` is only ever consumed
    // through those two, so admitting a synchronized method here is contained.
    //
    // Every OTHER entry runs the body with no monitor at all, and each must
    // refuse a synchronized callee independently:
    //   * compiled→compiled direct dispatch — `try_jit_compile_callee`, both
    //     on its compile path (`..._slow`'s `is_synchronized` gate) AND on its
    //     `jit_cache` fast path, which serves an already-published body and so
    //     never reaches that gate;
    //   * `jit_invoke_dispatch`'s own `jit_cache` arm (`vm/src/jit/helpers.rs`),
    //     which fills `DISPATCH_CACHE` and does not go through
    //     `try_jit_compile_callee` at all;
    //   * the specialized `get(I)D` scalar routes in the same file — `Vector.get`
    //     is `synchronized` in the JDK, so this one is not hypothetical;
    //   * inlining — `resolve_inline_site`'s `method.is_synchronized()` gate.
    //
    // OSR needs no gate (interpreter round i1 wave 10): its body runs inside
    // the interpreter frame that already holds the monitor, and is published
    // only in `JitCache`'s OSR slot, which none of the doors above reads.
    //
    // The first three ask `CompiledMethod::requires_wrapped_entry`, stamped at
    // publication, because they hold a raw entry pointer and no method handle.
    // Listing only the compile-time gates here is what let the fast paths drift:
    // the enumeration said "three" while `jit_cache` answered for two more.
    //
    // Why this matters: every layer Tomcat's BCEL annotation scan drives per
    // byte (`ByteArrayInputStream.read()`, `DataInputStream.readUnsignedByte`)
    // is an ACC_SYNCHRONIZED one-liner, so this gate kept the whole webapp
    // deploy interpreted — the method was rejected here *before* it was ever
    // counted, which is why `jit-method-stats` reported it neither compiled nor
    // `hot_but_stuck_in_interpreter`. See
    // docs/feature-designs/real-jdk-mode.md.
    //
    // Default-OFF pending the A/B and the concurrency soak: `CRATONVM_JIT=sync-methods`.
    if cached.is_synchronized && !jit_sync_methods_enabled() {
        return None;
    }
    // RBC.4 — short-circuit permanently-uncompilable methods BEFORE the
    // expensive gates below (two superclass-chain walks under the
    // class_manager read lock). The retry stride re-enters this function
    // every 64 calls for a hot method; with a scan-rejected (e.g. athrow)
    // method that meant tens of thousands of full gate evaluations per
    // suite run while `try_compile` would bail instantly anyway.
    if jit_verdicts(shared).is_bail_listed(
        cached.declaring_class_id,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        return None;
    }
    // N3 — the same short-circuit for the OTHER sticky verdict. This is the
    // callee-dispatch door, and it is the door the tomcat `Response.toAbsolute()`
    // fix needed a runtime `MakeNotCompilable` to reach: `jit_skip_set` gates
    // only the interpreter's own hotness path, so without a check here a
    // give-up decision reached through a JIT-compiled caller never stuck.
    //
    // `try_compile_request` below asks `compile_gate::admit`, which asks this
    // too, so this check is about COST and not about correctness — exactly the
    // argument RBC.4 makes for the bail-list check above it. The OSR door in
    // `compile_osr_artifact` deliberately does NOT get a copy: a loop in the
    // method may be perfectly compilable while one call site mis-speculates.
    if shared.jit.runtime_despec.is_not_compilable(
        cached.declaring_class_id,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    ) {
        return None;
    }
    // S111r15 — refuse to JIT a method that has a Rust native shadow.
    // Mirrors the equivalent gate in `try_jit_compile_callee` so the
    // caller-method-counter path doesn't bypass natives that the
    // dispatcher path correctly defers to. Concretely: without this
    // check, `Character.toLowerCase(C)C` got JIT-compiled (its JDK
    // bytecode delegates to `(I)I` → `CharacterData.of/toLowerCase`
    // virtual chain), and the resulting machine code returned 0 for
    // most inputs after warm-up, corrupting Spring's
    // `BeanPropertyName.toDashedForm` (`bannerMode` →
    // `r\0\0\0\0\0\0\0-\0\0\0\0\0\0`) and tripping
    // `InvalidConfigurationPropertyNameException` during SportMe boot.
    //
    // bytebuddy_probe / ANTLR cold-path follow-up: reject an override on this
    // upgrade path only when its body invokes a native-shadowed target such as
    // `Object.equals`. A bytecode override that merely has an identity native
    // somewhere up its ancestor chain is allowed to compile.
    {
        // Wave 38 (lane L2): the scan's verdict, remembered per VM (see
        // `JitBridgeState::upgrade_native_shadow_callers`).
        if upgrade_native_shadow_caller_known(shared, cached) {
            return None;
        }
        if registered_native_will_run(
            shared,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        ) {
            if crate::runtime::env_cache::dbg_bblp()
                && cached.class_name.contains("LazyProjection")
                && &*cached.method_name == "equals"
            {
                eprintln!(
                    "[BBLP-upgrade] direct-native skip {}.{}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                );
            }
            return None;
        }
        // No per-class exemption (interpreter round i1 wave 9): a method whose
        // only native-shadow hit is an abstract declaration's own registration
        // (`org/elasticsearch/tdigest/Dist.quantile`/`cdf` calling
        // `Function.apply`, which used to be exempted here BY NAME) passes the
        // scan itself — see `jit_invoke_targets_native_shadow`. The first-call
        // door asks the same scan, so the two doors now agree on those methods.
        //
        // Wave 42 (lane L2): the scan is the retired whole-method CALLER seal,
        // opt-in (`env_cache::jit_native_shadow_caller_seal`, where the per-site
        // screens that replace it are listed); this door's compile
        // (`compile_optimizing_artifact`) screens a baked CALL and a splice per
        // site, as the worker's does. The method's own registered native,
        // asked above, still refuses.
        if crate::runtime::env_cache::jit_native_shadow_caller_seal()
            && jit_method_calls_native_shadowed(
                shared,
                cached.declaring_class_id,
                &cached.code,
                cached.code.len().saturating_sub(2),
            )
        {
            if crate::runtime::env_cache::dbg_bblp()
                && cached.class_name.contains("LazyProjection")
                && &*cached.method_name == "equals"
            {
                eprintln!(
                    "[BBLP-upgrade] inner-native skip {}.{}{}",
                    cached.class_name, cached.method_name, cached.method_descriptor
                );
            }
            // PERF FIX (2026-07-15, companion to the invoke-cache fix above):
            // this verdict is a pure function of the method's bytecode (which
            // native-shadowed targets it calls never changes for a given
            // class+method+descriptor, mirroring the `any_class_redefined()`
            // coarse-invalidation already relied on by the `mark_jit_bail_listed`
            // call in `jit::try_compile` — see the "RBC.4" comment at the top
            // of this function). Without memoizing it here, a hot method that
            // calls ANY native-shadowed target (extremely common — e.g. one
            // that calls `Object.equals`/`String` methods internally) re-runs
            // this full O(method-bytecode-size) decode-and-scan
            // (`jit_method_calls_native_shadowed`) from scratch every
            // `JIT_RETRY_STRIDE` (64) invocations, forever, for the lifetime
            // of the process — the exact "tens of thousands of full gate
            // evaluations per suite run" cost pattern RBC.4 fixed for
            // scan-rejected methods, just via a different gate that wasn't
            // wired into the same short-circuit. Remembered so the check at
            // the top of this block short-circuits every future retry.
            //
            // NOT in the backend bail list any more (interpreter round i1 wave
            // 38, lane L2; item 6 of the wave-37 compile-door review): that
            // list is a statement about the backend, honoured at every door,
            // so this policy verdict of one door also refused both OSR routes
            // (which resolve native shadows per site and never ask it) -- a
            // method's loop was OSR-compiled or not depending on whether this
            // door or an OSR offer came first. The eager first-call door keeps
            // its own seal (`jit_skip_set`), which the OSR doors do not read
            // either.
            note_upgrade_native_shadow_caller(shared, cached);
            return None;
        }
        if crate::runtime::env_cache::dbg_bblp()
            && cached.class_name.contains("LazyProjection")
            && &*cached.method_name == "equals"
        {
            eprintln!(
                "[BBLP-upgrade] no native shadow, COMPILING {}.{}{}",
                cached.class_name, cached.method_name, cached.method_descriptor
            );
        }
    }
    // W2-CHM: honor the JIT skip list on this caller-method-counter
    // promotion path too. Previously only the first-call compile path
    // (interpreter.rs::~1112) and the callee-dispatcher path
    // (try_jit_compile_callee) consulted `should_skip_jit`; promotions
    // triggered by the caller's invocation count silently bypassed the
    // list and JIT'd skip-listed methods (notably `Integer.valueOf` /
    // `Integer.<init>`) anyway, defeating the W2-CHM box-method ban.
    // Reproducer: `apps/chm_basic/ChmScale` lost entries `k992..k999`
    // even after `is_known_miscompile` listed `Integer.valueOf` because
    // ChmScale's `main` outer-frame loops crossed the per-callee
    // invocation threshold (2000) and re-promoted `Integer.valueOf`
    // here.
    {
        // The static JIT ban list was deleted 2026-07-31 (see
        // docs/feature-designs/native-dispatch-architecture.md).
        // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
        // remaining force-interpret lever, applied in `jit::try_compile`.
        // GPU-offload JIT admission gate — see offload_jit_gate: a
        // promoted caller containing an offload-eligible invokestatic
        // would bypass the interpreter offload hook.
        #[cfg(feature = "gpu-offload")]
        if crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
            shared,
            cached.declaring_class_id,
            &cached.method_name,
            &cached.method_descriptor,
        ) {
            return None;
        }
    }
    // This VM's supersede epoch, read BEFORE the artifact below: a C2 publish
    // racing in after the read leaves the entry stale (re-resolved at its next
    // hit), where a snapshot taken after it could pin the replaced C1 body
    // until the next supersede.
    let supersede_epoch = shared.jit.supersede_gate();
    // Check shared JIT cache first
    {
        let jit_cache = shared.jit.jit_cache.read();
        if let Some(compiled) = jit_cache.get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        ) {
            let ret = cached.return_tag();
            let heap = compiled.needs_heap();
            return Some(CachedInvokeTarget::Jit {
                compiled: compiled.into(),
                num_params: cached.num_params,
                return_type: ret,
                needs_heap: heap,
                cached: cached.clone(),
                gate,
                supersede_epoch,
            });
        }
    }

    // Try to compile — build CP resolvers for multianewarray and field access
    // The assembly, which this door no longer owns. Its gates above are the
    // policy half; `compile_optimizing_artifact` is the half another door can
    // reuse without inheriting them. See that function for why the split is
    // not cosmetic.
    // Interpreter round i1 wave 43, lane L2 (`CompileInputs`): the entry's
    // snapshot is adopted inside the compile's epoch witness -- compared with
    // the class's current method after the witness opened -- instead of
    // relying on the gate asked above and again below; a redefinition that
    // lands after this makes the body unpublishable.
    let inputs = CompileInputs::adopt(shared, Arc::clone(cached))?;
    // `_callee_pins` is named, not `_`, so it lives to the end of this
    // function: past the `put` below. See `PinnedArtifact`.
    // This door has no use for the acceptance verdict (it never read the
    // slot); since round 15 wave 2 it is no longer left there either.
    let PinnedArtifact {
        mut compiled,
        callee_pins: _callee_pins,
        verdict: _,
    } = compile_optimizing_artifact(shared, &inputs, false)?;
    // A redefinition of the class landed while this compiled `cached`: the
    // body is the old code's (wave 38, lane L2; see the gate check above).
    if gate.is_stale() {
        return None;
    }
    let ret = cached.return_tag();
    let heap = compiled.needs_heap();

    // Store in shared JIT cache
    stamp_compilation_epoch(
        shared,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        &mut compiled,
    );
    // Round 13 wave 3 (lane replay2; R13-1): the compile's own template.
    stamp_compiled_source_of(&compiled, Arc::clone(cached));
    // Stamp the wrapped-entry requirement before the body is shared.
    // See `CompiledMethod::requires_wrapped_entry`: publication is the
    // last point that still knows this is an `ACC_SYNCHRONIZED` method,
    // and every unwrapped consumer downstream holds only a raw entry
    // pointer.
    compiled.requires_wrapped_entry = cached.is_synchronized;
    // JIT round 19, lane eager19 (TP16-23): the body this publish may displace.
    let forward_every_door = supersede_forward_every_door_enabled();
    let (compiled_arc, forward_displaced) = {
        let mut jit_cache = shared.jit.jit_cache.write();
        let before = forward_every_door
            .then(|| {
                jit_cache.get(
                    &cached.class_name,
                    &cached.method_name,
                    &cached.method_descriptor,
                    cached.declaring_class_id,
                )
            })
            .flatten();
        // VH3-1: see `compile_osr_body`.
        if !crate::jit::helpers::varhandle_thin_bound_artifact_is_stale(shared, &compiled) {
            jit_cache.put(
                cached.class_name.clone(),
                cached.method_name.clone(),
                cached.method_descriptor.clone(),
                cached.declaring_class_id,
                compiled,
            );
        }
        let published = jit_cache.get(
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
            cached.declaring_class_id,
        );
        (published, before)
    };
    // The lock is gone; a displaced body is released through `RetainedCode`
    // (inside `supersede_forward_displaced`) on every path below.
    let Some(compiled_arc) = compiled_arc else {
        if let Some(displaced) = forward_displaced {
            drop(cratonvm_jit::RetainedCode::new(displaced));
        }
        return None;
    };
    if let Some(displaced) = forward_displaced {
        supersede_forward_displaced(shared, displaced, &compiled_arc, &cached.method_name);
    }
    // JIT round 19, lane eager19 (TP16-21): this door publishes self-locking
    // bodies too.
    late_bind_self_lock_published(
        shared,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        &compiled_arc,
    );
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] upgrade-OK {}.{}{} entry={:p} len={}",
            cached.class_name,
            cached.method_name,
            cached.method_descriptor,
            compiled_arc.entry_ptr(),
            compiled_arc.code_bytes().len()
        );
    }
    crate::jit::disasm::maybe_dump(
        if compiled_arc.used_ir_backend {
            "upgrade/ir"
        } else {
            "upgrade/sp"
        },
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        compiled_arc.entry_ptr(),
        compiled_arc.code_bytes(),
    );

    Some(CachedInvokeTarget::Jit {
        compiled: compiled_arc.into(),
        num_params: cached.num_params,
        return_type: ret,
        needs_heap: heap,
        cached: cached.clone(),
        gate,
        supersede_epoch,
    })
}

/// RFJP.1 — the RETIRED JIT correctness workaround for
/// `RecursiveTask<Long>.compute()`, now OFF by default.
///
/// # What it was
///
/// Methods on a class transitively extending `java/util/concurrent/ForkJoinTask`
/// were force-interpreted, because a JIT'd `compute()` returned 0 once the
/// recursion depth reached ~10+. That was root-caused not to regalloc but to a
/// `Long.valueOf` boxing miscompile, and fixed in Session 108
/// (commit 6f605451d, "RFJP.1 closed as side-effect"). The blocklist was never
/// taken back out.
///
/// # Why leaving it in was expensive
///
/// Its own comment claimed it was "narrow enough to leave ... CompletableFuture
/// paths JIT-eligible because they don't extend `ForkJoinTask` directly in the
/// hot path". That claim was FALSE: `CompletableFuture$UniCompose` and
/// `$UniRelay` extend `Completion`, which extends `ForkJoinTask`, and they ARE
/// the composition hot path — 199 476 and 119 668 invocations on
/// `HibfixComposeProbe2`, every one of them refused here. The whole completion
/// machinery ran interpreted, at 1.57x on that probe.
///
/// # What retired it (2026-08-27)
///
/// `probes/FjpStress.java` sums a `long[200000]` to depth 17 through all three
/// FJP bases the blocklist names — `RecursiveTask<Long>` (the boxed-return
/// shape RFJP.1 was recorded against), `RecursiveAction` and
/// `CountedCompleter` — 20 rounds each, at C1 and at C2, with
/// `compute()` WITNESSED compiled (`CRATONVM_DBG=jit-method-stats` moves
/// `FjpDeepSum$SumTask.compute` from `3 145 652 invocations compile-failed` to
/// compiled with `hot_but_stuck_in_interpreter=0`). Plus `FjpProbe` at depth 10
/// and `FjpDeepSum` at depth 17. See
/// `completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`.
///
/// Returns `true` only when `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1` puts the
/// workaround back AND the named class transitively extends
/// `java/util/concurrent/ForkJoinTask`.
pub fn is_fjp_subclass_blocklisted(
    shared: &SharedVm,
    class_name: &str,
    requesting_class_id: Option<ClassId>,
) -> bool {
    // ROLLBACK LEVER — `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1`, default OFF.
    //
    // Everything below this line is dead on the shipped configuration. It is
    // kept, rather than deleted, so a regression that the FjpStress/FjpProbe/
    // FjpDeepSum evidence did not cover can be bisected against the old
    // behaviour in one environment variable instead of one revert.
    if !crate::runtime::env_cache::jit_fjp_subclass_blocklist() {
        return false;
    }
    // Cheap exact-name fast path — the JDK classes themselves are always
    // affected by the same regalloc shape if they ever get to JIT.
    if class_name == "java/util/concurrent/ForkJoinTask"
        || class_name == "java/util/concurrent/RecursiveTask"
        || class_name == "java/util/concurrent/RecursiveAction"
        || class_name == "java/util/concurrent/CountedCompleter"
    {
        return true;
    }
    let cm = shared.classes.class_manager.read();
    let start_cid = requesting_class_id
        .and_then(|requester| cm.find_class_by_name_for_class(class_name, requester))
        .or_else(|| cm.find_unique_class_by_name(class_name));
    let Some(start_cid) = start_cid else {
        return false;
    };
    let mut cid = start_cid;
    // Bound the walk so a corrupt/circular hierarchy can't loop forever.
    for _ in 0..64 {
        let Some(class) = cm.get_class(cid) else {
            return false;
        };
        if &*class.name == "java/util/concurrent/ForkJoinTask" {
            return true;
        }
        match class.superclass {
            Some(parent_id) => cid = parent_id,
            None => return false,
        }
    }
    false
}

/// FJP fix case (2)'s question for [`try_jit_compile_callee_slow`]: does the
/// native registered on the DECLARING class of an inherited method run for a
/// call that resolves to it?
///
/// It used to be registry presence alone, while case (1) (the receiver's own
/// class, the same function) asks [`registered_native_will_run`], which since
/// COMPAT21-3 answers "does not run" for a non-`Intrinsic` registration the
/// resolver yields to the loaded bytecode. So one method compiled when called
/// on its declaring class and was refused when the same call reached it
/// through a subclass receiver. JIT round 21 lane reflect3/4 (page
/// `jr21-reflect3-bytecode-only-fallback-bodies-always-interpret`): the
/// compiled `BuiltinClassLoader.loadClassOrNull` body calls
/// `ClassLoader.findLoadedClass` on `ClassLoaders$AppClassLoader`; the
/// `Bridge` on `ClassLoader` yields to that bytecode (the interpreter runs
/// it, and it compiles), but the MIC's compile probe was refused here, so
/// every call took the by-name `invoke_or_native` tail (~2 us a call, four
/// per `loadClass`), and the compiled body ran twice as slow as the
/// interpreted one.
///
/// Asked on the declaring class BY ID (`registered_native_yields_to_bytecode_of`,
/// a recursive read: the caller holds the class-manager guard), and only the
/// resolver's yield term: everything else keeps the old answer, so a
/// `SyntheticStub` or an `Intrinsic` still refuses as before.
/// `CRATONVM_JIT_INHERITED_NATIVE_ASKS_RESOLVER` (perf, default ON; `=0`
/// restores registry presence) and COMPAT21-3's own switch
/// (`CRATONVM_JIT_WILL_RUN_ASKS_RESOLVER`) must both be on.
pub(in crate::runtime::interpreter::jit_bridge) fn inherited_native_runs(
    shared: &SharedVm,
    declaring_id: ClassId,
    declaring_class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let Some((callback, kind)) = shared.natives.native_methods.find_with_kind(
        declaring_class_name,
        method_name,
        descriptor,
    ) else {
        return false;
    };
    let yields = kind != cratonvm_native_api::NativeKind::Intrinsic
        && will_run_asks_resolver_enabled(shared)
        && crate::vm::registered_native_yields_to_bytecode_of(
            shared,
            declaring_id,
            declaring_class_name,
            method_name,
            descriptor,
            callback,
            kind,
        );
    !yields
}

/// The name-level half of the direct-bind gate: may a compiled caller in
/// `requester_class_id` bake a raw CALL to `callee_class.callee_method
/// callee_desc` at all? Asked before the callee is resolved and before any
/// JIT-cache probe.
///
/// One copy for the three direct-bind closures (the mutator door's
/// `callee_compiler`, the background door's `direct_callee_lookup` and
/// [`cycle_edge_target_admitted`]); they used to carry their own, which had
/// already drifted in order and in the native-shadow question they asked.
///
/// * RFJP.1 (retired, lever-only): a class transitively extending
///   `java/util/concurrent/ForkJoinTask`; inert unless
///   `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1`.
/// * S111r15: a callee whose registered native will run. A raw CALL to the
///   bytecode would bypass it: the recursive callee-compile path once
///   direct-called `Character.toLowerCase(C)C`'s JDK bytecode, and the machine
///   code returned 0 for most inputs, so Spring's
///   `BeanPropertyName.toDashedForm` produced `r\0\0\0\0\0\0\0-\0\0\0\0\0\0`
///   for `bannerMode` (`InvalidConfigurationPropertyNameException` in
///   SportMe). The question is `registered_native_will_run` — the dispatch
///   arbitration — not a bare registry probe: a synthetic stub that yields to
///   real bytecode is not what runs, so binding that bytecode IS the
///   interpreter's decision.
pub(in crate::runtime::interpreter::jit_bridge) fn direct_bind_name_refusal(
    shared: &SharedVm,
    requester_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> Option<cratonvm_jit::DirectBindRefusal> {
    if is_fjp_subclass_blocklisted(shared, callee_class, Some(requester_class_id)) {
        return Some(cratonvm_jit::DirectBindRefusal::FjpBlocklist);
    }
    if registered_native_will_run(shared, callee_class, callee_method, callee_desc) {
        return Some(cratonvm_jit::DirectBindRefusal::NativeShadow);
    }
    // Interpreter round i7 wave 136, lane L2: an interface sender's unproven
    // `invokespecial` target stays on the dispatch helper, which makes the
    // receiver check (`CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, default
    // OFF; one per-VM bool read with it off).
    if iface_special_guarded_target(
        shared,
        requester_class_id,
        callee_class,
        callee_method,
        callee_desc,
    ) {
        return Some(cratonvm_jit::DirectBindRefusal::IfaceSpecialReceiverGuard);
    }
    None
}

/// The census name of a direct-bind refusal (the `intrinsic-stats` label).
pub(in crate::runtime::interpreter::jit_bridge) fn direct_bind_refusal_name(refusal: cratonvm_jit::DirectBindRefusal) -> &'static str {
    // Cast: fieldless `#[repr(usize)]` enum to its explicit discriminant.
    cratonvm_jit::DIRECT_BIND_REFUSAL_NAMES[refusal as usize]
}

/// The resolved-method half of the direct-bind gate, asked BEFORE any
/// JIT-cache probe (a cache hit must not bind what these refuse):
///
/// * a `synchronized` callee — a compiled body carries no monitor prologue, so
///   a raw CALL to it does not lock, and the background tiering door publishes
///   synchronized bodies on purpose (`execute_jit_call` wraps them). Checked
///   after the probe, a cache hit once bound such a body directly (the
///   `RSyncMethodJit` lost-increment defect). The way past this refusal is NOT
///   to drop it but to have the CALLER hold the monitor around the CALL, for
///   a callee body that can neither trap nor call (so no deopt can stop it
///   holding a monitor no frame state names). Why that design, the soundness
///   argument and the cross-lane steps: page
///   `r11w4-sync-compiled-caller-cannot-call-a-synchronized-callee` (Wave 9)
///   and `r11w9-sync-caller-held-monitor-direct-call-patch`;
/// * a callee with no `Code` attribute (nothing to CALL), or one declaring its
///   own exception table while `direct_call_exc_table_publish_enabled` is off
///   (BUG-H: a raw CALL let an implicit AIOOBE/NPE escape the callee's own
///   `catch`);
/// * JVMS §5.5: a `static` callee whose declaring class is not initialized
///   yet — the raw CALL bypasses every init check, and a compiled site never
///   re-binds (jit-invokestatic-clinit-gap). `declaring_class_initialized` is
///   the caller's `is_class_initialized_fast` answer; it is monotone, so
///   checking once at bind time is sound forever.
pub(in crate::runtime::interpreter::jit_bridge) fn direct_bind_method_refusal(
    method: &cratonvm_reader::method::ClassFileMethod,
    declaring_class_initialized: bool,
) -> Option<cratonvm_jit::DirectBindRefusal> {
    if method.is_synchronized() {
        return Some(cratonvm_jit::DirectBindRefusal::Synchronized);
    }
    let code_bars_direct_call = match method.code() {
        None => true,
        Some(c) => {
            !c.exception_table.is_empty() && !cratonvm_jit::direct_call_exc_table_publish_enabled()
        }
    };
    if code_bars_direct_call {
        return Some(cratonvm_jit::DirectBindRefusal::CalleeExceptionTable);
    }
    if method.is_static() && !declaring_class_initialized {
        return Some(cratonvm_jit::DirectBindRefusal::DeclaringClassNotInitialized);
    }
    None
}

/// Round 13 wave 6 (lane sync3, item 7 of
/// `r13w4-sync2-self-locking-bodies-with-deopt-exits-FIXED-20260930.md`): may a
/// direct bind that [`direct_bind_method_refusal`] refused ONLY as
/// `Synchronized` still bake a CALL to a published SELF-LOCKING body of
/// `method` ([`is_self_locking_body`])? Such a body takes its receiver's
/// monitor itself, so a raw CALL to it is a correctly locked call -- what the
/// inline caches and the dispatch cache already do with it since wave 4.
///
/// Only a method with no exception table can have one (`cratonvm_jit`
/// `self_lock_admits_method`); the refusals the `Synchronized` answer masked
/// (a declared table, an uninitialized static class) are asked again here.
/// The door then binds ONLY a published self-locking body: no cache hit, or a
/// wrapped one, is still the `Synchronized` refusal, and nothing is compiled
/// for the site. `CRATONVM_JIT_SELF_LOCKING_DIRECT_BIND=0` restores the
/// blanket refusal. Read per bind, at compile time.
///
/// Round 13 wave 10 (lane sync6): a `static synchronized` method too, whose
/// self-locking body locks its class mirror
/// (`CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`), but only once its declaring
/// class is initialized (`declaring_class_initialized`, the value the caller
/// hands `direct_bind_method_refusal`): a raw CALL bypasses the
/// initialisation check (JVMS 5.5), which the `Synchronized` refusal masked.
///
/// JIT round 17 wave 2 (lane syncdoor2, SD17-2): "only a method with no
/// exception table can have one" stopped being true with
/// `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS`, which publishes self-locking
/// bodies of methods WITH a table. Under that switch the table is asked by
/// `direct_bind_method_refusal`'s own rule
/// (`direct_call_exc_table_publish_enabled`: the baked CALL then carries the
/// callee-deopt service, through which the body's exceptional exits reach
/// `run_jit_callee_handler` as they do from the dispatch path), so a
/// `final` / `private` / `super.` site gets a CALL instead of a
/// `jit_invoke_dispatch` round trip per call. Kill switch
/// `CRATONVM_JIT_SELF_LOCKING_DIRECT_BIND_TABLES=0`. With the handlers switch
/// off no such body is published and the rule is the old one.
pub(in crate::runtime::interpreter::jit_bridge) fn self_locking_body_bind_candidate(
    method: &cratonvm_reader::method::ClassFileMethod,
    declaring_class_initialized: bool,
) -> bool {
    method.is_synchronized()
        && (!method.is_static() || declaring_class_initialized)
        && method.code().is_some_and(|c| {
            c.exception_table.is_empty()
                || (cratonvm_jit::direct_call_exc_table_publish_enabled()
                    && cratonvm_jit::self_locking_sync_handlers_enabled())
        })
}

/// A published body a raw CALL may enter although its method is
/// `synchronized`: a self-locking one (`CompiledMethod::self_locks_monitor`,
/// which `JitCache::put` publishes unwrapped). Round 13 wave 6 (lane sync3).
pub(in crate::runtime::interpreter::jit_bridge) fn is_self_locking_body(compiled: &cratonvm_jit::CompiledMethod) -> bool {
    compiled.self_locks_monitor && !compiled.requires_wrapped_entry
}

/// The `synchronized` callee a caller-held direct CALL may bind at the
/// invoke whose Methodref is `cp_idx` in `class_id`'s pool
/// (`CompileRequest::sync_direct_lookup`), with the artifact that owns its
/// entry. The caller must keep that artifact alive until it publishes (each
/// door pushes it onto its `bind_pins`).
///
/// Round 11 wave 10 (lane `synccall`, page
/// `r11w9-sync-caller-held-monitor-direct-call-patch`) wrote this as the
/// optimizing door's closure; round 11 wave 13 (lane `spsync`, page
/// `r11w12-calls-single-pass-sync-direct-patch`) extracted it unchanged so the
/// single-pass compile door asks the same question. Its refusals are already
/// counted once for the site, as `DirectBindRefusal::Synchronized`, by the
/// door's direct-bind lookup.
///
/// Until round 11 wave 17 it never compiled anything ("the caller's next
/// compile binds it"), which is false for a caller compiled once, such as an
/// OSR body: a callee with no published body now gets its wrapped-entry body
/// compiled here, after every refusal, under the eager callee chain's bounds
/// (`sync_direct_compile_unpublished_callee`).
pub(in crate::runtime::interpreter::jit_bridge) fn sync_direct_target(
    shared: &SharedVm,
    class_id: ClassId,
    cp: &CpResolvers<'_>,
    cp_idx: u16,
) -> Option<(
    cratonvm_jit::compile_request::SyncDirectTarget,
    std::sync::Arc<cratonvm_jit::CompiledMethod>,
)> {
    // Round 14 wave 3 (lane sync, SS-2): the monitor half is
    // `sync_direct_monitor`, shared with `sync_direct_monitor_answer`.
    let SyncDirectMonitor {
        callee_class,
        callee_method,
        callee_desc,
        class_cp_idx,
        method,
        declaring_id,
        instance,
        unoverridable,
    } = sync_direct_monitor(shared, class_id, cp, cp_idx)?;
    // BUG-H, STRICTER than `direct_bind_method_refusal` asks it: a callee
    // that declares any exception table is refused whatever
    // `direct_call_exc_table_publish_enabled` says. That switch admits a
    // table callee to a plain direct CALL because the CALL is followed by
    // `emit_inline_callee_deopt_check`, whose service runs the CALLEE's own
    // handler when its body returns the sentinel with an exception its
    // compiled code did not catch (an implicit signal the local-handler pad
    // declined, `local_handler_enter_implicit`'s `-1` arms; a callee with no
    // compiled handler for that bci). A caller-held synchronized CALL has no
    // such service on either tier: its cold side releases the monitor and
    // throws from the CALLER (`x64/op_invoke.rs` `sync_direct_site`,
    // `ir_lower.rs` `emit_sync_exit_then_throw`). So the callee's `catch`
    // was skipped and the exception escaped into the caller: round 11 wave
    // 15 (lane tier), the root cause of the wrong answer that withdrew the
    // single-pass OSR door's route (`R11TierSyncDoorCompiled.bumpCatch`,
    // `ArithmeticException` at `drive` line 71; page
    // `r11w13-spsync-osr-door-has-no-sync-direct-route`). A table callee
    // keeps the dispatch helper, whose door routes through the table.
    let code_attr = method.code()?;
    if !code_attr.exception_table.is_empty() {
        return None;
    }
    // Round 11 wave 17 (lane `tier`): a callee with NO published body yet is
    // compiled here, under the eager callee chain's bounds, instead of leaving
    // the site on the dispatch helper for the life of the caller's artifact.
    // See `sync_direct_compile_unpublished_callee` for the race this closes
    // (the residual bimodal `sync-method-1t`).
    let published =
        shared
            .jit
            .jit_cache
            .read()
            .get(&callee_class, &callee_method, &callee_desc, declaring_id);
    let compiled = match published {
        Some(compiled) => compiled,
        None => sync_direct_compile_unpublished_callee(
            shared,
            declaring_id,
            &callee_class,
            &callee_method,
            &callee_desc,
        )?,
    };
    // Whichever tier published the body. Since wave 10 `putstatic` is
    // IR-admitted, so an optimizing body of a callee like
    // `R11X64genSyncCost.incSync` can supersede the single-pass one.
    // Its `getstatic`/`putstatic` helper exits are exception sentinels
    // (a `<clinit>` the helper runs is its own activation, and a throw
    // from it comes back as a pending exception, which the caller's
    // exceptional edge releases), not deopts and not Java calls, so the
    // same predicate judges both bodies. The page's option (b),
    // decided on the artifact rather than on the tier: a superseding
    // body with any trap point is refused here and the site keeps the
    // dispatch helper from the caller's next compile, never a CALL
    // that could stop part-way (page
    // `r11w9-sync-caller-held-monitor-direct-call-patch`, "Interaction").
    if !compiled.requires_wrapped_entry || !caller_held_body_is_closed(&compiled) {
        // Round 11 wave 16 (lane `tier`): name the refusal, so a site that
        // keeps the dispatch helper is attributable from a `DBG_JITC` run
        // (the bound case prints `ir-sync-direct-call` / `sp-sync-direct-call`).
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] sync-direct REFUSED {callee_class}.{callee_method}{callee_desc}: \
                 published body not closed (ir={} deopt_points={} boxes={} calls={} \
                 wrapped={})",
                compiled.used_ir_backend,
                compiled.deopt_points.len(),
                compiled._deopt_point_boxes.len(),
                !(compiled._jit_invoke_infos.is_empty()
                    && compiled._direct_callee_entries.is_empty()
                    && compiled._jit_cycle_cells.is_empty()
                    && compiled._jit_mic_slots.is_empty()
                    && compiled._jit_pic_slots.is_empty()),
                compiled.requires_wrapped_entry,
            );
        }
        // Interpreter round i2 wave 59, lane L2: a SELF-LOCKING instance body
        // is not a caller-held target, but it is what the plain direct binder
        // binds (`self_locking_body_bind_candidate`), which ran before this
        // lookup could publish it. Answer a monitor-only target (`entry == 0`,
        // never CALLed) so the optimizing planner asks that binder again
        // (`cratonvm_jit::compile_request::self_locking_direct_bind_retry_enabled`).
        if instance
            && is_self_locking_body(&compiled)
            && cratonvm_jit::compile_request::self_locking_direct_bind_retry_enabled()
        {
            return Some((
                cratonvm_jit::compile_request::SyncDirectTarget {
                    entry: 0,
                    needs_ctx: false,
                    class_cp_idx,
                    mirror_slot: 0,
                    instance,
                    unoverridable,
                },
                compiled,
            ));
        }
        return None;
    }
    // Interpreter round i2 wave 53, lane L2: marked BEFORE the target is
    // returned, so before the caller's code that CALLs it can be published;
    // the VM then never tells it to leave on `SAFEPOINT_VERDICT_NAMED_BODY`
    // (`compiled_frame_exit_verdict`). A body with no poll exit has no use for
    // the mark; it is set for every bound body all the same (one store at
    // compile time), so no bound body can be missed.
    compiled.mark_bound_by_a_caller_held_site();
    if compiled.named_poll_exits_only && crate::runtime::env_cache::dbg_jitc() {
        // The positive control of the named-only poll exits' binding.
        eprintln!(
            "[cratonvm-jitc] sync-direct binds {callee_class}.{callee_method}{callee_desc}: \
             a named-only poll-exit body, marked caller-held"
        );
    }
    let entry = compiled.entry_ptr() as usize; // Cast: JIT entry point to address
    let needs_ctx = compiled.needs_context();
    let mirror_slot =
        sync_direct_mirror_slot(shared, class_id, class_cp_idx, declaring_id, instance);
    Some((
        cratonvm_jit::compile_request::SyncDirectTarget {
            entry,
            needs_ctx,
            class_cp_idx,
            mirror_slot,
            instance,
            unoverridable,
        },
        compiled,
    ))
}

/// The monitor half of [`sync_direct_target`] (round 14 wave 3, lane sync,
/// proposal SS-2 of `jit-r14-syncsplice-proposals-RETIRED-20260930.md`): WHICH object a
/// synchronized callee at the invoke `cp_idx` of `class_id`'s pool locks, and
/// whether that identity is safe to take, independent of any compiled body.
pub(in crate::runtime::interpreter::jit_bridge) struct SyncDirectMonitor {
    pub(in crate::runtime::interpreter::jit_bridge) callee_class: String,
    pub(in crate::runtime::interpreter::jit_bridge) callee_method: String,
    pub(in crate::runtime::interpreter::jit_bridge) callee_desc: String,
    /// The Methodref's `class_index` in the caller's pool.
    pub(in crate::runtime::interpreter::jit_bridge) class_cp_idx: u16,
    pub(in crate::runtime::interpreter::jit_bridge) method: cratonvm_reader::method::ClassFileMethod,
    pub(in crate::runtime::interpreter::jit_bridge) declaring_id: ClassId,
    pub(in crate::runtime::interpreter::jit_bridge) instance: bool,
    pub(in crate::runtime::interpreter::jit_bridge) unoverridable: bool,
}

/// See [`SyncDirectMonitor`]: every refusal [`sync_direct_target`] makes
/// before it looks at a published body.
pub(in crate::runtime::interpreter::jit_bridge) fn sync_direct_monitor(
    shared: &SharedVm,
    class_id: ClassId,
    cp: &CpResolvers<'_>,
    cp_idx: u16,
) -> Option<SyncDirectMonitor> {
    // A raw CALL skips the method-entry and monitor events JVMTI wants.
    if jvmti_requires_interpreter(shared) {
        return None;
    }
    let (callee_class, callee_method, callee_desc) = cp.invoke(cp_idx)?;
    // The Methodref's CONSTANT_Class, whose mirror the caller locks. An
    // interface method cannot be `synchronized`.
    let class_cp_idx = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        match class.constant_pool.get(cp_idx) {
            Some(ConstantPoolEntry::MethodReference { class_index, .. }) => *class_index,
            _ => return None,
        }
    };
    // FJP blocklist + native shadow, as every direct bind.
    if direct_bind_name_refusal(
        shared,
        class_id,
        &callee_class,
        &callee_method,
        &callee_desc,
    )
    .is_some()
    {
        return None;
    }
    // JVMS 2.11.10: the monitor is the DECLARING class's mirror, and
    // the caller locks the class the Methodref names. So only a method
    // that class declares itself is looked up (JVMS 5.4.3.3 selects it
    // before any superclass's); an inherited static is refused, never
    // mis-locked. No hierarchy walk is needed for that.
    //
    // Round 11 wave 11 (lane `calls`): an INSTANCE `synchronized`
    // method is answered too, with `instance: true`. Its monitor is the
    // receiver, which the caller locks (`IrBuilder::enter_receiver_monitor`);
    // the same "declared by the Methodref's own class" lookup is what
    // lets the planner check that the site's selected owner IS this
    // class, and `unoverridable` is what lets an `invokevirtual` site
    // bind it (no receiver can reach an override).
    let (method, declaring_id, initialized, class_is_final) = {
        let cm = shared.classes.class_manager.read();
        let callee_cid = jit_known_class(shared, &cm, &callee_class, class_id)?;
        let class = cm.class_store().get(callee_cid)?;
        let m = class.find_method(&callee_method, &callee_desc)?.clone();
        let initialized = crate::vm::is_class_initialized_fast(class);
        let class_is_final = class
            .access_flags
            .contains(cratonvm_reader::class_access_flags::ClassAccessFlags::FINAL);
        (m, callee_cid, initialized, class_is_final)
    };
    let instance = !method.is_static();
    if !method.is_synchronized() || (instance && &*callee_method == "<init>") {
        return None;
    }
    // JVMS 5.5: the raw CALL bypasses every init check, so a static
    // callee's class must be initialized now (monotone, so once is
    // enough). An instance callee needs no check: invoking it does not
    // initialize its class (JVMS 5.5 lists no instance invoke), and a
    // receiver of that class already exists.
    if !instance && !initialized {
        return None;
    }
    let unoverridable = instance
        && (class_is_final
            || method.access_flags.intersects(
                cratonvm_reader::class_access_flags::MethodAccessFlags::PRIVATE
                    | cratonvm_reader::class_access_flags::MethodAccessFlags::FINAL,
            ));
    Some(SyncDirectMonitor {
        callee_class,
        callee_method,
        callee_desc,
        class_cp_idx,
        method,
        declaring_id,
        instance,
        unoverridable,
    })
}

/// The class-mirror slot a caller-held static CALL (or a static synchronized
/// splice) locks through, or `0` (the `Op::ConstClass` then calls
/// `ldc_class_cp`). Outside every class-manager guard (its contract). An
/// instance callee locks its receiver, so no mirror is fetched for it.
pub(in crate::runtime::interpreter::jit_bridge) fn sync_direct_mirror_slot(
    shared: &SharedVm,
    class_id: ClassId,
    class_cp_idx: u16,
    declaring_id: ClassId,
    instance: bool,
) -> usize {
    if instance {
        return 0;
    }
    match crate::jit::helpers::compiled_ldc_slot_for(shared, class_id, class_cp_idx) {
        // Round 13 wave 10 (lane sync6, proposal S5-1): the Methodref class
        // IS the declaring class on this route ("declared by exactly that
        // class" in `sync_direct_monitor`), so its per-class mirror slot is the
        // same monitor. A caller compiled once (an OSR body) otherwise calls
        // `jit_ldc_class_cp` on every call for its life.
        // Round 13 wave 11 (lane sync7, S5-1 completed): no longer tied to
        // `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`. That switch guards the
        // DOOR's mint, which would root the mirror of every class whose
        // static synchronized method runs once; this route roots nothing
        // new: its `jit_ldc_class_cp` mints the site's own slot for the
        // same mirror on its first recorded hit (`recorded_ldc_reference`),
        // and a mutator compile mints this one only from a mirror the
        // class already has. The worker still only finds a minted slot.
        0 if true =>
        {
            crate::jit::helpers::class_mirror_slot_for(shared, declaring_id)
        }
        slot => slot,
    }
}

/// Round 14 wave 3 (lane sync, proposal SS-2): what `sync_direct_lookup`
/// answers for a `static synchronized` callee whose BODY [`sync_direct_target`]
/// refused (no published wrapped body: a self-locking one under
/// `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`, a body that is not closed, or none
/// and no compile): the monitor half alone, `entry == 0`. Every caller-held
/// CALL route binds only `entry != 0`, so nothing CALLs through this answer;
/// the optimizing planner's static synchronized splice reads its class mirror
/// (`class_cp_idx`, `mirror_slot`) and splices the callee's own bytecode
/// between the monitor pair, needing no compiled body at all. No pin: nothing
/// is bound. Instance callees are not answered (their monitor is the
/// receiver, which the splice has without a lookup).
pub(in crate::runtime::interpreter::jit_bridge) fn sync_direct_monitor_answer(
    shared: &SharedVm,
    class_id: ClassId,
    cp: &CpResolvers<'_>,
    cp_idx: u16,
) -> Option<cratonvm_jit::compile_request::SyncDirectTarget> {
    let m = sync_direct_monitor(shared, class_id, cp, cp_idx)?;
    if m.instance
        || !m
            .method
            .code()
            .is_some_and(|c| c.exception_table.is_empty())
    {
        return None;
    }
    let mirror_slot =
        sync_direct_mirror_slot(shared, class_id, m.class_cp_idx, m.declaring_id, false);
    Some(cratonvm_jit::compile_request::SyncDirectTarget {
        entry: 0,
        needs_ctx: false,
        class_cp_idx: m.class_cp_idx,
        mirror_slot,
        instance: false,
        unoverridable: false,
    })
}

/// Compile and publish the wrapped-entry body of a `synchronized` callee that
/// [`sync_direct_target`] found no published body for, so the caller being
/// compiled can bind it directly. `None` keeps the site on the dispatch helper.
///
/// Round 11 wave 17 (lane `tier`, page
/// `r11w4-sync-compiled-caller-cannot-call-a-synchronized-callee`, "Measured on
/// the wave-16 binary"): the residual slow mode of `R11X64genSyncCost`
/// `sync-method-1t` (812 ms in ~1 run of 6, `CRATONVM_C2_ACCEPT=always`). The
/// caller is `syncLoop`, called once, so its only compiled body is the OSR body,
/// compiled on the mutator at the loop's OSR threshold (1 000 back edges). The
/// callee `incSync` is nominated at its invocation threshold (~500 calls) and
/// compiled by the BACKGROUND worker. Each iteration is one back edge and one
/// call, so the two compiles are requested within ~500 iterations of each
/// other, and under `C2_ACCEPT=always` the callee's compile is an optimizing
/// one. When the OSR compile reached this lookup before the worker published
/// the callee, the lookup answered `None` ("never compiles anything"), the
/// site kept `jit_invoke_dispatch`, and an OSR body never re-binds: the whole
/// remaining loop ran the dispatch helper. The same race for a NON-synchronized
/// callee was closed by the eager callee chain
/// (`perf-splicecast-bimodal-a-racing-eager-callee-compile-leaves-a-blind-dispatch`,
/// `direct_callee_lookup_bound`); synchronized callees were never given it,
/// because the plain direct-bind door refuses them before its compile arm.
///
/// Same bounds as that chain, and the same kill switch
/// (`CRATONVM_JIT_EAGER_CALLEE_CHAIN=0`): depth, a cycle on this thread's
/// compile stack, and the per-top-level fan-out budget. Compiled through the
/// background tiering door's own function (`try_jit_compile_wrapped_entry`),
/// so the body is published exactly as that door publishes it, stamped
/// `requires_wrapped_entry`; the SINGLE-PASS backend (`optimize == false`),
/// whose `deopt_points.is_empty()` is the closedness fact
/// [`caller_held_body_is_closed`] reads, and whose compile is the cheapest. The
/// tiered manager may still supersede it later; the caller holds its own pin.
/// A compile that another thread's publish beat is answered from the cache.
pub(in crate::runtime::interpreter::jit_bridge) fn sync_direct_compile_unpublished_callee(
    shared: &SharedVm,
    declaring_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> Option<std::sync::Arc<cratonvm_jit::CompiledMethod>> {
    let declined = |why: &str| {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] sync-direct REFUSED {callee_class}.{callee_method}{callee_desc}: \
                 no published body ({why})"
            );
        }
    };
    // EAGER3-19-2: each bound is counted as the census's refusal rule.
    let refused = |why: &str, rule: usize| {
        declined(why);
        eager_chain_census_note_refused(shared, EAGER_DOOR_SYNC_DIRECT, rule);
    };
    if !crate::runtime::env_cache::jit_eager_callee_chain() {
        refused("eager callee chain off", EAGER_RULE_CHAIN_OFF);
        return None;
    }
    if cratonvm_jit::jit_active_compile_depth() > MAX_EAGER_CALLEE_CHAIN_DEPTH {
        refused("eager callee chain depth", EAGER_RULE_DEPTH);
        return None;
    }
    if cratonvm_jit::jit_active_compile_contains(callee_class, callee_method, callee_desc) {
        refused("already compiling on this thread", EAGER_RULE_CYCLE);
        return None;
    }
    // A lookup from outside any compile (the single-pass OSR door plans its
    // rows before it compiles) starts a fresh budget; a nested one spends the
    // enclosing compile's.
    eager_callee_chain_enter_top_level();
    if !eager_callee_chain_try_spend() {
        refused("eager callee chain budget", EAGER_RULE_BUDGET);
        return None;
    }
    let mut policy_declined = false;
    let body = match try_jit_compile_wrapped_entry(
        shared,
        declaring_id,
        callee_class,
        callee_method,
        callee_desc,
        false,
        &mut policy_declined,
    ) {
        Some((body, _entry, _needs_ctx)) => {
            // Round 11 wave 18, P1: published by this lookup, not by a tiered
            // task (`r11-orch-callee-compiled-bodies-are-never-promoted`).
            note_callee_publish_to_tiered_manager(
                shared,
                declaring_id,
                callee_class,
                callee_method,
                callee_desc,
                &body,
            );
            eager_chain_census_note_compiled(
                shared,
                EAGER_DOOR_SYNC_DIRECT,
                declaring_id,
                callee_method,
                callee_desc,
                Some(&body),
            );
            Some(body)
        }
        // Raced: another thread published while this compile ran. Counted as
        // declined at the compile, raced or not, as the ordinary door does.
        None => {
            eager_chain_census_note_compiled(
                shared,
                EAGER_DOOR_SYNC_DIRECT,
                declaring_id,
                callee_method,
                callee_desc,
                None,
            );
            shared
                .jit
                .jit_cache
                .read()
                .get(callee_class, callee_method, callee_desc, declaring_id)
        }
    };
    match &body {
        Some(_) if crate::runtime::env_cache::dbg_jitc() => eprintln!(
            "[cratonvm-jitc] sync-direct callee-compile {callee_class}.{callee_method}{callee_desc}"
        ),
        Some(_) => {}
        None => declined("its compile declined"),
    }
    body
}

/// `CRATONVM_JIT_SELF_LOCK_EAGER_BIND` (JIT round 19 wave 4, lane syncdoor4,
/// SYNCDOOR4-1 of `jit-r19-syncdoor4-proposals.md`; **default OFF**, `1`
/// enables): a statically bound call of a `synchronized` INSTANCE callee that
/// has NO published body yet compiles the callee's self-locking body in place
/// ([`compile_unpublished_self_locking_callee`]) and binds a raw CALL to it,
/// as the eager callee chain does for every unsynchronized callee. Before this
/// the two direct-bind doors (`try_jit_compile_callee_slow`'s
/// `direct_callee_lookup_bound` and [`compile_optimizing_artifact`]'s
/// `callee_compiler_bound`) bound only an ALREADY published self-locking body
/// ("never compiled from here"), so a caller compiled before its synchronized
/// callee -- the common order, since a caller with a loop gets hot first --
/// kept that site on `jit_invoke_dispatch` for the life of its body, about
/// 190 ns a call (`R19Syncdoor3Tables tab-*`, R17SyncDoor `tc`,
/// R18Syncdoor6Blocks `deopt`/`exc`/`prop`) against ~20 for a bound CALL. A
/// table-free callee had a way out (the caller-held route,
/// [`sync_direct_compile_unpublished_callee`]); a callee with an exception
/// table had none, because that route refuses every table (BUG-H).
///
/// Under the switch the optimizing door also remembers a METHOD-ENTRY compile
/// as a TP16-21 waiter ([`note_self_lock_late_bind_waiter`], effective only
/// under `CRATONVM_JIT_LATE_BIND_SELF_LOCK`) when the bind still fails, as the
/// background door already does. Read on the refusal only (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn self_lock_eager_bind_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SELF_LOCK_EAGER_BIND_STATIC` (JIT round 19 wave 5, lane
/// sync5, SYNC5-2 of `docs/feature-designs/jit-ir-optimizations.md`; **default on**, `1`
/// enables; effective only with [`self_lock_eager_bind_enabled`] and
/// `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`): let
/// [`compile_unpublished_self_locking_callee`] compile a `static
/// synchronized` callee too, when its class's mirror slot exists or this
/// thread may mint it (a mutator). Read on the refusal only (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn self_lock_eager_bind_static_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SELF_LOCK_OSR_LATE_BIND` (JIT round 19 wave 5, lane sync5,
/// SYNC5-3 of `docs/feature-designs/jit-ir-optimizations.md`; **default on**, `1` enables): the
/// OSR door's statically bound sites to a `synchronized` callee with no
/// self-locking body yet. See [`osr_synchronized_callee_late_bind`]. Read on
/// that refusal only (cold).
pub(in crate::runtime::interpreter::jit_bridge) fn self_lock_osr_late_bind_enabled() -> bool {
    true
}

/// SYNC5-3 ([`self_lock_osr_late_bind_enabled`]): the OSR door
/// (`compile_osr_body`'s `pending_callee_compiles`) asked
/// `try_jit_compile_callee_from_site` for the statically bound callee
/// `callee_class.callee_method callee_desc` and was refused. For a
/// `synchronized` callee that may bind a self-locking body
/// ([`self_locking_body_bind_candidate`]) the refusal is "not yet": that door
/// serves only an ALREADY published self-locking body, never compiles one,
/// and an OSR body never re-binds, so a loop whose OSR body was compiled while
/// the callee was cold calls it through `jit_invoke_dispatch` (the door, ~200
/// ns) for as long as the loop's method has no method-entry body. That is
/// `R19Syncdoor3Tables tab-catch` (first) / `tab-finally` / `tab-block`: one
/// shared caller `run`, whose OSR body was compiled in mode 0, before
/// `finisher` / `blocker` ever ran.
///
/// Two answers, in order:
/// * under `CRATONVM_JIT_SELF_LOCK_EAGER_BIND`, compile the callee's
///   self-locking body in place ([`compile_unpublished_self_locking_callee`],
///   door `osr`, with all its bounds; a cold callee declines) and bind it,
///   exactly as a published one would bind here;
/// * otherwise remember the OSR body's METHOD as a TP16-21 late-bind waiter
///   (`CRATONVM_JIT_LATE_BIND_SELF_LOCK`): the callee's self-locking
///   publication then asks a method-entry recompile of the method
///   ([`late_bind_key_served`]), whose doors bind the body, and the method's
///   next activations run it instead of re-entering the OSR body. The OSR
///   body itself is unchanged (EAGER19-4: it is never replaced).
///
/// Monitor semantics: a bound CALL here targets a published self-locking
/// body, the route this door already takes for one (`published_body_locks_itself`).
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn osr_synchronized_callee_late_bind(
    shared: &SharedVm,
    holder: ClassId,
    waiting_class: &Arc<str>,
    waiting_method: &Arc<str>,
    waiting_desc: &Arc<str>,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    if !self_lock_osr_late_bind_enabled() {
        return None;
    }
    let (declaring_id, declaring_name, is_static) = {
        let cm = shared.classes.class_manager.read();
        let callee_cid = jit_known_class(shared, &cm, callee_class, holder)?;
        let store = cm.class_store();
        let (method, declaring_id) = crate::classloading::find_method_recursive(
            callee_cid,
            callee_method,
            callee_desc,
            store,
        )?;
        let declaring = store.get(declaring_id)?;
        let initialized = crate::vm::is_class_initialized_fast(declaring);
        if !self_locking_body_bind_candidate(method, initialized) {
            return None;
        }
        (declaring_id, declaring.name.clone(), method.is_static())
    };
    let published =
        shared
            .jit
            .jit_cache
            .read()
            .get(&declaring_name, callee_method, callee_desc, declaring_id);
    if published.is_none() && self_lock_eager_bind_enabled() {
        if let Some(body) = compile_unpublished_self_locking_callee(
            shared,
            "osr",
            holder,
            callee_class,
            declaring_id,
            &declaring_name,
            callee_method,
            callee_desc,
            is_static,
        ) {
            // Cast: object/code pointer to integer address
            let entry = body.entry_ptr() as usize;
            let needs_ctx = body.needs_context();
            return Some((body, entry, needs_ctx));
        }
    }
    if published.as_deref().is_some_and(is_self_locking_body)
        || !late_bind_self_lock_enabled()
        || !late_bind_recompile_enabled(shared)
    {
        return None;
    }
    let key = self_lock_waiter_key(&declaring_name, callee_method, callee_desc);
    let waiter = LateBindWaiter {
        class_id: holder.as_u32(),
        class_name: Arc::clone(waiting_class),
        method_name: Arc::clone(waiting_method),
        descriptor: Arc::clone(waiting_desc),
    };
    if note_late_bind_waiter_entry(shared, key, waiter) && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] late-bind WAITING (osr) {waiting_class}.{waiting_method}{waiting_desc} \
             on the self-locking body of {declaring_name}.{callee_method}{callee_desc}"
        );
    }
    None
}

/// SYNCDOOR4-1 ([`self_lock_eager_bind_enabled`]): compile the method-entry
/// body of the synchronized instance method `declaring_name.callee_method
/// callee_desc` (declared by `declaring_id`, named at its site as
/// `callee_class` in `holder`'s constant pool) in place, for a direct bind that
/// found NO published body, and answer it only when it is a SELF-LOCKING body
/// a raw CALL may enter ([`is_self_locking_body`], no `invokedynamic` trap).
///
/// The compile is the background tiering's own synchronized door,
/// [`try_jit_compile_wrapped_entry`], at the single-pass tier (the only tier
/// that builds a self-locking body with an exception table), so what it
/// publishes is exactly the body the method's own tiering would publish: a
/// self-locking body when the admission and the body check pass, a wrapped
/// one otherwise (the site then keeps the dispatch helper, which serves it
/// through the door as before). Nothing about the body, its monitor protocol
/// or the CALL that binds it is new: a raw CALL to a published self-locking
/// body is what both doors already bake once it exists (round 13 wave 6, SD17-2
/// for a table). Only WHEN the site can bind changes.
///
/// The eager chain's bounds, in its order: the chain switch, the depth, a cycle
/// on this thread's compile stack, the warmth floor ([`eager_callee_chain_is_warm`]),
/// the fan-out budget, and the loader-faithful name check
/// (`jit_callee_name_is_another_loaders_copy`). A `static synchronized` callee
/// is never compiled here: its body needs the class's mirror slot, which the
/// background worker cannot mint (`request_static_self_lock_recompile`).
/// JIT round 19 wave 5 (lane sync5, SYNC5-2): except under
/// [`self_lock_eager_bind_static_enabled`], when the slot already exists (any
/// static synchronized method of the class entered through the JIT door minted
/// it) or this compile runs on a mutator, which mints it.
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter::jit_bridge) fn compile_unpublished_self_locking_callee(
    shared: &SharedVm,
    door: &str,
    holder: ClassId,
    callee_class: &str,
    declaring_id: ClassId,
    declaring_name: &str,
    callee_method: &str,
    callee_desc: &str,
    is_static: bool,
) -> Option<std::sync::Arc<cratonvm_jit::CompiledMethod>> {
    let declined = |why: &str| {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] self-lock-eager-bind DECLINED ({door}) \
                 {declaring_name}.{callee_method}{callee_desc}: {why}"
            );
        }
    };
    if is_static {
        // JIT round 19 wave 5 (lane sync5, SYNC5-2): under
        // `CRATONVM_JIT_SELF_LOCK_EAGER_BIND_STATIC` a static callee goes on
        // to the bounds below and is asked for its mirror slot after them.
        if !self_lock_eager_bind_static_enabled()
            || !crate::runtime::env_cache::jit_self_locking_sync_static()
        {
            declined("static (needs the mirror slot)");
            return None;
        }
    }
    if !crate::runtime::env_cache::jit_eager_callee_chain() {
        declined("eager callee chain off");
        return None;
    }
    if cratonvm_jit::jit_active_compile_depth() > MAX_EAGER_CALLEE_CHAIN_DEPTH {
        declined("eager callee chain depth");
        return None;
    }
    if cratonvm_jit::jit_active_compile_contains(declaring_name, callee_method, callee_desc) {
        declined("already compiling on this thread");
        return None;
    }
    if !eager_callee_chain_is_warm(shared, declaring_id, callee_method, callee_desc) {
        declined("cold");
        return None;
    }
    if !eager_callee_chain_try_spend() {
        declined("eager callee chain budget");
        return None;
    }
    if crate::jit::helpers::jit_callee_name_is_another_loaders_copy(shared, holder, callee_class) {
        declined("another loader's copy");
        return None;
    }
    // SYNC5-2: a static body locks its class mirror through the class's
    // GC-maintained mirror slot (`CompileRequest::self_lock_mirror_slot`).
    // `class_mirror_slot_for` FINDS a minted slot on any thread and MINTS one
    // only on a mutator (the background worker is no GC-cooperating thread, so
    // it may not read the mirror and publish a global reference). On the
    // worker with no slot yet the compile below would only publish a wrapped
    // body, so decline before it; the door's first entry mints the slot and
    // TP16-21 / the static recompile take it from there. Asked after every
    // other bound, so a mutator roots the mirror only for a callee this bind
    // is about to compile (the request builder would mint it there anyway).
    if is_static && crate::jit::helpers::class_mirror_slot_for(shared, declaring_id) == 0 {
        declined("static: no class mirror slot on this thread");
        return None;
    }
    let mut policy_declined = false;
    let body = match try_jit_compile_wrapped_entry(
        shared,
        declaring_id,
        declaring_name,
        callee_method,
        callee_desc,
        false,
        &mut policy_declined,
    ) {
        Some((body, _entry, _needs_ctx)) => {
            // Published by this bind, not by a tiered task (round 11 wave 18,
            // P1), so its tiering continues from C1.
            note_callee_publish_to_tiered_manager(
                shared,
                declaring_id,
                declaring_name,
                callee_method,
                callee_desc,
                &body,
            );
            body
        }
        // Raced: another thread published while this compile ran.
        None => match shared.jit.jit_cache.read().get(
            declaring_name,
            callee_method,
            callee_desc,
            declaring_id,
        ) {
            Some(body) => body,
            None => {
                declined("its compile declined");
                return None;
            }
        },
    };
    if !is_self_locking_body(&body) {
        declined("the published body is wrapped");
        return None;
    }
    if body.has_indy_trap {
        declined("indy trap");
        return None;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] self-lock-eager-bind COMPILED ({door}) \
             {declaring_name}.{callee_method}{callee_desc}"
        );
    }
    Some(body)
}

/// May a compiled caller hold this synchronized body's monitor itself around
/// a direct CALL to it (round 11 wave 10, lane `synccall`; page
/// `r11w9-sync-caller-held-monitor-direct-call-patch`, contract item 1)?
///
/// Only a body that can stop in no way other than a return or an exception
/// sentinel: the door's rule, `wrapped_body_attempt_never_reruns`, WITHOUT its
/// precise-resume escape. That escape hands the monitor to the resumed frame
/// through a door guard (`hand_monitor_to_resumed_frame`), and a caller-held
/// monitor has no guard, so here a deopt inside must be impossible rather than
/// resumable. Written as the door's predicate plus "no trap point at all" so
/// a future tightening of the door's rule reaches this one too.
///
/// Interpreter round i2 wave 53, lane L2 (the wave-25 design of
/// `docs/feature-designs/real-jdk-mode.md`):
/// or a body whose only deopt exits are poll exits on
/// `SAFEPOINT_VERDICT_NAMED_BODY` ([`caller_held_body_exits_are_named_polls`]).
/// Such an exit is taken only when the VM sets that bit, which it never does
/// for a body [`sync_direct_target`] marked before handing it out
/// (`CompiledMethod::mark_bound_by_a_caller_held_site`), so the CALL can still
/// stop only by a return or an exception sentinel.
pub(in crate::runtime::interpreter::jit_bridge) fn caller_held_body_is_closed(compiled: &crate::jit::CompiledMethod) -> bool {
    wrapped_body_attempt_never_reruns(compiled)
        && (wrapped_body_has_no_deopt_exit(compiled)
            || caller_held_body_exits_are_named_polls(compiled))
}

/// Is every deopt exit of `compiled` a back-edge poll exit that leaves only on
/// `SAFEPOINT_VERDICT_NAMED_BODY` (`CompiledMethod::named_poll_exits_only`,
/// stamped by the optimizing lowering; interpreter round i2 wave 53, lane L2)?
/// Asked again from the artifact, fail closed: each box an `OsrExit` that
/// rethrows nothing, no OSR-exit map, and the emission's own "no guard pad,
/// no rethrow pad" fact ([`wrapped_body_has_no_deopt_exit`]'s optimizing arm,
/// which the lowering reads before it emits the poll exits' pads).
pub(in crate::runtime::interpreter::jit_bridge) fn caller_held_body_exits_are_named_polls(compiled: &crate::jit::CompiledMethod) -> bool {
    compiled.used_ir_backend
        && compiled.named_poll_exits_only
        && compiled.osr_exit_points.is_empty()
        && (compiled.ir_osr_sentinel_free || compiled.ir_osr_exception_exit_bcis.is_some())
        && compiled._deopt_point_boxes.iter().all(|point| {
            point.reason == cratonvm_jit::deopt::DeoptReason::OsrExit
                && !point.semantics.rethrow_exception
        })
}

/// Can no exit of `compiled` transfer to the interpreter (a deopt), so that
/// every way it stops short of a return is an exception sentinel?
///
/// Round 11 wave 16 (lane `tier`, page
/// `r11w4-sync-compiled-caller-cannot-call-a-synchronized-callee`): the two
/// backends publish `deopt_points` with different meanings, and the test used
/// to read them as if they were one.
///
/// * **Single-pass** (`used_ir_backend == false`): a point is published for a
///   trap the body can take (`x64::driver` sets `can_deopt_resume =
///   !deopt_points.is_empty()`), so an empty list is the "no trap" fact.
/// * **Optimizing** (`used_ir_backend == true`): `build_deopt_points` publishes
///   a plain `TransferToInterpreter` resume point at EVERY safepoint snapshot,
///   unless the whole body is proven trap-free, and one `Op::LoadStatic` /
///   `Op::StoreStatic` helper (an exception-sentinel exit, not a deopt) is
///   enough to lose that proof. So the list is non-empty for nearly every
///   optimizing body, including a trap-free one such as
///   `R11X64genSyncCost.incSync`. Every production entry into this backend's
///   deopt stub is a guard pad (`emit_deopt_jcc_to_pad`) or a reason-9 rethrow
///   pad (`push_rethrow_pad`), and each bakes the address of a
///   `_deopt_point_boxes` entry, so an empty box list is the "no trap" fact
///   here. `ir_osr_exception_exit_bcis` is the emission's own answer to "no
///   guard pad" (it is `Some` only when `deopt_stub_patches` was empty before
///   the rethrow pads), asked too so that the two facts must agree; a
///   `sentinel_free` body has no exit of any kind.
///
/// Before this, the predicate answered `false` for every optimizing body. Once
/// the background tier superseded a closed single-pass body with the
/// optimizing one (`c2-supersede`), the caller-held direct CALL route
/// (`sync_direct_target`) refused the callee for every caller compiled after
/// that, and the monitored template door stopped entering the body and
/// interpreted it instead. That is the bimodal `sync-method-1t` (~80 ms when
/// the caller compiled first, ~800 ms when the supersede won the race, always
/// ~770 ms under `CRATONVM_C2_ACCEPT=always`).
///
/// OSR-exit maps (`osr_exit_points`, only under `CRATONVM_DEOPT_REAL`) are
/// another deopt exit and refuse on both backends.
pub(in crate::runtime::interpreter::jit_bridge) fn wrapped_body_has_no_deopt_exit(compiled: &crate::jit::CompiledMethod) -> bool {
    if !compiled._deopt_point_boxes.is_empty() || !compiled.osr_exit_points.is_empty() {
        return false;
    }
    if compiled.used_ir_backend {
        return compiled.ir_osr_sentinel_free || compiled.ir_osr_exception_exit_bcis.is_some();
    }
    compiled.deopt_points.is_empty()
}

/// Round 9 wave 7 (`mutrec7`): the direct-bind gates of the bg door's
/// `direct_callee_lookup`, asked for a compile-time recursion cycle's CLOSING
/// edge (`CompileRequest::cycle_edge_admission`).
///
/// That edge's target is open on the compile stack, so it never reaches a
/// `callee_compiler` and none of these gates ran for it. A
/// `cratonvm_jit::JitCycleEdgeCell` makes it a raw CALL once the target
/// publishes, so the same refusals apply, in the same order and at the same
/// moment (bake time): the FJP blocklist, a Rust native shadow, a
/// `synchronized` callee, a callee declaring its own exception table (unless
/// `direct_call_exc_table_publish_enabled`), and a static callee whose class is
/// not initialized yet (the raw CALL bypasses every init check). Cells are
/// planned for `invokestatic` only, so a non-static target is refused too.
/// `caller_class_id` is the class whose constant pool names the target.
pub(in crate::runtime::interpreter::jit_bridge) fn cycle_edge_target_admitted(
    shared: &SharedVm,
    caller_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> bool {
    if direct_bind_name_refusal(
        shared,
        caller_class_id,
        callee_class,
        callee_method,
        callee_desc,
    )
    .is_some()
    {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    let Some(callee_cid) = jit_known_class(shared, &cm, callee_class, caller_class_id) else {
        return false;
    };
    // Through `MemberResolver` (the one resolution entry point the resolve
    // guard enforces), not a raw `find_method_recursive` walk.
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let Ok(found) =
        resolver.declared_method(&cm, resolver.scope(callee_cid), callee_method, callee_desc)
    else {
        return false;
    };
    let Ok((declaring_id, method_index)) = resolver.adopt(found) else {
        return false;
    };
    // A cell is keyed by the class named in the call site's constant pool.
    // Do not plant one for an inherited static method: its compiled body is
    // published under the declaring class and could never safely fill a cell
    // keyed by the subclass.
    if declaring_id != callee_cid {
        return false;
    }
    let store = cm.class_store();
    let Some(method) = store
        .get(declaring_id)
        .and_then(|decl| decl.methods.get(method_index as usize))
    else {
        return false;
    };
    // Cells are planned for `invokestatic` only.
    if !method.is_static() {
        return false;
    }
    let declaring_class_initialized = store
        .get(declaring_id)
        .map(crate::vm::is_class_initialized_fast)
        .unwrap_or(false);
    direct_bind_method_refusal(method, declaring_class_initialized).is_none()
}

/// t1 wave 2, lane jit2 (B5, `CRATONVM_JIT_OSR_LATE_STATIC_CELL`): the class
/// id a late-bind cell of the single-pass OSR door is keyed by, or `None` when
/// the site may not take one.
///
/// The method-entry doors' gates ([`cycle_edge_target_admitted`]: name and
/// method refusals, `static`, the declaring class initialized at bake time),
/// the OSR door's own refusal of a callee it would not bind directly either
/// ([`osr_callee_bars_direct_call`]: no `Code`, or an exception table while
/// `direct_call_exc_table_publish_enabled` is off), the recursion-cycle
/// verdict the bound path asks (`direct_call_requires_dispatch_of`), and a
/// method DECLARED by the class the site names: `JitCache::put` publishes a
/// body under its declaring class, so a cell keyed by a subclass that only
/// inherits the static would never fill. No class-manager guard may be held
/// by the caller.
pub(in crate::runtime::interpreter::jit_bridge) fn osr_late_static_cell_target(
    shared: &SharedVm,
    caller_class_id: ClassId,
    callee_class: &str,
    callee_method: &str,
    callee_desc: &str,
) -> Option<ClassId> {
    let admitted = cycle_edge_target_admitted(
        shared,
        caller_class_id,
        callee_class,
        callee_method,
        callee_desc,
    );
    if !admitted
        || osr_callee_bars_direct_call(
            shared,
            caller_class_id,
            callee_class,
            callee_method,
            callee_desc,
        )
        .is_some()
    {
        return None;
    }
    let callee_cid = {
        let cm = shared.classes.class_manager.read();
        let callee_cid = jit_known_class(shared, &cm, callee_class, caller_class_id)?;
        let resolver = crate::runtime::resolve::MemberResolver::new(shared);
        let found = resolver
            .declared_method(&cm, resolver.scope(callee_cid), callee_method, callee_desc)
            .ok()?;
        let (declaring_id, _) = resolver.adopt(found).ok()?;
        if declaring_id != callee_cid {
            return None;
        }
        callee_cid
    };
    let in_cycle = jit_verdicts(shared).direct_call_requires_dispatch_of(
        callee_class,
        callee_method,
        callee_desc,
        || Some(callee_cid),
    );
    (!in_cycle).then_some(callee_cid)
}

/// Negative-result cache for [`try_jit_compile_callee`]: direct-mapped table
/// of 64-bit FNV-1a fingerprints of `(class, method, descriptor)` triples
/// whose compile attempt returned `None`.
///
/// WS1 (kafka JIT throughput): the JIT dispatch helpers
/// (`jit_invoke_virtual_mic` et al.) call `try_jit_compile_callee` on every
/// dispatch whose MIC has no compiled entry. For a callee that can never
/// compile — native-shadowed (`HashMap.get`, reflection), skip-listed,
/// FJP-blocklisted, or a backend bail — every such call re-paid the full
/// pipeline: two class-hierarchy walks with string-keyed native lookups per
/// level, `find_method_recursive`, a full `padded_bytecode` copy of the
/// method body, and a compile attempt. On call-heavy code this made JIT'd
/// dispatch dramatically slower than the interpreter. One fingerprint probe
/// replaces all of it.
///
/// Safety of staleness/collisions: a wrong "negative" answer only means the
/// callee is invoked through `invoke_or_native` (interpreted) instead of a
/// compiled entry — always correct, just slower. Recovery paths: a slot is
/// stamped with the `JitCache::generation` it was verified at, so once any
/// body is published the next probe re-checks the JIT cache (a callee compiled
/// later through the interpreter-upgrade or OSR path is picked up), and a
/// cached negative is re-verified every [`CALLEE_NEG_REPROBE_MASK`]+1'th hit
/// on each thread.
///
/// # Probe order
///
/// While the generation is unchanged, a slot hit answers BEFORE any lock:
/// nothing was published since the verdict, so no body can have appeared.
/// This used to be probed only after four shared-lock round trips (the class
/// lookup, the bail list, a second class lookup and hierarchy walk for the
/// synchronized check, the JIT-cache read), on every dispatch through a site
/// that never gets a compiled body. The bail-listed and synchronized refusals
/// are cached here too now; they were not cached at all.
///
/// Per VM ([`JitBridgeState`]): the table was a process-wide static keyed by
/// names only, so one VM's refusal withheld another VM's compile.
pub(in crate::runtime::interpreter) const CALLEE_NEG_CACHE_SLOTS: usize = 1 << 15;
pub(in crate::runtime::interpreter) const CALLEE_NEG_REPROBE_MASK: u64 = 0xFFF;
/// A slot's low bits hold the (truncated) `JitCache::generation` its verdict
/// was taken at; the rest hold the fingerprint's high bits, which the slot
/// index (the fingerprint's low bits) complements. Truncation only risks an
/// ABA after 2^24 publications between two probes of one slot, whose worst
/// case is the stale negative the re-probe already bounds.
pub(in crate::runtime::interpreter::jit_bridge) const CALLEE_NEG_GEN_MASK: u64 = (1 << 24) - 1;

/// The slot value recording "`fp` has no compiled entry, verified at
/// `generation`".
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn callee_neg_stamp(fp: u64, generation: u64) -> u64 {
    (fp & !CALLEE_NEG_GEN_MASK) | (generation & CALLEE_NEG_GEN_MASK)
}

/// Does the slot value `recorded` hold a verdict (at any generation) for `fp`?
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn callee_neg_names(recorded: u64, fp: u64) -> bool {
    recorded != 0 && (recorded & !CALLEE_NEG_GEN_MASK) == (fp & !CALLEE_NEG_GEN_MASK)
}

thread_local! {
    /// Negative-cache hits on this thread, pacing the periodic re-probe. A
    /// thread-local, not a shared counter: every hit used to be a
    /// `fetch_add` on one process-wide cache line.
    pub(in crate::runtime::interpreter::jit_bridge) static CALLEE_NEG_HITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Count one negative-cache hit; `true` when this one should re-run the full
/// pipeline instead (the first hit, then every [`CALLEE_NEG_REPROBE_MASK`]+1'th),
/// so a stale negative (a transient compile failure that would succeed now)
/// cannot pin a hot callee to the interpreter forever.
#[inline]
pub(in crate::runtime::interpreter::jit_bridge) fn callee_neg_hit_is_reprobe() -> bool {
    CALLEE_NEG_HITS.with(|hits| {
        let n = hits.get();
        hits.set(n.wrapping_add(1));
        n & CALLEE_NEG_REPROBE_MASK == 0
    })
}

pub(in crate::runtime::interpreter) fn callee_neg_fingerprint(class_name: &str, method_name: &str, descriptor: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for part in [class_name, method_name, descriptor] {
        for &b in part.as_bytes() {
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            h = (h ^ b as u64).wrapping_mul(FNV_PRIME);
        }
        // Separator so ("AB","C") and ("A","BC") fingerprint differently.
        h = (h ^ 0xff).wrapping_mul(FNV_PRIME);
    }
    // 0 is the table's "empty slot" sentinel.
    if h == 0 {
        1
    } else {
        h
    }
}

/// The negative-cache key of one probe of the by-name callee door: the
/// name-only [`callee_neg_fingerprint`], with the callee class id folded in
/// when the caller supplied one. Without it a refusal recorded for one
/// loader's copy of a class name answered for every other loader's copy
/// (`i1-L7-callee-compile-doors-are-loader-blind`). An unhinted probe (the
/// plain by-name [`try_jit_compile_callee`]) keeps the name-only key.
pub(in crate::runtime::interpreter) fn callee_neg_fingerprint_hinted(
    class_id_hint: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> u64 {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let fp = callee_neg_fingerprint(class_name, method_name, descriptor);
    match class_id_hint.map(|id| id.as_u32()).filter(|&id| id != 0) {
        None => fp,
        Some(id) => {
            let h = (fp ^ u64::from(id)).wrapping_mul(FNV_PRIME);
            // 0 is the table's "empty slot" sentinel.
            if h == 0 {
                1
            } else {
                h
            }
        }
    }
}

/// Compile a callee method by name, storing it in the JIT cache.
/// Called from `jit_invoke_dispatch` when a callee becomes hot.
/// Returns (entry_ptr, needs_context) on success.
///
/// wire-tiered-manager Step 3 — `optimize` selects the backend: inline
/// JIT-dispatch callers pass `true` (the optimizing C2-equivalent backend, the
/// historical behaviour); the background tiered compile worker passes the
/// C1/C2 value derived from the task's target tier
/// ([`crate::jit::tiered::tier_uses_optimized_backend`]). `false` routes to the
/// fast single-pass C1 backend. NOTE: the JIT-cache probe below returns any
/// already-published body regardless of `optimize`, so a method is compiled at
/// whatever tier reaches it *first* — there is no C1→C2 re-compile/supersede yet
/// (that needs safe code-cache replacement; tracked as a follow-up).
///
/// # Why this returns the artifact and not just its entry address
///
/// [`JitCache::put`] REPLACES the body stored under a key. The superseded
/// artifact's last `Arc` therefore drops, and `ExecutableBuffer::drop` unmaps
/// its code. A caller holding only `entry_ptr()` is racing exactly that: every
/// caller here either CALLs the address, publishes it into an inline cache, or
/// bakes it into generated code, and all three keep using it long after this
/// function returns. Handing back the `Arc` makes the address valid for as long
/// as the caller keeps the binding alive — and, because
/// `resolve_jit_entry_owner` can only succeed while the artifact lives, it is
/// also what lets the inline-cache publications inside that window take their
/// own keep-alive.
pub fn try_jit_compile_callee(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    try_jit_compile_callee_hinted(
        shared,
        None,
        class_name,
        method_name,
        descriptor,
        optimize,
        CalleeDoorMiss::MayDefer,
    )
}

/// [`try_jit_compile_callee`] for a call site that knows the class its own
/// code belongs to (`JitInvokeInfo::declaring_class_id`): `class_name` is
/// resolved through THAT class's loader, so a callee class name two loaders
/// define compiles the copy this site actually calls instead of being refused
/// as ambiguous (`i1-L7-callee-compile-doors-are-loader-blind`). A zero or
/// unresolvable site class falls back to the by-name rule.
pub fn try_jit_compile_callee_from_site(
    shared: &SharedVm,
    site_class_id: u32,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    try_jit_compile_callee_from_site_with(
        shared,
        site_class_id,
        class_name,
        method_name,
        descriptor,
        optimize,
        CalleeDoorMiss::MayDefer,
    )
}

/// [`try_jit_compile_callee_from_site`] that always compiles in place, never
/// handing the compile to the worker under
/// `CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER`: the OSR door's binds
/// ([`compile_osr_body`]), whose body is built once and keeps what it binds
/// (interpreter round i4 wave 93, lane L7; [`callee_door_defers_now`]).
pub(in crate::runtime::interpreter) fn try_jit_compile_callee_from_site_in_place(
    shared: &SharedVm,
    site_class_id: u32,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    try_jit_compile_callee_from_site_with(
        shared,
        site_class_id,
        class_name,
        method_name,
        descriptor,
        optimize,
        CalleeDoorMiss::CompileInPlace,
    )
}

/// [`try_jit_compile_callee_from_site`], with the door's answer on a miss
/// ([`CalleeDoorMiss`]).
pub(in crate::runtime::interpreter::jit_bridge) fn try_jit_compile_callee_from_site_with(
    shared: &SharedVm,
    site_class_id: u32,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
    on_miss: CalleeDoorMiss,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    let hint = if site_class_id == 0 {
        None
    } else {
        shared
            .classes
            .class_manager
            .read()
            .find_class_by_name_for_class(class_name, ClassId::new(site_class_id))
    };
    try_jit_compile_callee_hinted(
        shared,
        hint,
        class_name,
        method_name,
        descriptor,
        optimize,
        on_miss,
    )
}

/// [`try_jit_compile_callee`] for a caller that already holds the callee
/// class's own identity: a receiver's class id (the MIC/PIC and virtual
/// dispatch-helper doors), or the class a loader-faithful lookup answered (the
/// eager direct bind, the first-call door). The id is honoured only while it
/// names a loaded class called `class_name`, and the method is then resolved
/// from THAT class, so a class name two loaders define compiles this copy and
/// is probed, negative-cached and published under its id rather than refused
/// as ambiguous or answered with another loader's body
/// (`i1-L7-callee-compile-doors-are-loader-blind`).
pub fn try_jit_compile_callee_for_class(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    try_jit_compile_callee_for_class_on_miss(
        shared,
        class_id,
        class_name,
        method_name,
        descriptor,
        optimize,
        CalleeDoorMiss::MayDefer,
    )
}

/// [`try_jit_compile_callee_for_class`] with the door's answer on a miss
/// chosen by the caller ([`CalleeDoorMiss`]): the virtual dispatch counter's
/// crossing passes [`CalleeDoorMiss::ProbeOnly`] when its compile is left to
/// the tiered manager (j27 wave 3, lane DISPATCH2).
pub(crate) fn try_jit_compile_callee_for_class_on_miss(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
    on_miss: CalleeDoorMiss,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    try_jit_compile_callee_hinted(
        shared,
        Some(class_id),
        class_name,
        method_name,
        descriptor,
        optimize,
        on_miss,
    )
}

/// [`try_jit_compile_callee`] with the callee class's identity when the caller
/// has it. The hint is honoured only while it names a loaded class called
/// `class_name` (see [`try_jit_compile_callee_slow`]'s `class_id_hint`).
/// `on_miss`: what the door does when no published body serves the caller
/// ([`CalleeDoorMiss`]).
pub(in crate::runtime::interpreter::jit_bridge) fn try_jit_compile_callee_hinted(
    shared: &SharedVm,
    class_id_hint: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
    on_miss: CalleeDoorMiss,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    // Interpreter round i4 wave 91, lane L4: the finder census's JIT door
    // time (`CRATONVM_DBG=mh-finder-census`; one flag read otherwise).
    let _census_door = cratonvm_native_builtins::mh_finder_census::work_clock(
        cratonvm_native_builtins::mh_finder_census::Work::JitDoor,
    );
    // Interpreter round i6 wave 117, lane L2
    // (`CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`): a re-entry-only method is
    // never compiled FOR a caller (the body could not be handed out anyway,
    // and a probe miss here would recompile it at every ask). One load while
    // nothing is marked.
    if shared
        .jit
        .jit_cache
        .is_reentry_only_key(class_name, method_name, descriptor)
    {
        return None;
    }
    // The mutator-thread compile door, with its panics contained the way the
    // background workers' are (`cratonvm_jit::tiered::contain_compile_panic`).
    // Without this a codegen panic unwound through the interpreter frame that
    // asked for the callee.
    match cratonvm_jit::tiered::contain_compile_panic(|| {
        try_jit_compile_callee_uncontained(
            shared,
            class_id_hint,
            class_name,
            method_name,
            descriptor,
            optimize,
            on_miss,
        )
    }) {
        // Interpreter round i6 wave 117, lane L2
        // (`CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`): every caller of this
        // funnel binds what it answers (a compiled caller's dispatch, the
        // interpreter's invoke caches, inline caches), so a re-entry-only body
        // is never answered: the call keeps reaching the registered native.
        Ok(compiled) => compiled.filter(|(body, _, _)| !body.reentry_only),
        Err(payload) => {
            // The verdict store is keyed per loaded class; resolve the name the
            // way the probe in `try_jit_compile_callee_uncontained` does. Only
            // on this (rare) panic arm, with no class-manager guard held.
            let class_id = {
                let cm = shared.classes.class_manager.read();
                callee_probe_class_id(&cm, class_id_hint, class_name)
            };
            note_contained_mutator_compile_panic(
                shared,
                class_id,
                class_name,
                method_name,
                descriptor,
                &*payload,
            );
            None
        }
    }
}

/// A compile panic caught on a mutator thread: bail-list the method so no door
/// asks for it again, count it under the workers' `worker_panic` scheduling
/// event, and warn once per process.
pub(in crate::runtime::interpreter::jit_bridge) fn note_contained_mutator_compile_panic(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    payload: &(dyn std::any::Any + Send),
) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    jit_verdicts(shared).mark_bail_listed(class_id, class_name, method_name, descriptor);
    cratonvm_jit::metrics::record_scheduling_event(cratonvm_jit::metrics::SCHEDULING_EVENTS[6]);
    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            target: "cratonvm::jit",
            "compile of {class_name}.{method_name}{descriptor} panicked on a mutator thread and \
             was contained: {}. The method is bail-listed and keeps running interpreted; later \
             contained panics are counted under the `worker_panic` scheduling event.",
            cratonvm_jit::tiered::panic_payload_message(payload),
        );
    }
}

/// The class id the by-name callee door probes and records verdicts under: the
/// hint while it names a loaded class called `class_name`, otherwise whatever
/// the name resolves to (`0` when nothing does).
pub(in crate::runtime::interpreter::jit_bridge) fn callee_probe_class_id(
    cm: &crate::classloading::ClassManager,
    class_id_hint: Option<ClassId>,
    class_name: &str,
) -> ClassId {
    class_id_hint
        .filter(|&id| id.as_u32() != 0 && cm.get_class(id).is_some_and(|c| &*c.name == class_name))
        .or_else(|| cm.get_loaded_class_id(class_name))
        .unwrap_or(ClassId::new(0))
}

/// The stored name of the HIDDEN class `class_id_hint` when `class_name` is
/// that class's own class-file name as its constant pool spells it
/// (`ClassManager::hidden_self_cp_answer`, which also honours
/// `CRATONVM_HIDDEN_SELF_CP_BY_ID`), under `CRATONVM_JIT_HIDDEN_SELF_HINT_BY_ID`
/// (JIT r22 wave 2, lane inline; perf, opt-in). `None` for every other hint,
/// and whenever [`callee_probe_class_id`] already accepts the hint by name.
/// The switch is read only on that rare arm (a hint whose stored name differs
/// from the asked name), never on the door's common path.
pub(in crate::runtime::interpreter::jit_bridge) fn callee_probe_hidden_self_name(
    cm: &crate::classloading::ClassManager,
    hidden_bits: &cratonvm_classloading::HiddenClassBits,
    class_id_hint: Option<ClassId>,
    class_name: &str,
) -> Option<String> {
    let id = class_id_hint.filter(|id| id.as_u32() != 0)?;
    let class = cm.get_class(id)?;
    if &*class.name == class_name || !class.hidden {
        return None;
    }
    // Wave 3 (item 3 of `r22-review-w3-intrinsics-on-inline-FIXED-20261020.md`):
    // the per-VM latch, one relaxed load after the first read.
    if !hidden_bits.jit_self_hint_by_id() {
        return None;
    }
    (cm.hidden_self_cp_answer(class_name, id) == Some(id)).then(|| class.name.to_string())
}

/// [`try_jit_compile_callee`] without panic containment.
pub(in crate::runtime::interpreter::jit_bridge) fn try_jit_compile_callee_uncontained(
    shared: &SharedVm,
    class_id_hint: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
    on_miss: CalleeDoorMiss,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    use std::sync::atomic::Ordering;
    // Kill-switch: CRATONVM_DISABLE_JIT=1 forces interpreter-only execution.
    // Useful for bisecting JIT-vs-interpreter bugs during bootstrap crashes.
    if crate::runtime::env_cache::disable_jit() || compile_admission_closed_for_capture() {
        return None;
    }
    // Negative-cache fast path, before any lock (see `CALLEE_NEG_CACHE_SLOTS`).
    // The generation is read BEFORE every probe below and stamped on every
    // verdict this call records, so a publication after this read moves it and
    // the verdict cannot outlive the body that publication added.
    let neg_slots = shared.jit.bridge.callee_neg_slots();
    let fp = callee_neg_fingerprint_hinted(class_id_hint, class_name, method_name, descriptor);
    // Widening: small integer index -> usize (non-negative, fits in pointer width)
    let slot = &neg_slots[(fp as usize) & (CALLEE_NEG_CACHE_SLOTS - 1)];
    // `CRATONVM_JIT_CALLEE_NEG_KEYED_STAMP` (interpreter round i4 wave 92,
    // lane L7; default off): the verdict is stamped with the publications of
    // THIS `(method, descriptor)` (any class) plus flushes and redefinitions
    // (`JitCache::callee_publish_stamp`), not with the cache-wide generation,
    // which a define burst moves on every retirement and every other
    // method's publication. A stale-stamped verdict only re-runs the cheap
    // checks below (class lookup, bail list, synchronized walk, cache probe)
    // and is re-stamped unless a periodic re-probe is due, so the slow
    // compile path runs exactly as often either way; the checks can only
    // turn the answer positive through a publication of this name (the cache
    // probe, a declaring class's self-locking body), which moves this stamp.
    let neg_generation = if callee_neg_keyed_stamp_enabled(shared) {
        shared
            .jit
            .jit_cache
            .callee_publish_stamp(method_name, descriptor)
    } else {
        shared.jit.jit_cache.generation()
    };
    let neg_stamp = callee_neg_stamp(fp, neg_generation);
    let recorded = slot.load(Ordering::Relaxed);
    let neg_hit = callee_neg_names(recorded, fp);
    let neg_reprobe = neg_hit && callee_neg_hit_is_reprobe();
    if recorded == neg_stamp && !neg_reprobe {
        callee_probe_tally("NEG-CACHE", class_name, method_name);
        return None;
    }
    // The class this door probes, publishes and records verdicts under,
    // resolved once, up front, because the bail-list below is kept per loaded
    // class too. A caller that knows the callee class's identity
    // (`class_id_hint`: the receiver's class, the call site's loader's answer,
    // the background task's key) probes under THAT id, so a name two loaders
    // define is looked up as the copy the caller means; the plain by-name
    // door (`try_jit_compile_callee`, no hint) resolves the name globally, and
    // `try_jit_compile_callee_slow` records under the class the name resolves
    // to uniquely, which this lookup then names too.
    // JIT r22 wave 2, lane inline (`CRATONVM_JIT_HIDDEN_SELF_HINT_BY_ID`,
    // opt-in; "Left open" of `r21-hidden-self-cp-resolves-to-same-named-class`):
    // a hint naming a HIDDEN class whose constant pool spells its own
    // class-file name is that class (JVMS 5.4.3.1), so the door probes it under
    // its id AND its stored name, instead of falling to the by-name lookup that
    // answers a same-named ordinary class (which
    // `jit_callee_name_is_another_loaders_copy` then has to refuse).
    let (probe_class_id, hidden_self_name) = {
        let cm = shared.classes.class_manager.read();
        (
            callee_probe_class_id(&cm, class_id_hint, class_name),
            callee_probe_hidden_self_name(
                &cm,
                &shared.classes.hidden_classes,
                class_id_hint,
                class_name,
            ),
        )
    };
    let (probe_class_id, class_name): (ClassId, &str) = match hidden_self_name.as_deref() {
        Some(stored) => (class_id_hint.unwrap_or(probe_class_id), stored),
        None => (probe_class_id, class_name),
    };
    // RBC.4 — short-circuit permanently-uncompilable methods before the
    // FJP/native-shadow hierarchy walks (see try_jit_upgrade_with_gate).
    if jit_verdicts(shared).is_bail_listed(probe_class_id, class_name, method_name, descriptor) {
        if callee_probe_dbg() {
            // Carry the recorded refusal site into the tally key: "bail-listed"
            // alone says only that some earlier compile said no, and the
            // whole point of the tally is to name what has to be fixed.
            let why = jit_verdicts(shared)
                .bail_reason_for(probe_class_id, class_name, method_name, descriptor)
                .unwrap_or_else(|| "reason-not-recorded".to_string());
            callee_probe_note(
                &format!("BAIL-LISTED[{why}]"),
                class_name,
                method_name,
                descriptor,
            );
        }
        slot.store(neg_stamp, Ordering::Relaxed);
        return None;
    }
    // This API hands a raw entry pointer to direct dispatchers. Even if the
    // method has already been compiled for the ordinary interpreter entry,
    // those call sites have no implicit ACC_SYNCHRONIZED monitor wrapper.
    // Keep them on the generic invocation path; background tiering uses the
    // separate wrapped-entry helper below. Negative-cached like any other
    // refusal: the access flag cannot change without a redefinition, which
    // flushes the JIT cache and so moves the generation.
    // Round 13 wave 4 (lane sync2): a published SELF-LOCKING body takes its
    // own monitor, so the cache probe below may serve it (it re-checks
    // `requires_wrapped_entry`); the compile path keeps refusing.
    if named_method_is_synchronized(shared, probe_class_id, class_name, method_name, descriptor)
        && !published_body_locks_itself(shared, probe_class_id, class_name, method_name, descriptor)
    {
        // JIT round 16 (lane throwpath, TP-1): an INHERITED synchronized
        // method's self-locking body is published under its DECLARING class,
        // never under the receiver class this door probes, so the check above
        // missed it for every subclass receiver and the door refused -- the
        // `RuntimeException` receiver of `Throwable.fillInStackTrace()` sent
        // every compiled `new X(..)` down the by-name `invoke_or_native` tail.
        if let Some(found) = declaring_class_self_locking_body(
            shared,
            probe_class_id,
            class_name,
            method_name,
            descriptor,
        ) {
            if callee_neg_names(slot.load(Ordering::Relaxed), fp) {
                slot.store(0, Ordering::Relaxed);
            }
            return Some(found);
        }
        if callee_probe_dbg() {
            callee_probe_note("SYNCHRONIZED", class_name, method_name, descriptor);
        }
        slot.store(neg_stamp, Ordering::Relaxed);
        return None;
    }
    // JIT-cache probe. Deliberately BEFORE the negative cache so a method
    // compiled later through another path (interpreter invocation-count
    // upgrade, OSR) is returned even when an earlier attempt through this
    // function negative-cached. `JitCache::get` takes `&str` directly —
    // the previous `Arc::from` per name was three wasted heap allocations
    // on every dispatch-helper call.
    {
        // `probe_class_id` was resolved at the top of this function.
        // No `class_was_redefined` gate: a redefinition withdraws every body
        // that depends on the redefined class (all of them, or since wave 20
        // only its dependents, `JitRealm::note_class_redefinition_of`), so any
        // entry still present in the cache was compiled AFTER the most recent
        // redefinition of every class it depends on. Refusing the lookup
        // because the class was once redefined
        // permanently withheld compiled code from that class -- it is what
        // kept a mocked class interpreted forever.
        let jit_cache = shared.jit.jit_cache.read();
        if let Some(compiled) = jit_cache.get(class_name, method_name, descriptor, probe_class_id) {
            // A published body is not automatically a body THIS caller may
            // enter. `try_jit_compile_callee` is the by-name entry point for
            // the UNWRAPPED direct-call doors, and its slow path refuses an
            // `ACC_SYNCHRONIZED` callee for a reason that does not stop being
            // true once the body already exists: the compiled code carries no
            // monitor prologue, so a raw CALL to it simply does not lock.
            //
            // The background tiering door publishes synchronized bodies on
            // purpose — `try_jit_compile_wrapped_entry` — because
            // `execute_jit_call` wraps them. Serving one from here handed the
            // wrapped-entry body to a caller that supplies no monitor, which is
            // how `RSyncMethodJit`'s `static synchronized bumpStatic` lost
            // ~35 of 240 000 increments per run.
            if compiled.requires_wrapped_entry {
                cratonvm_jit::note_direct_callee_bind_refusal(
                    cratonvm_jit::DirectBindRefusal::Synchronized,
                );
                return None;
            }
            // Cast: object/code pointer to integer address
            let entry = compiled.entry_ptr() as usize; // Cast: JIT entry point to address
            let needs_ctx = compiled.needs_context();
            return Some((compiled, entry, needs_ctx));
        }
        // DIAG (`CRATONVM_DBG_CALLEE_PROBE=1`): the probe above requires an
        // EXACT `declaring_class_id`. Report the miss UNCONDITIONALLY, with the
        // exact strings — an empty `ids` list is as informative as a populated
        // one (it separates "wrong id" from "the name never matches at all").
        if callee_probe_dbg() {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n < 25 {
                let ids = jit_cache.debug_ids_for(class_name, method_name, descriptor);
                eprintln!(
                    "[callee-probe] CACHE-MISS class={class_name:?} method={method_name:?} \
                     desc={descriptor:?} probe_id={} ids_in_cache={:?}",
                    probe_class_id.as_u32(),
                    ids
                );
            }
            callee_probe_tally("CACHE-MISS", class_name, method_name);
        }
    }
    // A verdict from an older generation: the JIT-cache probe above has just
    // shown it still holds, so re-stamp it for the lock-free path and answer.
    // A due re-probe falls through and re-runs the full pipeline instead.
    if neg_hit && !neg_reprobe {
        slot.store(neg_stamp, Ordering::Relaxed);
        callee_probe_tally("NEG-CACHE", class_name, method_name);
        return None;
    }
    // j27 wave 3 (lane DISPATCH2): a probe-only caller stops at the miss,
    // WITHOUT a negative verdict: the stamp is shared with every caller of
    // this door, and a verdict recorded here would answer the next
    // compiling caller `None` before it deferred or compiled.
    if !on_miss.may_compile() {
        callee_probe_tally("PROBE-ONLY-MISS", class_name, method_name);
        return None;
    }
    // `CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER` (interpreter round i4 wave
    // 93, lane L7; default ON, WARNING: tomcat-defer-default-flip): under the background compiler this door was
    // the one mutator-side door that still compiled in place -- the tier-up
    // doors and the eager first-call door all enqueue -- because its callers
    // (the compiled-code dispatch helpers) want an entry NOW. Its `None` is
    // their ordinary fallback (the interpreter), so hand the compile to the
    // worker and answer that. The verdict is negative-cached like any other:
    // the worker's publication moves the stamp (the cache-wide generation, or
    // this name's bucket under the keyed stamp), and the cache probe above
    // then answers the body.
    if probe_class_id.as_u32() != 0
        && callee_door_defers_now(shared, on_miss.may_defer())
        && callee_compile_may_be_deferred(shared, probe_class_id, method_name, descriptor)
    {
        defer_callee_compile_to_worker(
            shared,
            probe_class_id,
            class_name,
            method_name,
            descriptor,
            callee_door_deferred_optimize(optimize, callee_door_c1_first_enabled(shared)),
        );
        slot.store(neg_stamp, Ordering::Relaxed);
        return None;
    }
    let mut cache_negative = true;
    // The by-name door negative-caches every cacheable `None` alike; it has no
    // retry budget for a policy verdict to spare.
    let mut policy_declined = false;
    let res = try_jit_compile_callee_slow(
        shared,
        class_id_hint,
        class_name,
        method_name,
        descriptor,
        optimize,
        &mut cache_negative,
        &mut policy_declined,
        false,
        false,
    );
    if res.is_none() && callee_probe_dbg() {
        callee_probe_note("SLOW-PATH-NONE", class_name, method_name, descriptor);
    }
    match res {
        None if cache_negative => slot.store(neg_stamp, Ordering::Relaxed),
        // A re-probe that succeeded — drop the stale negative entry.
        Some(_) if callee_neg_names(slot.load(Ordering::Relaxed), fp) => {
            slot.store(0, Ordering::Relaxed)
        }
        _ => {}
    }
    res
}

/// Is the callee-probe diagnostic on? (`CRATONVM_DBG_CALLEE_PROBE=1`)
///
/// `try_jit_compile_callee` has four independent ways to answer `None`, and
/// they were indistinguishable from the outside — which is what made
/// "`hit_entry=0` forever" unfalsifiable. Each now names itself.
pub(in crate::runtime::interpreter::jit_bridge) fn callee_probe_dbg() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)
}

pub(in crate::runtime::interpreter::jit_bridge) fn callee_probe_note(why: &str, class_name: &str, method_name: &str, descriptor: &str) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 25 {
        eprintln!("[callee-probe] {why} {class_name}.{method_name}{descriptor}");
    }
    callee_probe_tally(why, class_name, method_name);
}

/// Per-`(reason, callee)` tally behind the same flag as [`callee_probe_note`].
///
/// The first-25 sample above is spent entirely on class loading before the
/// workload's own code runs, so it cannot answer the question the counter
/// `pub_probe_none` raises — *which* callees are refused, and for which of the
/// four reasons. Bounded to [`CALLEE_PROBE_TALLY_CAP`] distinct keys so a
/// pathological run cannot grow it without limit.
pub(in crate::runtime::interpreter::jit_bridge) fn callee_probe_tally(why: &str, class_name: &str, method_name: &str) {
    if !callee_probe_dbg() {
        return;
    }
    const CALLEE_PROBE_TALLY_CAP: usize = 4096;
    let tally = CALLEE_PROBE_TALLY
        .get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let mut guard = tally.lock();
    let key = format!("{why} {class_name}.{method_name}");
    if guard.len() >= CALLEE_PROBE_TALLY_CAP && !guard.contains_key(&key) {
        return;
    }
    *guard.entry(key).or_insert(0) += 1;
}

#[allow(clippy::type_complexity)]
pub(in crate::runtime::interpreter::jit_bridge) static CALLEE_PROBE_TALLY: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::OnceLock::new();

/// Report a callee that only compiles because the generic-metadata scan reads
/// the DECLARING class's constant pool (see the call site in
/// `try_jit_compile_callee_slow`). Each distinct callee is printed ONCE, the
/// first time it is admitted.
///
/// Printed eagerly rather than accumulated into the tally above for two
/// reasons: the tally's dump is truncated to its 30 hottest rows, and the run
/// this exists to diagnose is one that fails -- possibly by aborting before any
/// end-of-run dump would happen. The line format is `class.method` first so the
/// output feeds `CRATONVM_JIT_DENY` (a substring match on `class.method`)
/// directly.
pub(in crate::runtime::interpreter::jit_bridge) fn note_newly_admitted_callee(class_name: &str, method_name: &str, descriptor: &str) {
    const NEWLY_ADMITTED_CAP: usize = 8192;
    static SEEN: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()));
    let key = format!("{class_name}.{method_name}");
    let mut guard = seen.lock();
    if guard.len() >= NEWLY_ADMITTED_CAP || !guard.insert(key.clone()) {
        return;
    }
    eprintln!("[callee-probe] NEWLY-ADMITTED {key} {descriptor}");
}

/// Dump the [`callee_probe_tally`] histogram, hottest first. Called from the
/// `mic-prof` dump so one run answers both "how often does the inline cache
/// fail to hold an entry" and "for which callees, and why".
pub(crate) fn dump_callee_probe_tally() {
    if !callee_probe_dbg() {
        return;
    }
    let Some(tally) = CALLEE_PROBE_TALLY.get() else {
        return;
    };
    let mut rows: Vec<(String, u64)> = tally.lock().iter().map(|(k, v)| (k.clone(), *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!("[callee-probe] tally ({} distinct keys)", rows.len());
    for (key, count) in rows.iter().take(30) {
        eprintln!("[callee-probe]   {count:>10} {key}");
    }
}

/// Is the method the callee door would bind `ACC_SYNCHRONIZED`? Asked of
/// `probe_class_id` — the class the door probes and publishes under
/// ([`callee_probe_class_id`]) — when it names one, so a name two loaders
/// define is answered for the copy the site calls rather than whichever one
/// the name happens to resolve to; `0` falls back to the name.
pub(in crate::runtime::interpreter) fn named_method_is_synchronized(
    shared: &SharedVm,
    probe_class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let cm = shared.classes.class_manager.read();
    let class_id = if probe_class_id.as_u32() != 0 {
        probe_class_id
    } else {
        match cm.get_loaded_class_id(class_name) {
            Some(id) => id,
            None => return false,
        }
    };
    let store = cm.class_store();
    crate::classloading::find_method_recursive(class_id, method_name, descriptor, store)
        .is_some_and(|(method, _)| method.is_synchronized())
}

/// Round 13 wave 4 (lane sync2): is the body published under this key a
/// self-locking one (`CompiledMethod::self_locks_monitor`), which a raw door
/// may CALL? The refusal above stays for every wrapped body and for the
/// compile path (`try_jit_compile_callee_slow` refuses a synchronized method
/// unless `allow_synchronized_wrapped_entry`).
pub(in crate::runtime::interpreter::jit_bridge) fn published_body_locks_itself(
    shared: &SharedVm,
    probe_class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    shared
        .jit
        .jit_cache
        .read()
        .get(class_name, method_name, descriptor, probe_class_id)
        .is_some_and(|compiled| compiled.self_locks_monitor && !compiled.requires_wrapped_entry)
}

/// JIT round 16 (lane throwpath, TP-1): the self-locking body of the
/// synchronized method the door resolves from `probe_class_id`, when that
/// method is INHERITED -- declared on a superclass, whose key (declaring class
/// name and id) is the one background tiering publishes the body under. The
/// probe key names the receiver's class, so [`published_body_locks_itself`]
/// never finds such a body, and the door refused every subclass receiver.
///
/// Measured (`R16ThrowpathLoop newfull`, sampled;
/// `docs/feature-designs/real-jdk-mode.md`): about half of a
/// compiled `new RuntimeException(..)` was that refusal's by-name tail
/// (`invoke_or_native` -> `invoke_on_class_shared_inner` -> the interpreter
/// entering the very body this returns).
///
/// The same body the by-name tail enters: the declaring class is the one
/// `find_method_recursive` selects from the receiver's own class id (exact,
/// so loader-faithful), and the body is the one published for that exact
/// class id. Only a SELF-LOCKING body (`self_locks_monitor`, not a wrapped
/// entry) is answered, as the probe-key rule does. `None` for a method
/// declared on the probe class itself (the probe key already answered it),
/// and when `CRATONVM_JIT_SYNC_CALLEE_DECLARING_BODY` is off (default ON since
/// round 16, measured: throwable new-only 5094→1997 ns, cce 7704→4922 ns,
/// caller-catch 5841→2555 ns; `=0`/`false`/`off`/`no` is the refusal as
/// before). Read only on this refusal path (a door publishes what it gets),
/// not memoised.
pub(in crate::runtime::interpreter::jit_bridge) fn declaring_class_self_locking_body(
    shared: &SharedVm,
    probe_class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    declaring_class_self_locking_body_unswitched(
        shared,
        probe_class_id,
        class_name,
        method_name,
        descriptor,
    )
}

/// [`declaring_class_self_locking_body`] without its switch.
pub(in crate::runtime::interpreter::jit_bridge) fn declaring_class_self_locking_body_unswitched(
    shared: &SharedVm,
    probe_class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    let (declaring_id, declaring_name) = {
        let cm = shared.classes.class_manager.read();
        let class_id = if probe_class_id.as_u32() != 0 {
            probe_class_id
        } else {
            cm.get_loaded_class_id(class_name)?
        };
        // Through the resolver (the hierarchy walk, memoised per VM), as
        // the walk4 lane moved the other lookup-only doors
        // (`runtime::resolve::guard`).
        let resolver = crate::runtime::resolve::MemberResolver::new(shared);
        let found = resolver
            .declared_method(&cm, resolver.scope(class_id), method_name, descriptor)
            .ok()?;
        let (declaring_id, method_index) = resolver.adopt(found).ok()?;
        let declaring = cm.class_store().get(declaring_id)?;
        let method = declaring.methods.get(method_index as usize)?;
        if !method.is_synchronized() || declaring_id == class_id {
            return None;
        }
        (declaring_id, std::sync::Arc::clone(&declaring.name))
    };
    let compiled =
        shared
            .jit
            .jit_cache
            .read()
            .get(&declaring_name, method_name, descriptor, declaring_id)?;
    if !compiled.self_locks_monitor || compiled.requires_wrapped_entry {
        return None;
    }
    if callee_probe_dbg() {
        callee_probe_note(
            "DECLARING-SELF-LOCKING",
            class_name,
            method_name,
            descriptor,
        );
    }
    // Cast: JIT entry point to address, as the cache probe's own arm.
    let entry = compiled.entry_ptr() as usize;
    let needs_ctx = compiled.needs_context();
    Some((compiled, entry, needs_ctx))
}

/// Background tiering publishes a body for `execute_jit_call`, whose wrapper
/// owns the implicit synchronized-method monitor. It must not be used by raw
/// direct-call sites; [`try_jit_compile_callee`] deliberately rejects those.
///
/// `class_id` is the task's own class identity (`MethodKey::class_id`); it
/// picks the right definition when two loaders define `class_name` (see
/// [`try_jit_compile_callee_slow`]'s `class_id_hint`).
///
/// `policy_declined` is set when a `None` is a standing policy verdict rather
/// than a failed compile (see the slow path's parameter of that name).
pub(in crate::runtime::interpreter) fn try_jit_compile_wrapped_entry(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    optimize: bool,
    policy_declined: &mut bool,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    // `named_class_was_redefined` was OR'd in here and, being permanent,
    // stopped a redefined class from ever supplying a compiled callee again.
    // Compilation reads the current (agent-woven) bytecode out of the class
    // store, and the previous artifacts were evicted by the redefinition, so
    // compiling now yields code for the body that is actually installed.
    if crate::runtime::env_cache::disable_jit() || compile_admission_closed_for_capture() {
        return None;
    }
    let mut cache_negative = false;
    try_jit_compile_callee_slow(
        shared,
        Some(class_id),
        class_name,
        method_name,
        descriptor,
        optimize,
        &mut cache_negative,
        policy_declined,
        true,
        false,
    )
}

/// JIT round 21 lane reflect4 (REFLECT4-21-1, `CRATONVM_JIT_BYTECODE_ONLY_KEY`;
/// page `jr21-reflect3-bytecode-only-fallback-bodies-always-interpret-20261014.md`):
/// compile `class_name.method_name descriptor` (declared by `class_id`, a
/// reviewed `Intrinsic`'s triple) for its own-body fallback, on this thread,
/// and answer the body published under the door-invisible key
/// (`bytecode_only_body::bytecode_only_key_descriptor`). Only
/// `bytecode_only_body` calls this; nothing else looks the key up, so no
/// ordinary caller can be served the body in place of the intrinsic. Never a
/// synchronized method (the one-shot door that enters the body holds no
/// monitor). Panics contained, as at the eager door.
pub(in crate::runtime::interpreter) fn try_jit_compile_bytecode_only_body(
    shared: &SharedVm,
    class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<std::sync::Arc<cratonvm_jit::CompiledMethod>> {
    if crate::runtime::env_cache::disable_jit() || compile_admission_closed_for_capture() {
        return None;
    }
    let mut cache_negative = false;
    let mut policy_declined = false;
    match cratonvm_jit::tiered::contain_compile_panic(|| {
        try_jit_compile_callee_slow(
            shared,
            Some(class_id),
            class_name,
            method_name,
            descriptor,
            true, // optimize: the IR tier, which falls back to single-pass itself
            &mut cache_negative,
            &mut policy_declined,
            false,
            true,
        )
    }) {
        Ok(Some((body, _entry, _needs_ctx))) => Some(body),
        Ok(None) | Err(_) => None,
    }
}

thread_local! {
    /// The key of the last optimizing body this thread offered to `JitCache::put`
    /// that `put` did not publish, while a single-pass body stayed live under the
    /// key. Written by [`note_refused_optimizing_publication`], consumed by the
    /// background C2 task through [`take_refused_optimizing_publication`].
    pub(in crate::runtime::interpreter::jit_bridge) static REFUSED_OPTIMIZING_PUBLICATION: std::cell::RefCell<Option<(String, String, String)>> =
        const { std::cell::RefCell::new(None) };
}

/// `put` refused an optimizing body and the read-back answered the single-pass
/// body it was meant to supersede.
///
/// `put` drops a body without a word when a direct-call target baked into it was
/// retired before publication (`prepare_for_publication`), when its dependencies
/// or the install epoch moved, or under a redefinition hold. The by-name door
/// then returns the live body, so a C2 task reported success and the tier
/// manager recorded C2 over a C1 body: the method was never offered C2 again.
/// Measured on `S17Kernel cap32` (str17): `String(AbstractStringBuilder, Void)`'s
/// C2 body baked a callee that its own C2 had just superseded, the publication
/// was refused (`[jit-retired-callee]`), and the C1 body, whose
/// `AbstractStringBuilder.getValue()` call is declined `eager-callee-chain-cold`,
/// served all 399 439 calls through `jit_invoke_dispatch`. That happened in 10
/// of 12 runs.
pub(in crate::runtime::interpreter::jit_bridge) fn note_refused_optimizing_publication(class_name: &str, method_name: &str, descriptor: &str) {
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] c2-publication REFUSED {class_name}.{method_name}{descriptor} -- the single-pass body stays live"
        );
    }
    REFUSED_OPTIMIZING_PUBLICATION.with(|r| {
        *r.borrow_mut() = Some((
            class_name.to_owned(),
            method_name.to_owned(),
            descriptor.to_owned(),
        ));
    });
}

/// Did this thread's last refused optimizing publication name this method?
/// Clears the record either way. Called with `None` before a compile to forget
/// any earlier record.
pub(in crate::runtime::interpreter::jit_bridge) fn take_refused_optimizing_publication(key: Option<(&str, &str, &str)>) -> bool {
    let taken = REFUSED_OPTIMIZING_PUBLICATION.with(|r| r.borrow_mut().take());
    match (taken, key) {
        (Some((c, m, d)), Some((kc, km, kd))) => c == kc && m == km && d == kd,
        _ => false,
    }
}

/// Deepest nested-compile stack at which the callee resolver will still compile a
/// not-yet-compiled statically bound callee in order to bind it directly
/// (`eager-callee-chain`).
///
/// Six levels covers the chains that matter in practice — a JUnit `assertEquals`
/// reaches `AssertionUtils.objectsAreEqual` in three, a `java.nio` absolute
/// accessor reaches its `ScopedMemoryAccess` leaf in three — while bounding the
/// worst-case native stack use and the worst-case latency of one tier-up. Past the
/// bound a site keeps the checked dispatch helper, which is always correct.
pub(in crate::runtime::interpreter::jit_bridge) const MAX_EAGER_CALLEE_CHAIN_DEPTH: usize = 6;

/// Transitive callee compiles allowed per TOP-LEVEL compile.
///
/// The depth bound alone does not bound fan-out: a method with forty statically
/// bound sites, each of whose callees has forty of its own, would turn one tier-up
/// into thousands of compiles and a visible pause. This caps the total. It is
/// deliberately generous — the chains this exists for are a handful of methods deep
/// and a handful wide — so an ordinary method never reaches it.
pub(in crate::runtime::interpreter::jit_bridge) const MAX_EAGER_CALLEE_CHAIN_PER_COMPILE: u32 = 96;

thread_local! {
    /// Transitive callee compiles spent under the current top-level compile. Reset
    /// by [`eager_callee_chain_enter_top_level`], which every entry to
    /// [`try_jit_compile_callee_slow`] calls; only the entry that finds no compile
    /// open on this thread actually clears it.
    pub(in crate::runtime::interpreter::jit_bridge) static EAGER_CALLEE_CHAIN_SPENT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Start a fresh `eager-callee-chain` budget if this is a top-level compile.
///
/// "Top level" is `jit_active_compile_depth() == 0`: the jit crate pushes its
/// compile-stack entry inside `try_compile`, which this function has not called
/// yet, so a nested (callee) entry always sees a non-zero depth here and keeps the
/// outer budget.
pub(in crate::runtime::interpreter::jit_bridge) fn eager_callee_chain_enter_top_level() {
    if cratonvm_jit::jit_active_compile_depth() == 0 {
        EAGER_CALLEE_CHAIN_SPENT.with(|c| c.set(0));
    }
}

/// `CRATONVM_JIT_COMPILE_STACK_CHAIN_BUDGET` (JIT round 19, lane eager19,
/// EAGERCOLD18-5 of `docs/feature-designs/jit-ir-optimizations.md`; **default on**): the
/// eager-chain fan-out budget ([`MAX_EAGER_CALLEE_CHAIN_PER_COMPILE`]) is the
/// one the jit crate keeps beside its compile stack, reset when a compile
/// enters at depth 0 (`JitCompileStackGuard::enter`), instead of this file's
/// [`EAGER_CALLEE_CHAIN_SPENT`], reset only by the entries that remember to
/// call [`eager_callee_chain_enter_top_level`]. Every door that compiles then
/// gets exactly one budget by construction; with it on,
/// `CRATONVM_JIT_OPTIMIZING_DOOR_CHAIN_BUDGET` (the optimizing door's own
/// explicit reset of the old counter) no longer matters -- the optimizing
/// door's `try_compile` resets the budget it spends. A door that runs two
/// top-level `try_compile`s in sequence (an optimizing attempt and a
/// single-pass retry) gets one budget per attempt. Read per spend (cold: a
/// compile-time miss).
pub(in crate::runtime::interpreter::jit_bridge) fn compile_stack_chain_budget_enabled() -> bool {
    true
}

/// Has the callee `declaring_class_id.callee_method callee_desc` been called often
/// enough (`jit_eager_chain_min_invocations`) to be compiled on its caller's
/// behalf? The count is the profile store's, which a door folds a method's own
/// counter into every few calls, so it trails the true count slightly.
pub(in crate::runtime::interpreter::jit_bridge) fn eager_callee_chain_is_warm(
    shared: &SharedVm,
    declaring_class_id: ClassId,
    callee_method: &str,
    callee_desc: &str,
) -> bool {
    if callee_method == "<init>" && crate::vm::class_is_java_lang_ref_reference(shared, declaring_class_id) {
        return true;
    }
    let min_calls = eager_callee_chain_floor(
        crate::runtime::env_cache::jit_eager_chain_min_invocations(),
        true,
        crate::runtime::env_cache::bg_compile(),
    );
    min_calls == 0
        || shared
            .jit
            .profile_store
            .invocation_count(cratonvm_jit_api::invoc_key_parts(
                declaring_class_id.as_u32(),
                callee_method,
                callee_desc,
            ))
            >= min_calls
}



/// The fewest profile-store invocations [`eager_callee_chain_is_warm`] asks of
/// a callee before the ordinary eager chain compiles it for its caller, `0`
/// meaning no gate.
///
/// Without `has_run_bg` (the default) it is `min_invocations`
/// (`CRATONVM_JIT_EAGER_CHAIN_MIN_INVOCATIONS`, a sixteenth of the tier-up
/// threshold by default, at least 1; `0` the unconditional chain).
///
/// With it (EAGERCOLD18-1): under the background compiler the floor is never
/// below 1, so a never-run callee is refused even with
/// `MIN_INVOCATIONS=0` -- there a caller reaches a compile only after its own
/// warm-up, so a zero-count callee sits on a path that has not run, and
/// compiling it now compiles every class it instantiates blind. Under
/// `CRATONVM_BG_COMPILE=0` the floor is `0`: the mutator compiles a method at
/// its threshold and the chain is the only thing that binds its callees in
/// order, so no warmth gate applies there (pre-2026-10-05 behaviour).
pub(in crate::runtime::interpreter::jit_bridge) fn eager_callee_chain_floor(min_invocations: u32, has_run_bg: bool, bg_compile: bool) -> u32 {
    match (has_run_bg, bg_compile) {
        (false, _) => min_invocations,
        (true, true) => min_invocations.max(1),
        (true, false) => 0,
    }
}

/// Charge one transitive callee compile against the current budget. `false` means
/// the budget is spent and the site must keep dispatch.
pub(in crate::runtime::interpreter::jit_bridge) fn eager_callee_chain_try_spend() -> bool {
    // JIT round 19, lane eager19 (EAGERCOLD18-5): one budget owner, the jit
    // crate's compile stack; see [`compile_stack_chain_budget_enabled`].
    if compile_stack_chain_budget_enabled() {
        return cratonvm_jit::jit_compile_chain_budget_try_spend(
            MAX_EAGER_CALLEE_CHAIN_PER_COMPILE,
        );
    }
    EAGER_CALLEE_CHAIN_SPENT.with(|c| {
        let spent = c.get();
        if spent >= MAX_EAGER_CALLEE_CHAIN_PER_COMPILE {
            false
        } else {
            c.set(spent + 1);
            true
        }
    })
}

