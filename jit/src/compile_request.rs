// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! One compile request: every input the method-entry compile door takes.
//!
//! The driver used to take these as 30-odd positional parameters, 13 of them
//! optional resolver closures, and read two more through thread-local side
//! channels a caller had to set before the call
//! (`jit-god-functions-and-request-side-channels-FIXED-20260912.md`). A request
//! is built once by each door and passed by reference; nothing about the
//! compile travels outside it.

use crate::{
    CachedBytecodeMethod, InlineSite, JitLdcConstant, JitNewSite, JitRuntimeHelpers,
    StringFieldLayout,
};

/// Every input of one method-entry compilation. `Copy`: it holds only
/// references, borrowed closures and flags, so a door can retry with the same
/// request.
#[derive(Clone, Copy)]
pub struct CompileRequest<'a> {
    pub cached: &'a CachedBytecodeMethod,

    pub cp_class_name_resolver: Option<&'a dyn Fn(u16) -> Option<String>>,

    pub cp_field_resolver: Option<&'a dyn Fn(u16) -> Option<(usize, u8, Option<(u32, bool)>)>>,

    pub cp_static_field_resolver: Option<&'a dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,

    /// `true` iff the `getfield`/`putfield` constant-pool index names a
    /// **volatile** instance field (`ACC_VOLATILE`). The instance twin of the
    /// `is_volatile` the static resolver above already returns; a separate
    /// resolver rather than a fourth tuple element so every existing
    /// `cp_field_resolver` closure keeps compiling.
    ///
    /// Consumed by both tiers: the single-pass backend emits the JMM's
    /// StoreLoad fence (`MFENCE`) after every volatile `putfield`
    /// (`x64::BackendRequest::volatile_field_pcs`), and the optimizing tier
    /// refuses a method with a volatile instance access
    /// (`ir::IrBuilder::set_volatile_field_pcs`), because its `Op::Load` /
    /// `Op::Store` carry no ordering and GVN / LICM could merge or hoist the
    /// access — a hoisted read of a flag another thread sets is a spin loop
    /// that never ends
    /// (`volatile-instance-fields-are-plain-accesses-in-compiled-code`).
    ///
    /// `None` = no volatility information: every field compiles as a plain
    /// access, the pre-fix behaviour. Every VM compile door supplies it; only
    /// hand-built test requests leave it absent.
    pub cp_field_volatile_resolver: Option<&'a dyn Fn(u16) -> bool>,

    pub cp_invoke_resolver: Option<&'a dyn Fn(u16) -> Option<(String, String, String)>>,

    /// JVMS §6.5 `invokespecial` super-call redirect: given an `invokespecial`
    /// (0xb7) call-site's CP index, returns the JVMS-correct class at which
    /// method selection actually begins when that differs from the plain
    /// `cp_invoke_resolver` class name — i.e. a genuine `super.m(...)` whose
    /// constant-pool reference names an ancestor further up than this
    /// method's own direct superclass. `None` (resolver absent, or it
    /// returns `None` for a given site — the overwhelmingly common case)
    /// leaves `class_name` exactly as `cp_invoke_resolver` returned it. See
    /// `classloading::invokespecial_selection_start` for the algorithm.
    ///
    /// Asked with the site's opcode: `0xb6` / `0xb9` answer the declaring
    /// class of a private (or, for `0xb6`, `final`) target, which makes the
    /// site statically bound. The answer is `(owner name, owner class id)`;
    /// the id is the class the substitution's hierarchy walk found (`0` when
    /// the resolver has none) and is baked into the site's
    /// `JitInvokeInfo::owner_class_id`, so the dispatch helper need not look
    /// the substituted name up again in the caller's loader.
    pub cp_invokespecial_owner_resolver: Option<&'a dyn Fn(u16, u8) -> Option<(String, u32)>>,

    pub callee_compiler: Option<&'a dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,

    /// CRIT-2 / cold-`new` fix — see [`JitNewSite`]. `Resolved` carries
    /// (class_id, num_fields, has_nonzero_tag_primitive_init, has_finalizer);
    /// the two flags feed the inline-TLAB `new` fast path and a resolver that
    /// cannot compute them must report `true, true` so the post-init helper
    /// call stays in place. `Deferred` means "class not loaded yet" and
    /// compiles to the CP-indexed runtime-resolving helper; only `None` (a
    /// malformed site) still bails the compile.
    pub cp_new_resolver: Option<&'a dyn Fn(u16) -> Option<JitNewSite>>,

    pub cp_ldc_resolver: Option<&'a dyn Fn(u16) -> Option<JitLdcConstant>>,

    /// Round 14 wave 3 (lane calls, I7-4): `String.hashCode()` of the literal
    /// an `ldc <String>` at `cp_idx` pushes, when the VM can state it; `None`
    /// otherwise. Read only by the optimizing tier's string-literal hash fold
    /// (`ea_ir_bridge::ir_fold_hash_code_of_string_literal`).
    pub cp_ldc_string_hash_resolver: Option<&'a dyn Fn(u16) -> Option<i32>>,

    /// Round 14 wave 4 (lane calls2, C14W3-4): the UTF-16 units of the literal
    /// an `ldc <String>` at `cp_idx` pushes, for exactly the literals
    /// `cp_ldc_string_hash_resolver` answers; `None` otherwise. Read only by the
    /// optimizing tier's literal query fold
    /// (`ea_ir_bridge::ir_fold_string_literal_queries`).
    pub cp_ldc_string_utf16_resolver: Option<&'a dyn Fn(u16) -> Option<Vec<u16>>>,

    /// inc 35: (bits, is_double)
    pub cp_ldc2w_resolver: Option<&'a dyn Fn(u16) -> Option<(i64, bool)>>,

    pub profile: Option<&'a crate::profile::MethodProfile>,

    pub helpers: &'a JitRuntimeHelpers,

    pub inline_resolver: Option<&'a dyn Fn(&str, &str, &str) -> Option<InlineSite>>,

    /// Resolves the field layout of `java/lang/String` for the String
    /// call-site intrinsics. Called at most once per compilation; see
    /// `StringFieldLayout`. `None` (resolver absent, or it returns `None`)
    /// makes String intrinsics bail to normal dispatch.
    pub string_layout_resolver: Option<&'a dyn Fn() -> Option<StringFieldLayout>>,

    /// Maps an invoke* constant-pool index to the class id of the call's
    /// *declared* class (the receiver class statically named at the site).
    /// Consumed by the CRC32/CRC32C `update` call-site intrinsics, whose
    /// codegen emits a receiver class-id guard against this constant. `None`
    /// (resolver absent, or it returns `None` for a given site) makes the
    /// CRC32 intrinsic at that site bail to normal dispatch.
    pub cp_invoke_class_id_resolver: Option<&'a dyn Fn(u16) -> Option<u32>>,

    /// activate-ir-optimizer (scalar-new wiring): given an `invokespecial`
    /// constant-pool index, returns `true` iff it targets a constructor whose
    /// *construction* is elidable for escape-analysis scalar replacement — a
    /// no-arg `<init>()V` of a direct `java/lang/Object` subclass whose body is
    /// exactly `aload_0; invokespecial Object.<init>()V; return` (no field
    /// initialiser, no escape, no side effect). `None` (the production default
    /// unless the soak flag is set) leaves scalar-replacement of `new` OFF: the
    /// IR builder bails on every `invokespecial`, so allocation-bearing methods
    /// take the single-pass backend exactly as before.
    pub cp_elidable_init_resolver: Option<&'a dyn Fn(u16) -> bool>,

    /// wire-tiered-manager Step 3: `true` → optimizing IR pipeline (C2);
    /// `false` → single-pass `x64::compile` only (the fast C1 tier). See the
    /// function doc above.
    pub optimize: bool,

    /// Gap B (activate-ir-optimizer): `true` lets the IR builder lower an
    /// int-only `invokestatic` in an oop-free method to `Op::Call` (dispatched
    /// via `invoke_dispatch`). `false` (the default) keeps every invoke on
    /// single-pass. Gated default-OFF behind `CRATONVM_JIT_IR_CALL` at the VM
    /// call sites until it soaks.
    pub ir_emit_calls: bool,

    /// inc 24 (Gap B): `true` additionally lets the IR builder lower a resolved
    /// non-`<init>` `invokespecial` (private / `super.` / non-virtual instance
    /// call) to `Op::Call`, with the receiver marshalled as arg0 and
    /// `invoke_kind == 1`. `false` (the default) keeps every `invokespecial`
    /// on single-pass (the builder bails). Gated default-OFF behind
    /// `CRATONVM_JIT_IR_CALL_SPECIAL` at the VM call sites until it soaks.
    pub ir_emit_special_calls: bool,

    /// inc 25 (category-2 foundation): `true` lets the optimizing IR path take
    /// **long**-using methods (the `method_uses_category2` gate otherwise bails
    /// the whole pipeline on any long/double opcode). Only long is admitted —
    /// double/float and int div/rem still bail (the latter to keep a `long`
    /// off a deopt point, since long deopt-resume is a follow-up). `false` (the
    /// default) preserves the int/ref-only IR path. Gated default-OFF behind
    /// `CRATONVM_JIT_IR_LONG` at the VM call sites until it soaks.
    pub ir_emit_long: bool,

    /// inc 26 (Gap B): `true` additionally lets the IR builder lower a resolved
    /// `invokevirtual` (0xb6) / `invokeinterface` (0xb9) to `Op::Call`, with the
    /// receiver marshalled as arg0 and `invoke_kind == 0` (virtual) / `2`
    /// (interface). Dispatch is fully dynamic: the baked `JitInvokeInfo` carries
    /// the static call-site class/name/descriptor and `invoke_dispatch` resolves
    /// the actual target on the receiver's RUNTIME class (no inline cache in the
    /// emitted code — the generic helper does the vtable/itable lookup). `false`
    /// (the default) keeps every virtual/interface invoke on single-pass. Gated
    /// default-OFF behind `CRATONVM_JIT_IR_CALL_VIRTUAL` at the VM call sites
    /// until it soaks.
    pub ir_emit_virtual_calls: bool,

    /// inc 30 (double/float XMM value tier): `true` admits a `float`/`double`-using
    /// method to the optimizing IR path (the `method_uses_fp` gate otherwise bails
    /// it to single-pass). The lowerer marshals FP values through XMM
    /// (`addsd`/`cvttsd2si`/…); FP value arithmetic (`fadd`/`dmul`/…), FP
    /// constants (`fconst`/`dconst`), FP-local load/store, and the int/long⇄FP
    /// conversions lower. `frem`/`drem`, FP compares/branches, FP array ops, and
    /// FP params/returns/call-args still bail (their builder arms are absent), as
    /// do int-div-bearing and `ldc2_w`-bearing FP methods (a follow-up). `false`
    /// (the default) preserves the int/long/ref IR path byte-for-byte: no
    /// FP-opcode method is admitted, so the builder never sees an FP opcode. Gated
    /// default-OFF behind `CRATONVM_JIT_IR_FP` at the VM call sites until it soaks.
    pub ir_emit_fp: bool,

    /// invokedynamic-uncommon-trap fix: resolves an `invokedynamic` (0xba)
    /// CP index to just its target descriptor string (e.g. via
    /// `NameAndType.descriptor` — no bootstrap/`CallSite` resolution needed).
    /// `jit_scan` is CP-blind and no longer bails on 0xba (it just records the
    /// site); this resolver lets the single-pass backend compute the site's
    /// stack effect (arg count via `count_param_slots`, return type via
    /// `return_type`) so it can lower the instruction to an unconditional jump
    /// to the existing uncommon-trap deopt stub (`DeoptReason::UnreachedCode`)
    /// while keeping the compiler's simulated operand stack consistent for
    /// whatever bytecode follows. `None` (resolver absent, or it returns `None`
    /// for a given site) bails the whole compile — see `try_compile_inner`.
    pub cp_invokedynamic_descriptor_resolver: Option<&'a dyn Fn(u16) -> Option<(String, usize)>>,

    /// PGO-02: maps a receiver CLASS ID (not a CP index - the runtime
    /// identity a guarded speculative inline's receiver class-id check
    /// resolved against) to its class name, so a Monomorphic/Bimorphic
    /// InlinePlan can record a SpeculatedReceiver invalidation dependency
    /// (plan_inline refuses via NoInvalidationDependency without one - the
    /// fail-closed rule in docs/feature-designs/profile-guided-inlining.md).
    /// `None` (resolver absent, or it returns `None` for a given id) refuses
    /// every speculative virtual/interface inline at that site; static/
    /// special DirectBind sites are unaffected (no receiver dependency).
    pub class_id_name_resolver: Option<&'a dyn Fn(u32) -> Option<String>>,

    /// PGO-02 R0: resolves the body a receiver of EXACTLY this class id would
    /// dispatch to at a call site declared `(cp_class, method, descriptor)`.
    ///
    /// Separate from `inline_resolver` because they answer different questions.
    /// `inline_resolver` resolves the CONSTANT-POOL callee, which is the right
    /// answer for `invokestatic`/`invokespecial` and the WRONG one for a
    /// guarded virtual/interface site: the guard admits a runtime class, and
    /// wherever that class overrides the declared method the two bodies differ.
    /// Splicing the constant-pool body behind such a guard is silent wrong code
    /// (see `InlineRequest::receiver_callee_resolver`). `None` — or a `None`
    /// answer — refuses the speculation; it never falls back to the other
    /// resolver.
    pub receiver_inline_resolver: Option<&'a dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,

    /// Class-hierarchy analysis (`NOTES-deopt2.md` M5): for a call site declared
    /// `(cp_class, method, descriptor)`, the single loaded CONCRETE class below
    /// `cp_class` that provides that method — or `None` when there are zero or
    /// more than one.
    ///
    /// This is the only input that lets a virtual or interface site be inlined
    /// on a compile with NO receiver profile. Without it
    /// `ReceiverShape::Unprofiled` refuses every such site
    /// (`InlineRefusal::NoProfileEvidence`), which is the state
    /// `docs/internal/jit-review-r6/NOTES-deopt2.md` records as "an unprofiled
    /// first compile inlines nothing at a virtual site".
    ///
    /// The answer is a HINT, not a proof obligation. The bind it enables is
    /// guarded by an exact receiver class-id compare whose miss edge is
    /// unchanged dispatch, so a stale or incomplete hierarchy costs a useless
    /// compare and never a wrong answer — see
    /// `InlineDependency::UniqueConcreteMethod` for the full argument and for
    /// why the unguarded form is NOT issued. `None` (resolver absent, or a
    /// `None` answer) plans byte-identically to before this existed.
    pub unique_concrete_resolver:
        Option<&'a dyn Fn(&str, &str, &str) -> Option<crate::UniqueConcreteBind>>,

    /// IR-tier inlining: resolve a callee body for `IrBuilder` to SPLICE, under
    /// the optimizing tier's own admission set (`resolve_ir_inline_site` in the
    /// VM). Separate from `inline_resolver` because the two answer different
    /// questions: that one resolves what the single-pass emitter can splice,
    /// which refuses `new`, array access and `arraylength` — the three shapes an
    /// allocating accessor is made of. `None` (no VM in scope, or the gate off)
    /// splices nothing, which is byte-identical to the pre-inlining IR path.
    pub ir_inline_resolver: Option<&'a dyn Fn(&str, &str, &str) -> Option<InlineSite>>,
    /// Round 12 wave 4 (lane iropt3, proposal W3-1): the exact-receiver twin
    /// of [`Self::ir_inline_resolver`]. Asked for an `invokevirtual` site the
    /// plain resolver refused, it resolves the body a receiver of EXACTLY the
    /// constant-pool class dispatches to, and answers that class's id with it.
    /// The planner hands the id to `IrBuilder::add_exact_receiver_site`, and
    /// the builder splices only when it can prove the receiver is that class
    /// (`IrBuilder::exact_receiver_proof`). Otherwise the site stays a call.
    /// `None` plans exactly as before.
    pub ir_exact_receiver_resolver:
        Option<&'a dyn Fn(&str, &str, &str) -> Option<(InlineSite, u32)>>,
    /// Round 13 wave 12 (lane guardsplice, W7-1): resolves the body a receiver
    /// of EXACTLY the given class id dispatches to at an `invokevirtual` /
    /// `invokeinterface` declared `(cp_class, method, descriptor)`, under the
    /// optimizing tier's admission set. Asked by the IR inline planner for the
    /// class the site's receiver profile says dominates it
    /// (`ir::profile_guarded_receiver_class`); the builder splices the body
    /// only behind an exact class test whose miss edge is the site's own call
    /// (`IrBuilder::begin_guarded_receiver_splice`). `None` plans exactly as
    /// before.
    pub ir_receiver_inline_resolver:
        Option<&'a dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,

    /// JDK-only execution policy for THIS VM's compilations.
    ///
    /// Was the process-global `JIT_COMPATIBILITY_MODE` latch until 2026-08-06
    /// (JDK-ONLY-WAVE2 §2 of the wave-2 markers record — the record's §6 is the
    /// JNI table, a different process global). The latch only ever
    /// moved toward strict, so one `JdkOnly` VM silently took the thin
    /// direct-call helpers away from every `Compatible` VM sharing the process
    /// — the hazard contract §2's no-process-globals rule exists to prevent.
    /// Threaded here because the compile path already carries per-VM state and
    /// this is per-VM state; the `JitRuntimeHelpers` table was the wrong home
    /// (it is a `#[repr(C)]` ABI of helper ADDRESSES with baked offsets, and a
    /// policy bit is not an address).
    pub jdk_only: bool,

    /// JDK-ONLY-WAVE2 §4: asks the VM whether the GENERIC TAIL would admit
    /// this triple's registered native, which is the only question a bind-time
    /// refusal can usefully answer.
    ///
    /// This is the policy half of the seven thin direct-call ladders below.
    /// Before it existed those ladders were refused wholesale under `JdkOnly`,
    /// which is stricter than the contract.
    ///
    /// # It used to ask a narrower question, and the narrower one was wrong
    ///
    /// Until 2026-09-22 this was `intrinsic_resolver`: *is this triple a
    /// reviewed `NativeKind::Intrinsic`* — §1.4's exception — with `false` for
    /// `Bridge`. That reads as the strict answer and is not one. A refused
    /// bind does not stop the call; the site keeps its `invoke_info` and the
    /// call goes to `vm_exec::invoke_or_native`, which computes §7 step 3's
    /// input as `has_real || dial` and evaluates `has_real` only for a
    /// `SyntheticStub`. For a `Bridge` that input is structurally `false`, so
    /// the tail ran the same native the bind had just been refused for — one
    /// three-string registry hash per call later. MEASURED at ~3x on the
    /// per-byte `MessageDigest.update(byte)` loop in
    /// `docs/internal/jdk-only/jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922-FIXED-20260922.md`.
    ///
    /// So a `Bridge` is admitted exactly when the tail would run it, and
    /// refused exactly when the tail would yield to bytecode
    /// (`CRATONVM_ENFORCE_NATIVE_SHADOW`). `Intrinsic` is admitted as before;
    /// `SyntheticStub` and unregistered triples stay refused. The VM side is
    /// `jit::helpers::direct_native_bind_admitted`, which is the same
    /// predicate the callee-side gate runs per call, so the two halves of this
    /// policy cannot drift into disagreeing.
    ///
    /// `None` refuses everything, which is the pre-2026-08-06 behaviour and the
    /// fail-closed direction — a compile with no way to ask cannot bake a
    /// native in front of real bytes.
    pub direct_bind_admission: Option<&'a dyn Fn(&str, &str, &str) -> bool>,

    /// The compiling VM's per-bci de-spec registry (`JitRealm::despec_registry`
    /// on the VM side). Was the process-global `deopt.rs` `DESPEC_SET` until
    /// 2026-09-12, which let one VM's despeculation verdicts strip speculations
    /// from every other VM's compiles. `None` (no VM in scope) consults nothing.
    pub despec: Option<&'a std::sync::Arc<crate::deopt::DespecRegistry>>,

    /// The compiling VM's runtime de-speculation registry
    /// (`JitRealm::runtime_despec`). Handed straight to
    /// `compile_gate::admit`, which raises
    /// `CompileRefusal::RuntimeDespeculated` at the method-entry and
    /// callee-dispatch doors and never at the OSR door.
    ///
    /// Was a process-global `OnceLock<RwLock<FxHashMap<..>>>` in
    /// `compile_gate.rs` until `NOTES-deopt2.md` M7. Its key —
    /// `(ClassId, class, method, descriptor)` — keeps two LOADERS apart but not
    /// two VMs: `ClassId`s are allocated per `ClassStore`, from 0, so two VMs in
    /// one process routinely produce the same tuple for different methods, and
    /// one VM's give-up decision shut the other's doors. `None` (no VM in
    /// scope) consults nothing, which is the fail-OPEN direction and is correct:
    /// a caller with no VM has no runtime in which a speculation could have
    /// failed.
    pub runtime_despec: Option<&'a std::sync::Arc<crate::compile_gate::RuntimeDespecRegistry>>,

    /// Review #80: maps an invoke constant-pool index to the name of the class
    /// that DECLARES the method the site resolves to. It sits beside
    /// `cp_invokespecial_owner_resolver` but asks a different question: that one
    /// answers only when a site binds statically (a private or final owner that
    /// no native screen refuses). This one answers for any resolvable
    /// `Methodref`. Consumed by the ATOMIC_INT / ATOMIC_LONG registration, so a
    /// `Counter extends AtomicInteger` site can reach the intrinsic. `None`
    /// keeps the exact constant-pool class match.
    pub cp_invoke_declaring_class_resolver: Option<&'a dyn Fn(u16) -> Option<String>>,

    /// The class ID twin of `cp_invoke_declaring_class_resolver`: the loaded
    /// class that declares the method an invoke constant-pool index resolves
    /// to, as the compiling class's loader finds it (interpreter round i1
    /// wave 17, lane L3; proposal
    /// `i13-L3-proposal-callee-resolvers-answer-the-bound-method-identity`,
    /// stage 1).
    ///
    /// Asked by the two bind ladders only for an inherited-name call site
    /// whose names a recursion-cycle member has (`cycle_site_declaring_class`),
    /// so the cycle checks match THAT class and not every same-named class of
    /// another loader. A separate resolver rather than a second tuple element
    /// so the name resolver's other consumers (the `Atomic*` intrinsic
    /// registration, the unbox match) keep their shape. `None`, or a `None`
    /// answer, matches the declaring class by names, the conservative
    /// pre-wave-17 answer.
    pub cp_invoke_declaring_class_id_resolver: Option<&'a dyn Fn(u16) -> Option<u32>>,

    /// The VM has proven that a call to this method's own name, class and
    /// descriptor resolves to this method (a builtin-loaded class nothing can
    /// redefine or shadow), so the emitter may bind a direct self-CALL.
    /// Replaces the `set_self_call_identity_stable` thread-local, which a
    /// caller set before the call and the door consumed.
    pub self_call_identity_stable: bool,
    /// The VM's thin direct-call helper addresses for this compile. Replaces
    /// the 24 process-wide `*_DIRECT_FN` cells.
    pub direct_helpers: &'a crate::DirectHelperTable,
    /// Round 9 wave 7 (`mutrec7`): may the statically bound CLOSING edge of a
    /// compile-time recursion cycle to `(class, method, descriptor)` be served
    /// by a [`crate::JitCycleEdgeCell`] (a guarded call that binds once the
    /// target publishes) instead of `jit_invoke_dispatch`?
    ///
    /// The closing edge never reaches `callee_compiler` (its target is open on
    /// the compile stack), so the VM's direct-bind gates -- the ones
    /// `callee_compiler` applies before a raw CALL: a native shadow, a
    /// `synchronized` or exception-table callee, a static callee whose class
    /// is not initialized yet, the FJP blocklist -- are asked here instead, at
    /// the same moment (bake time). `None` plans no cell at all. Only consulted
    /// under `CRATONVM_JIT_CYCLE_EDGE_CELL`.
    pub cycle_edge_admission: Option<&'a dyn Fn(&str, &str, &str) -> bool>,
    /// Round 9 wave 8 (`arraylist8`; `NOTES-w5-typecheck5.md` cross-lane
    /// request 2): is the class with this `ClassId` -- a type-check site's
    /// target as `cp_new_resolver` resolved it through the compiling class's
    /// loader -- a `final`, non-interface, non-array class that nothing can
    /// subclass in this VM? A `true` lets the single-pass inline `instanceof`
    /// answer a definite miss for a user class inline
    /// (`intern_typecheck_target_with_finality`). `None`, or `false`, keeps
    /// every such site on the helper, which is the pre-wave-8 behaviour.
    pub class_is_final_resolver: Option<&'a dyn Fn(u32) -> bool>,
    /// Round 11 wave 10 (lane `synccall`, page
    /// `r11w9-sync-caller-held-monitor-direct-call-patch`): the `static
    /// synchronized` callee a caller-held direct CALL may bind at the
    /// `invokestatic` whose Methodref is this constant-pool index, or `None`.
    ///
    /// A compiled body of an `ACC_SYNCHRONIZED` method takes no monitor, so no
    /// other door may CALL it raw (`callee_compiler` refuses it). The optimizing
    /// tier instead holds the class monitor AROUND the CALL itself, and that is
    /// sound only for a callee body that can neither trap nor call: nothing can
    /// then stop it part-way while the caller holds a monitor no frame state
    /// names. The VM side answers only for such bodies. Only consulted by the
    /// optimizing tier's invoke planner under `CRATONVM_JIT_SYNC_DIRECT`
    /// (default on). `None` binds nothing, which is the pre-wave-10 behaviour.
    ///
    /// Round 11 wave 11 (lane `calls`): the same lookup also answers for an
    /// INSTANCE `synchronized` method (`SyncDirectTarget::instance`), whose
    /// monitor is the receiver; the planner binds it only at a statically
    /// bound site (see `SyncDirectTarget::unoverridable`).
    pub sync_direct_lookup: Option<&'a dyn Fn(u16) -> Option<SyncDirectTarget>>,
    /// Interpreter round i1 wave 10 (`i1-L7-process-global-jit-bridge-state-not-vm-scoped`):
    /// the calling VM's deferred-`new` retry memo, where a build that bails on
    /// a not-yet-loaded `new` records its one retry.
    ///
    /// Required since interpreter round i1 wave 18 (lane L5,
    /// `interpreter-deferred-new-retries-silent-process-fallback-FIXED`): it
    /// was an `Option` whose `None` silently recorded into the process-wide
    /// memo, which no VM sweeps, so a door that forgot it left the method on
    /// its bailed body with its one retry never spent. [`CompileRequest::new`]
    /// takes it with the verdicts as one [`CompileRealm`].
    pub deferred_new_retries: &'a crate::DeferredNewRetries,
    /// Interpreter round i1 wave 11 (same page): the calling VM's compile
    /// verdicts (the bail list, refusal reasons, OSR entry rejects and the
    /// recursion-cycle sets), `TieredCompilationManager::verdicts`.
    ///
    /// Required since interpreter round i1 wave 17 (lane L3, proposal
    /// `i12-L3-proposal-compile-request-requires-a-verdict-registry`): it was
    /// an `Option` whose `None` silently read and wrote
    /// [`crate::process_jit_verdicts`], a registry no VM reads, so a door that
    /// built its request by hand re-attempted its bails forever and hid its
    /// cycle verdicts from the VM's other compiles. [`CompileRequest::new`]
    /// now takes it (inside a [`CompileRealm`] since wave 18); a caller with
    /// no VM in scope names [`CompileRealm::process`] (or a registry of its
    /// own) explicitly.
    pub verdicts: &'a crate::JitVerdictRegistry,
    /// Interpreter round i1 wave 15, lane L2
    /// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`): this is the
    /// optimizing OSR door's compile, whose body runs inside an interpreter
    /// frame's activation. Its unconditional back-edge polls then leave at the
    /// loop header when the slow path's verdict says that frame must run
    /// interpreted (`ir_lower::Lowerer::back_edge_mode_exit_state`); the
    /// flag-clear path is unchanged. The IR twin of the single-pass
    /// `x64::BackendRequest::mode_exit_polls`. `false` for every other door:
    /// the verdict describes the innermost INTERPRETER frame, which is some
    /// other method's for a body entered by a call. (Since wave 18 such a
    /// compile's polls may leave on the polling-body bit instead, decided in
    /// the IR door itself: `ir_lower::ir_entry_mode_exits_admitted`.)
    pub osr_mode_exit_polls: bool,
    /// Interpreter round i1 wave 38, lane L2 (item 1 of the wave-37
    /// compile-door review): this compile is the optimizing OSR route's
    /// (`build_osr_optimizing_artifact` in the VM), which is admitted through
    /// the method-entry door. A backend refusal is then the ROUTE's verdict,
    /// which the VM memoises per method itself (`osr_optimizing_refused`);
    /// [`crate::try_compile_request`] does not bail-list the method for it.
    /// The bail list is honoured at every door, so the route's refusal used to
    /// switch off the single-pass OSR door as well, which for a loop-only
    /// method (never compiled at method entry) was its only compiled form.
    /// `false` for every other compile.
    pub osr_optimizing_route: bool,
    /// A debugger may read this VM's locals (interpreter round i1 wave 19,
    /// lane L1; the VM answers it per VM, `runtime::jvmti::debugger_observes_locals`).
    /// A mode exit then never resumes a frame that shows a dead but assigned
    /// local as `0`, which is what a JDWP `StackFrame.GetValues` would read
    /// where HotSpot, keeping every local alive while an agent can access
    /// locals, reads the value: the single-pass tier keeps such a local alive
    /// and describes it at every deopt point (wave 21, lane L3), refusing only
    /// an exit that still cannot describe one
    /// (`x64::BackendRequest::debugger_observes_locals`), and the optimizing
    /// tier calls no local dead at its exits (no `ir_lower::PollExitBytecode`),
    /// so a value it no longer holds refuses the exit, as before wave 18. The
    /// body then finishes compiled. `false` (every compile without a
    /// debugger) changes nothing.
    pub debugger_observes_locals: bool,
    /// Round 13 wave 10 (lane sync6,
    /// `r13w8-sync5-static-synchronized-self-locking-design-FIXED-20260929.md` part
    /// B): the address of the VM's GC-maintained word holding the DECLARING
    /// class's `Class` mirror -- the monitor of a `static synchronized` method
    /// (JVMS 2.11.10) -- or `0`. The VM fills it for an `ACC_SYNCHRONIZED |
    /// ACC_STATIC` method only (`vm::jit::helpers::class_mirror_slot_for`, a JNI
    /// global reference minted by a mutator: a mirror moves, and the background
    /// compile worker may not mint one). Non-zero is what lets such a method's
    /// method-entry compile be a SELF-LOCKING body (`self_lock_admits_method`,
    /// `CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC`); `0` (every other method, a
    /// class no mutator minted a slot for yet, and every request built by
    /// [`CompileRequest::new`]) keeps the wrapped entry, exactly as before.
    pub self_lock_mirror_slot: usize,
}

/// A caller-held direct CALL's target (see
/// [`CompileRequest::sync_direct_lookup`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncDirectTarget {
    /// The callee's published, monitor-free compiled entry, or `0` for a
    /// monitor-only answer (a static callee with no published body: the
    /// synchronized splice needs only its mirror, SS-2); a CALL never uses
    /// a `0` entry.
    pub entry: usize,
    /// Whether that entry takes the hidden context pointer.
    pub needs_ctx: bool,
    /// The Methodref's `class_index` in the CALLER's pool: the
    /// `CONSTANT_Class` whose mirror is the monitor (JVMS 2.11.10). The VM
    /// answers only when the method is declared by exactly that class.
    pub class_cp_idx: u16,
    /// The VM's GC-maintained word holding that mirror
    /// (`JitLdcConstant::ClassMirrorSlot`), or `0`: the `Op::ConstClass`
    /// that fetches the monitor then calls `ldc_class_cp`.
    pub mirror_slot: usize,
    /// Round 11 wave 11 (lane `calls`): the callee is an INSTANCE
    /// `synchronized` method declared by the Methodref's own class, so the
    /// monitor is the RECEIVER (JVMS 2.11.10), not a class mirror;
    /// `class_cp_idx` and `mirror_slot` then play no part. The planner binds
    /// such a target only at a statically bound `invokespecial` /
    /// pinned `invokevirtual` whose selected owner IS that class.
    pub instance: bool,
    /// For an instance callee: no receiver can select an override of it --
    /// the method is `private` or `final`, or its class is `final`. Required
    /// for an `invokevirtual` site bound WITHOUT a class guard; an
    /// `invokespecial` selects the method statically and does not need it.
    ///
    /// Round 11 wave 13 (lane `cha`): a genuinely virtual site whose target
    /// is overridable binds too, behind an exact receiver-class guard the
    /// planner derives from class-hierarchy analysis
    /// (`cha_sync_exact_class`), not from this lookup. The VM half needs no
    /// new field: "declared by the Methodref's own class" is already what makes
    /// a receiver of exactly that class select this body.
    pub unoverridable: bool,
}

/// The exact receiver class a CHA-bound caller-held synchronized
/// `invokevirtual` is guarded on, or `None` to leave the site on dispatch
/// (round 11 wave 13, lane `cha`).
///
/// `bind` is `CompileRequest::unique_concrete_resolver`'s answer for the
/// site's `(Methodref class, name, descriptor)`; `cp_class_id` is
/// `cp_invoke_class_id_resolver`'s id for the same Methodref class. The site
/// binds only when the single instantiable provider below the Methodref class
/// IS that class (a subclass that merely inherits the method is a second
/// provider, and a guard on the base would miss every one of its receivers),
/// and both resolvers name the same class id. That id is the only class whose
/// instances select the method `sync_direct_lookup` answered for, which is
/// the declaration of exactly this class. `0` is refused because the builder
/// reads a zero row as "no class guard".
pub(crate) fn cha_sync_exact_class(
    bind: Option<crate::UniqueConcreteBind>,
    cp_class_id: Option<u32>,
) -> Option<u32> {
    let bind = bind?;
    let id = bind.implementor_class_id;
    (id != 0 && bind.static_type_class_id == id && cp_class_id == Some(id)).then_some(id)
}

/// The `ir_direct_calls` key of a caller-held synchronized direct CALL at
/// bytecode `pc` (round 11 wave 10, lane `synccall`).
///
/// Such a row shares the map the lowerer already receives, but in a disjoint
/// key space: every other reader looks a site up by its plain pc and can never
/// find it, so no route but the one that takes the monitor can emit a raw
/// CALL to a monitor-free synchronized body (contract item 5 of the patch
/// page). A bytecode pc, combined-buffer pcs included, is far below the tag.
pub(crate) const fn sync_direct_row_key(pc: usize) -> usize {
    pc | SYNC_DIRECT_ROW_TAG
}

/// The tag bit of [`sync_direct_row_key`].
const SYNC_DIRECT_ROW_TAG: usize = 1 << (usize::BITS - 2);

/// `CRATONVM_JIT_SYNC_DIRECT` — the caller-held synchronized direct CALL
/// ([`CompileRequest::sync_direct_lookup`]). Default ON; `0|false|off|no`
/// keeps every such site on `jit_invoke_dispatch`, the pre-wave-10 route.
/// Read at planning (compile time only), never cached: the same reasoning as
/// `ir_direct_calls_enabled`.
pub(crate) fn sync_direct_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SYNC_DIRECT")
}

/// May a caller-held synchronized direct CALL return a value whose descriptor
/// return tag is `ret` (round 11 wave 12, lane `calls`, proposal 6)?
///
/// Waves 10-11 admitted int-category and void results only. The two exclusions
/// are now discharged in `ir_lower`:
///
/// * a REFERENCE result: the exceptional edge's monitor release publishes a
///   map, and the call's own result word is allocated but not yet written
///   there. `emit_sync_exit_then_throw` hides that word from the map.
/// * a `long`/`float`/`double` result can legitimately be `i64::MIN`, the
///   exception sentinel. The cold side peeks `jit_dispatch_threw` before it
///   releases anything and keeps the value when nothing is pending, so it
///   needs that helper wired (`dispatch_threw`, its address).
///
/// Any other tag (a malformed descriptor) is refused.
pub(crate) fn sync_direct_result_admitted(ret: u8, dispatch_threw: usize) -> bool {
    match ret {
        b'V' | b'I' | b'Z' | b'B' | b'C' | b'S' | b'L' | b'[' => true,
        b'J' | b'F' | b'D' => dispatch_threw != 0,
        _ => false,
    }
}

/// May a SINGLE-PASS caller plan a caller-held synchronized direct CALL
/// (`JIT_SYNC_DIRECT_GUARD` row) for a `static synchronized` callee
/// `class.method descriptor` whose return tag is `ret`, given this compile's
/// helper tables? Every gate that does not depend on the VM's answer
/// (`sync_direct_lookup` / `jit_bridge::sync_direct_target`).
///
/// Round 11 wave 14 (lane `spsync`, page
/// `r11w13-spsync-osr-door-has-no-sync-direct-route`): the one predicate for
/// the two single-pass planners, `build_single_pass_tables` and the VM's
/// single-pass OSR door (`compile_osr_body`), which reaches the backend
/// without that function and so cannot inherit its conjunction. Each term is
/// something the backend arm (`x64/op_invoke.rs`, `sync_direct_site`) relies
/// on: the class-`ldc` helper and the two thin monitor helpers emit the
/// enter and both releases; the merged sentinel is the cold side the
/// exceptional release sits on; `dispatch_threw` tells a wide result's
/// `i64::MIN` from a throw. A GPU kernel (`keeps_dispatch_helper`) and an
/// eager-chain cycle (`JitVerdictRegistry::direct_call_requires_dispatch`)
/// keep dispatch as every direct bind does. `verdicts` is the compiling VM's
/// registry, the one its compiles record recursion cycles into; the ask is by
/// names (any same-named class's member counts), the conservative answer.
pub fn single_pass_sync_direct_site_admitted(
    verdicts: &crate::JitVerdictRegistry,
    helpers: &JitRuntimeHelpers,
    direct_helpers: &crate::DirectHelperTable,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    ret: u8,
) -> bool {
    crate::direct_jit_callee_calls_enabled()
        && sync_direct_enabled()
        && crate::x64::merged_call_sentinel_enabled()
        && helpers.ldc_class_cp != 0
        && direct_helpers.monitor_enter != 0
        && direct_helpers.monitor_exit != 0
        && sync_direct_result_admitted(ret, helpers.dispatch_threw)
        && !verdicts.direct_call_requires_dispatch(class_name, method_name, descriptor)
        && !crate::offload_hook::keeps_dispatch_helper(class_name, method_name, descriptor)
}

/// The compiling VM's per-VM compile registries, the required inputs of
/// [`CompileRequest::new`]: the compile verdicts and the deferred-`new` retry
/// memo. Bundled so a request cannot take one VM's registry and silently fall
/// back to the process-wide other (interpreter round i1 wave 18, lane L5).
///
/// The VM builds one from its `SharedVm` (`jit_bridge::jit_compile_realm`); a
/// caller with no VM in scope names [`CompileRealm::process`]; a test that
/// wants a verdict registry of its own writes
/// `CompileRealm { verdicts: &mine, ..CompileRealm::process() }`. The
/// de-speculation registries (`despec`, `runtime_despec`) are not here: they
/// are deliberately fail-open on `None` (no VM, no runtime in which a
/// speculation failed).
#[derive(Clone, Copy)]
pub struct CompileRealm<'a> {
    /// See [`CompileRequest::verdicts`].
    pub verdicts: &'a crate::JitVerdictRegistry,
    /// See [`CompileRequest::deferred_new_retries`].
    pub deferred_new_retries: &'a crate::DeferredNewRetries,
}

impl CompileRealm<'static> {
    /// The registries of compiles with no VM in scope: the process-wide
    /// verdict registry and deferred-`new` memo, which no VM reads or sweeps.
    /// For the positional shim and tests only; a VM door passes its own.
    pub fn process() -> Self {
        CompileRealm {
            verdicts: crate::process_jit_verdicts(),
            deferred_new_retries: crate::jfr_compile_decision::process_deferred_new_retries(),
        }
    }
}

impl<'a> CompileRequest<'a> {
    /// A request with every optional input absent and every flag off:
    /// no resolvers, no profile, no optimizing tier. `realm` holds the
    /// registries the compile reads and records into (the compiling VM's; see
    /// [`CompileRealm`]), required so no request can fall back to a registry
    /// its VM never reads.
    pub fn new(
        cached: &'a CachedBytecodeMethod,
        helpers: &'a JitRuntimeHelpers,
        realm: CompileRealm<'a>,
    ) -> Self {
        let CompileRealm {
            verdicts,
            deferred_new_retries,
        } = realm;
        CompileRequest {
            cached,
            cp_class_name_resolver: None,
            cp_field_resolver: None,
            cp_static_field_resolver: None,
            cp_field_volatile_resolver: None,
            cp_invoke_resolver: None,
            cp_invokespecial_owner_resolver: None,
            callee_compiler: None,
            cp_new_resolver: None,
            cp_ldc_resolver: None,
            cp_ldc_string_hash_resolver: None,
            cp_ldc_string_utf16_resolver: None,
            cp_ldc2w_resolver: None,
            profile: None,
            helpers,
            inline_resolver: None,
            string_layout_resolver: None,
            cp_invoke_class_id_resolver: None,
            cp_elidable_init_resolver: None,
            optimize: false,
            ir_emit_calls: false,
            ir_emit_special_calls: false,
            ir_emit_long: false,
            ir_emit_virtual_calls: false,
            ir_emit_fp: false,
            cp_invokedynamic_descriptor_resolver: None,
            class_id_name_resolver: None,
            receiver_inline_resolver: None,
            unique_concrete_resolver: None,
            ir_inline_resolver: None,
            ir_exact_receiver_resolver: None,
            ir_receiver_inline_resolver: None,
            jdk_only: false,
            direct_bind_admission: None,
            despec: None,
            runtime_despec: None,
            cp_invoke_declaring_class_resolver: None,
            cp_invoke_declaring_class_id_resolver: None,
            self_call_identity_stable: false,
            direct_helpers: &crate::DirectHelperTable::EMPTY,
            cycle_edge_admission: None,
            class_is_final_resolver: None,
            sync_direct_lookup: None,
            deferred_new_retries,
            verdicts,
            osr_mode_exit_polls: false,
            osr_optimizing_route: false,
            debugger_observes_locals: false,
            self_lock_mirror_slot: 0,
        }
    }

    /// The verdict registry this compile reads and records into: the one its
    /// builder handed [`CompileRequest::new`]. No fallback.
    pub fn verdicts(&self) -> &'a crate::JitVerdictRegistry {
        self.verdicts
    }
}

#[cfg(test)]
mod r11w10_synccall_tests {
    use super::*;

    /// A sync row can never answer a plain-pc lookup, and two sites never
    /// share a key.
    #[test]
    fn a_sync_row_key_is_disjoint_from_every_plain_pc() {
        for pc in [0usize, 1, 3, 65_535, 1 << 20, u32::MAX as usize] {
            let key = sync_direct_row_key(pc);
            assert_ne!(key, pc);
            assert_eq!(key & !SYNC_DIRECT_ROW_TAG, pc);
        }
        assert_ne!(sync_direct_row_key(3), sync_direct_row_key(6));
    }

    use crate::ir::{IrBuilder, IrInlineFrameSites, Op};
    use std::collections::HashMap;

    /// `static int caller() { return incSync(1); }`:
    /// `iconst_1; invokestatic #5; ireturn`, the invoke at pc 1.
    const CALLER: [u8; 7] = [0x04, 0xb8, 0x00, 0x05, 0xac, 0, 0];
    const INVOKE_PC: usize = 1;

    fn leaked_sync_info() -> usize {
        let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
            class_name: "C",
            method_name: "incSync",
            descriptor: "(I)I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: 7,
            owner_class_id: 0,
        }));
        info as *const crate::JitInvokeInfo as usize
    }

    fn build_caller(sync_row: bool) -> crate::ir::Graph {
        let mut b = IrBuilder::new(0, 0);
        b.set_invoke_info(HashMap::from([(
            INVOKE_PC,
            (leaked_sync_info(), 1usize, b'I'),
        )]));
        if sync_row {
            b.set_sync_direct_calls(HashMap::from([(INVOKE_PC, (7u32, 12u16, 0usize))]));
        }
        b.build(&CALLER, 5).expect("the caller builds")
    }

    /// The builder's half: `ConstClass; MonitorEnter; Call; MonitorExit` on
    /// one object, threaded on the memory chain in that order, and no frame
    /// state names the lock (contract item 2).
    #[test]
    fn a_sync_row_brackets_the_call_with_an_unrecorded_class_monitor() {
        let g = build_caller(true);
        fn find(g: &crate::ir::Graph, pred: impl Fn(&Op) -> bool) -> Option<crate::ir::NodeId> {
            g.nodes
                .iter()
                .position(|n| pred(&n.op))
                .map(|i| i as crate::ir::NodeId)
        }
        let m = find(&g, |op| {
            matches!(
                op,
                Op::ConstClass {
                    holder_class_id: 7,
                    cp_idx: 12,
                    slot_addr: 0
                }
            )
        })
        .expect("the monitor is the Methodref's class mirror");
        let enter = find(&g, |op| *op == Op::MonitorEnter).expect("an enter");
        let call = find(&g, |op| matches!(op, Op::Call { .. })).expect("the call");
        let exit = find(&g, |op| *op == Op::MonitorExit).expect("an exit");
        let node = |id: crate::ir::NodeId| &g.nodes[id as usize];
        assert_eq!(node(enter).input_opt(1), Some(m));
        assert_eq!(node(enter).input_opt(2), Some(m));
        assert_eq!(node(call).input_opt(1), Some(enter));
        assert_eq!(node(exit).input_opt(1), Some(call));
        assert_eq!(node(exit).input_opt(2), Some(m));
        for id in [m, enter, call, exit] {
            assert_eq!(node(id).bytecode_pc, Some(INVOKE_PC));
        }
        assert!(
            g.safepoints.iter().all(|sp| sp.monitors.is_empty()),
            "no frame state may name a caller-held method monitor"
        );
        // Without the row the site is an ordinary call.
        let plain = build_caller(false);
        assert!(!plain.nodes.iter().any(|n| matches!(
            n.op,
            Op::MonitorEnter | Op::MonitorExit | Op::ConstClass { .. }
        )));
    }

    extern "C" fn fake_enter(_vm: i64, obj: i64) -> i64 {
        obj
    }
    extern "C" fn fake_exit(_vm: i64, _obj: i64) -> i64 {
        1
    }
    extern "C" fn fake_ldc_class(_vm: i64, _holder: i64, _cp: i64) -> i64 {
        0x1000
    }
    extern "C" fn fake_callee(_arg: i64) -> i64 {
        0
    }

    fn lower_caller(
        graph: &crate::ir::Graph,
        rows: &HashMap<usize, (usize, bool)>,
    ) -> Option<crate::CompiledMethod> {
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
        let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        helpers.monitor_enter = fake_enter as *const () as usize;
        helpers.monitor_exit = fake_exit as *const () as usize;
        helpers.ldc_class_cp = fake_ldc_class as *const () as usize;
        let schedule = crate::ir_schedule::schedule(graph);
        crate::ir_lower::lower_inner(
            graph,
            &schedule,
            0,
            0,
            &helpers,
            &HashMap::new(),
            &[],
            None,
            rows,
            &HashMap::new(),
            &HashMap::new(),
            &IrInlineFrameSites::default(),
        )
    }

    /// The lowerer's half: the tagged row binds a raw CALL to the callee, and
    /// the class monitor is released on BOTH edges (the `MonitorExit` node and
    /// the exceptional-edge release), each through the exit helper when the
    /// thin path declines.
    #[test]
    fn a_tagged_row_lowers_to_a_direct_call_released_on_both_edges() {
        if !crate::x64::merged_call_sentinel_enabled() {
            return;
        }
        let entry = fake_callee as *const () as usize;
        let rows = HashMap::from([(sync_direct_row_key(INVOKE_PC), (entry, false))]);
        let cm = lower_caller(&build_caller(true), &rows).expect("the caller lowers");
        let bytes = cm.code_bytes();
        let count = |addr: usize| {
            let needle = addr.to_le_bytes();
            bytes.windows(8).filter(|w| *w == needle).count()
        };
        assert_eq!(count(entry), 1, "one raw CALL to the synchronized body");
        assert_eq!(count(fake_enter as *const () as usize), 1);
        assert_eq!(
            count(fake_exit as *const () as usize),
            2,
            "normal and exceptional release"
        );
    }

    /// Fail closed: a tagged row whose graph carries no monitor pair (the
    /// builder was not told) is refused, never lowered to a monitor-free CALL.
    #[test]
    fn a_tagged_row_without_its_monitor_pair_refuses_the_compile() {
        let entry = fake_callee as *const () as usize;
        let rows = HashMap::from([(sync_direct_row_key(INVOKE_PC), (entry, false))]);
        assert!(lower_caller(&build_caller(false), &rows).is_none());
    }
}

/// Round 11 wave 11 (lane `calls`): the INSTANCE twin of the caller-held
/// synchronized direct CALL -- the receiver is the monitor, a null receiver is
/// guarded BEFORE the enter, and two pairs lock coarsening merged still lower.
#[cfg(test)]
mod r11w11_calls_instance_sync_tests {
    use super::*;
    use crate::ir::{Graph, IrBuilder, IrInlineFrameSites, IrType, NodeId, Op};
    use std::collections::HashMap;

    fn leaked_instance_info() -> usize {
        let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
            class_name: "C",
            method_name: "inc",
            descriptor: "(I)I",
            num_jit_args: 2,
            return_type: b'I',
            invoke_kind: 1,
            declaring_class_id: 7,
            owner_class_id: 0,
        }));
        info as *const crate::JitInvokeInfo as usize
    }

    /// `static int caller(C c) { return c.inc(1); }` (`invokevirtual`, pc 2)
    /// or the same with `invokespecial` (`super.inc(1)` on `this`): `aload_0;
    /// iconst_1; invoke #5; ireturn`.
    fn one_call(opcode: u8) -> [u8; 8] {
        [0x2a, 0x04, opcode, 0x00, 0x05, 0xac, 0, 0]
    }

    /// `c.inc(1) + c.inc(2)`: invokes at pc 2 and pc 7, 12 bytes plus the
    /// VM's two padding bytes.
    const TWO_CALLS: [u8; 14] = [
        0x2a, 0x04, 0xb6, 0x00, 0x05, // aload_0; iconst_1; invokevirtual #5
        0x2a, 0x05, 0xb6, 0x00, 0x05, // aload_0; iconst_2; invokevirtual #5
        0x60, 0xac, // iadd; ireturn
        0x00, 0x00,
    ];

    fn build(code: &[u8], code_len: usize, sync_pcs: &[usize]) -> Graph {
        let info = leaked_instance_info();
        let mut b = IrBuilder::new(1, 1);
        b.set_param_types(&[IrType::Ref]);
        let mut invoke = HashMap::new();
        let mut sync = HashMap::new();
        for pc in [2usize, 7] {
            if pc + 3 <= code_len && matches!(code[pc], 0xb6 | 0xb7) {
                invoke.insert(pc, (info, 2usize, b'I'));
            }
        }
        for &pc in sync_pcs {
            sync.insert(pc, (0u32, 0u16, 0usize));
        }
        b.set_invoke_info(invoke);
        b.set_sync_direct_calls(sync);
        b.build(code, code_len).expect("the caller builds")
    }

    fn find_all(g: &Graph, pred: impl Fn(&Op) -> bool) -> Vec<NodeId> {
        g.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| pred(&n.op))
            .map(|(i, _)| i as NodeId)
            .collect()
    }

    /// The builder's half, for both statically bound opcodes: a guard on the
    /// receiver at the invoke's bci BEFORE the enter, the enter and exit on
    /// the receiver itself (no class mirror), the call between them on the
    /// memory chain, and no frame state naming the lock.
    #[test]
    fn an_instance_sync_row_guards_the_receiver_before_locking_it() {
        for opcode in [0xb6u8, 0xb7] {
            let code = one_call(opcode);
            let g = build(&code, 6, &[2]);
            let receiver = find_all(&g, |op| matches!(op, Op::Param(0)))[0];
            let guards = find_all(&g, |op| matches!(op, Op::Guard { bci: 2 }));
            let enters = find_all(&g, |op| *op == Op::MonitorEnter);
            let calls = find_all(&g, |op| matches!(op, Op::Call { .. }));
            let exits = find_all(&g, |op| *op == Op::MonitorExit);
            assert_eq!(
                (guards.len(), enters.len(), calls.len(), exits.len()),
                (1, 1, 1, 1),
                "opcode {opcode:#04x}"
            );
            let (guard, enter, call, exit) = (guards[0], enters[0], calls[0], exits[0]);
            assert!(
                guard < enter,
                "the null check precedes every monitor action"
            );
            let node = |id: NodeId| &g.nodes[id as usize];
            assert_eq!(node(enter).input_opt(2), Some(receiver));
            assert_eq!(node(call).input_opt(1), Some(enter));
            assert_eq!(node(call).input_opt(2), Some(receiver));
            assert_eq!(node(exit).input_opt(1), Some(call));
            assert_eq!(node(exit).input_opt(2), Some(receiver));
            assert!(find_all(&g, |op| matches!(op, Op::ConstClass { .. })).is_empty());
            assert!(
                g.safepoints.iter().all(|sp| sp.monitors.is_empty()),
                "no frame state may name a caller-held method monitor"
            );
        }
        // Without the row the site is an ordinary call.
        let plain = build(&one_call(0xb6), 6, &[]);
        assert!(find_all(&plain, |op| matches!(
            op,
            Op::MonitorEnter | Op::MonitorExit | Op::Guard { .. }
        ))
        .is_empty());
    }

    extern "C" fn fake_enter(_vm: i64, obj: i64) -> i64 {
        obj
    }
    extern "C" fn fake_exit(_vm: i64, _obj: i64) -> i64 {
        1
    }
    extern "C" fn fake_instance_callee(_recv: i64, _arg: i64) -> i64 {
        0
    }

    fn lower(graph: &Graph, rows: &HashMap<usize, (usize, bool)>) -> Option<crate::CompiledMethod> {
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
        let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        helpers.monitor_enter = fake_enter as *const () as usize;
        helpers.monitor_exit = fake_exit as *const () as usize;
        let schedule = crate::ir_schedule::schedule(graph);
        crate::ir_lower::lower_inner(
            graph,
            &schedule,
            1,
            1,
            &helpers,
            &HashMap::new(),
            &[],
            None,
            rows,
            &HashMap::new(),
            &HashMap::new(),
            &IrInlineFrameSites::default(),
        )
    }

    fn count(bytes: &[u8], addr: usize) -> usize {
        let needle = addr.to_le_bytes();
        bytes.windows(8).filter(|w| *w == needle).count()
    }

    /// Drop the receiver guards (and nothing else) so these lowering tests
    /// exercise the monitor pair alone; the guard is the builder test's.
    fn without_guards(mut g: Graph) -> Graph {
        for id in find_all(&g, |op| matches!(op, Op::Guard { .. })) {
            g.kill(id);
        }
        g
    }

    /// The lowerer's half: one raw CALL to the callee, the receiver locked
    /// once and released on both edges.
    #[test]
    fn an_instance_row_lowers_to_a_direct_call_released_on_both_edges() {
        if !crate::x64::merged_call_sentinel_enabled() {
            return;
        }
        let entry = fake_instance_callee as *const () as usize;
        let rows = HashMap::from([(sync_direct_row_key(2), (entry, false))]);
        let g = without_guards(build(&one_call(0xb6), 6, &[2]));
        let cm = lower(&g, &rows).expect("the caller lowers");
        let bytes = cm.code_bytes();
        assert_eq!(
            count(bytes, entry),
            1,
            "one raw CALL to the synchronized body"
        );
        assert_eq!(count(bytes, fake_enter as *const () as usize), 1);
        assert_eq!(
            count(bytes, fake_exit as *const () as usize),
            2,
            "normal and exceptional release"
        );
        // Fail closed: the tagged row with no pair in the graph.
        let bare = without_guards(build(&one_call(0xb6), 6, &[]));
        assert!(lower(&bare, &rows).is_none());
    }

    /// `c.inc(1) + c.inc(2)` after lock coarsening merged the first exit with
    /// the second enter (`ir_optimize::coarsen_adjacent_monitors`, simulated
    /// here exactly as it rewires): one enter, both calls on the chain, one
    /// exit. Both calls still lower to the caller-held route -- each with its
    /// own exceptional release -- instead of refusing the method.
    #[test]
    fn a_coarsened_pair_of_instance_calls_still_lowers() {
        if !crate::x64::merged_call_sentinel_enabled() {
            return;
        }
        let entry = fake_instance_callee as *const () as usize;
        let rows = HashMap::from([
            (sync_direct_row_key(2), (entry, false)),
            (sync_direct_row_key(7), (entry, false)),
        ]);
        let mut g = without_guards(build(&TWO_CALLS, 12, &[2, 7]));
        let enters = find_all(&g, |op| *op == Op::MonitorEnter);
        let exits = find_all(&g, |op| *op == Op::MonitorExit);
        assert_eq!((enters.len(), exits.len()), (2, 2));
        let (first_exit, second_enter) = (exits[0], enters[1]);
        let token = g.nodes[first_exit as usize]
            .input_opt(1)
            .expect("the first exit has a token");
        g.replace_all_uses(second_enter, token);
        g.kill(second_enter);
        g.kill(first_exit);
        let cm = lower(&g, &rows).expect("the coarsened chain lowers");
        let bytes = cm.code_bytes();
        assert_eq!(count(bytes, entry), 2);
        assert_eq!(count(bytes, fake_enter as *const () as usize), 1);
        assert_eq!(
            count(bytes, fake_exit as *const () as usize),
            3,
            "one normal release and one exceptional release per call"
        );
    }
}

/// Round 11 wave 12 (lane `calls`, proposal 6): the caller-held synchronized
/// direct CALL for `long`/`float`/`double` and REFERENCE results, which waves
/// 10-11 refused (`sync_direct_result_admitted`).
#[cfg(test)]
mod r11w12_calls_sync_result_tests {
    use super::*;
    use crate::ir::{IrBuilder, IrInlineFrameSites};
    use std::collections::HashMap;

    const INVOKE_PC: usize = 1;

    extern "C" fn fake_enter(_vm: i64, obj: i64) -> i64 {
        obj
    }
    extern "C" fn fake_exit(_vm: i64, _obj: i64) -> i64 {
        1
    }
    extern "C" fn fake_ldc_class(_vm: i64, _holder: i64, _cp: i64) -> i64 {
        0x1000
    }
    // Distinct bodies: a release link folds identical functions (`/OPT:ICF`),
    // and two `return 0` fakes sharing one address make every count below
    // count both.
    extern "C" fn fake_threw() -> i64 {
        0
    }
    extern "C" fn fake_callee(arg: i64) -> i64 {
        arg ^ 0x5a5a
    }

    /// `static R caller() { return syncValue(1); }`: `iconst_1; invokestatic
    /// #5; <ret_op>`, the invoke at pc 1, with its caller-held sync row.
    fn build_caller(ret: u8, ret_op: u8) -> crate::ir::Graph {
        let descriptor: &'static str = match ret {
            b'J' => "(I)J",
            _ => "(I)Ljava/lang/Object;",
        };
        let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
            class_name: "C",
            method_name: "syncValue",
            descriptor,
            num_jit_args: 1,
            return_type: ret,
            invoke_kind: 3,
            declaring_class_id: 7,
            owner_class_id: 0,
        }));
        let code = [0x04, 0xb8, 0x00, 0x05, ret_op, 0, 0];
        let mut b = IrBuilder::new(0, 0);
        b.set_invoke_info(HashMap::from([(
            INVOKE_PC,
            (info as *const crate::JitInvokeInfo as usize, 1usize, ret),
        )]));
        b.set_sync_direct_calls(HashMap::from([(INVOKE_PC, (7u32, 12u16, 0usize))]));
        b.build(&code, 5).expect("the caller builds")
    }

    fn lower(graph: &crate::ir::Graph, dispatch_threw: usize) -> Option<crate::CompiledMethod> {
        let entry = fake_callee as *const () as usize;
        let rows = HashMap::from([(sync_direct_row_key(INVOKE_PC), (entry, false))]);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
        let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        helpers.monitor_enter = fake_enter as *const () as usize;
        helpers.monitor_exit = fake_exit as *const () as usize;
        helpers.ldc_class_cp = fake_ldc_class as *const () as usize;
        helpers.dispatch_threw = dispatch_threw;
        let schedule = crate::ir_schedule::schedule(graph);
        crate::ir_lower::lower_inner(
            graph,
            &schedule,
            0,
            0,
            &helpers,
            &HashMap::new(),
            &[],
            None,
            &rows,
            &HashMap::new(),
            &HashMap::new(),
            &IrInlineFrameSites::default(),
        )
    }

    fn count(bytes: &[u8], addr: usize) -> usize {
        let needle = addr.to_le_bytes();
        bytes.windows(8).filter(|w| *w == needle).count()
    }

    #[test]
    fn the_planner_admits_every_result_kind_and_wide_ones_only_with_the_peek() {
        for ret in [b'V', b'I', b'Z', b'B', b'C', b'S', b'L', b'['] {
            assert!(sync_direct_result_admitted(ret, 0), "{}", ret as char);
        }
        for ret in [b'J', b'F', b'D'] {
            assert!(!sync_direct_result_admitted(ret, 0), "{}", ret as char);
            assert!(sync_direct_result_admitted(ret, 0x1234), "{}", ret as char);
        }
        assert!(!sync_direct_result_admitted(b'X', 0x1234));
    }

    /// A `long` result: one raw CALL, the class monitor released on both
    /// edges, and the `dispatch_threw` peek on the cold side so a genuine
    /// `Long.MIN_VALUE` keeps the normal edge.
    #[test]
    fn a_long_result_binds_with_the_dispatch_threw_peek() {
        if !crate::x64::merged_call_sentinel_enabled() {
            return;
        }
        let threw = fake_threw as *const () as usize;
        let cm = lower(&build_caller(b'J', 0xad), threw).expect("the caller lowers");
        let bytes = cm.code_bytes();
        assert_eq!(count(bytes, fake_callee as *const () as usize), 1);
        assert!(count(bytes, threw) >= 1, "the cold side peeks the signal");
        assert_eq!(count(bytes, fake_enter as *const () as usize), 1);
        assert_eq!(count(bytes, fake_exit as *const () as usize), 2);
    }

    /// Fail closed: without the peek helper a wide result cannot be told from
    /// the sentinel, so the lowerer refuses the compile.
    #[test]
    fn a_long_result_without_the_peek_refuses_the_compile() {
        assert!(lower(&build_caller(b'J', 0xad), 0).is_none());
    }

    /// A reference result: bound and released on both edges, with no peek
    /// (a reference is never `i64::MIN`).
    #[test]
    fn a_reference_result_binds_and_is_released_on_both_edges() {
        if !crate::x64::merged_call_sentinel_enabled() {
            return;
        }
        let threw = fake_threw as *const () as usize;
        let cm = lower(&build_caller(b'L', 0xb0), threw).expect("the caller lowers");
        let bytes = cm.code_bytes();
        assert_eq!(count(bytes, fake_callee as *const () as usize), 1);
        assert_eq!(count(bytes, threw), 0, "no peek for a reference result");
        assert_eq!(count(bytes, fake_exit as *const () as usize), 2);
    }
}

/// Round 11 wave 13 (lane `cha`, page
/// `r11w12-calls-cha-sync-target-needs-a-class-guard-before-the-enter`): a
/// CHA-bound caller-held synchronized `invokevirtual` takes an exact receiver
/// class guard BEFORE its monitor enter, and `Op::ExactClassIs` answers
/// exactly "non-null, plain object, this class id".
#[cfg(test)]
mod r11w13_cha_sync_tests {
    use super::*;
    use crate::ir::{Graph, IrBuilder, IrInlineFrameSites, IrType, NodeId, Op};
    use std::collections::HashMap;

    const CLASS_ID: u32 = 0x0012_3457;

    fn bind(implementor: u32, static_type: u32) -> Option<crate::UniqueConcreteBind> {
        Some(crate::UniqueConcreteBind {
            implementor_class_id: implementor,
            static_type_class_id: static_type,
            static_type_name: "C".to_string(),
        })
    }

    #[test]
    fn the_planner_guards_only_on_the_methodref_class_itself() {
        assert_eq!(cha_sync_exact_class(bind(7, 7), Some(7)), Some(7));
        // A single concrete SUBCLASS provides the body: a guard on it would
        // select an inherited or overriding body the VM did not answer for.
        assert_eq!(cha_sync_exact_class(bind(8, 7), Some(7)), None);
        // The two resolvers disagree about which class the Methodref names.
        assert_eq!(cha_sync_exact_class(bind(7, 7), Some(9)), None);
        assert_eq!(cha_sync_exact_class(bind(7, 7), None), None);
        // No hierarchy answer ("many", or CHA switched off).
        assert_eq!(cha_sync_exact_class(None, Some(7)), None);
        // Zero is the builder's "no class guard" row.
        assert_eq!(cha_sync_exact_class(bind(0, 0), Some(0)), None);
    }

    fn leaked_info() -> usize {
        let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
            class_name: "C",
            method_name: "inc",
            descriptor: "(I)I",
            num_jit_args: 2,
            return_type: b'I',
            // The planner re-marks a guarded CHA site statically bound.
            invoke_kind: 1,
            declaring_class_id: 7,
            owner_class_id: 0,
        }));
        info as *const crate::JitInvokeInfo as usize
    }

    /// `static int caller(C c) { return c.inc(1); }`: `aload_0; iconst_1;
    /// invokevirtual #5; ireturn`, the invoke at pc 2.
    const CALLER: [u8; 8] = [0x2a, 0x04, 0xb6, 0x00, 0x05, 0xac, 0, 0];

    fn build(row_class_id: u32) -> Graph {
        let mut b = IrBuilder::new(1, 1);
        b.set_param_types(&[IrType::Ref]);
        b.set_invoke_info(HashMap::from([(2usize, (leaked_info(), 2usize, b'I'))]));
        b.set_sync_direct_calls(HashMap::from([(2usize, (row_class_id, 0u16, 0usize))]));
        b.build(&CALLER, 6).expect("the caller builds")
    }

    fn find_all(g: &Graph, pred: impl Fn(&Op) -> bool) -> Vec<NodeId> {
        g.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| pred(&n.op))
            .map(|(i, _)| i as NodeId)
            .collect()
    }

    /// `Guard(recv != null); Guard(ExactClassIs(recv)); MonitorEnter recv`,
    /// all at the invoke's bci and in that order; a pinned row (`0`) has no
    /// class guard at all.
    #[test]
    fn a_cha_row_guards_the_exact_class_before_the_enter() {
        let g = build(CLASS_ID);
        let receiver = find_all(&g, |op| matches!(op, Op::Param(0)))[0];
        let exact = find_all(&g, |op| *op == Op::ExactClassIs { class_id: CLASS_ID });
        assert_eq!(exact.len(), 1, "one class test");
        let exact = exact[0];
        let node = |id: NodeId| &g.nodes[id as usize];
        assert_eq!(node(exact).inputs, vec![receiver]);
        let guards = find_all(&g, |op| matches!(op, Op::Guard { bci: 2 }));
        assert_eq!(guards.len(), 2, "the null guard and the class guard");
        let class_guard = guards
            .iter()
            .copied()
            .find(|&gd| node(gd).input_opt(1) == Some(exact))
            .expect("a guard consumes the class test");
        let enter = find_all(&g, |op| *op == Op::MonitorEnter)[0];
        assert!(guards.iter().all(|&gd| gd < enter));
        assert!(class_guard < enter, "the class guard precedes the enter");
        assert_eq!(node(enter).input_opt(2), Some(receiver));
        assert!(g.safepoints.iter().all(|sp| sp.monitors.is_empty()));

        let pinned = build(0);
        assert!(find_all(&pinned, |op| matches!(op, Op::ExactClassIs { .. })).is_empty());
        assert_eq!(
            find_all(&pinned, |op| matches!(op, Op::Guard { .. })).len(),
            1,
            "a statically bound row keeps the null guard only"
        );
    }

    extern "C" fn fake_enter(_vm: i64, obj: i64) -> i64 {
        obj
    }
    extern "C" fn fake_exit(_vm: i64, _obj: i64) -> i64 {
        1
    }
    extern "C" fn fake_callee(_recv: i64, _arg: i64) -> i64 {
        0
    }

    /// The lowerer's half, guards included: the class test is emitted (its
    /// `CMP DWORD [RAX+0], id`), nothing that can deopt lands inside the held
    /// window (`sync_direct_call_monitor` would refuse the compile), and the
    /// callee is CALLed raw once with the receiver locked once.
    #[test]
    fn a_cha_row_lowers_with_both_guards_outside_the_window() {
        if !crate::x64::merged_call_sentinel_enabled() {
            return;
        }
        let g = build(CLASS_ID);
        let entry = fake_callee as *const () as usize;
        let rows = HashMap::from([(sync_direct_row_key(2), (entry, false))]);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
        let mut helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        helpers.monitor_enter = fake_enter as *const () as usize;
        helpers.monitor_exit = fake_exit as *const () as usize;
        let schedule = crate::ir_schedule::schedule(&g);
        let cm = crate::ir_lower::lower_inner(
            &g,
            &schedule,
            1,
            1,
            &helpers,
            &HashMap::new(),
            &[],
            None,
            &rows,
            &HashMap::new(),
            &HashMap::new(),
            &IrInlineFrameSites::default(),
        )
        .expect("the guarded caller lowers");
        let bytes = cm.code_bytes();
        let mut cmp = vec![0x81u8, 0x78, 0x00];
        cmp.extend_from_slice(&CLASS_ID.to_le_bytes());
        assert!(
            bytes.windows(cmp.len()).any(|w| w == cmp.as_slice()),
            "the exact class compare is emitted"
        );
        let count = |addr: usize| {
            let needle = addr.to_le_bytes();
            bytes.windows(8).filter(|w| *w == needle).count()
        };
        assert_eq!(count(entry), 1, "one raw CALL to the synchronized body");
        assert_eq!(count(fake_enter as *const () as usize), 1);
    }

    /// EXECUTED: `int f(Object o) { return ExactClassIs(o); }` against
    /// hand-made headers -- the class id word at offset 0 and the kind byte.
    #[test]
    fn exact_class_is_answers_non_null_plain_objects_of_that_class_only() {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let _mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let obj = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let test = g.add(
            Op::ExactClassIs { class_id: CLASS_ID },
            IrType::Int,
            vec![obj],
            Some(0),
        );
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, test], Some(0));
        let schedule = crate::ir_schedule::schedule(&g);
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; this body calls no helper.
        let helpers: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        let cm = crate::ir_lower::lower(&g, &schedule, 1, 1, &helpers).expect("lowers");
        assert!(
            cratonvm_types::KIND_TAGS_BYTE_OFFSET < 32,
            "precondition: the kind byte lies in the fake header"
        );
        let run = |class_id: u32, kind_byte: u8| -> i32 {
            let mut header = [0u64; 4];
            let base = header.as_mut_ptr().cast::<u8>();
            // SAFETY: both writes are inside the 32-byte, 8-aligned `header`.
            unsafe {
                base.cast::<u32>().write(class_id);
                *base.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET) = kind_byte;
            }
            // SAFETY: an `int f(ref)` body with no call and no guard; the
            // reference is a live 32-byte buffer. Cast: pointer as the oop.
            let got = unsafe { cm.try_call(&[base as i64]) }.expect("call");
            // Cast: an `Int` result is the low 32 bits.
            got as i32
        };
        let plain = cratonvm_types::ObjectKind::Object as u8;
        assert_eq!(run(CLASS_ID, plain), 1, "exact class, plain object");
        assert_eq!(run(CLASS_ID + 1, plain), 0, "another class");
        assert_eq!(
            run(CLASS_ID ^ 0x0001_0000, plain),
            0,
            "a high-half difference"
        );
        assert_eq!(
            run(CLASS_ID, plain | 1),
            0,
            "an array whose COMPONENT is the class"
        );
        // SAFETY: as above; a null reference is answered without a load.
        let null = unsafe { cm.try_call(&[0]) }.expect("call");
        // Cast: an `Int` result is the low 32 bits.
        assert_eq!(null as i32, 0, "null");
    }
}

/// Round 11 wave 14 (lane `spsync`, page
/// `r11w13-spsync-osr-door-has-no-sync-direct-route`): the one single-pass
/// gate, asked by `build_single_pass_tables` and by the VM's OSR door.
#[cfg(test)]
mod r11w14_spsync_single_pass_gate_tests {
    /// The gate over the no-VM registry, which no test here puts a cycle in.
    fn admitted(
        helpers: &crate::JitRuntimeHelpers,
        direct_helpers: &crate::DirectHelperTable,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        ret: u8,
    ) -> bool {
        super::single_pass_sync_direct_site_admitted(
            crate::process_jit_verdicts(),
            helpers,
            direct_helpers,
            class_name,
            method_name,
            descriptor,
            ret,
        )
    }

    fn tables(
        ldc_class_cp: usize,
        dispatch_threw: usize,
        enter: usize,
        exit: usize,
    ) -> (crate::JitRuntimeHelpers, crate::DirectHelperTable) {
        let mut helpers = crate::JitRuntimeHelpers::default();
        helpers.ldc_class_cp = ldc_class_cp;
        helpers.dispatch_threw = dispatch_threw;
        let direct = crate::DirectHelperTable {
            monitor_enter: enter,
            monitor_exit: exit,
            ..crate::DirectHelperTable::EMPTY
        };
        (helpers, direct)
    }

    /// Each helper the backend arm emits is required: without it the site
    /// keeps the dispatch helper, whatever the flags say.
    #[test]
    fn a_missing_helper_refuses_the_site() {
        for (ldc, enter, exit) in [(0, 3, 4), (1, 0, 4), (1, 3, 0)] {
            let (h, d) = tables(ldc, 2, enter, exit);
            assert!(!admitted(&h, &d, "r11w14/SpsyncGate", "inc", "()V", b'V'));
        }
        // A wide result needs the `dispatch_threw` peek.
        let (h, d) = tables(1, 0, 3, 4);
        assert!(!admitted(&h, &d, "r11w14/SpsyncGate", "get", "()J", b'J'));
        assert!(!admitted(&h, &d, "r11w14/SpsyncGate", "bad", "()X", b'X'));
    }

    /// With every helper wired, the site is admitted exactly when the three
    /// flags are on (their defaults).
    #[test]
    fn a_fully_wired_site_is_admitted_under_the_default_flags() {
        let on = crate::direct_jit_callee_calls_enabled()
            && super::sync_direct_enabled()
            && crate::x64::merged_call_sentinel_enabled();
        let (h, d) = tables(1, 2, 3, 4);
        assert_eq!(
            admitted(&h, &d, "r11w14/SpsyncGate", "inc", "()V", b'V'),
            on
        );
        assert_eq!(
            admitted(&h, &d, "r11w14/SpsyncGate", "get", "()J", b'J'),
            on
        );
        assert_eq!(
            admitted(
                &h,
                &d,
                "r11w14/SpsyncGate",
                "obj",
                "()Ljava/lang/Object;",
                b'L'
            ),
            on
        );
    }
}
