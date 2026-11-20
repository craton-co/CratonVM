#[allow(unused_imports)]
use super::*;

// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// A compiled `invokevirtual java/lang/invoke/MethodHandle.invokeExact` reaches
// `jit_invoke_virtual_mic` (or `jit_invoke_dispatch`) with the handle as its
// receiver, and from there the native site cache runs the door: the native
// transition, the direct lane, and the target's call from the native side
// (`R16MhFloor`: `exact-static` ~1.5 us against HotSpot's ~36 ns on Linux,
// `native-floor` 197 ns). When the handle is the same object call after call
// -- a `static final` field, a loop-invariant local -- the site is BOUND here
// to a direct dispatch of the handle's target: the helper re-enters
// `jit_invoke_dispatch_body` with a `JitInvokeInfo` naming the target and the
// caller's own argument slots past the handle, exactly as if the caller had
// compiled `invokestatic C.m` / `invokevirtual C.m` at that site. The guard is
// the handle's identity (its address within one collection epoch, plus the
// slots the admission decided on: `MH_COMPILED_BIND_GUARD_SLOTS`), checked on
// every call; anything else, a null receiver included, takes the door as
// before. The admission is the native side's
// (`cratonvm_native_builtins::lang_invoke::mh_compiled_bind_target`): only
// handles whose door does nothing but that call for every argument a verified
// caller can pass.

/// `CRATONVM_JIT_MH_CONSTANT_BIND` -- default ON since round 16 (lane mh;
/// MH15-5), `0`/`false`/`off`/`no` is the kill switch: compiled
/// `MethodHandle.invokeExact` sites whose handle stays the same object are
/// bound to a direct call of its target ([`mh_constant_bind_redirect`]).
/// Measured (`R16MhFloor`, Linux): exact-static 1599 -> 403 ns, exact-virtual
/// 1954 -> 534 ns, exact-local 1593 -> 401 ns. The first flip was reverted: a
/// target that deopted was resumed against the bootstrap loader
/// (`r16-mh-constant-bind-callee-resume-refused-FIXED-20261001.md`, fixed by the
/// target's class id in [`mh_bind_leaf_info`]); re-flipped after 0/90 loaded
/// runs. Read once.
pub fn mh_constant_bind_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

/// One bound (or refused) `invokeExact` site of this thread.
pub struct MhBindSite {
    pub vm: usize,
    pub epoch: u64,
    /// The handle's address, then the values of
    /// `MH_COMPILED_BIND_GUARD_SLOTS` ([`mh_bind_slot_key`]).
    pub guard: [u64; 7],
    /// The target's dispatch info, or `None`: the admission refused this handle.
    pub leaf: Option<&'static JitInvokeInfo>,
    /// The admission bound a `bindTo` handle (round 16 wave 8,
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_BOUND`): the call's receiver is the guard
    /// word of `MH_BOUND`, spliced in place of the handle ([`mh_constant_bind_call`]).
    pub bound: bool,
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT` as the fill read it (round 16
    /// wave 10, lane mhfloor; MH16-8): the bound call enters the target's
    /// cached compiled entry without the dispatch ladder
    /// ([`mh_bind_direct_call`]). Carried here so the warm path reads no flag.
    pub direct: bool,
    /// The bound handle's shape (round 16 wave 11, lane mhbind; MHF-1,
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_SHAPE`): another handle of this shape
    /// in this epoch is called as this site's target ([`mh_bind_shape_matches`]).
    /// `None`: the switch was off at the fill, or the admission refused.
    pub shape: Option<MhBindShape>,
    /// The converting bind's per-call work (round 16 wave 15, lane mh6; MHB-1,
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING`), or `None` for a site that
    /// calls its target with the caller's own words.
    pub conv: Option<MhBindConv>,
    /// Cold fills so far; a site whose handle keeps changing stops binding
    /// ([`MH_BIND_MAX_FILLS`]).
    pub fills: u32,
    /// [`MhBindHit::real`].
    pub real: bool,
    /// [`MhBindHit::iface`].
    pub iface: u32,
    /// The last receiver class the interface guard accepted at this site
    /// (`0`: none yet), so a monomorphic site's guard is one compare. Reset
    /// with the site on every fill (a new epoch re-fills, so an unloaded
    /// class's id never survives here).
    pub iface_seen: std::cell::Cell<u32>,
    /// JIT r22 retirement (lane inline; the wave-3 count's cost): the bound
    /// REAL handle (`guard[0]`, in `epoch`) was seen with a CUSTOMIZED form,
    /// so its later calls need no `checkCustomized` read
    /// ([`mh_bind_customization_serves`]). Exact: JDK 25's `customize()` is
    /// one-way and `LambdaForm.customized` is `final`; the only other
    /// `MethodHandle.updateForm` caller, `DirectMethodHandle.ensureInitialized`,
    /// runs on the handle's first call, before any customization. Reset with
    /// the row on every fill (a new epoch re-fills).
    pub customized_seen: std::cell::Cell<bool>,
    /// gcj j3/rt3 (`gcj-j2jt2-mh-site-memos-keyed-by-a-reusable-jit-invoke-info-address`):
    /// the row's proof that the `JitInvokeInfo` address it is keyed by still
    /// names the site that filled it ([`MhSiteIdentity`]).
    pub identity: MhSiteIdentity,
    /// A REAL `bindTo` result's shape (JIT round 21 wave 17, lane mhbind;
    /// MHB-3, `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE`): another species
    /// instance of this shape in this epoch is called as this site's target
    /// with ITS bound value as the receiver ([`mh_real_bound_shape_receiver`]).
    /// `None`: not a real bound site, or the switch was off at the fill.
    pub real_shape: Option<MhRealBoundShape>,
    /// A REAL `asType` result's per-call conversions (j27 wave 5, lane
    /// MHASTYPE; `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING`), or `None`.
    pub real_conv: Option<MhRealConv>,
}

/// What a REAL `bindTo` result's site compares on a species instance at
/// another address (JIT round 21 wave 17, lane mhbind; MHB-3): the class, the
/// direct handle (`argL0`), the `form` and the `type` words of the admitted
/// handle, and the exact class of the bound value (`argL1`). The admission
/// (`lang_invoke::mh_real_bound_compiled_bind`) decides on nothing else: the
/// direct handle and the form are the same objects (an equal address in an
/// equal epoch), so they name the same member and the same `bindTo` shape;
/// the type is the same object, so the site's exact type; and a bound value of
/// the admitted value's exact class passes the admission's receiver question
/// as that value did. Every field is `final` (a `customize()`d form is that
/// one instance's own object, so a different instance never matches it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MhRealBoundShape {
    /// The species class (`BoundMethodHandle$Species_LL`).
    pub class_id: u32,
    /// The slots of `argL0`, `argL1`, `form` and `type` in the species.
    pub target_slot: usize,
    pub receiver_slot: usize,
    pub form_slot: usize,
    pub type_slot: usize,
    /// The admitted handle's `argL0`, `form` and `type` words.
    pub target_word: u64,
    pub form_word: u64,
    pub type_word: u64,
    /// The admitted bound value's exact class.
    pub receiver_class: u32,
    /// A second exact class a fill of this same shape admitted in this epoch
    /// (JIT round 21 wave 19, lane mhbind2;
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE_TWO_CLASSES`), or `0`: none.
    pub receiver_class_alt: u32,
}

/// What identifies a compiled `MethodHandle` call site behind its
/// `JitInvokeInfo` ADDRESS (gcj j3/rt3;
/// `gcj-j2jt2-mh-site-memos-keyed-by-a-reusable-jit-invoke-info-address-20261019.md`).
///
/// An info is freed with its `CompiledMethod` and the allocator can re-issue
/// its address to a site of a later compile, so a row keyed by the address
/// alone (the per-thread first link of [`MH_LINKED_SITES`], a bound site's
/// `fills` budget and "seen" bit in [`MH_BIND_SITES`]) could be inherited by
/// an unrelated site, which then skipped its own first link -- the step that
/// resolves its descriptor's classes through ITS caller's loader. The same
/// argument as [`callee_verdict_retire_epoch`]: the address cannot be
/// re-issued while the JIT cache's retire count still reads the value it read
/// when the row was made, so an equal count proves identity with one load.
/// After a retire the row is re-proved by content: the caller class, the
/// `invoke` / `invokeExact` choice and the descriptor -- everything
/// [`mh_compiled_site_link`] and the bind's admission read off the info (the
/// class and method names are fixed by [`mh_constant_bind_redirect`]'s
/// screen). A row that fails both is not this site's.
pub struct MhSiteIdentity {
    /// The retire count the row was made, or last re-proved, at.
    pub retire_epoch: std::cell::Cell<u64>,
    pub declaring: u32,
    pub invoke: bool,
    pub descriptor: Box<str>,
}

impl MhSiteIdentity {
    /// The identity of `info`, read while `info` is live (the caller is
    /// crossing its site), so the epoch is one at which the address names it.
    pub fn of(info: &JitInvokeInfo) -> Self {
        Self::at(info, callee_verdict_retire_epoch())
    }

    pub fn at(info: &JitInvokeInfo, retire_epoch: u64) -> Self {
        Self {
            retire_epoch: std::cell::Cell::new(retire_epoch),
            declaring: info.declaring_class_id,
            invoke: info.method_name == "invoke",
            descriptor: Box::from(info.descriptor),
        }
    }

    /// Does the row still name `info`'s site? `retire_epoch` is the current
    /// [`callee_verdict_retire_epoch`]. An equal epoch answers with no other
    /// read; otherwise the content decides and a match re-stamps the row.
    /// `enabled` is [`mh_site_identity_enabled`], asked only on an epoch
    /// mismatch; `false` (`CRATONVM_JIT_MH_SITE_IDENTITY=0`) answers as before
    /// the fix: the row is always this site's.
    pub fn names(
        &self,
        info: &JitInvokeInfo,
        retire_epoch: u64,
        enabled: impl FnOnce() -> bool,
    ) -> bool {
        if self.retire_epoch.get() == retire_epoch || !enabled() {
            return true;
        }
        let same = self.declaring == info.declaring_class_id
            && self.invoke == (info.method_name == "invoke")
            && *self.descriptor == *info.descriptor;
        if same {
            self.retire_epoch.set(retire_epoch);
        }
        same
    }
}

/// `CRATONVM_JIT_MH_SITE_IDENTITY` -- correctness, default ON, `=0` restores
/// the address-only rows (gcj j3/rt3; [`MhSiteIdentity`]). Every collector:
/// the defect is the JIT's dispatch memo, not a collector's. Read only when a
/// row's retire epoch is not the current one (after a code retirement), never
/// on a warm call.
pub fn mh_site_identity_enabled() -> bool {
    true
}

/// What a shape-keyed bound site compares on a handle at another address
/// (round 16 wave 11, lane mhbind; MHF-1): with the guard words of
/// `MH_COMPILED_BIND_GUARD_SLOTS`, every slot `mh_compiled_bind_target`
/// decides on, so two handles that agree on all of them get the same
/// admission answer -- the same target, and for a `bindTo` handle the same
/// receiver (`MH_BOUND` is a guard slot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MhBindShape {
    /// The handle's class (the admission requires `java/lang/invoke/MethodHandle`).
    pub class_id: u32,
    /// The handle's slot count (the admission's width screens, and the
    /// downcall test's, read it).
    pub width: usize,
    /// The slot of the class's `type` field (`get_field_by_name`'s resolution).
    pub type_slot: usize,
    /// The words of `MH_COMPILED_BIND_SHAPE_SLOTS`, then `type`'s.
    pub words: [u64; 5],
}

/// A converting bound site's per-call work (JIT round 16 wave 15, lane mh6;
/// MHB-1): the admission's [`MhBindConversion`] plus the two slots the warm
/// path reads, resolved on the fill ([`mh_bind_conv_of`]).
///
/// [`MhBindConversion`]: cratonvm_native_builtins::lang_invoke::MhBindConversion
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MhBindConv {
    /// The handle's `type` slot, and its word at the fill. The conversions
    /// are a function of the declared type (and the site's), so the warm path
    /// re-reads it with the guard slots: the one `MethodType` identity check.
    pub type_slot: usize,
    pub type_word: u64,
    /// Argument words (after the handle) to unbox from an exact `Integer`.
    pub unbox_int: u32,
    /// Box the member's `int` result through `Integer.valueOf`.
    pub box_int_return: bool,
    /// `java/lang/Integer`'s class id, and the slot of its `value`.
    pub integer_class_id: u32,
    pub value_slot: usize,
}

/// A REAL `asType` result's per-call work (j27 wave 5, lane MHASTYPE; the
/// admission is `lang_invoke::mh_real_converting_compiled_bind`, its answer
/// resolved on the fill by [`mh_real_conv_of`]). The handle is real, so its
/// guard is its address in the epoch (`MhBindHit::real`): no slot to re-read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MhRealConv {
    /// Per member argument word: `0` passes the caller's word; otherwise the
    /// bit mask over `wrappers` of the exact classes whose value replaces it
    /// ([`mh_real_unbox_word`]).
    pub unbox: [u8; MH_BIND_MAX_SPLICED_ARGS],
    /// Per member argument word: `0` when no reference cast applies; otherwise
    /// the target `ClassId` (`Class.cast`) the caller's reference must satisfy.
    pub cast: [u32; MH_BIND_MAX_SPLICED_ARGS],
    /// `(class id, slot of value)` of each `lang_invoke::MH_REAL_UNBOX_WRAPPERS`
    /// entry; class id `0`: not loaded (never matched).
    pub wrappers: [(u32, usize); 5],
    pub box_return: cratonvm_native_builtins::lang_invoke::MhRealBox,
}

/// What the redirect's site lookup (or its fill) answers for a bound call.
#[derive(Clone, Copy)]
pub struct MhBindHit {
    pub leaf: &'static JitInvokeInfo,
    pub guard: [u64; 7],
    pub bound: bool,
    pub direct: bool,
    pub conv: Option<MhBindConv>,
    /// A REAL `DirectMethodHandle` (SBR3 wave 2, lane mhperf): none of the carrier's
    /// slots to re-read, every field the admission decided on is `final`; the handle's
    /// address in this epoch is the whole guard.
    pub real: bool,
    /// An interface member's class id (JIT round 18 wave 11, lane mhresid;
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_INTERFACE`): every call's receiver must be
    /// an instance of a class implementing it, else the door takes the call.
    /// `0`: no receiver type guard.
    pub iface: u32,
    /// [`MhBindSite::iface_seen`] as the warm path read it.
    pub iface_seen: u32,
    /// [`MhBindSite::customized_seen`] as the warm path read it, for the
    /// handle the row was filled with only.
    pub customized_seen: bool,
    /// [`MhBindSite::real_conv`].
    pub real_conv: Option<MhRealConv>,
}

/// A converting call's member words ([`mh_bind_convert_args`]): the site's
/// words after the handle, unboxed where the conversion says.
#[derive(Clone, Copy)]
pub struct MhBindConverted {
    pub words: [i64; MH_BIND_MAX_SPLICED_ARGS],
    pub box_int_return: bool,
    /// Box the member's `long` result through `Long.valueOf` (j27 wave 5,
    /// lane MHASTYPE; a real `asType` result only).
    pub box_long_return: bool,
}

/// How [`mh_constant_bind_call`] makes a bound call: the target's info, the
/// bound receiver's word for a `bindTo` handle (round 16 wave 8), the site's
/// [`MhBindSite::direct`] (round 16 wave 10), and a converting call's words
/// (round 16 wave 15).
#[derive(Clone, Copy)]
pub struct MhBindRoute {
    pub leaf: &'static JitInvokeInfo,
    pub bound: Option<i64>,
    pub direct: bool,
    pub converted: Option<MhBindConverted>,
}

/// Fills after which a site is left to the door for good (a megamorphic
/// handle site would pay the admission on every call).
pub const MH_BIND_MAX_FILLS: u32 = 8;
/// The widest bound call [`mh_constant_bind_call`] splices (the handle's slot
/// becomes the receiver's, so the leaf takes as many words as the site);
/// a wider bound site keeps the door.
pub const MH_BIND_MAX_SPLICED_ARGS: usize = 16;
/// Bound sites and interned target infos kept per thread.
pub const MH_BIND_MAX_SITES: usize = 1024;
/// JIT round 20 wave 19, lane pRefl: 256 -> 1024, since the compiled
/// signature-polymorphic links (`mh_compiled_link_redirect`) intern their
/// targets here too and must not crowd the constant binds out.
pub const MH_BIND_MAX_LEAVES: usize = 1024;

/// `(vm_identity, target ClassId, class, name, descriptor, kind)`: class ids are
/// per VM, so the VM leads the key.
pub type MhLeafKey = (usize, u32, Box<str>, Box<str>, Box<str>, u8);

/// `CRATONVM_MH_COMPILED_LINK_UNBOUND` -- correctness, default ON, `=0`
/// restores round 18 wave 10's behaviour (JIT round 18 wave 11, lane mhresid;
/// MHRESID18-1, `r18-mh-compiled-first-link-rides-on-constant-bind-FIXED-20261006.md`):
/// with `CRATONVM_JIT_MH_CONSTANT_BIND` (or, for an `invoke` site,
/// `CRATONVM_JIT_MH_CONSTANT_BIND_INVOKE`) off, a compiled `MethodHandle` site
/// is still linked on its first compiled call per thread
/// ([`mh_compiled_site_link`]), recorded in [`MH_LINKED_SITES`]. Asked only
/// with those switches off, on every such compiled call -- so it is read once
/// per VM (`SharedVm::mh_compiled_link_unbound`, gcj j3/rt3 residual 1), not
/// through the flag snapshot per call.
pub fn mh_compiled_link_unbound_enabled(vm: &SharedVm) -> bool {
    vm.mh_compiled_link_unbound
}

thread_local! {
    /// `(vm_identity, caller's JitInvokeInfo address)` of the compiled
    /// `MethodHandle` sites this thread has linked while the constant bind is
    /// off (JIT round 18 wave 11, lane mhresid; MHRESID18-1). A failed link is
    /// not recorded (it links, and throws, again). Bounded by
    /// [`MH_BIND_MAX_SITES`] (cleared when full: a re-link of a linked site
    /// loads nothing). gcj j3/rt3: each row carries its [`MhSiteIdentity`], so
    /// a site at a re-issued info address is not "already linked".
    pub static MH_LINKED_SITES: std::cell::RefCell<
        // addr-keyed: not a heap address -- VM identity + `JitInvokeInfo` (retire epoch + content).
        rustc_hash::FxHashMap<(usize, usize), MhSiteIdentity>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    /// Bound sites, by the caller's `JitInvokeInfo` address; a row answers
    /// only while its [`MhSiteIdentity`] names the site (gcj j3/rt3).
    // addr-keyed: not a heap address -- `JitInvokeInfo` (retire epoch + content).
    pub static MH_BIND_SITES: std::cell::RefCell<rustc_hash::FxHashMap<usize, MhBindSite>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    /// Target infos, interned per (class, name, descriptor, kind): leaked, so
    /// the dispatch caches keyed by an info's address
    /// (`jit_invoke_dispatch_body`'s per-site memos) stay valid for the life
    /// of the thread. Bounded by [`MH_BIND_MAX_LEAVES`].
    pub static MH_BIND_LEAVES: std::cell::RefCell<rustc_hash::FxHashMap<MhLeafKey, &'static JitInvokeInfo>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// A slot value as a guard word (an object's address, an `Int` tagged so it
/// can never equal an address).
pub fn mh_bind_slot_key(v: Value) -> u64 {
    match v {
        Value::Object(Some(o)) => o.as_ptr() as u64,
        Value::Object(None) => 0,
        Value::Int(i) => (1u64 << 63) | u64::from(i as u32),
        _ => u64::MAX,
    }
}

/// The interned dispatch info for a bound target.
pub fn mh_bind_leaf_info(
    vm_id: usize,
    bind: &cratonvm_native_builtins::lang_invoke::MhCompiledBind,
) -> Option<&'static JitInvokeInfo> {
    let key: MhLeafKey = (
        vm_id,
        bind.class_id,
        Box::from(bind.class.as_str()),
        Box::from(bind.name.as_str()),
        Box::from(bind.desc.as_str()),
        bind.invoke_kind,
    );
    MH_BIND_LEAVES.with(|leaves| {
        let mut leaves = leaves.borrow_mut();
        if let Some(info) = leaves.get(&key) {
            return Some(*info);
        }
        if leaves.len() >= MH_BIND_MAX_LEAVES {
            return None;
        }
        // LEAK(intentional, bounded): see `MH_BIND_LEAVES`.
        let class: &'static str = Box::leak(Box::from(bind.class.as_str()));
        let name: &'static str = Box::leak(Box::from(bind.name.as_str()));
        let desc: &'static str = Box::leak(Box::from(bind.desc.as_str()));
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: class,
            method_name: name,
            descriptor: desc,
            // One slot per value plus the receiver, the convention every
            // compiled site's info carries.
            num_jit_args: cratonvm_jit_api::count_param_slots(desc)
                + usize::from(bind.invoke_kind != 3),
            return_type: cratonvm_jit::return_type(desc),
            invoke_kind: bind.invoke_kind,
            // The TARGET'S own class as the call's class context (round 16,
            // `r16-mh-constant-bind-callee-resume-refused-FIXED-20261001.md`): every
            // by-name resolution of `class_name` on this path -- the dispatch,
            // and the callee-resume service when the target deopts -- asks
            // the target class's own loader, which answers that class. With
            // `0` the resume asked the BOOTSTRAP loader for an application
            // class, refused, and raised `InternalError`.
            declaring_class_id: bind.class_id,
            owner_class_id: 0,
        }));
        leaves.insert(key, info);
        Some(info)
    })
}

/// The target a compiled `invokeExact` site's call is redirected to, or `None`
/// for the door (every site that is not a bound `MethodHandle.invokeExact`,
/// and every call whose handle is not the one the site bound). Called first
/// thing by [`jit_invoke_virtual_mic_body`] and [`jit_invoke_dispatch_body`],
/// before either derives the JIT thread, so the cold fill's own derivation is
/// not a sibling borrow. The caller hands the answer to
/// [`mh_constant_bind_call`]: the target's info, and for a `bindTo` handle
/// (round 16 wave 8) the bound receiver's word.
///
/// Round 16 wave 8 (lane mh): with `CRATONVM_JIT_MH_CONSTANT_BIND_INVOKE`, a
/// `MethodHandle.invoke` site too: the admission requires the site's
/// descriptor to BE the handle's own type, where `invoke` is `invokeExact`
/// (`lang_invoke::mh_constant_bind_invoke_enabled`).
///
/// Round 16 wave 10 (lane mhfloor, MH16-8): the third word is the site's
/// [`MhBindSite::direct`], which [`mh_constant_bind_call`] hands on.
///
/// Round 16 wave 15 (lane mh6; MHB-1): the answer is an [`MhBindRoute`]; a
/// converting site's carries its member words, unboxed here
/// ([`mh_bind_convert_args`]), and a word it cannot unbox is the door's call.
///
/// JIT round 18 wave 11 (lane mhresid; item 4 of
/// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`): when the site's
/// first compiled link failed (a name the caller's loader cannot load), the
/// link has stashed the error as the pending throwable and `*link_threw` is
/// set; the caller returns the `i64::MIN` sentinel instead of calling the door.
//
// SAFETY: `vm_ptr`, `info_ptr` and `args_ptr` / `num_args` are the dispatch
// helpers' own arguments, as compiled code passed them.
pub unsafe fn mh_constant_bind_redirect(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    link_threw: &mut bool,
) -> Option<MhBindRoute> {
    if num_args < 1 || args_ptr == 0 || info_ptr == 0 {
        return None;
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    if info.invoke_kind != 0
        || info.class_name != "java/lang/invoke/MethodHandle"
        || !matches!(info.method_name, "invokeExact" | "invoke")
    {
        return None;
    }
    let bind_on = mh_constant_bind_enabled()
        && (info.method_name == "invokeExact"
            || cratonvm_native_builtins::lang_invoke::mh_constant_bind_invoke_enabled());
    let vm = &*(vm_ptr as *const SharedVm);
    if !bind_on {
        // JIT round 18 wave 11 (lane mhresid; MHRESID18-1,
        // `r18-mh-compiled-first-link-rides-on-constant-bind-FIXED-20261006.md`): the
        // compiled-first link is a correctness step and runs with the bind's
        // perf switches off too, once per (thread, site) until it succeeds.
        if mh_compiled_link_unbound_enabled(vm) {
            let key = (vm.vm_identity, info_ptr as usize);
            // gcj j3/rt3: linked only while the row still names THIS site.
            let retire_epoch = callee_verdict_retire_epoch();
            let linked = MH_LINKED_SITES.with(|s| {
                s.borrow()
                    .get(&key)
                    .is_some_and(|row| row.names(info, retire_epoch, mh_site_identity_enabled))
            });
            if !linked {
                match mh_compiled_site_link(vm, info, args_ptr, num_args) {
                    MhCompiledSiteLink::Threw => *link_threw = true,
                    MhCompiledSiteLink::Nothing | MhCompiledSiteLink::Loaded => {
                        MH_LINKED_SITES.with(|s| {
                            let mut s = s.borrow_mut();
                            if s.len() >= MH_BIND_MAX_SITES && !s.contains_key(&key) {
                                s.clear();
                            }
                            s.insert(key, MhSiteIdentity::at(info, retire_epoch));
                        });
                    }
                }
            }
        }
        return None;
    }
    // Read by word, not through a slice: the compiled site's link below may
    // write the buffer (JIT round 17 wave 3, lane mh4).
    let raw = std::ptr::read(args_ptr as *const i64) as u64;
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let vm_id = vm.vm_identity;
    let epoch = vm.mem.address_epoch();
    // JIT round 17 wave 3 (lane mh4): whether this thread has seen the site at
    // all (a fill recorded it, bound or not); the first one links it.
    let mut site_seen = false;
    // The warm path: the same handle (address, epoch) as the bound one, then
    // its decided-on slots unchanged. The slots are read only on an address
    // match: an equal address in an equal epoch is the object the admission
    // measured wide enough.
    // Round 16 wave 11 (lane mhbind; MHF-1): a handle at ANOTHER address is
    // checked against the site's shape, when the fill recorded one; the
    // `Option<MhBindShape>` below is `Some` only for that case.
    // Round 16 wave 15 (lane mh6; MHB-1): a converting site also re-reads the
    // handle's `type` word ([`MhBindConv::type_word`]); its fill records no
    // shape.
    // gcj j3/rt3: a row whose `JitInvokeInfo` address was re-issued to another
    // site is not this site's: it is neither "seen" (the first link runs) nor
    // read for its give-up budget.
    let retire_epoch = callee_verdict_retire_epoch();
    let warm = MH_BIND_SITES.with(|sites| {
        let sites = sites.borrow();
        let site = sites.get(&(info_ptr as usize))?;
        if !site.identity.names(info, retire_epoch, mh_site_identity_enabled) {
            return None;
        }
        site_seen = true;
        if site.fills > MH_BIND_MAX_FILLS {
            return Some(None);
        }
        if site.vm != vm_id || site.epoch != epoch {
            return None;
        }
        let other_address = if site.guard[0] == raw {
            None
        } else if let Some(real_shape) = site.real_shape {
            // MHB-3 (JIT round 21 wave 17, lane mhbind): a real `bindTo` result.
            Some(MhOtherAddress::RealBound(real_shape))
        } else {
            Some(MhOtherAddress::Carrier(site.shape?))
        };
        Some(site.leaf.map(|leaf| {
            (
                MhBindHit {
                    leaf,
                    guard: site.guard,
                    bound: site.bound,
                    direct: site.direct,
                    conv: site.conv,
                    real: site.real,
                    iface: site.iface,
                    iface_seen: site.iface_seen.get(),
                    customized_seen: other_address.is_none() && site.customized_seen.get(),
                    real_conv: site.real_conv,
                },
                other_address,
            )
        }))
    });
    // MHB-3 (JIT round 21 wave 17, lane mhbind): the bound value of a real
    // `bindTo` result at another address of the site's shape.
    let mut shape_receiver: Option<u64> = None;
    let hit = match warm {
        Some(None) => return None,
        Some(Some((hit, other_address))) => {
            let obj = ObjectRef::from_raw(raw as *mut u8);
            let slots = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_GUARD_SLOTS;
            let same = match other_address {
                None if hit.real => true,
                None => {
                    slots.iter().enumerate().all(|(i, slot)| {
                        mh_bind_slot_key(vm.mem.heap.get_field(obj, *slot)) == hit.guard[i + 1]
                    }) && hit.conv.is_none_or(|conv| {
                        mh_bind_slot_key(vm.mem.heap.get_field(obj, conv.type_slot))
                            == conv.type_word
                    })
                }
                Some(MhOtherAddress::Carrier(shape)) => {
                    mh_bind_shape_matches(vm, obj, &hit.guard, &shape)
                }
                Some(MhOtherAddress::RealBound(shape)) => {
                    shape_receiver = mh_real_bound_shape_receiver(vm, obj, &shape);
                    shape_receiver.is_some()
                }
            };
            if same {
                Some(hit)
            } else {
                mh_constant_bind_fill(vm, info, info_ptr as usize, raw, vm_id, epoch)
            }
        }
        None => {
            // JIT round 17 wave 3 (lane mh4; item 1 of
            // `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`): the
            // first compiled call of a site on this thread links it as the
            // interpreter's first execution does, which may load and collect:
            // the handle and the epoch are read again after.
            // Round 18 wave 11 (lane mhresid): a failed link has stashed its
            // `NoClassDefFoundError`; no row is recorded, so the next compiled
            // call of the site links (and fails) again, as HotSpot does not
            // cache an `invokehandle` link failure.
            let link = if site_seen {
                MhCompiledSiteLink::Nothing
            } else {
                mh_compiled_site_link(vm, info, args_ptr, num_args)
            };
            let (raw, epoch) = match link {
                MhCompiledSiteLink::Threw => {
                    *link_threw = true;
                    return None;
                }
                MhCompiledSiteLink::Loaded => {
                    let raw = std::ptr::read(args_ptr as *const i64) as u64;
                    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
                        return None;
                    }
                    (raw, vm.mem.address_epoch())
                }
                MhCompiledSiteLink::Nothing => (raw, epoch),
            };
            mh_constant_bind_fill(vm, info, info_ptr as usize, raw, vm_id, epoch)
        }
    }?;
    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
    let MhBindHit {
        leaf,
        guard,
        bound,
        direct,
        conv,
        real,
        iface,
        iface_seen,
        customized_seen,
        real_conv,
    } = hit;
    if bound {
        // The handle's slot becomes the bound receiver's: the target takes as
        // many words as the site passes. The receiver is the `MH_BOUND` guard
        // word, which the check above (or the fill) just read off the handle:
        // a non-null object address (the admission refused any other value).
        //
        // MHB-3 (JIT round 21 wave 17, lane mhbind): a real `bindTo` result at
        // another address of the site's shape brings its own bound value,
        // read off it by the shape check above.
        let receiver = shape_receiver.unwrap_or(
            guard[1 + cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_BOUND_GUARD],
        );
        if leaf.num_jit_args != num_args as usize
            || num_args as usize > MH_BIND_MAX_SPLICED_ARGS
            || receiver == 0
            || receiver >= (1u64 << 48)
        {
            return None;
        }
        // JIT r22 wave 3 (lane inline): `Invokers.checkCustomized`'s count.
        if real
            && !customized_seen
            && !mh_bind_customization_serves(vm, info_ptr as usize, args[0])
        {
            return None;
        }
        return Some(MhBindRoute {
            leaf,
            bound: Some(receiver as i64),
            direct,
            converted: None,
        });
    }
    // A null receiver is the door's (its message-less NPE), and so is a site
    // whose argument count is not the target's. Interpreter round i5 wave 109,
    // lane L2: a statically bound (`invoke_kind` 1) target too
    // (`mh_compiled_bind_special_target`): the ladder's kind-1 arm would raise
    // the helpful `Cannot invoke` NPE where the door raises none.
    if matches!(leaf.invoke_kind, 0 | 1 | 2) && args.get(1).is_none_or(|r| *r == 0) {
        return None;
    }
    // JIT round 18 wave 11 (lane mhresid; THROWS18-4): an interface member's
    // receiver type guard. A receiver whose class does not implement the
    // interface is the door's (HotSpot's `checkReceiver` refusal).
    if iface != 0
        && !mh_bind_interface_receiver_ok(vm, info_ptr as usize, iface, iface_seen, args[1])
    {
        return None;
    }
    if leaf.num_jit_args + 1 != num_args as usize {
        return None;
    }
    // j27 wave 5 (lane MHASTYPE): a real `asType` result's words; a word
    // that will not unbox as the JDK's unboxer would without raising is the
    // door's, which runs that unboxer (its CCE / NPE).
    let converted = match (conv, real_conv) {
        (Some(conv), _) => Some(mh_bind_convert_args(&conv, &args[1..], |word| {
            mh_bind_unbox_int(vm, &conv, word)
        })?),
        (None, Some(rc)) => Some(mh_real_convert_args(
            &rc,
            &args[1..],
            |mask, word| mh_real_unbox_word(vm, &rc, mask, word),
            |target_cid, word| mh_real_cast_ok(vm, target_cid, word),
        )?),
        (None, None) => None,
    };
    // JIT r22 wave 3 (lane inline): `Invokers.checkCustomized`'s count, last,
    // so a call this route serves is counted exactly once and a declined one
    // is the door's to count.
    if real
        && !customized_seen
        && !mh_bind_customization_serves(vm, info_ptr as usize, args[0])
    {
        return None;
    }
    Some(MhBindRoute {
        leaf,
        bound: None,
        direct,
        converted,
    })
}

/// JIT round 22 wave 3, lane inline (handed by lane runtime; HotSpot parity,
/// under lane runtime's `CRATONVM_MH_LANES_KEEP_CUSTOMIZATION`, default ON,
/// `=0` the old uncounted bind; page
/// `docs/feature-designs/class-resolution-and-caching.md`):
/// may the constant bind serve this call of the REAL handle at `handle_word`?
///
/// A bound site calls the handle's target in place of
/// `Invokers$Holder.invokeExact_MT`, whose `checkCustomized` counts every
/// call of a handle that is not a compile constant and customizes it at call
/// 128 (`MethodHandle.customizationCount`, `LambdaForm.customized`). The bind
/// skipped that count, so a handle called from compiled code was never
/// customized -- visible through reflection on the handle and in
/// `SHOW_HIDDEN_FRAMES` walks. The count is now `lang_invoke`'s own
/// (`lane_check_customized`, per-VM resolved slots: two slot reads once the
/// handle is customized, three reads and one write before): `true` -- the
/// call is counted (or nothing is left to count) and the bind serves it;
/// `false` -- the call is due (or the layout is unreadable, or the threshold
/// is not the default), and the site declines to the door, whose JDK linker
/// runs `checkCustomized` and customizes the handle on HotSpot's call. Every
/// later call reads it customized and is served without counting.
///
/// With no JIT thread on this thread there is no context to count through;
/// the call is served as before (unreachable from compiled code in practice).
//
// SAFETY: `handle_word` is the site's argument word 0, a non-null object
// address the redirect's guard has just accepted; nothing between that guard
// and this read can collect.
pub unsafe fn mh_bind_customization_serves(vm: &SharedVm, site_key: usize, handle_word: i64) -> bool {
    // The steady state, a customized handle, from two raw slot reads (JIT r22
    // retirement: through the context below its two reads were the native
    // door's descriptor-aware field read each, ~240 ns a call), then never
    // again for this site's handle in this epoch ([`MhBindSite::customized_seen`]).
    let mh = ObjectRef::from_raw(handle_word as usize as *mut u8);
    if mh_bind_handle_is_customized(vm, mh) {
        MH_BIND_SITES.with(|sites| {
            if let Some(site) = sites.borrow().get(&site_key) {
                if site.guard[0] == handle_word as u64 {
                    site.customized_seen.set(true);
                }
            }
        });
        return true;
    }
    // j27 wave 2 (lane INVOKE; `CRATONVM_JIT_MH_BIND_COUNT_RAW`): an uncustomized
    // handle not yet due is counted from raw slots too.
    if mh_bind_handle_count_raw(vm, mh) {
        return true;
    }
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        return true;
    };
    let ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    cratonvm_native_builtins::lang_invoke::lane_check_customized(&ctx, mh).is_some()
}

/// `lane_check_customized`'s `Count` arm read and written raw (j27 wave 2, lane
/// INVOKE; `lang_invoke::lane_raw_count_slots`): when this thread's counting path
/// has settled the slots and a default `CUSTOMIZE_THRESHOLD`, the form of `mh` is
/// not customized and `customizationCount` (a `byte`) is below the threshold, the
/// count is raised by one and the call is served. `false` -- ask the counting
/// path, which answers every other case (due, unreadable, unsettled) -- with
/// nothing written. The same bounds as `lane_customize_slots` /
/// `lane_customization`; a primitive store, so no barrier.
//
// SAFETY: `mh` is a live object address the redirect's guard accepted.
pub unsafe fn mh_bind_handle_count_raw(vm: &SharedVm, mh: ObjectRef) -> bool {
    use cratonvm_native_builtins::lang_invoke::{lane_raw_count_slots, lane_raw_count_step};
    let Some(slots) = lane_raw_count_slots(vm.vm_identity) else {
        return false;
    };
    let heap = &vm.mem.heap;
    let fields = heap.get_header(mh).num_slots() as usize;
    if fields > slots.widest || slots.form >= fields || slots.count >= fields {
        return false;
    }
    let Value::Object(Some(form)) = heap.get_field(mh, slots.form) else {
        return false;
    };
    if slots.customized >= heap.get_header(form).num_slots() as usize
        || !matches!(heap.get_field(form, slots.customized), Value::Object(None))
    {
        return false;
    }
    let Some(next) = lane_raw_count_step(heap.get_field_as(mh, slots.count, b'B')) else {
        return false;
    };
    heap.set_field_as(mh, slots.count, Value::Int(next), b'B');
    true
}

/// Is the real handle `mh`'s form customized (`form.customized != null`),
/// read raw through the slots `lane_check_customized` resolved on this thread
/// (`lang_invoke::lane_customize_slots_resolved`)? `false` -- ask the counting
/// path -- when they are not resolved yet, the switch is off, or a bound fails
/// (the same bounds as `lane_customize_slots` / `lane_customization`).
//
// SAFETY: `mh` is a live object address the redirect's guard accepted.
pub unsafe fn mh_bind_handle_is_customized(vm: &SharedVm, mh: ObjectRef) -> bool {
    use cratonvm_native_builtins::lang_invoke::lane_customize_slots_resolved;
    let Some((form_slot, customized_slot, widest)) = lane_customize_slots_resolved(vm.vm_identity)
    else {
        return false;
    };
    let fields = vm.mem.heap.get_header(mh).num_slots() as usize;
    if fields > widest || form_slot >= fields {
        return false;
    }
    let Value::Object(Some(form)) = vm.mem.heap.get_field(mh, form_slot) else {
        return false;
    };
    customized_slot < vm.mem.heap.get_header(form).num_slots() as usize
        && matches!(vm.mem.heap.get_field(form, customized_slot), Value::Object(Some(_)))
}

/// The member's argument words for a converting bound call (JIT round 16 wave
/// 15, lane mh6; MHB-1): the site's words after the handle, each word of
/// [`MhBindConv::unbox_int`] replaced by the `int` its `Integer` holds. `None`
/// -- the door's call -- when a word to unbox is not an object whose class is
/// exactly `java/lang/Integer` (a `null`, any other class or an array), or the
/// call is wider than the buffer. Reads only: no allocation, no GC point, so
/// the words read here are the values at the helper's entry, as the guard's.
/// `unbox` is [`mh_bind_unbox_int`] (a parameter so the tests need no VM).
pub fn mh_bind_convert_args(
    conv: &MhBindConv,
    words: &[i64],
    unbox: impl Fn(i64) -> Option<i64>,
) -> Option<MhBindConverted> {
    let words = mh_bind_convert_words(words, |i, word| {
        if conv.unbox_int & (1 << i) != 0 {
            unbox(word)
        } else {
            Some(word)
        }
    })?;
    Some(MhBindConverted {
        words,
        box_int_return: conv.box_int_return,
        box_long_return: false,
    })
}

/// The member's argument words of a converting call: `convert(i, word)` of
/// each of the site's words after the handle, or `None` -- the door's call --
/// when one does not convert or the call is wider than the buffer. (j27
/// wave 5, lane MHASTYPE: the loop of [`mh_bind_convert_args`], shared with
/// [`mh_real_convert_args`].)
pub fn mh_bind_convert_words(
    words: &[i64],
    convert: impl Fn(usize, i64) -> Option<i64>,
) -> Option<[i64; MH_BIND_MAX_SPLICED_ARGS]> {
    if words.len() > MH_BIND_MAX_SPLICED_ARGS {
        return None;
    }
    let mut out = [0i64; MH_BIND_MAX_SPLICED_ARGS];
    for (i, word) in words.iter().enumerate() {
        out[i] = convert(i, *word)?;
    }
    Some(out)
}

/// A real `asType` result's member words (j27 wave 5, lane MHASTYPE): each
/// word whose [`MhRealConv::unbox`] mask is set replaced by `unbox(mask,
/// word)` ([`mh_real_unbox_word`], a parameter so the tests need no VM), the
/// rest as the caller passed them. Reads only, like [`mh_bind_convert_args`].
pub fn mh_real_cast_ok(vm: &SharedVm, target_cid: u32, raw: i64) -> bool {
    if raw == 0 {
        return true;
    }
    let Some((_, class)) = mh_bind_exact_class(vm, raw) else {
        return false;
    };
    let target = ClassId::new(target_cid);
    let cls = ClassId::new(class);
    class == target_cid
        || jit_is_subclass_of_cached(vm, cls, target)
        || crate::runtime::interpreter::lambda_proxy_satisfies_public(vm, cls, target)
}

pub fn mh_real_convert_args(
    conv: &MhRealConv,
    words: &[i64],
    unbox: impl Fn(u8, i64) -> Option<i64>,
    cast_ok: impl Fn(u32, i64) -> bool,
) -> Option<MhBindConverted> {
    use cratonvm_native_builtins::lang_invoke::MhRealBox;
    let words = mh_bind_convert_words(words, |i, word| {
        if conv.cast[i] != 0 {
            return cast_ok(conv.cast[i], word).then_some(word);
        }
        match conv.unbox[i] {
            0 => Some(word),
            mask => unbox(mask, word),
        }
    })?;
    Some(MhBindConverted {
        words,
        box_int_return: conv.box_return == MhRealBox::Int,
        box_long_return: conv.box_return == MhRealBox::Long,
    })
}

/// The object at the word `raw` and its exact class id, or `None` for a
/// null, misaligned or non-object word.
pub fn mh_bind_exact_class(vm: &SharedVm, raw: i64) -> Option<(ObjectRef, u32)> {
    let bits = raw as u64;
    if bits == 0 || (bits & 0x7) != 0 || bits >= (1u64 << 48) {
        return None;
    }
    let obj = vm.mem.heap.is_object_address(bits as usize)?;
    let class = jit_dispatch_class_validated(vm, obj).class_id()?;
    Some((obj, class.as_u32()))
}

/// The `int` an exact `java/lang/Integer` at `raw` holds, sign-extended to a
/// word as compiled code passes an `int`; `None` for anything else.
pub fn mh_bind_unbox_int(vm: &SharedVm, conv: &MhBindConv, raw: i64) -> Option<i64> {
    let (obj, class) = mh_bind_exact_class(vm, raw)?;
    if class != conv.integer_class_id {
        return None;
    }
    match vm.mem.heap.get_field(obj, conv.value_slot) {
        Value::Int(v) => Some(i64::from(v)),
        _ => None,
    }
}

/// The word a real `asType` unbox passes for the object at `raw` (j27 wave
/// 5, lane MHASTYPE): its wrapper's value widened to the member's `int` /
/// `long` (both travel as a sign-extended word), when its exact class is a
/// wrapper in `mask`; `None` -- the door, which raises the JDK's CCE / NPE --
/// for a null, any other class, or a wrapper the mask leaves out.
pub fn mh_real_unbox_word(vm: &SharedVm, conv: &MhRealConv, mask: u8, raw: i64) -> Option<i64> {
    let (obj, class) = mh_bind_exact_class(vm, raw)?;
    let k = conv
        .wrappers
        .iter()
        .position(|(id, _)| *id != 0 && *id == class)?;
    if mask & (1u8 << k) == 0 {
        return None;
    }
    mh_real_widen(k, vm.mem.heap.get_field(obj, conv.wrappers[k].1))
}

/// The value of a wrapper of `lang_invoke::MH_REAL_UNBOX_WRAPPERS[k]` as a
/// word: a `Long`'s as is, an `Integer`'s sign-extended, a `Short`'s / `Byte`'s
/// narrowed to its own width first and a `Character`'s to 16 unsigned bits
/// (whatever the slot's `Int` carries above them), as `long` / `int` widening
/// of that primitive gives. `None` for a value of another variant. Pure.
pub fn mh_real_widen(k: usize, value: Value) -> Option<i64> {
    use cratonvm_native_builtins::lang_invoke::{
        MH_REAL_UNBOX_BYTE, MH_REAL_UNBOX_CHAR, MH_REAL_UNBOX_INT, MH_REAL_UNBOX_LONG,
        MH_REAL_UNBOX_SHORT,
    };
    match (k, value) {
        (MH_REAL_UNBOX_LONG, Value::Long(v)) => Some(v),
        (MH_REAL_UNBOX_INT, Value::Int(v)) => Some(i64::from(v)),
        // Casts: narrowing to the wrapper's own primitive, then widening.
        (MH_REAL_UNBOX_SHORT, Value::Int(v)) => Some(i64::from(v as i16)),
        (MH_REAL_UNBOX_BYTE, Value::Int(v)) => Some(i64::from(v as i8)),
        (MH_REAL_UNBOX_CHAR, Value::Int(v)) => Some(i64::from(v as u16)),
        _ => None,
    }
}

/// Dispatch a call [`mh_constant_bind_redirect`] bound: the target's info,
/// with the caller's argument words after the handle (an unbound handle), or
/// with the bound receiver's word in the handle's place (a `bindTo` handle,
/// round 16 wave 8, `CRATONVM_JIT_MH_CONSTANT_BIND_BOUND`).
///
/// The spliced words live in this helper's frame for the extent of the call.
/// They need no root of their own: [`jit_invoke_dispatch_body`] treats its
/// argument buffer as the values of its entry, not as a root a collection
/// rewrites. A collection inside its SATB flush is undone by
/// `forward_jit_reference_args` (which already leaves the caller's buffer for
/// a forwarded heap copy), the compile window by `CompileArgPins` (pinned and
/// read back), and the call itself hands every word to the callee's own
/// frame. Liveness is the handle's: the bound receiver is reachable from the
/// handle's `MH_BOUND`, and the handle from the compiled caller's own
/// argument slot, which this does not touch.
///
/// Round 16 wave 10 (lane mhfloor, MH16-8): `direct` (the site's
/// [`MhBindSite::direct`]) sends the call through [`mh_bind_direct_call`],
/// which enters the target's cached compiled entry when the dispatch ladder
/// would end in that same entry, and hands every other call to the ladder.
///
/// Round 16 wave 15 (lane mh6; MHB-1, `CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING`):
/// a converting call dispatches the redirect's converted words
/// ([`MhBindConverted`]). An unboxed word is an `int`, no reference, so the
/// buffer holds no reference the caller's does not (the pass-through words
/// are the caller's own, under the argument above). An `int` result the site
/// takes as a reference is boxed after the call through
/// `jit_integer_value_of_direct_body` (`Integer.valueOf`'s identity cache, and
/// its pending-return handoff, which this helper's caller completes); the
/// exception / deopt sentinel `i64::MIN`, which no `int` result can equal, is
/// handed back unboxed.
//
// SAFETY: as [`mh_constant_bind_redirect`]; `num_args` is at most
// `MH_BIND_MAX_SPLICED_ARGS` when `bound` is `Some` (the redirect declines a
// wider one).
pub unsafe fn mh_constant_bind_call(
    vm_ptr: i64,
    route: MhBindRoute,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    let MhBindRoute {
        leaf,
        bound,
        direct,
        converted,
    } = route;
    let leaf_ptr = leaf as *const JitInvokeInfo as i64;
    if let Some(converted) = converted {
        let words = converted.words;
        let words_ptr = words.as_ptr() as i64;
        let rc = if direct {
            mh_bind_direct_call(vm_ptr, leaf, words_ptr, num_args - 1)
        } else {
            jit_invoke_dispatch_body(vm_ptr, leaf_ptr, words_ptr, num_args - 1)
        };
        // j27 wave 5 (lane MHASTYPE): a `long` result can BE `i64::MIN`
        // (`Long.MIN_VALUE`), so for it the sentinel is that word with an
        // exception pending; any other `i64::MIN` is the value, boxed.
        if converted.box_long_return {
            if rc == i64::MIN && jit_pending_exception_is_set() {
                return rc;
            }
            return jit_long_value_of_direct_body(vm_ptr, rc);
        }
        if !converted.box_int_return || rc == i64::MIN {
            return rc;
        }
        // Cast: the member returns `int`; its word's low half is the value.
        return jit_integer_value_of_direct_body(vm_ptr, i64::from(rc as i32));
    }
    let Some(receiver) = bound else {
        if direct {
            return mh_bind_direct_call(vm_ptr, leaf, args_ptr + 8, num_args - 1);
        }
        return jit_invoke_dispatch_body(vm_ptr, leaf_ptr, args_ptr + 8, num_args - 1);
    };
    let n = num_args as usize;
    let args = std::slice::from_raw_parts(args_ptr as *const i64, n);
    let mut spliced = [0i64; MH_BIND_MAX_SPLICED_ARGS];
    spliced[0] = receiver;
    spliced[1..n].copy_from_slice(&args[1..n]);
    if direct {
        return mh_bind_direct_call(vm_ptr, leaf, spliced.as_ptr() as i64, num_args);
    }
    jit_invoke_dispatch_body(vm_ptr, leaf_ptr, spliced.as_ptr() as i64, num_args)
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT` -- default ON since its A/B (`=0` off; JIT round 16
/// wave 10, lane mhfloor; proposal MH16-8 of `jit-r16-mh-proposals-RETIRED-20261006.md`): a
/// constant-bound `MethodHandle` site enters its target's cached compiled
/// entry ([`mh_bind_direct_call`]) instead of walking `jit_invoke_dispatch_body`
/// for the target's interned info on every call. `1`/`true`/`on`/`yes` turns
/// it on. Read on the cold fill only ([`mh_constant_bind_fill`]); the answer
/// rides on the bound site, so this getter holds no static.
pub fn mh_constant_bind_direct_flag() -> bool {
    true
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT_INTERFACE` -- perf, **default ON**
/// since JIT round 20 (`=0` is the kill switch; 483 -> 432 ns on
/// `R17Mh4Virtual exact-interface` for a site the optimizing tier does not
/// take; JIT round 19 wave 1, lane mhiface; the helper part of MHRESID18-4): an
/// INTERFACE handle's bound site (`CRATONVM_JIT_MH_CONSTANT_BIND_INTERFACE`)
/// enters its target's cached compiled entry through [`mh_bind_direct_call`]
/// like a virtual one, after the redirect's receiver type guard, instead of
/// walking `jit_invoke_dispatch_body` on every call. Read on the cold fill
/// only ([`mh_constant_bind_fill`]); the answer rides on the bound site.
pub fn mh_constant_bind_direct_interface_flag() -> bool {
    true
}

/// May [`mh_bind_direct_call`] serve `leaf` at all, by its names and kind
/// alone? Every arm of `jit_invoke_dispatch_body` that can answer a call
/// before its compiled-entry memos, and that a NAME decides, is a `false`
/// here (the ladder keeps those calls):
///
/// * `java/lang/Integer` (the boxing cache), a `VarHandle` owner (the
///   exact-type gate), and the three `ClassLoader` resource names (the
///   null-name NPE);
/// * an `invokestatic` while offload kernels are registered (`gpu-offload`);
/// * every diagnostic the ladder prints or counts per call (`mic_prof`, the
///   per-call trace, `CRATONVM_DBG_JIT_DISPATCH`, the putfield callee record,
///   the site-alias detector), and the `disable_jit` kill switch, so a
///   diagnostic run sees every call it saw before;
/// * an invoke kind the constant bind does not mint.
///
/// JIT round 19 wave 1 (lane mhiface; MHRESID18-4): an interface target
/// (kind 2) is eligible: the ladder answers it exactly as a virtual one (the
/// object-native memo, the site-native probe, then the receiver-keyed
/// `VIRTUAL_DISPATCH_CACHE` memo-first hit), and the redirect has already
/// tested the receiver against the interface. Whether a kind-2 site goes
/// direct at all is the fill's decision
/// ([`mh_constant_bind_direct_interface_flag`]).
pub fn mh_bind_direct_leaf_eligible(leaf: &JitInvokeInfo) -> bool {
    if !matches!(leaf.invoke_kind, 0 | 1 | 2 | 3) {
        return false;
    }
    if leaf.class_name == "java/lang/Integer"
        || crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(leaf.class_name)
        || matches!(
            leaf.method_name,
            "getResource" | "getResources" | "getResourceAsStream"
        )
    {
        return false;
    }
    #[cfg(feature = "gpu-offload")]
    if leaf.invoke_kind == 3 && cratonvm_jit::offload_hook::any_kernels() {
        return false;
    }
    !(mic_prof::enabled()
        || mic_prof::trace_enabled()
        || crate::runtime::env_cache::jit_dispatch_dbg()
        || crate::runtime::env_cache::jit_putfield_diag()
        || site_alias_detect_enabled()
        || crate::runtime::env_cache::disable_jit())
}

/// Would `jit_invoke_dispatch_body`'s native arms decline `leaf`'s site, by
/// their own memos? `true` only on an answer those memos already hold:
///
/// * the exact-receiver object-native cache (kinds 0/2) holds no entry for the
///   site (an entry may serve the call; the ladder decides);
/// * the leaf/site native cache is off, does not take the kind, or holds this
///   site's cached refusal at the registry's current generation -- the one
///   answer with which `try_jit_site_cached_native_dispatch` returns `None`
///   before any other work. No entry yet (the first calls) is `false`: the
///   ladder resolves the site and fills it.
pub fn mh_bind_direct_natives_decline(vm: &SharedVm, leaf: &JitInvokeInfo, key: JitSiteKey) -> bool {
    if matches!(leaf.invoke_kind, 0 | 2)
        && OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| cache.borrow().get(&key).is_some())
    {
        return false;
    }
    if !native_site_cache_enabled() || !site_cache_admits_invoke_kind(leaf.invoke_kind) {
        return true;
    }
    let generation = vm.natives.native_methods.generation();
    NATIVE_SITE_CACHE
        .with(|cache| matches!(cache.borrow().get(&key), Some((gen, None)) if *gen == generation))
}

/// A constant-bound call to `leaf` through its cached compiled entry (round 16
/// wave 10, lane mhfloor; MH16-8, `CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT`).
///
/// The bound target's info is interned per thread ([`mh_bind_leaf_info`]) and
/// only bound sites dispatch through it, so its `DISPATCH_CACHE` /
/// `VIRTUAL_DISPATCH_CACHE` entries are the ones `jit_invoke_dispatch_body`
/// itself filled for it. This enters such an entry exactly as the ladder's
/// hit does, and asks every question the ladder asks before that hit, in its
/// order, with its functions:
///
/// 1. the JIT-boundary note and the SATB flush, then the argument words
///    forwarded (`forward_jit_reference_args`) -- the flush can move them;
/// 2. the raw-entry generation flush, then the native arms' memos
///    ([`mh_bind_direct_natives_decline`]) and the name-decided arms
///    ([`mh_bind_direct_leaf_eligible`]);
/// 3. the class-identity flush and the redefinition / interpreter-only
///    quiescence (`compiled_entry_forbidden`);
/// 4. the memo itself, a stale body dropped as the ladder drops it
///    (`DispatchCache::names_a_stale_body`); a virtual target is keyed by the
///    receiver's dispatch class (`jit_dispatch_class`), only when the
///    ladder's own memo-first arm is on;
/// 5. the dispatch depth guard (`enter_jit_dispatch`), held across the call,
///    and the call through `call_compiled_then_route` with the memoised
///    `rerun_pins`.
///
/// Anything else -- no entry yet, a quiesced site, a native the site may be
/// served by, an entry call that made no call -- is the ladder's, entered
/// with the words as step 1 left them (a second forward of forwarded words
/// is a no-op).
///
/// What is skipped is only what decides nothing for such a site: the ladder's
/// diagnostics (every one of them sends the call to the ladder instead), the
/// native-site probe's JIT-thread derivation, and the second pass through the
/// helper's prologue for an info it has already answered.
//
// SAFETY: as [`mh_constant_bind_call`]: `args_ptr` holds `num_args` words,
// in the dispatch ABI, for `leaf`.
pub unsafe fn mh_bind_direct_call(
    vm_ptr: i64,
    leaf: &'static JitInvokeInfo,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    let leaf_ptr = leaf as *const JitInvokeInfo as i64;
    if vm_ptr == 0
        || num_args < 0
        || (num_args > 0 && args_ptr == 0)
        || !mh_bind_direct_leaf_eligible(leaf)
    {
        return jit_invoke_dispatch_body(vm_ptr, leaf_ptr, args_ptr, num_args);
    }
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    let raw_args = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };
    let forwarded = forward_jit_reference_args(vm, leaf, raw_args);
    let args_slice = forwarded.as_deref().unwrap_or(raw_args);
    let words = args_slice.as_ptr() as i64;
    let ladder = || jit_invoke_dispatch_body(vm_ptr, leaf_ptr, words, num_args);
    flush_raw_entry_dispatch_caches();
    let key = jit_site_key(vm.vm_identity, leaf_ptr as usize);
    if !mh_bind_direct_natives_decline(vm, leaf, key) {
        return ladder();
    }
    flush_class_identity_dispatch_memos(vm);
    if site_quiesced_for_redefinition(vm, leaf, key)
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        return ladder();
    }
    flush_raw_entry_dispatch_caches();
    let statically_bound = matches!(leaf.invoke_kind, 1 | 3);
    let hit = if statically_bound {
        if !direct_static_compiled_callee_entry_enabled() {
            return ladder();
        }
        DISPATCH_CACHE.with(|dc| {
            dc.borrow()
                .get(&key)
                .filter(|c| !c.names_a_stale_body())
                .map(|c| {
                    (
                        c.entry,
                        c.needs_context,
                        c.owner.clone(),
                        c.rerun_pins,
                        ClassId::new(0),
                    )
                })
        })
    } else {
        if !direct_virtual_compiled_callee_entry_enabled() || !dispatch_virtual_memo_first_enabled()
        {
            return ladder();
        }
        let raw = args_slice.first().copied().unwrap_or(0) as u64;
        if raw == 0 || (raw & 7) != 0 || raw >= (1u64 << 48) {
            return ladder();
        }
        let receiver = ObjectRef::from_raw(raw as usize as *mut u8);
        let Some(receiver_cid) = jit_dispatch_class(vm, receiver).class_id() else {
            return ladder();
        };
        VIRTUAL_DISPATCH_CACHE.with(|dc| {
            dc.borrow()
                .get(&(key, receiver_cid.as_u32()))
                .filter(|c| !c.names_a_stale_body())
                .map(|c| {
                    (
                        c.entry,
                        c.needs_context,
                        c.owner.clone(),
                        c.rerun_pins,
                        receiver_cid,
                    )
                })
        })
    };
    let Some((entry, needs_ctx, owner, rerun_pins, receiver_cid)) = hit else {
        return ladder();
    };
    let routed = {
        let _depth = match enter_jit_dispatch(vm, constructor_receiver(leaf, args_slice)) {
            Ok(guard) => guard,
            Err(sentinel) => return sentinel,
        };
        call_compiled_then_route(
            vm,
            leaf,
            args_slice,
            receiver_cid,
            rerun_pins,
            || match owner.as_ref() {
                Some(owner) => try_call_compiled_entry_reentrant_owned(
                    owner, entry, needs_ctx, vm_ptr, args_slice,
                ),
                None => try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice),
            },
        )
    };
    match routed {
        Some(v) => v,
        // No call was made (wider than the entry tables): the ladder's bail.
        None => ladder(),
    }
}

/// The three signature-polymorphic shapes [`mh_compiled_link_redirect`] links.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MhLinkShape {
    /// `invokevirtual MethodHandle.invokeExact`: the linker
    /// (`Invokers$Holder.invokeExact_MT`-style `MemberName`) with the
    /// `MethodType` appendix as its trailing argument.
    Linker,
    /// `invokevirtual MethodHandle.invokeBasic`: the handle's `form.vmentry`.
    Basic,
    /// `invokestatic MethodHandle.linkTo{Static,Virtual,Interface,Special}`:
    /// the trailing `MemberName` itself (`linkToSpecial` only under
    /// `CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL`, asked by
    /// [`mh_compiled_link_redirect`]).
    LinkTo,
}

/// The shape of a compiled site [`mh_compiled_link_redirect`] may link, by
/// its names and kind alone.
pub fn mh_link_shape(info: &JitInvokeInfo) -> Option<MhLinkShape> {
    if info.class_name != "java/lang/invoke/MethodHandle" {
        return None;
    }
    match (info.invoke_kind, info.method_name) {
        (0, "invokeExact") => Some(MhLinkShape::Linker),
        (0, "invokeBasic") => Some(MhLinkShape::Basic),
        (3, "linkToStatic" | "linkToVirtual" | "linkToInterface" | "linkToSpecial") => {
            Some(MhLinkShape::LinkTo)
        }
        _ => None,
    }
}

/// A method descriptor's parameters and return as JVM BASIC types (`L` for
/// every reference and array, `I` for `Z B S C I`, `J F D`, and `V` for a
/// void return): the types a `LambdaForm` and the JIT's dispatch words agree
/// on. `None` for a malformed descriptor.
pub fn mh_link_basic_sig(desc: &str) -> Option<(Vec<u8>, u8)> {
    let b = desc.as_bytes();
    if b.first() != Some(&b'(') {
        return None;
    }
    let mut i = 1usize;
    let mut params = Vec::with_capacity(8);
    let basic = |b: &[u8], i: &mut usize| -> Option<u8> {
        let mut c = *b.get(*i)?;
        let mut array = false;
        while c == b'[' {
            array = true;
            *i += 1;
            c = *b.get(*i)?;
        }
        if c == b'L' {
            while *b.get(*i)? != b';' {
                *i += 1;
            }
        }
        *i += 1;
        Some(match c {
            _ if array => b'L',
            b'L' => b'L',
            b'Z' | b'B' | b'S' | b'C' | b'I' => b'I',
            b'J' | b'F' | b'D' | b'V' => c,
            _ => return None,
        })
    };
    while *b.get(i)? != b')' {
        let t = basic(b, &mut i)?;
        if t == b'V' {
            return None;
        }
        params.push(t);
    }
    i += 1;
    let ret = basic(b, &mut i)?;
    (i == b.len()).then_some((params, ret))
}

/// Do a compiled site's descriptor `site` and the member it is linked to
/// agree in basic types, as the shape lays the words out? The member's
/// parameters must be `member_lead_l` references, then the site's parameters
/// without its first `site_front` and last `site_back` ones (each of which
/// must be a reference: the handle / receiver / `MemberName` words), then
/// `member_trail_l` references; the returns must have one basic type. Any
/// other pair -- a boxed word where the member takes a primitive, which the
/// door would unbox -- is the door's.
pub fn mh_link_basic_agree(
    site: &str,
    member: &str,
    member_lead_l: usize,
    site_front: usize,
    site_back: usize,
    member_trail_l: usize,
) -> bool {
    // JIT round 21 lane reflect2 (`CRATONVM_JIT_MH_COMPILED_LINK_LEAN`): the
    // same answer from stack buffers; this ran twice per `Method.invoke` with
    // two heap `Vec`s each time.
    if cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_lean_enabled() {
        return jr21_reflect2_basic_agree(
            site,
            member,
            member_lead_l,
            site_front,
            site_back,
            member_trail_l,
        );
    }
    let (Some((sp, sr)), Some((mp, mr))) = (mh_link_basic_sig(site), mh_link_basic_sig(member))
    else {
        return false;
    };
    if sr != mr || sp.len() < site_front + site_back {
        return false;
    }
    let core = &sp[site_front..sp.len() - site_back];
    if sp[..site_front]
        .iter()
        .chain(&sp[sp.len() - site_back..])
        .any(|t| *t != b'L')
        || mp.len() != member_lead_l + core.len() + member_trail_l
    {
        return false;
    }
    mp[..member_lead_l].iter().all(|t| *t == b'L')
        && &mp[member_lead_l..member_lead_l + core.len()] == core
        && mp[member_lead_l + core.len()..].iter().all(|t| *t == b'L')
}

/// The parameter capacity of [`jr21_reflect2_basic_sig`]'s buffer: a legal
/// descriptor has at most 255 parameter slots.
pub const JR21_REFLECT2_SIG_CAP: usize = 256;

/// [`mh_link_basic_sig`] into a caller's buffer: the parameter count and the
/// return's basic type, the parameters' basic types in `out[..count]`.
/// `Err(())` when the descriptor has more parameters than the buffer holds
/// (the caller then asks [`mh_link_basic_sig`]); `Ok(None)` exactly where
/// [`mh_link_basic_sig`] answers `None`.
pub fn jr21_reflect2_basic_sig(
    desc: &str,
    out: &mut [u8; JR21_REFLECT2_SIG_CAP],
) -> Result<Option<(usize, u8)>, ()> {
    let b = desc.as_bytes();
    if b.first() != Some(&b'(') {
        return Ok(None);
    }
    let basic = |b: &[u8], i: &mut usize| -> Option<u8> {
        let mut c = *b.get(*i)?;
        let mut array = false;
        while c == b'[' {
            array = true;
            *i += 1;
            c = *b.get(*i)?;
        }
        if c == b'L' {
            while *b.get(*i)? != b';' {
                *i += 1;
            }
        }
        *i += 1;
        Some(match c {
            _ if array => b'L',
            b'L' => b'L',
            b'Z' | b'B' | b'S' | b'C' | b'I' => b'I',
            b'J' | b'F' | b'D' | b'V' => c,
            _ => return None,
        })
    };
    let mut i = 1usize;
    let mut n = 0usize;
    loop {
        match b.get(i) {
            None => return Ok(None),
            Some(b')') => break,
            Some(_) => {}
        }
        let Some(t) = basic(b, &mut i) else {
            return Ok(None);
        };
        if t == b'V' {
            return Ok(None);
        }
        if n == JR21_REFLECT2_SIG_CAP {
            return Err(());
        }
        out[n] = t;
        n += 1;
    }
    i += 1;
    let Some(ret) = basic(b, &mut i) else {
        return Ok(None);
    };
    Ok((i == b.len()).then_some((n, ret)))
}

/// [`mh_link_basic_agree`] without a heap allocation (JIT round 21 lane
/// reflect2, `CRATONVM_JIT_MH_COMPILED_LINK_LEAN`): the same rule over
/// [`jr21_reflect2_basic_sig`]'s stack buffers; a descriptor wider than the
/// buffer takes the allocating rule.
pub fn jr21_reflect2_basic_agree(
    site: &str,
    member: &str,
    member_lead_l: usize,
    site_front: usize,
    site_back: usize,
    member_trail_l: usize,
) -> bool {
    let mut sbuf = [0u8; JR21_REFLECT2_SIG_CAP];
    let mut mbuf = [0u8; JR21_REFLECT2_SIG_CAP];
    let (s, m) = match (
        jr21_reflect2_basic_sig(site, &mut sbuf),
        jr21_reflect2_basic_sig(member, &mut mbuf),
    ) {
        (Ok(Some(s)), Ok(Some(m))) => (s, m),
        (Ok(None), _) | (_, Ok(None)) => return false,
        _ => {
            let (Some((sp, sr)), Some((mp, mr))) =
                (mh_link_basic_sig(site), mh_link_basic_sig(member))
            else {
                return false;
            };
            return jr21_reflect2_basic_agree_slices(
                &sp,
                sr,
                &mp,
                mr,
                member_lead_l,
                site_front,
                site_back,
                member_trail_l,
            );
        }
    };
    jr21_reflect2_basic_agree_slices(
        &sbuf[..s.0],
        s.1,
        &mbuf[..m.0],
        m.1,
        member_lead_l,
        site_front,
        site_back,
        member_trail_l,
    )
}

/// The comparison of [`mh_link_basic_agree`] over parsed basic types.
#[allow(clippy::too_many_arguments)]
pub fn jr21_reflect2_basic_agree_slices(
    sp: &[u8],
    sr: u8,
    mp: &[u8],
    mr: u8,
    member_lead_l: usize,
    site_front: usize,
    site_back: usize,
    member_trail_l: usize,
) -> bool {
    if sr != mr || sp.len() < site_front + site_back {
        return false;
    }
    let core = &sp[site_front..sp.len() - site_back];
    if sp[..site_front]
        .iter()
        .chain(&sp[sp.len() - site_back..])
        .any(|t| *t != b'L')
        || mp.len() != member_lead_l + core.len() + member_trail_l
    {
        return false;
    }
    mp[..member_lead_l].iter().all(|t| *t == b'L')
        && &mp[member_lead_l..member_lead_l + core.len()] == core
        && mp[member_lead_l + core.len()..].iter().all(|t| *t == b'L')
}

/// The object at a raw dispatch word, or `None` (null, misaligned, outside
/// the address space or no object start).
pub fn mh_link_object(vm: &SharedVm, word: i64) -> Option<ObjectRef> {
    let bits = word as u64;
    if bits == 0 || (bits & 0x7) != 0 || bits >= (1u64 << 48) {
        return None;
    }
    vm.mem.heap.is_object_address(bits as usize)
}

/// The interned call info for `member` as a compiled call of `invoke_kind`,
/// filed on the member's memoised record so a warm site builds no string; a
/// refusal is filed too (`1`), for the record's epoch.
pub fn mh_link_leaf(
    vm: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    member: &cratonvm_native_builtins::MhCompiledMember,
    invoke_kind: u8,
) -> Option<&'static JitInvokeInfo> {
    match member.leaf_word() {
        0 => {}
        // `1`: refused. JIT round 21 wave 1, lane reflect: a word with a low tag
        // bit (`MH_LINK_HIDDEN_TAG`) is a hidden-owner leaf
        // ([`mh_link_leaf_static`]), which this by-name route never calls.
        word if word & 7 != 0 => return None,
        word => {
            // SAFETY: a non-sentinel word is the address of an info
            // `mh_bind_leaf_info` interned and leaked (`MH_BIND_LEAVES`) on this
            // thread, which the record (a thread-local memo line) never outlives
            // the use of; it is never freed.
            let leaf = unsafe { &*(word as *const JitInvokeInfo) };
            return (leaf.invoke_kind == invoke_kind).then_some(leaf);
        }
    }
    let leaf = member
        .bind_checked(ctx, invoke_kind)
        .and_then(|bind| mh_bind_leaf_info(vm.vm_identity, &bind))
        // JIT round 21 lane reflect4 (`CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`):
        // a static member of a hidden class, called by its class id.
        .or_else(|| jr21_reflect4_hidden_leaf(vm, ctx, member, invoke_kind));
    // JIT round 21 lane reflect (caching fix, unconditional): a static member
    // of a class not initialized yet is refused only until the door that
    // serves the call initializes it; memoising that refusal kept the site on
    // its door for the rest of the collection epoch. Left unfiled (`0`), the
    // next call asks again.
    let word = match leaf {
        Some(l) => l as *const JitInvokeInfo as usize,
        None if member.refusal_may_lift(ctx, invoke_kind) => 0,
        None => 1,
    };
    member.set_leaf_word(word);
    // JIT round 21 lane reflect: name each refusal once (it is memoised on
    // the record), so a run shows which compiled links keep their door -- a
    // hidden owner is DEPTH21-1's `LambdaForm$MH` hop (`R20PAllocEntryCheck`).
    if word == 1 && crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[mh-compiled-link] door kept for {}.{}{} kind={invoke_kind}{}",
            member.class_name(),
            member.name(),
            member.desc(),
            if member.owner_is_hidden(ctx) {
                " (hidden owner)"
            } else {
                ""
            },
        );
    }
    leaf
}

/// The call info of a compiled `invokeBasic` / `linkToStatic` link whose
/// target is a static member of a HIDDEN class (JIT round 21 lane reflect4,
/// REFLECT3-21-2 / REFLECT21-1, `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`, default
/// ON, `=0` off; page
/// `jr21-reflect3-method-invoke-steady-state-string-work-20261014.md`).
///
/// Every customized `Method.invoke` adapter's `form.vmentry` is such a member
/// (a spun `LambdaForm$MH`), and `MhCompiledMember::bind_checked` refuses it
/// because the services around a compiled call resolved the callee by NAME,
/// which no loader answers for a hidden class: each call took the
/// `invokeBasic` door (`safe_native_call_impl` -> `mh_invoke_basic` ->
/// `mh_real_lf_entry_fast` -> `interpreter::execute`). The leaf minted here
/// carries the class id as its `owner_class_id` with `invoke_kind` 3, a pair
/// no compile mints, and every service that would resolve the name asks
/// [`jit_substituted_owner`] / [`jit_hidden_static_owner`] first: the
/// `DISPATCH_CACHE` fill's `jit_cache` key, the callee compile
/// ([`try_compile_callee`]), the handler template and re-run resolution
/// (`jit_site_target_method`), [`callee_has_exception_table_uncached`], the
/// deopt resume of a trapped callee ([`try_resume_trapped_callee`],
/// [`stash_reachable_from_site`]), the re-run from entry
/// ([`bail_to_interpreter`]) and the dispatch tail. The body itself is the one
/// the door's `execute` compiles and publishes under the same id, so before it
/// is published the call takes the ladder's static tail by id (the
/// interpreter), which counts it toward that compile.
///
/// `None` (the door, as before) for any other member, a class not initialized
/// yet (asked again later: `refusal_may_lift`), or the switch off.
pub fn jr21_reflect4_hidden_leaf(
    vm: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    member: &cratonvm_native_builtins::MhCompiledMember,
    invoke_kind: u8,
) -> Option<&'static JitInvokeInfo> {
    if invoke_kind != 3 {
        return None;
    }
    let bind = member.bind_checked_hidden(ctx)?;
    // The class store's own name for the id, the name the body is published
    // under and the one `jit_substituted_owner` compares.
    let class_name: std::sync::Arc<str> = {
        let cm = vm.classes.class_manager.read();
        std::sync::Arc::clone(&cm.get_class(ClassId::new(bind.class_id))?.name)
    };
    mh_bind_hidden_leaf_info(vm.vm_identity, &bind, &class_name)
}

/// [`mh_bind_leaf_info`] for a hidden static owner
/// ([`jr21_reflect4_hidden_leaf`]): the info names `class_name` (the class
/// store's) and carries the owner's id as both its `declaring_class_id` and
/// its `owner_class_id`. Interned in the same bounded per-thread table, under
/// a key whose kind byte is tagged (`| 0x80`) so it never shares a row with
/// an ordinary leaf.
pub fn mh_bind_hidden_leaf_info(
    vm_id: usize,
    bind: &cratonvm_native_builtins::lang_invoke::MhCompiledBind,
    class_name: &str,
) -> Option<&'static JitInvokeInfo> {
    let key: MhLeafKey = (
        vm_id,
        bind.class_id,
        Box::from(class_name),
        Box::from(bind.name.as_str()),
        Box::from(bind.desc.as_str()),
        bind.invoke_kind | 0x80,
    );
    MH_BIND_LEAVES.with(|leaves| {
        let mut leaves = leaves.borrow_mut();
        if let Some(info) = leaves.get(&key) {
            return Some(*info);
        }
        if leaves.len() >= MH_BIND_MAX_LEAVES {
            return None;
        }
        // LEAK(intentional, bounded): see `MH_BIND_LEAVES`.
        let class: &'static str = Box::leak(Box::from(class_name));
        let name: &'static str = Box::leak(Box::from(bind.name.as_str()));
        let desc: &'static str = Box::leak(Box::from(bind.desc.as_str()));
        let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
            class_name: class,
            method_name: name,
            descriptor: desc,
            // A static call: one slot per value, no receiver.
            num_jit_args: cratonvm_jit_api::count_param_slots(desc),
            return_type: cratonvm_jit::return_type(desc),
            invoke_kind: 3,
            declaring_class_id: bind.class_id,
            owner_class_id: bind.class_id,
        }));
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[mh-compiled-link] hidden leaf {class}.{name}{desc} id={} (by class id)",
                bind.class_id
            );
        }
        leaves.insert(key, info);
        Some(info)
    })
}

/// The layout key of [`jr21_reflect3_basic_agree_memo`]: the four counts of
/// [`mh_link_basic_agree`], two bits each, or `None` when one does not fit
/// (no shape passes more than 1 today; such a call is not memoised).
pub fn jr21_reflect3_agree_layout(
    member_lead_l: usize,
    site_front: usize,
    site_back: usize,
    member_trail_l: usize,
) -> Option<u8> {
    let parts = [member_lead_l, site_front, site_back, member_trail_l];
    if parts.iter().any(|&p| p > 3) {
        return None;
    }
    Some(
        parts
            .iter()
            .enumerate()
            .fold(0u8, |key, (i, &p)| key | ((p as u8) << (2 * i))),
    )
}

/// [`mh_link_basic_agree`] of a compiled site against `member`, memoised on
/// the member's per-epoch decode record (JIT round 21 lane reflect3,
/// `CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO`, default ON, `=0` off; page
/// `jr21-reflect3-method-invoke-steady-state-string-work-20261014.md`): the
/// two descriptor parses ran per shape per `Method.invoke`
/// (`jr21_reflect2_basic_sig` 3.4 % of the w3f profile). The verdict is a
/// pure function of the site's `'static` descriptor text, the layout and the
/// member's descriptor, so a memoised verdict is the computed one.
pub fn jr21_reflect3_basic_agree_memo(
    member: &cratonvm_native_builtins::MhCompiledMember,
    site: &'static str,
    member_lead_l: usize,
    site_front: usize,
    site_back: usize,
    member_trail_l: usize,
) -> bool {
    let memo_on = cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_agree_memo_enabled();
    let layout = jr21_reflect3_agree_layout(member_lead_l, site_front, site_back, member_trail_l)
        .filter(|_| memo_on);
    if let Some(verdict) = layout.and_then(|key| member.agree_memo(site, key)) {
        return verdict;
    }
    let verdict = mh_link_basic_agree(
        site,
        member.desc(),
        member_lead_l,
        site_front,
        site_back,
        member_trail_l,
    );
    if let Some(key) = layout {
        member.set_agree_memo(site, key, verdict);
    }
    verdict
}

/// The low tag of a member record's leaf word that names a hidden-owner leaf
/// ([`mh_link_leaf_static`]); interned infos are 8-aligned.
pub const MH_LINK_HIDDEN_TAG: usize = 2;

/// `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN` (perf, opt-in; JIT round 21 wave 1,
/// lane reflect): read per call through `NativeFlags`.
pub fn mh_compiled_link_hidden_enabled() -> bool {
    cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_hidden_enabled()
}

/// [`mh_link_leaf`] for a STATIC member, with the hidden-owner arm: `(leaf,
/// hidden)`. A member whose owner a name resolves (`bind_checked`) is the
/// ordinary compiled call (`hidden == false`). With
/// `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`, a static member of an initialized
/// HIDDEN class -- a spun `LambdaForm$MH` adapter, invoker or `$DMH` form --
/// is a leaf the redirect calls by its owner's id
/// ([`mh_hidden_static_call`]) instead of the door (`hidden == true`). Its
/// info is filed on the record tagged ([`MH_LINK_HIDDEN_TAG`]), so neither
/// route rebuilds a string on a warm call and the by-name route never takes
/// it. A hidden refusal is not filed: the owner may only be uninitialized yet.
pub fn mh_link_leaf_static(
    vm: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    member: &cratonvm_native_builtins::MhCompiledMember,
) -> Option<(&'static JitInvokeInfo, bool)> {
    let word = member.leaf_word();
    if word & 7 == MH_LINK_HIDDEN_TAG {
        if !mh_compiled_link_hidden_enabled() {
            return None;
        }
        // SAFETY: as `mh_link_leaf`'s; the tag is stripped first.
        let leaf = unsafe { &*((word & !7) as *const JitInvokeInfo) };
        return Some((leaf, true));
    }
    if let Some(leaf) = mh_link_leaf(vm, ctx, member, 3) {
        return Some((leaf, false));
    }
    if !mh_compiled_link_hidden_enabled() || member.leaf_word() != 1 {
        return None;
    }
    let leaf = member
        .bind_hidden_static(ctx)
        .and_then(|bind| mh_bind_leaf_info(vm.vm_identity, &bind))?;
    member.set_leaf_word(leaf as *const JitInvokeInfo as usize | MH_LINK_HIDDEN_TAG);
    Some((leaf, true))
}

/// A compiled call of a hidden class's static member `leaf` (JIT round 21
/// wave 1, lane reflect; `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`): the door's
/// own re-entry -- `interpreter::execute` on the owner BY ID, which runs its
/// compiled body when it has one and handles a deopt inside it as any
/// VM-side entry does -- without the native funnel, the door's decode and
/// its by-name settling. The member's frame is the real frame the door made,
/// so a `SHOW_HIDDEN_FRAMES` walk lists what it listed before.
///
/// As the dispatch helpers: the boundary note and the SATB flush first (a
/// possible GC point), then the words forwarded and decoded (`leaf`'s
/// descriptor names every reference), the depth guard, the call. A throwable
/// is stashed for the compiled caller (`handle_jit_dispatch_error`, the
/// `i64::MIN` sentinel).
//
// SAFETY: as [`mh_bind_direct_call`]: `args_ptr` holds `num_args` words for
// `leaf`, which is a static (`invoke_kind` 3) info whose `declaring_class_id`
// is the owner.
pub unsafe fn mh_hidden_static_call(
    vm_ptr: i64,
    leaf: &'static JitInvokeInfo,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    let raw_args = if num_args <= 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };
    let forwarded = forward_jit_reference_args(vm, leaf, raw_args);
    let args_slice = forwarded.as_deref().unwrap_or(raw_args);
    let values = decode_dispatch_values(vm, leaf, args_slice);
    let _depth = match enter_jit_dispatch(vm, constructor_receiver(leaf, args_slice)) {
        Ok(guard) => guard,
        Err(sentinel) => return sentinel,
    };
    let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
        // JIT round 22, lane sync: the sentinel carries the deopt flag
        // (`helper_guard`, "Two `i64::MIN` sentinel checks").
        set_jit_deopt_pending();
        return i64::MIN;
    };
    let result = crate::runtime::interpreter::execute(
        vm,
        thread,
        ClassId::new(leaf.declaring_class_id),
        leaf.method_name,
        leaf.descriptor,
        &values,
    );
    match result {
        Ok(Some(Value::Int(v))) => v as i64,
        Ok(Some(Value::Long(v))) => v,
        Ok(Some(Value::Float(f))) => f.to_bits() as i64,
        Ok(Some(Value::Double(d))) => d.to_bits() as i64,
        Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        Ok(Some(Value::Object(None))) | Ok(None) => 0,
        Ok(_) => 0,
        Err(e) => handle_jit_dispatch_error(vm, thread, e, leaf),
    }
}

/// The compiled call a signature-polymorphic site makes instead of its door
/// ([`mh_compiled_link_redirect`]): the target's info and the dispatch words
/// (written into `words`), with their count. Pure reads: no allocation of a
/// Java object and no GC point, so every word is the value at the helper's
/// entry (or, for the appendix, a row of the VM's rooted linker table).
///
/// `why` (JIT round 21 lane reflect4, REFLECT4-21-3) is set to the
/// [`MH_LINK_CENSUS_REASONS`] row of each step before it is tried, so a `None`
/// leaves the row of the step that refused; read only by the census.
pub fn mh_compiled_link_target(
    vm: &SharedVm,
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    info: &JitInvokeInfo,
    shape: MhLinkShape,
    args: &[i64],
    words: &mut [i64; MH_BIND_MAX_SPLICED_ARGS],
    why: &mut usize,
) -> Option<(&'static JitInvokeInfo, usize, bool)> {
    use cratonvm_native_builtins::lang_invoke as li;
    let n = args.len();
    match shape {
        MhLinkShape::Linker => {
            *why = MHL_LINKER_WIDE;
            if n + 1 > MH_BIND_MAX_SPLICED_ARGS {
                return None;
            }
            *why = MHL_LINKER_HANDLE;
            let handle = mh_link_object(vm, args[0])?;
            // JIT round 21 lane reflect3 (`CRATONVM_JIT_MH_COMPILED_LINK_PAIR_MEMO`):
            // the same pair, memoised on the handle's per-epoch record.
            *why = MHL_LINKER_PAIR;
            let (member, appendix, null_first_ok) = li::mh_compiled_linker_pair_memo(
                ctx,
                handle,
                info.descriptor,
                info.declaring_class_id,
                args.get(1) == Some(&0),
            )?;
            *why = MHL_LINKER_NULL_FIRST;
            if !null_first_ok && args.get(1) == Some(&0) {
                return None;
            }
            *why = MHL_LINKER_DECODE;
            let member = cratonvm_native_builtins::mh_compiled_member(ctx, member)?;
            // The linker takes the handle, the site's values and the appendix.
            *why = MHL_LINKER_SHAPE;
            if member.ref_kind() != 6
                || member.param_count() != n + 1
                || !jr21_reflect3_basic_agree_memo(&member, info.descriptor, 1, 0, 0, 1)
            {
                return None;
            }
            *why = MHL_LINKER_LEAF;
            let (leaf, hidden) = mh_link_leaf_static(vm, ctx, &member)?;
            words[..n].copy_from_slice(args);
            words[n] = appendix.as_ptr() as i64;
            *why = MHL_LINKER_SERVED;
            Some((leaf, n + 1, hidden))
        }
        MhLinkShape::Basic => {
            *why = MHL_BASIC_WIDE;
            if n > MH_BIND_MAX_SPLICED_ARGS {
                return None;
            }
            *why = MHL_BASIC_HANDLE;
            let handle = mh_link_object(vm, args[0])?;
            *why = MHL_BASIC_VMENTRY;
            let entry = li::mh_compiled_basic_vmentry(&*ctx, handle)?;
            *why = MHL_BASIC_DECODE;
            let member = cratonvm_native_builtins::mh_compiled_member(ctx, entry)?;
            // A form's entry is a static method of the handle, then the site's values.
            *why = MHL_BASIC_SHAPE;
            if member.ref_kind() != 6
                || member.param_count() != n
                || !jr21_reflect3_basic_agree_memo(&member, info.descriptor, 1, 0, 0, 0)
            {
                return None;
            }
            *why = MHL_BASIC_LEAF;
            let Some((leaf, hidden)) = mh_link_leaf_static(vm, ctx, &member) else {
                // Census only: name the `LambdaForm$MH` hop apart.
                if mh_link_census_on() && member.owner_is_hidden(&*ctx) {
                    *why = MHL_BASIC_LEAF_HIDDEN;
                }
                return None;
            };
            words[..n].copy_from_slice(args);
            *why = MHL_BASIC_SERVED;
            Some((leaf, n, hidden))
        }
        MhLinkShape::LinkTo => {
            *why = MHL_LINKTO_WIDE;
            if n < 1 || n - 1 > MH_BIND_MAX_SPLICED_ARGS {
                return None;
            }
            *why = MHL_LINKTO_MEMBER_WORD;
            let member_obj = mh_link_object(vm, args[n - 1])?;
            *why = MHL_LINKTO_DECODE;
            let member = cratonvm_native_builtins::mh_compiled_member(ctx, member_obj)?;
            *why = MHL_LINKTO_KIND;
            let (kind, receiver) = match (info.method_name, member.ref_kind()) {
                ("linkToStatic", 6) => (3u8, 0usize),
                ("linkToVirtual", 5) => (0u8, 1usize),
                ("linkToInterface", 9) => (2u8, 1usize),
                // Interpreter round i5 wave 109, lane L2
                // (`CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL`, checked by the
                // redirect before this): `linkToSpecial` binds its member
                // exactly, as `invokespecial` (`native_method_handle_link_to`'s
                // `REF_invokeSpecial` arm): a statically bound virtual the
                // resolve settled as special (`CRATONVM_MHN_RESOLVE_DIRECT_CALL`),
                // a `findSpecial` member, a constructor's `<init>`
                // (`asSpecial`). The member's class is the call's class context
                // (`bind_checked` requires its name to answer that very class).
                ("linkToSpecial", 7) => (1u8, 1usize),
                _ => return None,
            };
            // A null receiver is the door's message-less NPE.
            *why = MHL_LINKTO_SHAPE;
            if (receiver == 1 && args[0] == 0)
                || member.param_count() + receiver != n - 1
                || !jr21_reflect3_basic_agree_memo(&member, info.descriptor, 0, receiver, 1, 0)
            {
                return None;
            }
            *why = MHL_LINKTO_LEAF;
            let (leaf, hidden) = if kind == 3 {
                mh_link_leaf_static(vm, ctx, &member)?
            } else {
                (mh_link_leaf(vm, ctx, &member, kind)?, false)
            };
            // JIT round 21 wave 8, lane reflect
            // (`CRATONVM_MH_CTOR_REGISTERS_FINALIZABLE`, default ON): the
            // constructor call of a `newInvokeSpecial` form registers its
            // finalizable receiver, as the native `linkToSpecial` does
            // (native-builtins `link_to_special_constructor_registers`) and
            // HotSpot's `Object.<init>` return does. No GC point (a processor
            // lock, no allocation); the receiver is read before the call.
            if kind == 1
                && leaf.method_name == "<init>"
                && true
            {
                if let Some(receiver) = mh_link_object(vm, args[0]) {
                    crate::runtime::interpreter::register_if_finalizable_shared(vm, receiver);
                }
            }
            words[..n - 1].copy_from_slice(&args[..n - 1]);
            *why = MHL_LINKTO_SERVED;
            Some((leaf, n - 1, hidden))
        }
    }
}

/// Would a compiled `invokeBasic` linked by class id
/// ([`mh_compiled_basic_by_id`]) be sound calling `body` without the by-name
/// machinery behind it? The first slice of
/// `i115-L2-proposal-compiled-invoke-basic-linked-by-class-id` (interpreter
/// round i6 wave 116, lane L2): every way a compiled call's sentinel is
/// serviced by RE-RESOLVING the callee by name (a deopt resume, an uncommon
/// trap, a frameless trap stub, an own exit after a call, a re-run at the
/// callee's own handler) must be absent, because a hidden class's name
/// resolves to nothing. What is left -- a return, or a throwable the callee
/// cannot catch, which `route_implicit_exc_through_callee` hands outward
/// unchanged -- needs no name. A live, current body only: retired,
/// superseded and not-entrant bodies are the door's.
pub fn mh_by_id_body_admissible(body: &cratonvm_jit::CompiledMethod) -> bool {
    use std::sync::atomic::Ordering::Acquire;
    !body.retired.load(Acquire)
        && !body.superseded.load(Acquire)
        && !body.is_not_entrant()
        && body.method_declares_no_handlers
        && body.deopt_points.is_empty()
        && body.trap_cause_points.is_empty()
        && body.osr_exit_points.is_empty()
        && body.post_call_own_exit_bcis.is_empty()
        && !body.can_osr_exit
        && !body.has_indy_trap
        && !body.has_frameless_trap_stub
        && !body.requires_wrapped_entry
        && !body.self_locks_monitor
}

/// `CRATONVM_JIT_MH_COMPILED_LINK_BY_ID` (interpreter round i6 wave 116,
/// lane L2, default off; page
/// `i115-L2-proposal-compiled-invoke-basic-linked-by-class-id`): the target of
/// a compiled `invokeBasic` whose `form.vmentry` the by-name link refused --
/// a hidden spun `LambdaForm$MH`, or a name two classes share -- as that
/// entry's PUBLISHED compiled body, found under the owner's class id
/// (`JitCache::get` is keyed by it), with the interned call info the routing
/// of its result names. `None` (the site keeps its door, whose re-entry is
/// what compiles the form) for anything else: not a static entry of the
/// site's basic shape, an owner not initialized, no body yet, or a body
/// [`mh_by_id_body_admissible`] refuses.
///
/// Same reads as the `Basic` arm of [`mh_compiled_link_target`]: no Java
/// allocation and no GC point. The interned info is memoised on the member's
/// record (`MhCompiledMember::by_id_word`, kept apart from the by-name leaf
/// word), so a warm call builds no string; the body is probed per call
/// (lock-free) so a withdrawal is seen at once.
pub fn mh_compiled_basic_by_id(
    vm: &SharedVm,
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<(&'static JitInvokeInfo, std::sync::Arc<cratonvm_jit::CompiledMethod>)> {
    use cratonvm_native_builtins::lang_invoke as li;
    let n = args.len();
    if n == 0 || n > MH_BIND_MAX_SPLICED_ARGS {
        return None;
    }
    let handle = mh_link_object(vm, args[0])?;
    let entry = li::mh_compiled_basic_vmentry(&*ctx, handle)?;
    let member = cratonvm_native_builtins::mh_compiled_member(ctx, entry)?;
    // As the `Basic` arm: a form's entry is a static method taking the
    // handle, then the site's values, in the site's basic types.
    if member.ref_kind() != 6
        || member.param_count() != n
        || !mh_link_basic_agree(info.descriptor, member.desc(), 1, 0, 0, 0)
    {
        return None;
    }
    let leaf = match member.by_id_word() {
        1 => return None,
        0 => {
            // Not memoised while the owner is uninitialized (transient).
            let bind = member.bind_by_id(&*ctx, 3)?;
            let leaf = mh_bind_leaf_info(vm.vm_identity, &bind);
            member.set_by_id_word(leaf.map_or(1, |l| l as *const JitInvokeInfo as usize));
            leaf?
        }
        // SAFETY: a non-sentinel word is the address of an info
        // `mh_bind_leaf_info` interned and leaked (`MH_BIND_LEAVES`) on this
        // thread; it is never freed.
        word => unsafe { &*(word as *const JitInvokeInfo) },
    };
    let body = vm.jit.jit_cache.get(
        leaf.class_name,
        leaf.method_name,
        leaf.descriptor,
        ClassId::new(leaf.declaring_class_id),
    )?;
    if !mh_by_id_body_admissible(&body) {
        return None;
    }
    Some((leaf, body))
}

/// `CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID` (interpreter round i6 wave
/// 122, lane L2, default off; page
/// `i100-L4-proposal-method-handle-warm-call-directions`, item 2): the target
/// of a compiled `invokeExact` site whose linker pair the VM holds
/// (`lang_invoke::mh_compiled_linker_pair`, a shareable site or, under
/// `CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE`, an application-class one) but
/// whose linker `MemberName` the by-name link refused -- a hidden spun
/// `LambdaForm$MH` (`invokeExact_MT` of a basic type `Invokers$Holder` does
/// not pregenerate), or a name two classes share. It is that linker's
/// PUBLISHED compiled body, found under its owner's class id, exactly as
/// [`mh_compiled_basic_by_id`] finds a form's entry, with the words the
/// `Linker` arm of [`mh_compiled_link_target`] builds (the site's words, then
/// the appendix) written into `words`, and their count.
///
/// Every check of the `Linker` arm is asked first, in the same order: the
/// pair, a null first argument only for a bound species, a static member of
/// the site's arity plus the appendix in the site's basic types. Then
/// [`mh_by_id_body_admissible`]. `None` (the door, whose re-entry is what
/// compiles the linker) for anything else. Same reads as that arm: no Java
/// allocation and no GC point; the appendix is a row of the VM's rooted
/// linker table.
pub fn mh_compiled_linker_by_id(
    vm: &SharedVm,
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    info: &JitInvokeInfo,
    args: &[i64],
    words: &mut [i64; MH_BIND_MAX_SPLICED_ARGS],
) -> Option<(
    &'static JitInvokeInfo,
    std::sync::Arc<cratonvm_jit::CompiledMethod>,
    usize,
)> {
    use cratonvm_native_builtins::lang_invoke as li;
    let n = args.len();
    if n == 0 || n + 1 > MH_BIND_MAX_SPLICED_ARGS {
        return None;
    }
    let handle = mh_link_object(vm, args[0])?;
    let (member, appendix, null_first_ok) = li::mh_compiled_linker_pair(
        &*ctx,
        handle,
        info.descriptor,
        info.declaring_class_id,
        args.get(1) == Some(&0),
    )?;
    if !null_first_ok && args.get(1) == Some(&0) {
        return None;
    }
    let member = cratonvm_native_builtins::mh_compiled_member(ctx, member)?;
    if member.ref_kind() != 6
        || member.param_count() != n + 1
        || !mh_link_basic_agree(info.descriptor, member.desc(), 1, 0, 0, 1)
    {
        return None;
    }
    let leaf = match member.by_id_word() {
        1 => return None,
        0 => {
            // Not memoised while the owner is uninitialized (transient).
            let bind = member.bind_by_id(&*ctx, 3)?;
            let leaf = mh_bind_leaf_info(vm.vm_identity, &bind);
            member.set_by_id_word(leaf.map_or(1, |l| l as *const JitInvokeInfo as usize));
            leaf?
        }
        // SAFETY: as in `mh_compiled_basic_by_id`: an info
        // `mh_bind_leaf_info` interned and leaked; it is never freed.
        word => unsafe { &*(word as *const JitInvokeInfo) },
    };
    let body = vm.jit.jit_cache.get(
        leaf.class_name,
        leaf.method_name,
        leaf.descriptor,
        ClassId::new(leaf.declaring_class_id),
    )?;
    if !mh_by_id_body_admissible(&body) {
        return None;
    }
    words[..n].copy_from_slice(args);
    words[n] = appendix.as_ptr() as i64;
    Some((leaf, body, n + 1))
}


/// Call the body [`mh_compiled_basic_by_id`] found, as
/// [`mh_bind_direct_call`]'s statically bound arm calls a cached entry: the
/// boundary note and SATB flush, the reference words forwarded, the dispatch
/// depth guard, an owning reference across the call, and the result through
/// [`call_compiled_then_route`] (no argument pins: the body declares no
/// handler, so it cannot be re-run). `None` when no call was made (a JVMTI
/// agent wants the interpreter, or the words are wider than the entry
/// tables); the site's door then serves it.
///
/// SAFETY: as [`mh_compiled_link_redirect`]; `args` are the site's words, in
/// the dispatch ABI of `leaf` (a static call: the handle, then the values).
pub unsafe fn mh_compiled_basic_by_id_call(
    vm: &SharedVm,
    vm_ptr: i64,
    leaf: &'static JitInvokeInfo,
    body: &std::sync::Arc<cratonvm_jit::CompiledMethod>,
    args: &[i64],
) -> Option<i64> {
    if crate::runtime::interpreter::jvmti_requires_interpreter(vm) {
        return None;
    }
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let forwarded = forward_jit_reference_args(vm, leaf, args);
    let args_slice = forwarded.as_deref().unwrap_or(args);
    flush_raw_entry_dispatch_caches();
    let owner = cratonvm_jit::RetainedCode::new(std::sync::Arc::clone(body));
    let entry = body.entry_ptr() as usize;
    let needs_ctx = body.needs_context();
    let _depth = match enter_jit_dispatch(vm, constructor_receiver(leaf, args_slice)) {
        Ok(guard) => guard,
        Err(sentinel) => return Some(sentinel),
    };
    call_compiled_then_route(vm, leaf, args_slice, ClassId::new(0), false, || {
        try_call_compiled_entry_reentrant_owned(&owner, entry, needs_ctx, vm_ptr, args_slice)
    })
}

/// JIT round 20 wave 19, lane pRefl (`CRATONVM_JIT_MH_COMPILED_LINK`, default
/// ON, `=0` off; page
/// `docs/feature-designs/profile-guided-inlining.md`):
/// a compiled signature-polymorphic site calls its Java target as an ordinary
/// compiled call -- the target's interned info through
/// [`mh_bind_direct_call`] (its cached compiled entry, else the dispatch
/// ladder) -- instead of the registered native door, its funnel and its
/// VM-side re-entry (`interpreter::execute`). HotSpot links these sites the
/// same way: `invokehandle` to the linker `LambdaForm` with its appendix,
/// `invokeBasic` to the form's `vmentry`, `linkTo*` to the `MemberName`'s
/// method. Three shapes ([`MhLinkShape`]):
///
/// * `invokeExact` whose `(MemberName, appendix)` pair the door already filed
///   in the VM's linker table for this site descriptor and caller class
///   (`lang_invoke::mh_compiled_linker_pair`): the linker, words = the site's
///   words + the appendix;
/// * `invokeBasic` on a real JDK handle: `form.vmentry`, the very arm
///   `mh_invoke_basic` runs first (`lang_invoke::mh_compiled_basic_vmentry`);
/// * `linkToStatic` / `linkToVirtual` / `linkToInterface`: the trailing
///   `MemberName`'s method, static / virtual / interface, the words without
///   the `MemberName` (what `native_method_handle_link_to` dispatches).
///
/// Every target is decoded by `linkTo*`'s own memoised decode
/// (`cratonvm_native_builtins::mh_compiled_member`) and must be a class its
/// NAME resolves to uniquely (no hidden `LambdaForm$MH` class, no second
/// definition: the ladder resolves names), initialized when static, with
/// the site's words in the member's basic types. Anything else -- the first
/// calls of a site (the door files the linker pair), a downcall or VM carrier
/// handle, a null receiver, a converting word, a hidden spun form -- returns
/// `None` and the site keeps its door. Exceptions, deopt and the callee's GC
/// map are the ordinary compiled call's: the target's frame is a real
/// activation, as the door's re-entry made it, and its throwable propagates
/// through the compiled caller (`DirectMethodHandleAccessor.invoke`'s own
/// `catch` wraps it in `InvocationTargetException`, as on HotSpot).
///
/// The words handed on are the caller's (its argument buffer, read at entry)
/// and, for the linker, the appendix, a row of the VM's rooted linker table;
/// [`mh_bind_direct_call`] forwards every reference word after its SATB flush,
/// the target's descriptor naming each one.
///
/// SAFETY: as [`mh_constant_bind_redirect`].
pub unsafe fn mh_compiled_link_redirect(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> Option<i64> {
    if vm_ptr == 0 || info_ptr == 0 || args_ptr == 0 || num_args < 1 {
        return None;
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    let shape = mh_link_shape(info)?;
    // JIT round 21 wave 1, lane reflect (`CRATONVM_JIT_REFLECT_ACCESSOR_LINK`,
    // opt-in): `DirectMethodHandleAccessor.invokeImpl`'s `invokeExact` calls
    // the reflected method itself. Its own switch, so the A/B is independent
    // of `CRATONVM_JIT_MH_COMPILED_LINK`; every refusal falls to the arms below.
    if shape == MhLinkShape::Linker
        && cratonvm_native_builtins::lang_invoke::jit_reflect_accessor_link_enabled()
    {
        let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
        if let Some(bits) = reflect_accessor_link_redirect(vm_ptr, info, args) {
            return Some(bits);
        }
    }
    // REFLECT21-3 (`CRATONVM_JIT_REFLECT_CTOR_LINK`, opt-in): the constructor
    // accessor's `invokeExact`.
    if shape == MhLinkShape::Linker
        && cratonvm_native_builtins::lang_invoke::jit_reflect_ctor_link_enabled()
    {
        let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
        if let Some(bits) = reflect_ctor_link_redirect(vm_ptr, info, args) {
            return Some(bits);
        }
    }
    if !cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_enabled() {
        return None;
    }
    // Interpreter round i5 wave 109, lane L2: `linkToSpecial` is linked only
    // under its own opt-in switch (default OFF until its A/B).
    if info.method_name == "linkToSpecial"
        && !cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_special_enabled()
    {
        note_mh_link_census(MHL_DISABLED);
        return None;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
    let mut words = [0i64; MH_BIND_MAX_SPLICED_ARGS];
    // JIT round 21 lane reflect4 (REFLECT4-21-3): the census row of the step
    // that refused, or of the shape served (`mh_compiled_link_target`).
    let mut why = MHL_NO_THREAD;
    // Interpreter round i4 wave 95, lane L3 (page
    // `interpreter-i94-orch-dev-jit-round-20-regresses-four-interpreter-probes-FIXED-20261128.md`,
    // `L3W79NestedReflectDepth` `depth-400`): the `invokeExact` door this
    // redirect replaces (`lang_invoke::mh_linked_invoker`) re-anchors the
    // compiled-recursion budget for its nested call, as the reflective doors do
    // (wave 79): each `Method.invoke` makes one such `invokeExact`
    // (`DirectMethodHandleAccessor.invokeImpl`), so nested reflection
    // re-anchored once per ~2 MiB of native frames. With the door skipped,
    // 400 nested `Method.invoke` levels of compiled frames and dispatch
    // helpers were charged to the single 4 MiB budget anchored at the
    // thread's first compiled entry and tripped `jit_native_stack_exhausted`
    // (`StackOverflowError` inside `InvocationTargetException`). The same
    // rebase keeps the redirect's stack behaviour the door's: one TLS read
    // and a compare unless the stack pointer is within 2 MiB of the floor,
    // never below the thread's guard floor.
    let rebase = matches!(shape, MhLinkShape::Linker);
    let target = {
        let (thread, _guard) = jit_thread_mut()?;
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let linked = mh_compiled_link_target(vm, &mut ctx, info, shape, args, &mut words, &mut why);
        // JIT round 21 lane reflect4 (REFLECT4-21-3, `CRATONVM_DBG_MH_LINK_CENSUS`):
        // count every outcome of the by-name link.
        if mh_link_census_on() {
            note_mh_link_census(match linked {
                Some((leaf, count, _)) => {
                    mh_link_census_served_row(vm, why, leaf, &words[..count.min(words.len())])
                }
                None => why,
            });
        }
        match linked {
            Some(target) => Ok(target),
            // Interpreter round i6 wave 116, lane L2
            // (`CRATONVM_JIT_MH_COMPILED_LINK_BY_ID`, default off): an
            // `invokeBasic` the by-name link refused (a hidden spun form) may
            // still call its entry's published body, found by class id.
            None if shape == MhLinkShape::Basic
                && cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_by_id_enabled() =>
            {
                let (leaf, body) = mh_compiled_basic_by_id(vm, &mut ctx, info, args)?;
                Err((leaf, body, None))
            }
            // Interpreter round i6 wave 122, lane L2
            // (`CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID`, default off): an
            // `invokeExact` whose linker the by-name link refused (a hidden
            // spun form) may call the linker's published body by class id,
            // with the words (site + appendix) written into `words`.
            None if shape == MhLinkShape::Linker
                && cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_linker_by_id_enabled() =>
            {
                let (leaf, body, count) =
                    mh_compiled_linker_by_id(vm, &mut ctx, info, args, &mut words)?;
                Err((leaf, body, Some(count)))
            }
            None => return None,
        }
    };
    let (leaf, count, hidden) = match target {
        Ok(target) => target,
        Err((leaf, body, None)) => {
            return mh_compiled_basic_by_id_call(vm, vm_ptr, leaf, &body, args)
        }
        // The linker by id: the stack floor is re-anchored as for the
        // by-name linker below (the door it replaces did so).
        Err((leaf, body, Some(count))) => {
            let budget = rebase_self_call_stack_floor(None);
            let result = mh_compiled_basic_by_id_call(vm, vm_ptr, leaf, &body, &words[..count]);
            restore_self_call_stack_floor(budget);
            return result;
        }
    };
    let budget = if rebase {
        rebase_self_call_stack_floor(None)
    } else {
        FLOOR_NOT_REBASED
    };
    // JIT round 21 wave 1, lane reflect (`CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`,
    // opt-in): a hidden owner's static member is called by its id.
    let result = if hidden {
        mh_hidden_static_call(vm_ptr, leaf, words.as_ptr() as i64, count as i64)
    } else {
        mh_bind_direct_call(vm_ptr, leaf, words.as_ptr() as i64, count as i64)
    };
    restore_self_call_stack_floor(budget);
    Some(result)
}

// ---------------------------------------------------------------------------
// JIT round 21 lane reflect4: two exit censuses for the reflective fallbacks.
//
// REFLECT4-21-3 (`CRATONVM_DBG_MH_LINK_CENSUS`): what every compiled
// signature-polymorphic link ([`mh_compiled_link_redirect`]) did, per call --
// served (and whether the served leaf already had a compiled entry memoised),
// or which step sent it to its door. `Method.invoke` makes one `invokeExact`
// (linker), one `invokeBasic` (the adapter's `form.vmentry`) and one `linkTo*`
// per call, so dividing a row by the probe's call count reads "per
// `Method.invoke`". Lane reflect's `[mh-compiled-link] door kept` line names a
// refused member once per epoch; this counts every call, which is what tells
// a hop that costs the whole row from one that fires once.
//
// REFLECT4-21-1 (`CRATONVM_DBG_BYTECODE_ONLY_BODY`): what the bytecode-only
// door (`interpreter::bytecode_only_body`) did per call.
//
// Both are per-thread [`JitCounterBlock`] slots (no new static), bumped only
// with their switch on, and printed once at exit by the launcher
// (`maybe_dump_shutdown_reports`).
// ---------------------------------------------------------------------------

/// [`MH_LINK_CENSUS_REASONS`] rows. A `..._SERVED` row is followed by its
/// ladder twin (`+ 1`, [`mh_link_census_served_row`]).
pub const MHL_DISABLED: usize = 0;
pub const MHL_NO_THREAD: usize = 1;
pub const MHL_LINKER_WIDE: usize = 2;
pub const MHL_LINKER_HANDLE: usize = 3;
pub const MHL_LINKER_PAIR: usize = 4;
pub const MHL_LINKER_NULL_FIRST: usize = 5;
pub const MHL_LINKER_DECODE: usize = 6;
pub const MHL_LINKER_SHAPE: usize = 7;
pub const MHL_LINKER_LEAF: usize = 8;
pub const MHL_LINKER_SERVED: usize = 9;
pub const MHL_BASIC_WIDE: usize = 11;
pub const MHL_BASIC_HANDLE: usize = 12;
pub const MHL_BASIC_VMENTRY: usize = 13;
pub const MHL_BASIC_DECODE: usize = 14;
pub const MHL_BASIC_SHAPE: usize = 15;
pub const MHL_BASIC_LEAF_HIDDEN: usize = 16;
pub const MHL_BASIC_LEAF: usize = 17;
pub const MHL_BASIC_SERVED: usize = 18;
pub const MHL_LINKTO_WIDE: usize = 20;
pub const MHL_LINKTO_MEMBER_WORD: usize = 21;
pub const MHL_LINKTO_DECODE: usize = 22;
pub const MHL_LINKTO_KIND: usize = 23;
pub const MHL_LINKTO_SHAPE: usize = 24;
pub const MHL_LINKTO_LEAF: usize = 25;
pub const MHL_LINKTO_SERVED: usize = 26;

/// The reason each [`JitCounterBlock::mh_link_census`] slot names. "door"
/// means the site ran its registered native door for that call.
pub const MH_LINK_CENSUS_REASONS: [&str; 28] = [
    "any: CRATONVM_JIT_MH_COMPILED_LINK=0 (door)",
    "any: no JIT thread context (door)",
    "linker (invokeExact): wider than the spliced-argument buffer (door)",
    "linker (invokeExact): handle word is not a heap object (door)",
    "linker (invokeExact): no linker pair filed for this site descriptor and caller (door; the door files it)",
    "linker (invokeExact): null first argument the filed pair does not admit (door)",
    "linker (invokeExact): linker MemberName decode refused (door)",
    "linker (invokeExact): linker is not a static of the site's arity and basic types (door)",
    "linker (invokeExact): linker leaf refused (door; CRATONVM_DBG_JITC names it once)",
    "linker (invokeExact): served, the leaf's compiled entry was memoised",
    "linker (invokeExact): served, no compiled entry memoised (dispatch ladder: compile, interpret or native)",
    "basic (invokeBasic): wider than the spliced-argument buffer (door)",
    "basic (invokeBasic): handle word is not a heap object (door)",
    "basic (invokeBasic): no form.vmentry (door)",
    "basic (invokeBasic): vmentry MemberName decode refused (door)",
    "basic (invokeBasic): vmentry is not a static of the site's arity and basic types (door)",
    "basic (invokeBasic): vmentry leaf refused, hidden owner (door: the LambdaForm$MH hop, DEPTH21-1)",
    "basic (invokeBasic): vmentry leaf refused, other (door)",
    "basic (invokeBasic): served, the leaf's compiled entry was memoised",
    "basic (invokeBasic): served, no compiled entry memoised (dispatch ladder: compile, interpret or native)",
    "linkTo*: argument count outside the spliced-argument buffer (door)",
    "linkTo*: MemberName word is not a heap object (door)",
    "linkTo*: MemberName decode refused (door)",
    "linkTo*: member reference kind does not match the linkTo* name (door)",
    "linkTo*: null receiver, or arity or basic types disagree (door)",
    "linkTo*: member leaf refused (door)",
    "linkTo*: served, the leaf's compiled entry was memoised",
    "linkTo*: served, no compiled entry memoised (dispatch ladder: compile, interpret or native)",
];

/// `CRATONVM_DBG_MH_LINK_CENSUS` (REFLECT4-21-3): one load of the process
/// flag snapshot.
#[inline]
pub fn mh_link_census_on() -> bool {
    cratonvm_types::flags().jit.dbg_mh_link_census
}

/// This thread's +1 for census row `row`, with the census on.
#[inline]
pub fn note_mh_link_census(row: usize) {
    if mh_link_census_on() {
        JIT_COUNTERS.with(|b| {
            if let Some(c) = b.mh_link_census.get(row) {
                JitCounterBlock::bump(c);
            }
        });
    }
}

/// The census row of a served link: `served` when the leaf's compiled entry
/// is memoised where [`mh_bind_direct_call`] looks first (`DISPATCH_CACHE`
/// for a statically bound leaf, `VIRTUAL_DISPATCH_CACHE` by the receiver's
/// dispatch class otherwise), `served + 1` (the dispatch ladder) when not.
/// Read-only and census-only; the call itself decides again.
pub fn mh_link_census_served_row(
    vm: &SharedVm,
    served: usize,
    leaf: &'static JitInvokeInfo,
    words: &[i64],
) -> usize {
    let key = jit_site_key(vm.vm_identity, leaf as *const JitInvokeInfo as usize);
    let memoised = if matches!(leaf.invoke_kind, 1 | 3) {
        DISPATCH_CACHE.with(|dc| {
            dc.borrow()
                .get(&key)
                .is_some_and(|c| !c.names_a_stale_body())
        })
    } else {
        match words
            .first()
            .and_then(|w| mh_link_object(vm, *w))
            .and_then(|receiver| jit_dispatch_class(vm, receiver).class_id())
        {
            Some(cid) => VIRTUAL_DISPATCH_CACHE.with(|dc| {
                dc.borrow()
                    .get(&(key, cid.as_u32()))
                    .is_some_and(|c| !c.names_a_stale_body())
            }),
            None => false,
        }
    };
    if memoised {
        served
    } else {
        served + 1
    }
}

/// The REFLECT4-21-3 exit report, `None` with `CRATONVM_DBG_MH_LINK_CENSUS`
/// off. Rows with a zero count are left out; an empty report says no compiled
/// signature-polymorphic site ran (an interpreted caller takes the door and is
/// not counted).
pub fn mh_link_census_lines() -> Option<Vec<String>> {
    if !mh_link_census_on() {
        return None;
    }
    let mut lines = vec![
        "[cratonvm] mh-compiled-link census (REFLECT4-21-3; compiled signature-polymorphic calls, all threads; \
         one Method.invoke = one invokeExact + one invokeBasic + one linkTo*):"
            .to_string(),
    ];
    for (row, reason) in MH_LINK_CENSUS_REASONS.iter().enumerate() {
        let n = jit_counter_total(|b| {
            b.mh_link_census
                .get(row)
                .map_or(0, |c| c.load(std::sync::atomic::Ordering::Relaxed))
        });
        if n > 0 {
            lines.push(format!("[cratonvm]   {reason}: {n}"));
        }
    }
    Some(lines)
}

/// [`BYTECODE_ONLY_CENSUS_REASONS`] rows (REFLECT4-21-1).
pub const BCO_ENTERED_RETURNED: usize = 0;
pub const BCO_ENTERED_THREW: usize = 1;
pub const BCO_ENTERED_DECLINED: usize = 2;
pub const BCO_BODY_NOT_ENTERABLE: usize = 3;
pub const BCO_SWITCH_OFF: usize = 4;
pub const BCO_NO_JIT: usize = 5;
pub const BCO_NOT_INTRINSIC: usize = 6;
pub const BCO_SHAPE_REFUSED: usize = 7;
pub const BCO_COUNTING: usize = 8;
pub const BCO_COMPILE_PUBLISHED: usize = 9;
pub const BCO_COMPILE_REFUSED: usize = 10;
pub const BCO_ATTEMPTS_SPENT: usize = 11;

/// The reason each [`JitCounterBlock::bytecode_only_census`] slot names.
pub const BYTECODE_ONLY_CENSUS_REASONS: [&str; 12] = [
    "entered the compiled body: returned",
    "entered the compiled body: threw (propagated as from the interpreted body)",
    "entered the compiled body: declined after entry (deopt with no resumable frame) -> interpreted re-run",
    "body published but not enterable (no compiled source, an argument word or count the ABI refuses) -> interpreted",
    "no body: CRATONVM_JIT_BYTECODE_ONLY_KEY unset -> interpreted",
    "no body: JIT off, a virtual thread, or a JVMTI agent that needs the interpreter -> interpreted",
    "no body: not a reviewed Intrinsic whose native runs (none, Bridge, SyntheticStub, or body-compilable) -> execute as before",
    "no body: method refused (<clinit>, synchronized, no Code, or its body invokes this very method) -> interpreted",
    "no body: counting toward the compile threshold -> interpreted",
    "no body: compiled now and published (this call interpreted, the next enters it)",
    "no body: compile attempted and refused (CRATONVM_DBG_JITC names the reason) -> interpreted",
    "no body: compile attempts spent -> interpreted",
];

/// This thread's +1 for bytecode-only door row `row`, with
/// `CRATONVM_DBG_BYTECODE_ONLY_BODY` on.
#[inline]
pub fn note_bytecode_only_outcome(row: usize) {
    if cratonvm_types::flags().jit.dbg_bytecode_only_body {
        JIT_COUNTERS.with(|b| {
            if let Some(c) = b.bytecode_only_census.get(row) {
                JitCounterBlock::bump(c);
            }
        });
    }
}

/// The REFLECT4-21-1 exit report, `None` with
/// `CRATONVM_DBG_BYTECODE_ONLY_BODY` off.
pub fn bytecode_only_census_lines() -> Option<Vec<String>> {
    if !cratonvm_types::flags().jit.dbg_bytecode_only_body {
        return None;
    }
    let mut lines = vec![
        "[cratonvm] bytecode-only door census (REFLECT4-21-1; Intrinsic own-body fallbacks, all threads):"
            .to_string(),
    ];
    for (row, reason) in BYTECODE_ONLY_CENSUS_REASONS.iter().enumerate() {
        let n = jit_counter_total(|b| {
            b.bytecode_only_census
                .get(row)
                .map_or(0, |c| c.load(std::sync::atomic::Ordering::Relaxed))
        });
        if n > 0 {
            lines.push(format!("[cratonvm]   {reason}: {n}"));
        }
    }
    Some(lines)
}

/// The class whose compiled `invokeExact` site [`reflect_accessor_link_redirect`]
/// serves.
pub const REFLECT_ACCESSOR_CLASS: &str = "jdk/internal/reflect/DirectMethodHandleAccessor";

/// `DirectMethodHandleAccessor.invokeImpl(Object, Object[])`'s specialized
/// `invokeExact` sites (JDK 25, `paramCount` 0 to 3), by the accessor's
/// parameter count: the receiver word, then one word per argument, all
/// `Object`, returning `Object`. The spreading arm (`(Object, Object[])`, more
/// than 3 parameters) and the caller-sensitive arms (a trailing `Class`) are
/// not among them.
pub const REFLECT_ACCESSOR_SITES: [&str; 4] = [
    "(Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
];

/// The accessor parameter count a site descriptor stands for, or `None` for
/// any other descriptor ([`REFLECT_ACCESSOR_SITES`]).
pub fn reflect_accessor_site_params(desc: &str) -> Option<usize> {
    REFLECT_ACCESSOR_SITES.iter().position(|s| *s == desc)
}

/// A method descriptor's parameter descriptors (at most `out.len()`, written
/// into `out`) and its return type's first byte; `None` when malformed or
/// wider than `out`. No allocation.
pub fn reflect_accessor_split<'a>(desc: &'a str, out: &mut [&'a str; 3]) -> Option<(usize, u8)> {
    let b = desc.as_bytes();
    if b.first() != Some(&b'(') {
        return None;
    }
    let mut i = 1usize;
    let mut n = 0usize;
    while *b.get(i)? != b')' {
        let start = i;
        while *b.get(i)? == b'[' {
            i += 1;
        }
        if *b.get(i)? == b'L' {
            while *b.get(i)? != b';' {
                i += 1;
            }
        }
        i += 1;
        if n == out.len() {
            return None;
        }
        out[n] = desc.get(start..i)?;
        n += 1;
    }
    let ret = *b.get(i + 1)?;
    Some((n, ret))
}

/// The primitive descriptors in the order of `ReflectLinkIds::wrappers`.
pub const REFLECT_WRAPPER_ORDER: [u8; 8] = *b"IJZBSCFD";

/// REFLECT21-4 (JIT round 21 wave 1, lane reflect): the class id `cell`
/// caches for the JDK class `name` (one definition per VM), filed on first
/// use from the class manager's name index; `None` while no class of that
/// name is loaded (nothing is filed then).
pub fn reflect_link_class_id(
    ctx: &crate::vm::NativeContextImpl<'_>,
    cell: &std::sync::atomic::AtomicU32,
    name: &str,
) -> Option<u32> {
    use std::sync::atomic::Ordering;
    let id = cell.load(Ordering::Relaxed);
    if id != 0 {
        return Some(id);
    }
    let cratonvm_native_api::NameLookup::Unique(cid) = ctx.classify_class_name(name) else {
        return None;
    };
    cell.store(cid.as_u32(), Ordering::Relaxed);
    Some(cid.as_u32())
}

/// The wrapper class an `asType` unboxing from `Object` accepts exactly for a
/// primitive parameter descriptor `p`, or `None` for a reference one.
pub fn reflect_accessor_wrapper(p: u8) -> Option<&'static str> {
    Some(match p {
        b'I' => "java/lang/Integer",
        b'J' => "java/lang/Long",
        b'Z' => "java/lang/Boolean",
        b'B' => "java/lang/Byte",
        b'S' => "java/lang/Short",
        b'C' => "java/lang/Character",
        b'F' => "java/lang/Float",
        b'D' => "java/lang/Double",
        _ => return None,
    })
}

/// The member's word for the accessor's argument word `word` and parameter
/// descriptor `param`, as the accessor target's `asType` adapter makes it --
/// or `None` wherever that adapter could do anything but pass or unbox: a
/// primitive parameter takes only an object of exactly its wrapper class
/// (`null`, a widening `Short` for an `int`, any other class: the JDK's path,
/// which raises or widens), a reference parameter takes `null`, anything for
/// `Object`, and otherwise only a plain object whose class is a subtype of the
/// one class of that name (an array, a lambda proxy, a name two loaders
/// define: the JDK's path, whose cast decides). The accessor's own adapter
/// raises its `ClassCastException` / `NullPointerException` from frames
/// `AccessorUtils.isIllegalArgument` reads, so no refusal is raised here.
/// Pure reads.
#[derive(Clone, Copy)]
enum ReflectParamKind {
    ObjectAny,
    RefClass(ClassId),
    Primitive {
        kind: u8,
        wrapper_cid: u32,
        value_slot: usize,
    },
}

impl ReflectParamKind {
    #[inline(always)]
    fn is_ref(self) -> bool {
        matches!(self, Self::ObjectAny | Self::RefClass(_))
    }
}

fn reflect_param_kind(
    vm: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    param: &str,
) -> Option<ReflectParamKind> {
    let pb = param.as_bytes();
    if let Some(wrapper) = pb
        .first()
        .copied()
        .filter(|_| pb.len() == 1)
        .and_then(reflect_accessor_wrapper)
    {
        let wrapper_index = REFLECT_WRAPPER_ORDER.iter().position(|b| *b == pb[0])?;
        let wrapper_cid = reflect_link_class_id(
            ctx,
            &vm.jit.reflect_link_ids.wrappers[wrapper_index],
            wrapper,
        )?;
        let value_slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(
            vm,
            ClassId::new(wrapper_cid),
            "value",
        )?;
        return Some(ReflectParamKind::Primitive {
            kind: pb[0],
            wrapper_cid,
            value_slot,
        });
    }
    if param == "Ljava/lang/Object;" {
        return Some(ReflectParamKind::ObjectAny);
    }
    let name = param.strip_prefix('L')?.strip_suffix(';')?;
    let cratonvm_native_api::NameLookup::Unique(target) = ctx.classify_class_name(name) else {
        return None;
    };
    Some(ReflectParamKind::RefClass(target))
}

#[inline(always)]
#[inline]
unsafe fn reflect_eval_param_obj(
    vm: &SharedVm,
    kind: ReflectParamKind,
    o: Option<ObjectRef>,
) -> Option<i64> {
    match kind {
        ReflectParamKind::ObjectAny => Some(o.map_or(0, |r| r.as_ptr() as usize as i64)),
        ReflectParamKind::RefClass(target) => {
            let Some(obj) = o else {
                return Some(0);
            };
            let cid = vm.mem.heap.get_header(obj).class_id;
            if cid == target
                || (cid.as_u32() < 0x8000_0000
                    && crate::runtime::interpreter::class_is_subtype(vm, cid, target))
            {
                Some(obj.as_ptr() as usize as i64)
            } else {
                None
            }
        }
        ReflectParamKind::Primitive {
            kind,
            wrapper_cid,
            value_slot,
        } => {
            let obj = o?;
            let hdr = vm.mem.heap.get_header(obj);
            if hdr.class_id.as_u32() != wrapper_cid || value_slot >= hdr.num_slots() as usize {
                return None;
            }
            if let Some((disp, storage)) =
                cratonvm_types::compact_object_field_disp(hdr, value_slot)
            {
                let v = cratonvm_types::read_compact_field(obj.as_ptr().add(disp), storage, std::sync::atomic::Ordering::Relaxed);
                match (kind, v) {
                    (b'I' | b'Z' | b'B' | b'S' | b'C', Value::Int(v)) => Some(i64::from(v)),
                    (b'J', Value::Long(v)) => Some(v),
                    (b'F', Value::Float(v)) => Some(i64::from(v.to_bits())),
                    (b'D', Value::Double(v)) => Some(v.to_bits() as i64),
                    _ => None,
                }
            } else {
                None
            }
        }
    }
}

unsafe fn reflect_eval_param(vm: &SharedVm, kind: ReflectParamKind, word: i64) -> Option<i64> {
    match kind {
        ReflectParamKind::ObjectAny => Some(word),
        _ if word == 0 => reflect_eval_param_obj(vm, kind, None),
        _ => reflect_eval_param_obj(vm, kind, Some(mh_link_object(vm, word)?)),
    }
}

pub fn reflect_accessor_arg(
    vm: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    param: &str,
    word: i64,
) -> Option<i64> {
    let kind = reflect_param_kind(vm, ctx, param)?;
    unsafe { reflect_eval_param(vm, kind, word) }
}

/// The compiled call [`reflect_accessor_link_redirect`] makes: the reflected
/// method's interned info, its words (written into `words`) and their count,
/// and the member's return type's first byte. Pure reads (no Java allocation,
/// no GC point), so every word is the caller's value at the helper's entry.
pub fn reflect_accessor_link_target(
    vm: &SharedVm,
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    info: &JitInvokeInfo,
    params: usize,
    args: &[i64],
    words: &mut [i64; 4],
) -> Option<(&'static JitInvokeInfo, usize, u8)> {
    // REFLECT21-4: the accessor class by its id, filed per VM.
    if reflect_link_class_id(
        ctx,
        &vm.jit.reflect_link_ids.accessors[0],
        REFLECT_ACCESSOR_CLASS,
    ) != Some(info.declaring_class_id)
    {
        return None;
    }
    let handle = mh_link_object(vm, args[0])?;
    let member_obj = cratonvm_native_builtins::lang_invoke::mh_reflect_accessor_member(
        ctx,
        handle,
        info.descriptor,
    )?;
    let member = cratonvm_native_builtins::mh_compiled_member(ctx, member_obj)?;
    // The kinds the compiled `linkTo*` links map them to
    // (`mh_compiled_link_target`): a statically bound member (`REF_invokeSpecial`:
    // private, final, of a final class) as `invokespecial`, under the same switch.
    let kind = match member.ref_kind() {
        6 => 3u8,
        5 => 0u8,
        9 => 2u8,
        7 if cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_special_enabled() => 1u8,
        _ => return None,
    };
    if member.param_count() != params {
        return None;
    }
    let leaf = mh_link_leaf(vm, ctx, &member, kind)?;
    let mut ptypes = [""; 3];
    let (n, ret) = reflect_accessor_split(leaf.descriptor, &mut ptypes)?;
    // Returns the helper boxes as the adapter does: `void` (null), an `int`
    // (`Integer.valueOf`), a reference (as is). Every other one keeps the chain.
    if n != params || !matches!(ret, b'V' | b'I' | b'J' | b'Z' | b'L' | b'[') {
        return None;
    }
    let mut count = 0usize;
    if kind != 3 {
        // `checkReceiver` ran before `invokeImpl`; asked again, so a receiver
        // the member's class does not take is never handed to it.
        let receiver = mh_link_object(vm, args[1])?;
        let cid = jit_dispatch_class_validated(vm, receiver).class_id()?;
        if cid.as_u32() >= 0x8000_0000
            || !crate::runtime::interpreter::class_is_subtype(
                vm,
                cid,
                ClassId::new(leaf.declaring_class_id),
            )
        {
            return None;
        }
        words[0] = args[1];
        count = 1;
    }
    for (i, p) in ptypes[..n].iter().enumerate() {
        words[count] = reflect_accessor_arg(vm, ctx, p, *args.get(2 + i)?)?;
        count += 1;
    }
    reflect_link_note_adapter(vm, ctx, leaf, handle);
    Some((leaf, count, ret))
}

/// JIT round 22 wave 1, lane runtime (flip review R1 / R3 of
/// `r22-optin-flip-review-FIXED-20261021.md`): file, once per
/// linked member (review w3 item 3: keyed by the member, not the per-thread leaf), the spun `LambdaForm$MH` adapter its accessor target
/// (`handle`, the accessor's `asType` handle) runs through -- the target's
/// `form.vmentry` class and method name -- in
/// [`crate::vm::realms::jit_realm::ReflectLinkIds::adapters`]. The linked call
/// skips that adapter's activation; the hidden stand-in table names it from
/// this row (`stackwalker::hidden_invoke_standin_frames_with`), so a
/// `SHOW_HIDDEN_FRAMES` walk or a `-XX:+ShowHiddenFrames` trace from inside
/// the member lists the frames HotSpot lists. Pure reads (field reads, the
/// mirror reverse map, a string read): no Java, no GC point. One read lock
/// and one hash probe on a warm leaf.
/// `CRATONVM_JIT_REFLECT_LINK_HIDDEN_LATCH` (JIT round 22 wave 2, lane
/// runtime; HotSpot parity, **default ON**, `=0` links whatever can observe
/// hidden frames): see [`reflect_link_declines_for_hidden_frames`]. Read once
/// per VM.
pub fn reflect_link_hidden_latch_enabled(vm: &SharedVm) -> bool {
    *vm.jit.reflect_link_ids.hidden_latch_switch.get_or_init(|| {
        true
    })
}

/// `CRATONVM_JIT_REFLECT_LINK_HIDDEN_STANDINS` (JIT round 22 wave 1-2, lane
/// runtime; **default ON since j27 2026-10-28**, `=0` kills): the stand-in
/// synthesis of a linked call's hidden frames (the adapter rows
/// [`reflect_link_note_adapter`] files,
/// `stackwalker::hidden_invoke_standin_frames_with`). The wave-1 build showed
/// it cannot be exact (HotSpot's spun / customised `LambdaForm` shape changes
/// per call); the latch (`CRATONVM_JIT_REFLECT_LINK_HIDDEN_LATCH`, default ON)
/// stays the answer for a VM where hidden frames can be observed, and `=0`
/// here only stops filing the rows. Read once per VM.
pub fn reflect_link_hidden_standins_enabled(vm: &SharedVm) -> bool {
    *vm.jit.reflect_link_ids.hidden_standins_switch.get_or_init(|| {
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
        true
    })
}

/// JIT round 22 wave 2, lane runtime (flip review R1 / R3): may anything in
/// this VM observe the hidden method-handle frames a reflective link skips?
/// The per-VM "hidden frames never observed" latch's terms
/// (`NativeContextImpl::hidden_frames_never_observed`, interpreter round i6
/// wave 117): a `SHOW_HIDDEN_FRAMES` walker created or walked
/// (`NativeMethodRegistry::note_hidden_frames_observed`, never cleared),
/// `-XX:+ShowHiddenFrames` (walks and throwable traces), a JVMTI agent, JDWP,
/// or a JVMTI listener. Then `reflect_accessor_link_redirect` /
/// `reflect_ctor_link_redirect` decline at call time -- every call, already
/// linked compiled sites included -- and the call takes the JDK chain, whose
/// real frames are HotSpot's. Under `CRATONVM_JIT_REFLECT_LINK_HIDDEN_LATCH=0`
/// never. A few loads per call.
pub fn reflect_link_declines_for_hidden_frames(vm: &SharedVm) -> bool {
    if !reflect_link_hidden_latch_enabled(vm) {
        return false;
    }
    let config = &vm.config;
    vm.natives.native_methods.hidden_frames_observed()
        || config.show_hidden_frames
        || !config.jvmti_agent_options.is_empty()
        || config.jdwp_port.is_some()
        || config.jdwp_agent.is_some()
        || crate::runtime::jvmti::any_listener_active()
}

/// JIT round 22 wave 2, lane runtime: class unload drops the
/// [`crate::vm::realms::jit_realm::ReflectLinkIds::adapters`] rows that name an
/// unloaded class (the member's declaring class or the adapter's), so a reused
/// class id is never matched against a dead member's row.
pub fn reflect_link_forget_classes(vm: &SharedVm, unloaded: &dyn Fn(u32) -> bool) {
    vm.jit
        .reflect_link_ids
        .adapters
        .write()
        .retain(|_, row| !unloaded(row.declaring_class_id) && !unloaded(row.adapter_class_id));
}

pub fn reflect_link_note_adapter(
    vm: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    leaf: &'static JitInvokeInfo,
    handle: ObjectRef,
) {
    // Only the synthesis reads the rows (default ON since j27 2026-10-28; `=0`
    // files none).
    if !reflect_link_hidden_standins_enabled(vm) {
        return;
    }
    // Review w3 item 3: one row per member, whichever thread's leaf links it.
    let key = (
        leaf.declaring_class_id,
        leaf.method_name,
        leaf.descriptor,
        leaf.invoke_kind,
    );
    let adapters = &vm.jit.reflect_link_ids.adapters;
    if adapters.read().contains_key(&key) {
        return;
    }
    let Value::Object(Some(form)) = ctx.get_field_by_name(handle, "form") else {
        return;
    };
    let Value::Object(Some(entry)) = ctx.get_field_by_name(form, "vmentry") else {
        return;
    };
    let (Value::Object(Some(clazz)), Value::Object(Some(name))) = (
        ctx.get_field_by_name(entry, "clazz"),
        ctx.get_field_by_name(entry, "name"),
    ) else {
        return;
    };
    let (Some(adapter), Some(method)) = (ctx.class_id_from_mirror(clazz), ctx.read_string(name))
    else {
        return;
    };
    adapters.write().entry(key).or_insert_with(|| {
        crate::vm::realms::jit_realm::ReflectLinkAdapter {
            declaring_class_id: leaf.declaring_class_id,
            method_name: leaf.method_name,
            descriptor: leaf.descriptor,
            adapter_class_id: adapter.as_u32(),
            adapter_method: std::sync::Arc::from(method),
            holder_kind: leaf.invoke_kind,
        }
    });
}

/// JIT round 21 wave 1, lane reflect (`CRATONVM_JIT_REFLECT_ACCESSOR_LINK`,
/// opt-in; probe `R21ReflectInvoke.java`): the compiled
/// `invokeExact` of `DirectMethodHandleAccessor.invokeImpl` -- the call every
/// non-caller-sensitive `Method.invoke` of 0 to 3 parameters makes -- calls
/// the reflected method as an ordinary compiled call ([`mh_bind_direct_call`]
/// with the member's interned info, as the compiled `linkTo*` links do).
/// Before, each call ran the linker chain: `Invokers$Holder.invokeExact_MT`,
/// an `invokeBasic` into the accessor target's spun `LambdaForm$MH` adapter
/// (a hidden class the compiled links refuse, so its native door), the
/// adapter's unboxing, `DirectMethodHandle$Holder.invokeStatic` and
/// `linkToStatic`, then the boxing.
///
/// The adapter's work is done here, and only where it cannot fail
/// ([`reflect_accessor_arg`]): every argument an exact wrapper of its
/// primitive parameter or an instance of its reference parameter, a non-null
/// receiver of the member's class, a `void` / `int` / reference return.
/// Everything else -- the first calls (the member's class not yet initialized
/// or the decode not yet memoised), a widening or failing argument, another
/// return type, a field / constructor / caller-sensitive accessor -- returns
/// `None` and the chain runs, so every refusal (`IllegalArgumentException`
/// "argument type mismatch", the null-unboxing NPE) is still the JDK's own,
/// raised from its own frames. A throwable of the member propagates through
/// the compiled `invokeImpl` frame into `DirectMethodHandleAccessor.invoke`'s
/// `catch`, which wraps it in `InvocationTargetException` as on HotSpot; the
/// member's stack trace lists the frames HotSpot shows (the chain's frames
/// are all `@Hidden` there). A `SHOW_HIDDEN_FRAMES` walk (or a
/// `-XX:+ShowHiddenFrames` trace) from inside the member lists
/// `DirectMethodHandle$Holder.invokeStatic`, `LambdaForm$MH.invoke` and
/// `Invokers$Holder.invokeExact_MT` as HotSpot does: the hidden stand-in table
/// names them for the linked shape (JIT round 22 wave 1, lane runtime; the
/// adapter from the row [`reflect_link_note_adapter`] files).
///
/// GC: the words are read with no GC point between the read and the call;
/// an unboxed word is a primitive, a pass-through word is the caller's own
/// (rooted by the compiled caller's frame), and [`mh_bind_direct_call`]
/// forwards every reference word after its SATB flush. The `Integer.valueOf`
/// box after the call holds no reference of this helper's.
///
/// SAFETY: as [`mh_compiled_link_redirect`]; `args` is the site's argument
/// buffer (the handle first).
const REFLECT_LINK_CACHE_LEN: usize = 8;

#[derive(Clone)]
struct ReflectAccessorCacheEntry {
    vm_id: usize,
    site_key: usize,
    handle_raw: u64,
    epoch: u64,
    retire_epoch: u64,
    leaf: &'static JitInvokeInfo,
    kind: u8,
    ret: u8,
    num_params: usize,
    params: [ReflectParamKind; 3],
}

#[derive(Clone)]
struct ReflectCtorCacheEntry {
    vm_id: usize,
    site_key: usize,
    handle_raw: u64,
    epoch: u64,
    retire_epoch: u64,
    leaf: &'static JitInvokeInfo,
    instance: ClassId,
    num_fields: usize,
    total_size: usize,
    body_size: u32,
    gc_flags: u8,
    num_params: usize,
    params: [ReflectParamKind; 3],
    has_ref_params: bool,
    trivial_init: TrivialInitOp,
    redefine_epoch: u32,
    dispatch: Option<(u64, u32, usize, bool, Option<std::rc::Rc<cratonvm_jit::RetainedCode>>, bool)>,
}

#[derive(Clone, Copy)]
struct TrivialCtorFast {
    override_slot: usize,
    instance: ClassId,
    num_fields: usize,
    total_size: usize,
    body_size: u32,
    gc_flags: u8,
    num_params: usize,
    params: [ReflectParamKind; 3],
    trivial_init: TrivialInitOp,
    redefine_epoch: u32,
}

#[derive(Clone)]
struct CtorNewInstanceCacheEntry {
    vm_id: usize,
    ctor_raw: u64,
    epoch: u64,
    retire_epoch: u64,
    override_slot: usize,
    fast: TrivialCtorFast,
    ctor: ReflectCtorCacheEntry,
}

thread_local! {
    static REFLECT_ACCESSOR_CACHE: std::cell::RefCell<[Option<ReflectAccessorCacheEntry>; REFLECT_LINK_CACHE_LEN]> =
        const { std::cell::RefCell::new([const { None }; REFLECT_LINK_CACHE_LEN]) };
    static REFLECT_CTOR_CACHE: std::cell::RefCell<[Option<ReflectCtorCacheEntry>; REFLECT_LINK_CACHE_LEN]> =
        const { std::cell::RefCell::new([const { None }; REFLECT_LINK_CACHE_LEN]) };
    static CTOR_NEW_INSTANCE_CACHE: std::cell::RefCell<[Option<CtorNewInstanceCacheEntry>; REFLECT_LINK_CACHE_LEN]> =
        const { std::cell::RefCell::new([const { None }; REFLECT_LINK_CACHE_LEN]) };
}

fn reflect_accessor_cache_link_target(
    vm: &SharedVm,
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    info: &JitInvokeInfo,
    params: usize,
    handle_raw: u64,
    epoch: u64,
    retire_epoch: u64,
) -> Option<ReflectAccessorCacheEntry> {
    if reflect_link_class_id(
        ctx,
        &vm.jit.reflect_link_ids.accessors[0],
        REFLECT_ACCESSOR_CLASS,
    ) != Some(info.declaring_class_id)
    {
        return None;
    }
    let handle = mh_link_object(vm, handle_raw as i64)?;
    let member_obj = cratonvm_native_builtins::lang_invoke::mh_reflect_accessor_member(
        ctx,
        handle,
        info.descriptor,
    )?;
    let member = cratonvm_native_builtins::mh_compiled_member(ctx, member_obj)?;
    let kind = match member.ref_kind() {
        6 => 3u8,
        5 => 0u8,
        9 => 2u8,
        7 if cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_special_enabled() => 1u8,
        _ => return None,
    };
    if member.param_count() != params {
        return None;
    }
    let leaf = mh_link_leaf(vm, ctx, &member, kind)?;
    let mut ptypes = [""; 3];
    let (n, ret) = reflect_accessor_split(leaf.descriptor, &mut ptypes)?;
    if n != params || !matches!(ret, b'V' | b'I' | b'L' | b'[') {
        return None;
    }
    let mut param_kinds = [ReflectParamKind::ObjectAny; 3];
    for (i, p) in ptypes[..n].iter().enumerate() {
        param_kinds[i] = reflect_param_kind(vm, ctx, p)?;
    }
    reflect_link_note_adapter(vm, ctx, leaf, handle);
    Some(ReflectAccessorCacheEntry {
        vm_id: vm.vm_identity,
        site_key: info as *const JitInvokeInfo as usize,
        handle_raw,
        epoch,
        retire_epoch,
        leaf,
        kind,
        ret,
        num_params: n,
        params: param_kinds,
    })
}

pub unsafe fn reflect_accessor_link_redirect(
    vm_ptr: i64,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<i64> {
    let params = reflect_accessor_site_params(info.descriptor)?;
    if info.invoke_kind != 0
        || info.method_name != "invokeExact"
        || info.declaring_class_id == 0
        || args.len() != params + 2
    {
        return None;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    // JIT round 22 wave 2, lane runtime (flip review R1): per call, so an
    // already linked compiled site takes the JDK chain once hidden frames can
    // be observed.
    if reflect_link_declines_for_hidden_frames(vm) {
        return None;
    }
    let vm_id = vm.vm_identity;
    let site_key = info as *const JitInvokeInfo as usize;
    let handle_raw = args[0] as u64;
    let epoch = vm.mem.address_epoch();
    let retire_epoch = callee_verdict_retire_epoch();

    let cached = REFLECT_ACCESSOR_CACHE.with(|cache| {
        let mut arr = cache.borrow_mut();
        for i in 0..REFLECT_LINK_CACHE_LEN {
            if let Some(ref e) = arr[i] {
                if e.vm_id == vm_id
                    && e.site_key == site_key
                    && e.handle_raw == handle_raw
                    && e.epoch == epoch
                    && e.retire_epoch == retire_epoch
                {
                    let hit = e.clone();
                    if i != 0 {
                        arr[..=i].rotate_right(1);
                    }
                    return Some(hit);
                }
            }
        }
        None
    });

    let entry = match cached {
        Some(e) => e,
        None => {
            let filled = {
                let (thread, _guard) = jit_thread_mut()?;
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                reflect_accessor_cache_link_target(
                    vm,
                    &mut ctx,
                    info,
                    params,
                    handle_raw,
                    epoch,
                    retire_epoch,
                )?
            };
            REFLECT_ACCESSOR_CACHE.with(|cache| {
                let mut arr = cache.borrow_mut();
                arr.rotate_right(1);
                arr[0] = Some(filled.clone());
            });
            filled
        }
    };

    let mut words = [0i64; 4];
    let mut count = 0usize;
    if entry.kind != 3 {
        let receiver = mh_link_object(vm, args[1])?;
        let cid = jit_dispatch_class_validated(vm, receiver).class_id()?;
        let decl_cid = ClassId::new(entry.leaf.declaring_class_id);
        if cid != decl_cid
            && (cid.as_u32() >= 0x8000_0000
                || !crate::runtime::interpreter::class_is_subtype(vm, cid, decl_cid))
        {
            return None;
        }
        words[0] = args[1];
        count = 1;
    }
    for i in 0..entry.num_params {
        words[count] = reflect_eval_param(vm, entry.params[i], *args.get(2 + i)?)?;
        count += 1;
    }

    // As the linker arm: the door this replaces re-anchored the
    // compiled-recursion budget for its nested call (`L3W79NestedReflectDepth`).
    let budget = rebase_self_call_stack_floor(None);
    let rc = mh_bind_direct_call(vm_ptr, entry.leaf, words.as_ptr() as i64, count as i64);
    restore_self_call_stack_floor(budget);
    if rc != i64::MIN && params == 0 {
        publish_reflect_invoke_cache(
            &vm.jit.reflect_link_ids.invoke_cache,
            vm,
            entry.leaf,
            args[0] as u64,
            words[0],
            0,
            entry.ret,
        );
    }
    // The exception / deopt sentinel `i64::MIN` goes back as is (no `int`
    // result and no `void` call's `0` can equal it).
    Some(match entry.ret {
        _ if rc == i64::MIN => rc,
        b'V' => 0,
        b'I' => jit_integer_value_of_direct_body(vm_ptr, i64::from(rc as i32)),
        b'J' => jit_long_value_of_direct_body(vm_ptr, rc),
        b'Z' => {
            let ids = &vm.jit.reflect_link_ids;
            let cached = if (rc & 1) != 0 {
                ids.boolean_true.load(std::sync::atomic::Ordering::Acquire)
            } else {
                ids.boolean_false.load(std::sync::atomic::Ordering::Acquire)
            };
            if cached != 0 {
                cached as i64
            } else {
                return None;
            }
        }
        _ => rc,
    })
}

/// The class whose compiled `invokeExact` site [`reflect_ctor_link_redirect`]
/// serves.
pub const REFLECT_CTOR_ACCESSOR_CLASS: &str = "jdk/internal/reflect/DirectConstructorHandleAccessor";

/// `DirectConstructorHandleAccessor.invokeImpl(Object[])`'s specialized
/// `invokeExact` sites (JDK 25, `paramCount` 0 to 3), by parameter count.
pub const REFLECT_CTOR_SITES: [&str; 4] = [
    "()Ljava/lang/Object;",
    "(Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
];

/// What [`reflect_ctor_link_redirect`] calls: the `<init>`'s interned info
/// (an `invokespecial`), the class to allocate and its slot count, and the
/// converted argument words after the receiver slot (`words[0]` is left `0`).
/// Pure reads, as [`reflect_accessor_link_target`].
pub fn reflect_ctor_link_target(
    vm: &SharedVm,
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    info: &JitInvokeInfo,
    params: usize,
    args: &[i64],
    words: &mut [i64; 4],
) -> Option<(&'static JitInvokeInfo, ClassId, usize, usize)> {
    if reflect_link_class_id(
        ctx,
        &vm.jit.reflect_link_ids.accessors[1],
        REFLECT_CTOR_ACCESSOR_CLASS,
    ) != Some(info.declaring_class_id)
    {
        return None;
    }
    let handle = mh_link_object(vm, args[0])?;
    let (member_obj, instance) =
        cratonvm_native_builtins::lang_invoke::mh_reflect_ctor_accessor_member(
            ctx,
            handle,
            info.descriptor,
        )?;
    let member = cratonvm_native_builtins::mh_compiled_member(ctx, member_obj)?;
    // `REF_newInvokeSpecial`, of the class the handle allocates (a
    // serialization constructor's ancestor `<init>` keeps the chain).
    if member.ref_kind() != 8 || member.param_count() != params {
        return None;
    }
    let leaf = mh_link_leaf(vm, ctx, &member, 1)?;
    if leaf.declaring_class_id != instance.as_u32()
        || leaf.method_name != "<init>"
        || !ctx.is_class_initialized(instance)
    {
        return None;
    }
    let mut ptypes = [""; 3];
    let (n, ret) = reflect_accessor_split(leaf.descriptor, &mut ptypes)?;
    if n != params || ret != b'V' {
        return None;
    }
    let (num_fields, trivial_init) = {
        let cm = vm.classes.class_manager.read();
        let class = cm.get_class(instance)?;
        if class.is_abstract() || class.is_interface() {
            return None;
        }
        (class.num_total_fields.max(1), trivial_init_plan(vm, &cm, class, leaf, &ptypes[..n]))
    };
    words[0] = 0;
    for (i, p) in ptypes[..n].iter().enumerate() {
        words[1 + i] = reflect_accessor_arg(vm, ctx, p, *args.get(1 + i)?)?;
    }
    reflect_link_note_adapter(vm, ctx, leaf, handle);
    Some((leaf, instance, num_fields, n + 1))
}

/// JIT round 21 wave 1, lane reflect (REFLECT21-3,
/// `CRATONVM_JIT_REFLECT_CTOR_LINK`, opt-in; probe
/// `R21ReflectInvoke.java` `reflect-ctor`): the compiled
/// `invokeExact` of `DirectConstructorHandleAccessor.invokeImpl` -- every
/// `Constructor.newInstance` of 0 to 3 parameters -- does what the target's
/// `newInvokeSpecial` form does, as compiled code would: the compiled `new`
/// (`jit_new_object_body`: initialization barrier, allocation, OOM raise),
/// then the `<init>` as an ordinary compiled `invokespecial`
/// ([`mh_bind_direct_call`]), answering the instance.
///
/// Admission as [`reflect_accessor_link_redirect`]'s: exact-wrapper unboxing
/// and instance-of reference parameters only ([`reflect_accessor_arg`]),
/// every other argument keeps the chain and its JDK refusals; a class not yet
/// initialized, abstract or an interface, or a handle whose `instanceClass`
/// is not the constructor's own (a serialization constructor), keeps it too.
///
/// GC: the argument words are pinned across the allocation (a GC point) and
/// read back; the fresh instance and the words are pinned across the
/// `<init>` call ([`CompileArgPins`], as `call_compiled_then_route` roots a
/// call's arguments) and the instance's current address is what the site
/// receives. A throwable of the allocation (`OutOfMemoryError`) or of the
/// constructor reaches `DirectConstructorHandleAccessor.newInstance`'s own
/// `catch` (an `InvocationTargetException`, as HotSpot's form's
/// `allocateInstance` and `<init>` inside the handle give).
///
/// Frames: the constructor runs directly below the compiled `invokeImpl`; a
/// `SHOW_HIDDEN_FRAMES` walk from inside it gets the hidden frames HotSpot
/// lists (`DirectMethodHandle$Holder.newInvokeSpecial`, the spun adapter,
/// `Invokers$Holder.invokeExact_MT`) from the hidden stand-in table (JIT round
/// 22 wave 1, lane runtime); ordinary walks and traces are unchanged.
///
/// SAFETY: as [`reflect_accessor_link_redirect`].
#[derive(Clone, Copy)]
enum TrivialInitOp {
    None,
    Empty,
    Fields {
        len: u8,
        ops: [(u8, usize, u8); 3],
    },
}

fn trivial_init_plan(
    vm: &SharedVm,
    cm: &cratonvm_classloading::ClassManager,
    class: &cratonvm_classloading::Class,
    leaf: &JitInvokeInfo,
    ptypes: &[&str],
) -> TrivialInitOp {
    if class.find_method("finalize", "()V").is_some() {
        return TrivialInitOp::None;
    }
    let Some(super_id) = class.superclass else {
        return TrivialInitOp::None;
    };
    let Some(obj_cls) = cm.get_class(super_id) else {
        return TrivialInitOp::None;
    };
    if &*obj_cls.name != "java/lang/Object" {
        return TrivialInitOp::None;
    }
    let Some(om) = obj_cls.find_method("<init>", "()V") else {
        return TrivialInitOp::None;
    };
    if om.is_synchronized() {
        return TrivialInitOp::None;
    }
    let Some(oc) = om.code() else {
        return TrivialInitOp::None;
    };
    if oc.code.as_bytes() != [0xb1] || !oc.exception_table.is_empty() {
        return TrivialInitOp::None;
    }
    let Some(m) = class.find_method("<init>", leaf.descriptor) else {
        return TrivialInitOp::None;
    };
    if m.is_synchronized() {
        return TrivialInitOp::None;
    }
    let Some(c) = m.code() else {
        return TrivialInitOp::None;
    };
    let b = c.code.as_bytes();
    if !c.exception_table.is_empty() || b.len() < 5 || b[0] != 0x2a || b[1] != 0xb7 {
        return TrivialInitOp::None;
    }
    let cp_idx = u16::from_be_bytes([b[2], b[3]]);
    let Some(cratonvm_reader::constant_pool::ConstantPoolEntry::MethodReference {
        class_index,
        name_and_type_index,
    }) = class.constant_pool.get(cp_idx)
    else {
        return TrivialInitOp::None;
    };
    if class.constant_pool.get_class_name(*class_index) != Some("java/lang/Object")
        || class.constant_pool.get_name_and_type(*name_and_type_index) != Some(("<init>", "()V"))
    {
        return TrivialInitOp::None;
    }
    if b.len() == 5 && b[4] == 0xb1 && ptypes.is_empty() {
        return TrivialInitOp::Empty;
    }
    // Map local slot index (1..) to parameter index (0..ptypes.len()).
    let mut local_to_param = [u8::MAX; 8];
    let mut slot_idx = 1usize;
    for (pi, pt) in ptypes.iter().enumerate() {
        if slot_idx < local_to_param.len() {
            local_to_param[slot_idx] = pi as u8;
        }
        slot_idx += if matches!(*pt, "J" | "D") { 2 } else { 1 };
    }
    let mut pc = 4usize;
    let mut ops = [(0u8, 0usize, 0u8); 3];
    let mut len = 0usize;
    while pc < b.len() {
        if b[pc] == 0xb1 && pc + 1 == b.len() {
            return if len == 0 && ptypes.is_empty() {
                TrivialInitOp::Empty
            } else if len > 0 {
                TrivialInitOp::Fields {
                    len: len as u8,
                    ops,
                }
            } else {
                TrivialInitOp::None
            };
        }
        if len >= 3 || pc + 5 >= b.len() || b[pc] != 0x2a {
            return TrivialInitOp::None;
        }
        let op = b[pc + 1];
        let (local, load_kind) = match op {
            0x1a..=0x1d => ((op - 0x1a) as usize, b'I'),
            0x1e..=0x21 => ((op - 0x1e) as usize, b'J'),
            0x22..=0x25 => ((op - 0x22) as usize, b'F'),
            0x26..=0x29 => ((op - 0x26) as usize, b'D'),
            0x2a..=0x2d => ((op - 0x2a) as usize, b'L'),
            _ => return TrivialInitOp::None,
        };
        if local >= local_to_param.len() {
            return TrivialInitOp::None;
        }
        let param_idx = local_to_param[local];
        if param_idx == u8::MAX || b[pc + 2] != 0xb5 {
            return TrivialInitOp::None;
        }
        let f_cp = u16::from_be_bytes([b[pc + 3], b[pc + 4]]);
        let Some(cratonvm_reader::constant_pool::ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }) = class.constant_pool.get(f_cp)
        else {
            return TrivialInitOp::None;
        };
        if class.constant_pool.get_class_name(*class_index) != Some(&*class.name) {
            return TrivialInitOp::None;
        }
        let Some((fname, fdesc)) = class.constant_pool.get_name_and_type(*name_and_type_index) else {
            return TrivialInitOp::None;
        };
        let Some(fbyte) = fdesc.as_bytes().first().copied() else {
            return TrivialInitOp::None;
        };
        let kind_ok = match load_kind {
            b'I' => matches!(fbyte, b'I' | b'Z' | b'B' | b'S' | b'C'),
            b'J' => fbyte == b'J',
            b'F' => fbyte == b'F',
            b'D' => fbyte == b'D',
            b'L' => matches!(fbyte, b'L' | b'['),
            _ => false,
        };
        if !kind_ok {
            return TrivialInitOp::None;
        }
        let Some(fslot) = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class.id, fname) else {
            return TrivialInitOp::None;
        };
        ops[len] = (param_idx, fslot, fbyte);
        len += 1;
        pc += 5;
    }
    TrivialInitOp::None
}

fn reflect_ctor_cache_link_target(
    vm: &SharedVm,
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    info: &JitInvokeInfo,
    params: usize,
    handle_raw: u64,
    epoch: u64,
    retire_epoch: u64,
) -> Option<ReflectCtorCacheEntry> {
    if reflect_link_class_id(
        ctx,
        &vm.jit.reflect_link_ids.accessors[1],
        REFLECT_CTOR_ACCESSOR_CLASS,
    ) != Some(info.declaring_class_id)
    {
        return None;
    }
    let handle = mh_link_object(vm, handle_raw as i64)?;
    let (member_obj, instance) =
        cratonvm_native_builtins::lang_invoke::mh_reflect_ctor_accessor_member(
            ctx,
            handle,
            info.descriptor,
        )?;
    let member = cratonvm_native_builtins::mh_compiled_member(ctx, member_obj)?;
    if member.ref_kind() != 8 || member.param_count() != params {
        return None;
    }
    let leaf = mh_link_leaf(vm, ctx, &member, 1)?;
    if leaf.declaring_class_id != instance.as_u32()
        || leaf.method_name != "<init>"
        || !ctx.is_class_initialized(instance)
    {
        return None;
    }
    let mut ptypes = [""; 3];
    let (n, ret) = reflect_accessor_split(leaf.descriptor, &mut ptypes)?;
    if n != params || ret != b'V' {
        return None;
    }
    let (num_fields, trivial_init) = {
        let cm = vm.classes.class_manager.read();
        let class = cm.get_class(instance)?;
        if class.is_abstract() || class.is_interface() {
            return None;
        }
        (class.num_total_fields.max(1), trivial_init_plan(vm, &cm, class, leaf, &ptypes[..n]))
    };
    let mut param_kinds = [ReflectParamKind::ObjectAny; 3];
    let mut has_ref_params = false;
    for (i, p) in ptypes[..n].iter().enumerate() {
        let pk = reflect_param_kind(vm, ctx, p)?;
        if pk.is_ref() {
            has_ref_params = true;
        }
        param_kinds[i] = pk;
    }
    let (total_size, body_size, gc_flags) = crate::runtime::interpreter::plan_tlab_object_shape_for(
        vm,
        instance,
        num_fields,
        crate::runtime::interpreter::tlab_site::JIT_NEW,
    );
    reflect_link_note_adapter(vm, ctx, leaf, handle);
    Some(ReflectCtorCacheEntry {
        vm_id: vm.vm_identity,
        site_key: info as *const JitInvokeInfo as usize,
        handle_raw,
        epoch,
        retire_epoch,
        leaf,
        instance,
        num_fields,
        total_size,
        body_size,
        gc_flags,
        num_params: n,
        params: param_kinds,
        has_ref_params,
        trivial_init,
        redefine_epoch: vm.jit.jit_cache.redefine_epoch(),
        dispatch: None,
    })
}

pub unsafe fn reflect_ctor_link_redirect(
    vm_ptr: i64,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<i64> {
    let params = REFLECT_CTOR_SITES.iter().position(|s| *s == info.descriptor)?;
    if info.invoke_kind != 0
        || info.method_name != "invokeExact"
        || info.declaring_class_id == 0
        || args.len() != params + 1
    {
        return None;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    // JIT round 22 wave 2, lane runtime (flip review R3): as the method link.
    if reflect_link_declines_for_hidden_frames(vm) {
        return None;
    }
    let vm_id = vm.vm_identity;
    let site_key = info as *const JitInvokeInfo as usize;
    let handle_raw = args[0] as u64;
    let epoch = vm.mem.address_epoch();
    let retire_epoch = callee_verdict_retire_epoch();

    let cached = REFLECT_CTOR_CACHE.with(|cache| {
        let mut arr = cache.borrow_mut();
        for i in 0..REFLECT_LINK_CACHE_LEN {
            if let Some(ref e) = arr[i] {
                if e.vm_id == vm_id
                    && e.site_key == site_key
                    && e.handle_raw == handle_raw
                    && e.epoch == epoch
                    && e.retire_epoch == retire_epoch
                {
                    let hit = e.clone();
                    if i != 0 {
                        arr[..=i].rotate_right(1);
                    }
                    return Some(hit);
                }
            }
        }
        None
    });

    let mut entry = match cached {
        Some(e) => e,
        None => {
            let filled = {
                let (thread, _guard) = jit_thread_mut()?;
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                reflect_ctor_cache_link_target(
                    vm,
                    &mut ctx,
                    info,
                    params,
                    handle_raw,
                    epoch,
                    retire_epoch,
                )?
            };
            REFLECT_CTOR_CACHE.with(|cache| {
                let mut arr = cache.borrow_mut();
                arr.rotate_right(1);
                arr[0] = Some(filled.clone());
            });
            filled
        }
    };

    let mut words = [0i64; 4];
    for i in 0..entry.num_params {
        words[1 + i] = reflect_eval_param(vm, entry.params[i], *args.get(1 + i)?)?;
    }
    reflect_ctor_alloc_and_invoke(vm_ptr, vm, &entry, &mut words, false, true)
}

unsafe fn reflect_ctor_alloc_and_invoke(
    vm_ptr: i64,
    vm: &SharedVm,
    entry: &ReflectCtorCacheEntry,
    words: &mut [i64; 4],
    wrap_ite: bool,
    from_accessor_cache: bool,
) -> Option<i64> {
    let vm_id = vm.vm_identity;
    let count = entry.num_params + 1;
    let cur_redef = vm.jit.jit_cache.redefine_epoch();

    // Trivial `<init>` (empty or direct field stores after `Object.<init>()`): allocate and store directly.
    if !matches!(entry.trivial_init, TrivialInitOp::None)
        && cur_redef == entry.redefine_epoch
        && !crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        let mut obj_out = 0i64;
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(obj_ref) = crate::runtime::interpreter::tlab_bump_object_fast(
                &mut *thread,
                vm,
                entry.instance,
                entry.num_fields,
                entry.total_size,
                entry.body_size,
                entry.gc_flags,
            ) {
                if let TrivialInitOp::Fields { len, ops } = entry.trivial_init {
                    for k in 0..(len as usize) {
                        let (pi, slot, fbyte) = ops[k];
                        let w = words[1 + pi as usize];
                        let val = match fbyte {
                            b'I' | b'Z' | b'B' | b'S' | b'C' => Value::Int(w as i32),
                            b'J' => Value::Long(w),
                            b'F' => Value::Float(f32::from_bits(w as u32)),
                            b'D' => Value::Double(f64::from_bits(w as u64)),
                            _ => {
                                let o = if w == 0 {
                                    None
                                } else {
                                    let r = ObjectRef::from_raw(w as *mut u8);
                                    Some(vm.mem.heap.load_and_forward(r))
                                };
                                Value::Object(o)
                            }
                        };
                        vm.mem.heap.set_field(obj_ref, slot, val);
                    }
                }
                return Some(obj_ref.as_ptr() as usize as i64);
            }
            // TLAB full: fall through to general path if fields, or jit_new_object_body if empty.
            if matches!(entry.trivial_init, TrivialInitOp::Empty) {
                drop(_guard);
                let obj = jit_new_object_body(
                    vm_ptr,
                    i64::from(entry.instance.as_u32()),
                    entry.num_fields as i64,
                );
                return Some(if obj == 0 { i64::MIN } else { obj });
            }
            let _ = &mut obj_out;
        }
    }

    // Fast path: pure TLAB bump + pin in ONE `jit_thread_mut()` borrow (no GC point).
    let pin_base = {
        let (thread, _guard) = jit_thread_mut()?;
        if let Some(obj_ref) = crate::runtime::interpreter::tlab_bump_object_fast(
            &mut *thread,
            vm,
            entry.instance,
            entry.num_fields,
            entry.total_size,
            entry.body_size,
            entry.gc_flags,
        ) {
            words[0] = obj_ref.as_ptr() as usize as i64;
            let base = thread.native_pin_roots.len();
            thread.native_pin_roots.push(obj_ref);
            if entry.has_ref_params {
                for i in 0..entry.num_params {
                    if entry.params[i].is_ref() && words[1 + i] != 0 {
                        if let Some(arg_obj) = mh_link_object(vm, words[1 + i]) {
                            let (fwd, valid) = vm.mem.heap.load_and_forward_checked(arg_obj);
                            if valid {
                                words[1 + i] = fwd.as_ptr() as usize as i64;
                                thread.native_pin_roots.push(fwd);
                            }
                        }
                    }
                }
            }
            base
        } else {
            let mut pre_pin_indices = [0u8; 3];
            let mut pre_pin_len = 0usize;
            let base = thread.native_pin_roots.len();
            if entry.has_ref_params {
                for i in 0..entry.num_params {
                    if entry.params[i].is_ref() && words[1 + i] != 0 {
                        if let Some(arg_obj) = mh_link_object(vm, words[1 + i]) {
                            let (fwd, valid) = vm.mem.heap.load_and_forward_checked(arg_obj);
                            if valid {
                                words[1 + i] = fwd.as_ptr() as usize as i64;
                                thread.native_pin_roots.push(fwd);
                                pre_pin_indices[pre_pin_len] = (1 + i) as u8;
                                pre_pin_len += 1;
                            }
                        }
                    }
                }
            }
            drop(_guard);
            let obj = jit_new_object_body(
                vm_ptr,
                i64::from(entry.instance.as_u32()),
                entry.num_fields as i64,
            );
            let Some((thread, _guard)) = jit_thread_mut() else {
                set_jit_deopt_pending();
                return Some(i64::MIN);
            };
            for k in 0..pre_pin_len {
                let idx = pre_pin_indices[k] as usize;
                if let Some(pinned) = thread.native_pin_roots.get(base + k) {
                    words[idx] = pinned.as_ptr() as usize as i64;
                }
            }
            thread.native_pin_roots.truncate(base);
            if obj == 0 {
                return Some(i64::MIN);
            }
            words[0] = obj;
            thread.native_pin_roots.push(ObjectRef::from_raw(obj as *mut u8));
            for k in 0..pre_pin_len {
                let idx = pre_pin_indices[k] as usize;
                if words[idx] != 0 {
                    thread.native_pin_roots.push(ObjectRef::from_raw(words[idx] as *mut u8));
                }
            }
            base
        }
    };

    let leaf = entry.leaf;
    let leaf_ptr = leaf as *const JitInvokeInfo as usize;
    let leaf_key = jit_site_key(vm_id, leaf_ptr);
    flush_raw_entry_dispatch_caches();
    let cur_gen = DISPATCH_CACHE_JIT_GENERATION.with(|g| g.get());
    let mut rc_opt = None;
    if let Some((dgen, dredef, d_entry, d_needs_ctx, ref d_owner, d_rerun_pins)) = entry.dispatch {
        let stale = d_owner.as_ref().is_some_and(|rc| {
            let cm = rc.arc();
            cm.superseded.load(std::sync::atomic::Ordering::Acquire)
                || cm.retired.load(std::sync::atomic::Ordering::Acquire)
        });
        if dgen == cur_gen
            && dredef == cur_redef
            && !stale
            && !crate::runtime::interpreter::jvmti_requires_interpreter(vm)
        {
            rc_opt = Some(match enter_jit_dispatch(vm, words[0]) {
                Ok(_depth) => {
                    let budget = rebase_self_call_stack_floor(None);
                    let routed = call_compiled_then_route(
                        vm,
                        leaf,
                        &words[..count],
                        ClassId::new(0),
                        d_rerun_pins,
                        || match d_owner.as_ref() {
                            Some(owner) => try_call_compiled_entry_reentrant_owned(
                                owner,
                                d_entry,
                                d_needs_ctx,
                                vm_ptr,
                                &words[..count],
                            ),
                            None => try_call_compiled_entry_reentrant(
                                d_entry,
                                d_needs_ctx,
                                vm_ptr,
                                &words[..count],
                            ),
                        },
                    );
                    restore_self_call_stack_floor(budget);
                    match routed {
                        Some(v) => v,
                        None => jit_invoke_dispatch_body(
                            vm_ptr,
                            leaf_ptr as i64,
                            words.as_ptr() as i64,
                            count as i64,
                        ),
                    }
                }
                Err(sentinel) => sentinel,
            });
        }
    }

    let rc = match rc_opt {
        Some(v) => v,
        None => {
            let budget = rebase_self_call_stack_floor(None);
            let v = mh_bind_direct_call(vm_ptr, leaf, words.as_ptr() as i64, count as i64);
            restore_self_call_stack_floor(budget);
            if direct_static_compiled_callee_entry_enabled()
                && mh_bind_direct_leaf_eligible(leaf)
                && mh_bind_direct_natives_decline(vm, leaf, leaf_key)
                && !site_quiesced_for_redefinition(vm, leaf, leaf_key)
            {
                let new_dc = DISPATCH_CACHE.with(|dc| {
                    dc.borrow()
                        .get(&leaf_key)
                        .filter(|c| !c.names_a_stale_body())
                        .map(|c| (c.entry, c.needs_context, c.owner.clone(), c.rerun_pins))
                });
                if let Some((e, nc, ow, rp)) = new_dc {
                    let gen_now = DISPATCH_CACHE_JIT_GENERATION.with(|g| g.get());
                    let redef_now = vm.jit.jit_cache.redefine_epoch();
                    if from_accessor_cache {
                        REFLECT_CTOR_CACHE.with(|cache| {
                            let mut arr = cache.borrow_mut();
                            for slot in arr.iter_mut().flatten() {
                                if slot.vm_id == vm_id
                                    && slot.site_key == entry.site_key
                                    && slot.handle_raw == entry.handle_raw
                                    && slot.epoch == entry.epoch
                                {
                                    slot.dispatch = Some((gen_now, redef_now, e, nc, ow, rp));
                                    break;
                                }
                            }
                        });
                    } else {
                        CTOR_NEW_INSTANCE_CACHE.with(|cache| {
                            let mut arr = cache.borrow_mut();
                            for slot in arr.iter_mut().flatten() {
                                if slot.vm_id == vm_id
                                    && slot.ctor.handle_raw == entry.handle_raw
                                    && slot.epoch == entry.epoch
                                {
                                    slot.ctor.dispatch = Some((gen_now, redef_now, e, nc, ow, rp));
                                    break;
                                }
                            }
                        });
                    }
                }
            }
            v
        }
    };

    let Some((thread, _guard)) = jit_thread_mut() else {
        set_jit_deopt_pending();
        return Some(i64::MIN);
    };
    let now = thread
        .native_pin_roots
        .get(pin_base)
        .map_or(words[0], |p| p.as_ptr() as usize as i64);
    thread.native_pin_roots.truncate(pin_base);
    if rc == i64::MIN && wrap_ite {
        if let Some(exc) = take_jit_pending_exception(thread) {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            if let Ok(Some(Value::Object(Some(ite)))) =
                ctx.new_object("java/lang/reflect/InvocationTargetException")
            {
                let _ = ctx.invoke_special(
                    "java/lang/reflect/InvocationTargetException",
                    "<init>",
                    "(Ljava/lang/Throwable;)V",
                    &[Value::Object(Some(ite)), Value::Object(Some(exc))],
                );
                set_jit_pending_exception(thread, ite);
            } else {
                set_jit_pending_exception(thread, exc);
            }
        }
        return Some(i64::MIN);
    }
    Some(if rc == i64::MIN { rc } else { now })
}

/// Fast path for `java/lang/reflect/Constructor.newInstance([Ljava/lang/Object;)Ljava/lang/Object;`.
#[inline]
unsafe fn ctor_new_instance_eval_args(
    vm: &SharedVm,
    num_params: usize,
    params: &[ReflectParamKind; 3],
    args_raw: i64,
    words: &mut [i64; 4],
) -> bool {
    if num_params == 0 {
        if args_raw != 0 {
            let Some(arr) = mh_link_object(vm, args_raw) else {
                return false;
            };
            if vm.mem.heap.array_length(arr) != 0 {
                return false;
            }
        }
        return true;
    }
    let Some(arr) = mh_link_object(vm, args_raw) else {
        return false;
    };
    if vm.mem.heap.array_length(arr) != num_params {
        return false;
    }
    let hdr = vm.mem.heap.get_header(arr);
    let fast_ref_arr = hdr.kind() == cratonvm_types::ObjectKind::Array
        && hdr.element_type() == cratonvm_types::ArrayElementType::Reference
        && cratonvm_types::narrow_oop::ref_element_size() == 8;
    for i in 0..num_params {
        let obj = if fast_ref_arr {
            let raw = *(arr.as_ptr().add(cratonvm_types::ARRAY_DATA_OFFSET + i * 8) as *const usize);
            if raw == 0 {
                None
            } else if cratonvm_types::plausible_heap_pointer(raw as u64) {
                Some(vm.mem.heap.load_and_forward(ObjectRef::from_raw(raw as *mut u8)))
            } else {
                match vm.mem.heap.get_array_element(arr, i) {
                    Ok(Value::Object(o)) => o,
                    _ => return false,
                }
            }
        } else {
            match vm.mem.heap.get_array_element(arr, i) {
                Ok(Value::Object(o)) => o,
                _ => return false,
            }
        };
        let Some(w) = reflect_eval_param_obj(vm, params[i], obj) else {
            return false;
        };
        words[1 + i] = w;
    }
    true
}

#[inline]
unsafe fn ctor_new_instance_run_trivial(
    vm: &SharedVm,
    ctor_obj: ObjectRef,
    fast: &TrivialCtorFast,
    args_raw: i64,
    set_pending_return: bool,
) -> Option<i64> {
    if matches!(fast.trivial_init, TrivialInitOp::None)
        || vm.jit.jit_cache.redefine_epoch() != fast.redefine_epoch
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        return None;
    }
    if fast.override_slot >= vm.mem.heap.num_fields(ctor_obj)
        || !matches!(vm.mem.heap.get_field(ctor_obj, fast.override_slot), Value::Int(v) if v != 0)
    {
        return None;
    }
    let mut words = [0i64; 4];
    if !ctor_new_instance_eval_args(vm, fast.num_params, &fast.params, args_raw, &mut words) {
        return None;
    }
    let (thread, _guard) = jit_thread_mut()?;
    let obj_ref = crate::runtime::interpreter::tlab_bump_object_fast(
        &mut *thread,
        vm,
        fast.instance,
        fast.num_fields,
        fast.total_size,
        fast.body_size,
        fast.gc_flags,
    )?;
    if let TrivialInitOp::Fields { len, ops } = fast.trivial_init {
        let hdr = vm.mem.heap.get_header(obj_ref);
        let ref_sz = cratonvm_types::narrow_oop::ref_element_size();
        for k in 0..(len as usize) {
            let (pi, slot, fbyte) = ops[k];
            let w = words[1 + pi as usize];
            if let Some((disp, storage)) = cratonvm_types::compact_object_field_disp(hdr, slot) {
                let ptr = obj_ref.as_ptr().add(disp);
                if storage.is_reference() && ref_sz == 8 {
                    let fwd = if w == 0 {
                        0usize
                    } else {
                        let r = ObjectRef::from_raw(w as *mut u8);
                        vm.mem.heap.load_and_forward(r).as_ptr() as usize
                    };
                    *(ptr as *mut usize) = fwd;
                    continue;
                }
                if !storage.is_reference() {
                    let val = match fbyte {
                        b'I' | b'Z' | b'B' | b'S' | b'C' => Value::Int(w as i32),
                        b'J' => Value::Long(w),
                        b'F' => Value::Float(f32::from_bits(w as u32)),
                        b'D' => Value::Double(f64::from_bits(w as u64)),
                        _ => Value::Int(w as i32),
                    };
                    cratonvm_types::write_compact_field(ptr, storage, val, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
            }
            let val = match fbyte {
                b'I' | b'Z' | b'B' | b'S' | b'C' => Value::Int(w as i32),
                b'J' => Value::Long(w),
                b'F' => Value::Float(f32::from_bits(w as u32)),
                b'D' => Value::Double(f64::from_bits(w as u64)),
                _ => {
                    let o = if w == 0 {
                        None
                    } else {
                        let r = ObjectRef::from_raw(w as *mut u8);
                        Some(vm.mem.heap.load_and_forward(r))
                    };
                    Value::Object(o)
                }
            };
            vm.mem.heap.set_field(obj_ref, slot, val);
        }
    }
    if set_pending_return {
        thread.native_pending_return = Some(obj_ref);
    }
    Some(obj_ref.as_ptr() as usize as i64)
}

pub(crate) unsafe fn ctor_new_instance_fast(
    vm_ptr: i64,
    ctor_raw_i64: i64,
    args_raw: i64,
    set_pending_return: bool,
) -> Option<i64> {
    let vm = &*(vm_ptr as *const SharedVm);
    if reflect_link_declines_for_hidden_frames(vm) {
        return None;
    }
    let ctor_raw = ctor_raw_i64 as u64;
    let ctor_obj = mh_link_object(vm, ctor_raw_i64)?;
    let vm_id = vm.vm_identity;
    let epoch = vm.mem.address_epoch();
    let retire_epoch = callee_verdict_retire_epoch();

    let fast = CTOR_NEW_INSTANCE_CACHE.with(|cache| {
        let mut arr = cache.borrow_mut();
        for i in 0..REFLECT_LINK_CACHE_LEN {
            if let Some(ref e) = arr[i] {
                if e.vm_id == vm_id
                    && e.ctor_raw == ctor_raw
                    && e.epoch == epoch
                    && e.retire_epoch == retire_epoch
                {
                    let f = e.fast;
                    if i != 0 {
                        arr[..=i].rotate_right(1);
                    }
                    return Some(f);
                }
            }
        }
        None
    });

    if let Some(f) = fast {
        return ctor_new_instance_run_trivial(vm, ctor_obj, &f, args_raw, set_pending_return);
    }
    None
}

pub unsafe fn ctor_new_instance_redirect(
    vm_ptr: i64,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<i64> {
    if args.len() != 2 {
        return None;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if reflect_link_declines_for_hidden_frames(vm) {
        return None;
    }
    let ctor_raw = args[0] as u64;
    let ctor_obj = mh_link_object(vm, args[0])?;
    let vm_id = vm.vm_identity;
    let epoch = vm.mem.address_epoch();
    let retire_epoch = callee_verdict_retire_epoch();

    enum CtorCacheHit {
        Trivial(TrivialCtorFast),
        Full(CtorNewInstanceCacheEntry),
    }

    let cached = CTOR_NEW_INSTANCE_CACHE.with(|cache| {
        let mut arr = cache.borrow_mut();
        for i in 0..REFLECT_LINK_CACHE_LEN {
            if let Some(ref e) = arr[i] {
                if e.vm_id == vm_id
                    && e.ctor_raw == ctor_raw
                    && e.epoch == epoch
                    && e.retire_epoch == retire_epoch
                {
                    let hit = if !matches!(e.fast.trivial_init, TrivialInitOp::None) {
                        CtorCacheHit::Trivial(e.fast)
                    } else {
                        CtorCacheHit::Full(e.clone())
                    };
                    if i != 0 {
                        arr[..=i].rotate_right(1);
                    }
                    return Some(hit);
                }
            }
        }
        None
    });

    let entry = match cached {
        Some(CtorCacheHit::Trivial(f)) => {
            if let Some(res) = ctor_new_instance_run_trivial(vm, ctor_obj, &f, args[1], false) {
                return Some(res);
            }
            return None;
        }
        Some(CtorCacheHit::Full(e)) => e,
        None => {
            let filled = {
                let (thread, _guard) = jit_thread_mut()?;
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                let ctor_cid = jit_dispatch_class_validated(vm, ctor_obj).class_id()?;
                let override_slot =
                    crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, ctor_cid, "override")?;
                if override_slot >= vm.mem.heap.num_fields(ctor_obj) {
                    return None;
                }
                if !matches!(vm.mem.heap.get_field(ctor_obj, override_slot), Value::Int(v) if v != 0) {
                    return None;
                }
                let Value::Object(Some(accessor)) =
                    ctx.get_field_by_name(ctor_obj, "constructorAccessor")
                else {
                    return None;
                };
                let acc_cid = jit_dispatch_class_validated(vm, accessor).class_id()?;
                let acc_expected = reflect_link_class_id(
                    &ctx,
                    &vm.jit.reflect_link_ids.accessors[1],
                    REFLECT_CTOR_ACCESSOR_CLASS,
                )?;
                if acc_cid.as_u32() != acc_expected {
                    return None;
                }
                let Value::Object(Some(target_mh)) = ctx.get_field_by_name(accessor, "target") else {
                    return None;
                };
                let handle_raw = target_mh.as_ptr() as usize as u64;
                let handle = mh_link_object(vm, handle_raw as i64)?;
                let acc_pc = ctx
                    .get_field_by_name(accessor, "paramCount")
                    .as_int()
                    .and_then(|n| usize::try_from(n).ok())?;
                let site_desc = REFLECT_CTOR_SITES.get(acc_pc)?;
                let (member_obj, instance) =
                    cratonvm_native_builtins::lang_invoke::mh_reflect_ctor_accessor_member(
                        &mut ctx,
                        handle,
                        site_desc,
                    )?;
                let member = cratonvm_native_builtins::mh_compiled_member(&mut ctx, member_obj)?;
                let params = member.param_count();
                if member.ref_kind() != 8 || params > 3 {
                    return None;
                }
                let leaf = mh_link_leaf(vm, &mut ctx, &member, 1)?;
                if leaf.declaring_class_id != instance.as_u32()
                    || leaf.method_name != "<init>"
                    || !ctx.is_class_initialized(instance)
                {
                    return None;
                }
                let mut ptypes = [""; 3];
                let (n, ret) = reflect_accessor_split(leaf.descriptor, &mut ptypes)?;
                if n != params || ret != b'V' {
                    return None;
                }
                let (num_fields, trivial_init) = {
                    let cm = vm.classes.class_manager.read();
                    let class = cm.get_class(instance)?;
                    if class.is_abstract() || class.is_interface() {
                        return None;
                    }
                    (class.num_total_fields.max(1), trivial_init_plan(vm, &cm, class, leaf, &ptypes[..n]))
                };
                let mut param_kinds = [ReflectParamKind::ObjectAny; 3];
                let mut has_ref_params = false;
                for (i, p) in ptypes[..n].iter().enumerate() {
                    let pk = reflect_param_kind(vm, &ctx, p)?;
                    if pk.is_ref() {
                        has_ref_params = true;
                    }
                    param_kinds[i] = pk;
                }
                let (total_size, body_size, gc_flags) =
                    crate::runtime::interpreter::plan_tlab_object_shape_for(
                        vm,
                        instance,
                        num_fields,
                        crate::runtime::interpreter::tlab_site::JIT_NEW,
                    );
                reflect_link_note_adapter(vm, &ctx, leaf, handle);
                let redefine_epoch = vm.jit.jit_cache.redefine_epoch();
                CtorNewInstanceCacheEntry {
                    vm_id,
                    ctor_raw,
                    epoch,
                    retire_epoch,
                    override_slot,
                    fast: TrivialCtorFast {
                        override_slot,
                        instance,
                        num_fields,
                        total_size,
                        body_size,
                        gc_flags,
                        num_params: n,
                        params: param_kinds,
                        trivial_init,
                        redefine_epoch,
                    },
                    ctor: ReflectCtorCacheEntry {
                        vm_id,
                        site_key: info as *const JitInvokeInfo as usize,
                        handle_raw,
                        epoch,
                        retire_epoch,
                        leaf,
                        instance,
                        num_fields,
                        total_size,
                        body_size,
                        gc_flags,
                        num_params: n,
                        params: param_kinds,
                        has_ref_params,
                        trivial_init,
                        redefine_epoch,
                        dispatch: None,
                    },
                }
            };
            CTOR_NEW_INSTANCE_CACHE.with(|cache| {
                let mut arr = cache.borrow_mut();
                arr.rotate_right(1);
                arr[0] = Some(filled.clone());
            });
            filled
        }
    };

    if entry.override_slot >= vm.mem.heap.num_fields(ctor_obj)
        || !matches!(vm.mem.heap.get_field(ctor_obj, entry.override_slot), Value::Int(v) if v != 0)
    {
        return None;
    }

    let mut words = [0i64; 4];
    if !ctor_new_instance_eval_args(vm, entry.ctor.num_params, &entry.ctor.params, args[1], &mut words) {
        return None;
    }
    reflect_ctor_alloc_and_invoke(vm_ptr, vm, &entry.ctor, &mut words, true, false)
}
