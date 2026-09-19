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
    pub cp_invokespecial_owner_resolver: Option<&'a dyn Fn(u16, u8) -> Option<String>>,

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

    /// JDK-ONLY-WAVE2 §4: asks the registry whether a triple is a reviewed
    /// `NativeKind::Intrinsic`, i.e. the §1.4 exception that MAY shadow
    /// concrete bytecode. Returns `false` for `Bridge`, for `SyntheticStub`,
    /// and for a triple the registry has never heard of.
    ///
    /// This is the policy half of the seven thin direct-call ladders below.
    /// Before it existed those ladders were refused wholesale under `JdkOnly`,
    /// which is stricter than the contract: three of the seven are registered
    /// `Intrinsic` and §1.4 permits exactly those.
    ///
    /// `None` refuses everything, which is the pre-2026-08-06 behaviour and the
    /// fail-closed direction — a compile with no way to ask cannot bake a
    /// native in front of real bytes.
    pub intrinsic_resolver: Option<&'a dyn Fn(&str, &str, &str) -> bool>,

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
}

impl<'a> CompileRequest<'a> {
    /// A request with every optional input absent and every flag off:
    /// no resolvers, no profile, no optimizing tier.
    pub fn new(cached: &'a CachedBytecodeMethod, helpers: &'a JitRuntimeHelpers) -> Self {
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
            jdk_only: false,
            intrinsic_resolver: None,
            despec: None,
            runtime_despec: None,
            cp_invoke_declaring_class_resolver: None,
            self_call_identity_stable: false,
            direct_helpers: &crate::DirectHelperTable::EMPTY,
            cycle_edge_admission: None,
            class_is_final_resolver: None,
        }
    }
}
