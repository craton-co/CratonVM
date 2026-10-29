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
fn mh_constant_bind_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND")
    })
}

/// One bound (or refused) `invokeExact` site of this thread.
struct MhBindSite {
    vm: usize,
    epoch: u64,
    /// The handle's address, then the values of
    /// `MH_COMPILED_BIND_GUARD_SLOTS` ([`mh_bind_slot_key`]).
    guard: [u64; 7],
    /// The target's dispatch info, or `None`: the admission refused this handle.
    leaf: Option<&'static JitInvokeInfo>,
    /// The admission bound a `bindTo` handle (round 16 wave 8,
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_BOUND`): the call's receiver is the guard
    /// word of `MH_BOUND`, spliced in place of the handle ([`mh_constant_bind_call`]).
    bound: bool,
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT` as the fill read it (round 16
    /// wave 10, lane mhfloor; MH16-8): the bound call enters the target's
    /// cached compiled entry without the dispatch ladder
    /// ([`mh_bind_direct_call`]). Carried here so the warm path reads no flag.
    direct: bool,
    /// The bound handle's shape (round 16 wave 11, lane mhbind; MHF-1,
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_SHAPE`): another handle of this shape
    /// in this epoch is called as this site's target ([`mh_bind_shape_matches`]).
    /// `None`: the switch was off at the fill, or the admission refused.
    shape: Option<MhBindShape>,
    /// The converting bind's per-call work (round 16 wave 15, lane mh6; MHB-1,
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING`), or `None` for a site that
    /// calls its target with the caller's own words.
    conv: Option<MhBindConv>,
    /// Cold fills so far; a site whose handle keeps changing stops binding
    /// ([`MH_BIND_MAX_FILLS`]).
    fills: u32,
    /// [`MhBindHit::real`].
    real: bool,
    /// [`MhBindHit::iface`].
    iface: u32,
    /// The last receiver class the interface guard accepted at this site
    /// (`0`: none yet), so a monomorphic site's guard is one compare. Reset
    /// with the site on every fill (a new epoch re-fills, so an unloaded
    /// class's id never survives here).
    iface_seen: std::cell::Cell<u32>,
    /// JIT r22 retirement (lane inline; the wave-3 count's cost): the bound
    /// REAL handle (`guard[0]`, in `epoch`) was seen with a CUSTOMIZED form,
    /// so its later calls need no `checkCustomized` read
    /// ([`mh_bind_customization_serves`]). Exact: JDK 25's `customize()` is
    /// one-way and `LambdaForm.customized` is `final`; the only other
    /// `MethodHandle.updateForm` caller, `DirectMethodHandle.ensureInitialized`,
    /// runs on the handle's first call, before any customization. Reset with
    /// the row on every fill (a new epoch re-fills).
    customized_seen: std::cell::Cell<bool>,
    /// gcj j3/rt3 (`gcj-j2jt2-mh-site-memos-keyed-by-a-reusable-jit-invoke-info-address`):
    /// the row's proof that the `JitInvokeInfo` address it is keyed by still
    /// names the site that filled it ([`MhSiteIdentity`]).
    identity: MhSiteIdentity,
    /// A REAL `bindTo` result's shape (JIT round 21 wave 17, lane mhbind;
    /// MHB-3, `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE`): another species
    /// instance of this shape in this epoch is called as this site's target
    /// with ITS bound value as the receiver ([`mh_real_bound_shape_receiver`]).
    /// `None`: not a real bound site, or the switch was off at the fill.
    real_shape: Option<MhRealBoundShape>,
    /// A REAL `asType` result's per-call conversions (j27 wave 5, lane
    /// MHASTYPE; `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING`), or `None`.
    real_conv: Option<MhRealConv>,
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
struct MhRealBoundShape {
    /// The species class (`BoundMethodHandle$Species_LL`).
    class_id: u32,
    /// The slots of `argL0`, `argL1`, `form` and `type` in the species.
    target_slot: usize,
    receiver_slot: usize,
    form_slot: usize,
    type_slot: usize,
    /// The admitted handle's `argL0`, `form` and `type` words.
    target_word: u64,
    form_word: u64,
    type_word: u64,
    /// The admitted bound value's exact class.
    receiver_class: u32,
    /// A second exact class a fill of this same shape admitted in this epoch
    /// (JIT round 21 wave 19, lane mhbind2;
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE_TWO_CLASSES`), or `0`: none.
    receiver_class_alt: u32,
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
struct MhSiteIdentity {
    /// The retire count the row was made, or last re-proved, at.
    retire_epoch: std::cell::Cell<u64>,
    declaring: u32,
    invoke: bool,
    descriptor: Box<str>,
}

impl MhSiteIdentity {
    /// The identity of `info`, read while `info` is live (the caller is
    /// crossing its site), so the epoch is one at which the address names it.
    fn of(info: &JitInvokeInfo) -> Self {
        Self::at(info, callee_verdict_retire_epoch())
    }

    fn at(info: &JitInvokeInfo, retire_epoch: u64) -> Self {
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
    fn names(
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
fn mh_site_identity_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_SITE_IDENTITY")
}

/// What a shape-keyed bound site compares on a handle at another address
/// (round 16 wave 11, lane mhbind; MHF-1): with the guard words of
/// `MH_COMPILED_BIND_GUARD_SLOTS`, every slot `mh_compiled_bind_target`
/// decides on, so two handles that agree on all of them get the same
/// admission answer -- the same target, and for a `bindTo` handle the same
/// receiver (`MH_BOUND` is a guard slot).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MhBindShape {
    /// The handle's class (the admission requires `java/lang/invoke/MethodHandle`).
    class_id: u32,
    /// The handle's slot count (the admission's width screens, and the
    /// downcall test's, read it).
    width: usize,
    /// The slot of the class's `type` field (`get_field_by_name`'s resolution).
    type_slot: usize,
    /// The words of `MH_COMPILED_BIND_SHAPE_SLOTS`, then `type`'s.
    words: [u64; 5],
}

/// A converting bound site's per-call work (JIT round 16 wave 15, lane mh6;
/// MHB-1): the admission's [`MhBindConversion`] plus the two slots the warm
/// path reads, resolved on the fill ([`mh_bind_conv_of`]).
///
/// [`MhBindConversion`]: cratonvm_native_builtins::lang_invoke::MhBindConversion
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MhBindConv {
    /// The handle's `type` slot, and its word at the fill. The conversions
    /// are a function of the declared type (and the site's), so the warm path
    /// re-reads it with the guard slots: the one `MethodType` identity check.
    type_slot: usize,
    type_word: u64,
    /// Argument words (after the handle) to unbox from an exact `Integer`.
    unbox_int: u32,
    /// Box the member's `int` result through `Integer.valueOf`.
    box_int_return: bool,
    /// `java/lang/Integer`'s class id, and the slot of its `value`.
    integer_class_id: u32,
    value_slot: usize,
}

/// A REAL `asType` result's per-call work (j27 wave 5, lane MHASTYPE; the
/// admission is `lang_invoke::mh_real_converting_compiled_bind`, its answer
/// resolved on the fill by [`mh_real_conv_of`]). The handle is real, so its
/// guard is its address in the epoch (`MhBindHit::real`): no slot to re-read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MhRealConv {
    /// Per member argument word: `0` passes the caller's word; otherwise the
    /// bit mask over `wrappers` of the exact classes whose value replaces it
    /// ([`mh_real_unbox_word`]).
    unbox: [u8; MH_BIND_MAX_SPLICED_ARGS],
    /// `(class id, slot of value)` of each `lang_invoke::MH_REAL_UNBOX_WRAPPERS`
    /// entry; class id `0`: not loaded (never matched).
    wrappers: [(u32, usize); 5],
    box_return: cratonvm_native_builtins::lang_invoke::MhRealBox,
}

/// What the redirect's site lookup (or its fill) answers for a bound call.
#[derive(Clone, Copy)]
struct MhBindHit {
    leaf: &'static JitInvokeInfo,
    guard: [u64; 7],
    bound: bool,
    direct: bool,
    conv: Option<MhBindConv>,
    /// A REAL `DirectMethodHandle` (SBR3 wave 2, lane mhperf): none of the carrier's
    /// slots to re-read, every field the admission decided on is `final`; the handle's
    /// address in this epoch is the whole guard.
    real: bool,
    /// An interface member's class id (JIT round 18 wave 11, lane mhresid;
    /// `CRATONVM_JIT_MH_CONSTANT_BIND_INTERFACE`): every call's receiver must be
    /// an instance of a class implementing it, else the door takes the call.
    /// `0`: no receiver type guard.
    iface: u32,
    /// [`MhBindSite::iface_seen`] as the warm path read it.
    iface_seen: u32,
    /// [`MhBindSite::customized_seen`] as the warm path read it, for the
    /// handle the row was filled with only.
    customized_seen: bool,
    /// [`MhBindSite::real_conv`].
    real_conv: Option<MhRealConv>,
}

/// A converting call's member words ([`mh_bind_convert_args`]): the site's
/// words after the handle, unboxed where the conversion says.
#[derive(Clone, Copy)]
struct MhBindConverted {
    words: [i64; MH_BIND_MAX_SPLICED_ARGS],
    box_int_return: bool,
    /// Box the member's `long` result through `Long.valueOf` (j27 wave 5,
    /// lane MHASTYPE; a real `asType` result only).
    box_long_return: bool,
}

/// How [`mh_constant_bind_call`] makes a bound call: the target's info, the
/// bound receiver's word for a `bindTo` handle (round 16 wave 8), the site's
/// [`MhBindSite::direct`] (round 16 wave 10), and a converting call's words
/// (round 16 wave 15).
#[derive(Clone, Copy)]
struct MhBindRoute {
    leaf: &'static JitInvokeInfo,
    bound: Option<i64>,
    direct: bool,
    converted: Option<MhBindConverted>,
}

/// Fills after which a site is left to the door for good (a megamorphic
/// handle site would pay the admission on every call).
const MH_BIND_MAX_FILLS: u32 = 8;
/// The widest bound call [`mh_constant_bind_call`] splices (the handle's slot
/// becomes the receiver's, so the leaf takes as many words as the site);
/// a wider bound site keeps the door.
const MH_BIND_MAX_SPLICED_ARGS: usize = 16;
/// Bound sites and interned target infos kept per thread.
const MH_BIND_MAX_SITES: usize = 1024;
/// JIT round 20 wave 19, lane pRefl: 256 -> 1024, since the compiled
/// signature-polymorphic links (`mh_compiled_link_redirect`) intern their
/// targets here too and must not crowd the constant binds out.
const MH_BIND_MAX_LEAVES: usize = 1024;

/// `(vm_identity, target ClassId, class, name, descriptor, kind)`: class ids are
/// per VM, so the VM leads the key.
type MhLeafKey = (usize, u32, Box<str>, Box<str>, Box<str>, u8);

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
fn mh_compiled_link_unbound_enabled(vm: &SharedVm) -> bool {
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
    static MH_LINKED_SITES: std::cell::RefCell<
        // addr-keyed: not a heap address -- VM identity + `JitInvokeInfo` (retire epoch + content).
        rustc_hash::FxHashMap<(usize, usize), MhSiteIdentity>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    /// Bound sites, by the caller's `JitInvokeInfo` address; a row answers
    /// only while its [`MhSiteIdentity`] names the site (gcj j3/rt3).
    // addr-keyed: not a heap address -- `JitInvokeInfo` (retire epoch + content).
    static MH_BIND_SITES: std::cell::RefCell<rustc_hash::FxHashMap<usize, MhBindSite>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    /// Target infos, interned per (class, name, descriptor, kind): leaked, so
    /// the dispatch caches keyed by an info's address
    /// (`jit_invoke_dispatch_body`'s per-site memos) stay valid for the life
    /// of the thread. Bounded by [`MH_BIND_MAX_LEAVES`].
    static MH_BIND_LEAVES: std::cell::RefCell<rustc_hash::FxHashMap<MhLeafKey, &'static JitInvokeInfo>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// A slot value as a guard word (an object's address, an `Int` tagged so it
/// can never equal an address).
fn mh_bind_slot_key(v: Value) -> u64 {
    match v {
        Value::Object(Some(o)) => o.as_ptr() as u64,
        Value::Object(None) => 0,
        Value::Int(i) => (1u64 << 63) | u64::from(i as u32),
        _ => u64::MAX,
    }
}

/// The interned dispatch info for a bound target.
fn mh_bind_leaf_info(
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
unsafe fn mh_constant_bind_redirect(
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
        (None, Some(rc)) => Some(mh_real_convert_args(&rc, &args[1..], |mask, word| {
            mh_real_unbox_word(vm, &rc, mask, word)
        })?),
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
unsafe fn mh_bind_customization_serves(vm: &SharedVm, site_key: usize, handle_word: i64) -> bool {
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
    let Some((thread, _guard)) = jit_thread_mut() else {
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
unsafe fn mh_bind_handle_count_raw(vm: &SharedVm, mh: ObjectRef) -> bool {
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
unsafe fn mh_bind_handle_is_customized(vm: &SharedVm, mh: ObjectRef) -> bool {
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
fn mh_bind_convert_args(
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
fn mh_bind_convert_words(
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
fn mh_real_convert_args(
    conv: &MhRealConv,
    words: &[i64],
    unbox: impl Fn(u8, i64) -> Option<i64>,
) -> Option<MhBindConverted> {
    use cratonvm_native_builtins::lang_invoke::MhRealBox;
    let words = mh_bind_convert_words(words, |i, word| match conv.unbox[i] {
        0 => Some(word),
        mask => unbox(mask, word),
    })?;
    Some(MhBindConverted {
        words,
        box_int_return: conv.box_return == MhRealBox::Int,
        box_long_return: conv.box_return == MhRealBox::Long,
    })
}

/// The object at the word `raw` and its exact class id, or `None` for a
/// null, misaligned or non-object word.
fn mh_bind_exact_class(vm: &SharedVm, raw: i64) -> Option<(ObjectRef, u32)> {
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
fn mh_bind_unbox_int(vm: &SharedVm, conv: &MhBindConv, raw: i64) -> Option<i64> {
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
fn mh_real_unbox_word(vm: &SharedVm, conv: &MhRealConv, mask: u8, raw: i64) -> Option<i64> {
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
fn mh_real_widen(k: usize, value: Value) -> Option<i64> {
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
unsafe fn mh_constant_bind_call(
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
fn mh_constant_bind_direct_flag() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT")
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
fn mh_constant_bind_direct_interface_flag() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT_INTERFACE")
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
fn mh_bind_direct_leaf_eligible(leaf: &JitInvokeInfo) -> bool {
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
fn mh_bind_direct_natives_decline(vm: &SharedVm, leaf: &JitInvokeInfo, key: JitSiteKey) -> bool {
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
unsafe fn mh_bind_direct_call(
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
enum MhLinkShape {
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
fn mh_link_shape(info: &JitInvokeInfo) -> Option<MhLinkShape> {
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
fn mh_link_basic_sig(desc: &str) -> Option<(Vec<u8>, u8)> {
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
fn mh_link_basic_agree(
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
const JR21_REFLECT2_SIG_CAP: usize = 256;

/// [`mh_link_basic_sig`] into a caller's buffer: the parameter count and the
/// return's basic type, the parameters' basic types in `out[..count]`.
/// `Err(())` when the descriptor has more parameters than the buffer holds
/// (the caller then asks [`mh_link_basic_sig`]); `Ok(None)` exactly where
/// [`mh_link_basic_sig`] answers `None`.
fn jr21_reflect2_basic_sig(
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
fn jr21_reflect2_basic_agree(
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
fn jr21_reflect2_basic_agree_slices(
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
fn mh_link_object(vm: &SharedVm, word: i64) -> Option<ObjectRef> {
    let bits = word as u64;
    if bits == 0 || (bits & 0x7) != 0 || bits >= (1u64 << 48) {
        return None;
    }
    vm.mem.heap.is_object_address(bits as usize)
}

/// The interned call info for `member` as a compiled call of `invoke_kind`,
/// filed on the member's memoised record so a warm site builds no string; a
/// refusal is filed too (`1`), for the record's epoch.
fn mh_link_leaf(
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
fn jr21_reflect4_hidden_leaf(
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
fn mh_bind_hidden_leaf_info(
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
fn jr21_reflect3_agree_layout(
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
fn jr21_reflect3_basic_agree_memo(
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
const MH_LINK_HIDDEN_TAG: usize = 2;

/// `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN` (perf, opt-in; JIT round 21 wave 1,
/// lane reflect): read per call through `NativeFlags`.
fn mh_compiled_link_hidden_enabled() -> bool {
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
fn mh_link_leaf_static(
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
unsafe fn mh_hidden_static_call(
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
    let Some((thread, _guard)) = jit_thread_mut() else {
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
fn mh_compiled_link_target(
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
                && cratonvm_types::flags().natives.mh_ctor_registers_finalizable
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
fn mh_by_id_body_admissible(body: &cratonvm_jit::CompiledMethod) -> bool {
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
fn mh_compiled_basic_by_id(
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
fn mh_compiled_linker_by_id(
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

#[cfg(test)]
mod i116_l2_by_id_body_tests {
    use super::mh_by_id_body_admissible;
    use std::sync::atomic::Ordering;

    fn body() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
        buf.emit(&[0xC3]); // RET
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.method_declares_no_handlers = true;
        cm
    }

    #[test]
    fn a_plain_live_handlerless_body_is_admitted() {
        assert!(mh_by_id_body_admissible(&body()));
    }

    #[test]
    fn every_by_name_service_and_a_stale_body_refuse() {
        let mut cm = body();
        cm.method_declares_no_handlers = false;
        assert!(!mh_by_id_body_admissible(&cm), "handlers: a re-run resolves by name");
        let cm = body();
        cm.retired.store(true, Ordering::Release);
        assert!(!mh_by_id_body_admissible(&cm));
        let cm = body();
        cm.superseded.store(true, Ordering::Release);
        assert!(!mh_by_id_body_admissible(&cm));
        let mut cm = body();
        cm.can_osr_exit = true;
        assert!(!mh_by_id_body_admissible(&cm));
        let mut cm = body();
        cm.has_indy_trap = true;
        assert!(!mh_by_id_body_admissible(&cm));
        let mut cm = body();
        cm.has_frameless_trap_stub = true;
        assert!(!mh_by_id_body_admissible(&cm));
        let mut cm = body();
        cm.trap_cause_points = vec![(1u64, 8u8)].into_boxed_slice();
        assert!(!mh_by_id_body_admissible(&cm));
        let mut cm = body();
        cm.osr_exit_points = vec![4];
        assert!(!mh_by_id_body_admissible(&cm));
        let mut cm = body();
        cm.post_call_own_exit_bcis = vec![4];
        assert!(!mh_by_id_body_admissible(&cm));
        let mut cm = body();
        cm.requires_wrapped_entry = true;
        assert!(!mh_by_id_body_admissible(&cm));
        let mut cm = body();
        cm.self_locks_monitor = true;
        assert!(!mh_by_id_body_admissible(&cm));
    }
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
unsafe fn mh_compiled_basic_by_id_call(
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
unsafe fn mh_compiled_link_redirect(
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
const MHL_DISABLED: usize = 0;
const MHL_NO_THREAD: usize = 1;
const MHL_LINKER_WIDE: usize = 2;
const MHL_LINKER_HANDLE: usize = 3;
const MHL_LINKER_PAIR: usize = 4;
const MHL_LINKER_NULL_FIRST: usize = 5;
const MHL_LINKER_DECODE: usize = 6;
const MHL_LINKER_SHAPE: usize = 7;
const MHL_LINKER_LEAF: usize = 8;
const MHL_LINKER_SERVED: usize = 9;
const MHL_BASIC_WIDE: usize = 11;
const MHL_BASIC_HANDLE: usize = 12;
const MHL_BASIC_VMENTRY: usize = 13;
const MHL_BASIC_DECODE: usize = 14;
const MHL_BASIC_SHAPE: usize = 15;
const MHL_BASIC_LEAF_HIDDEN: usize = 16;
const MHL_BASIC_LEAF: usize = 17;
const MHL_BASIC_SERVED: usize = 18;
const MHL_LINKTO_WIDE: usize = 20;
const MHL_LINKTO_MEMBER_WORD: usize = 21;
const MHL_LINKTO_DECODE: usize = 22;
const MHL_LINKTO_KIND: usize = 23;
const MHL_LINKTO_SHAPE: usize = 24;
const MHL_LINKTO_LEAF: usize = 25;
const MHL_LINKTO_SERVED: usize = 26;

/// The reason each [`JitCounterBlock::mh_link_census`] slot names. "door"
/// means the site ran its registered native door for that call.
const MH_LINK_CENSUS_REASONS: [&str; 28] = [
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
fn mh_link_census_on() -> bool {
    cratonvm_types::flags().jit.dbg_mh_link_census
}

/// This thread's +1 for census row `row`, with the census on.
#[inline]
fn note_mh_link_census(row: usize) {
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
fn mh_link_census_served_row(
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
pub(crate) const BCO_ENTERED_RETURNED: usize = 0;
pub(crate) const BCO_ENTERED_THREW: usize = 1;
pub(crate) const BCO_ENTERED_DECLINED: usize = 2;
pub(crate) const BCO_BODY_NOT_ENTERABLE: usize = 3;
pub(crate) const BCO_SWITCH_OFF: usize = 4;
pub(crate) const BCO_NO_JIT: usize = 5;
pub(crate) const BCO_NOT_INTRINSIC: usize = 6;
pub(crate) const BCO_SHAPE_REFUSED: usize = 7;
pub(crate) const BCO_COUNTING: usize = 8;
pub(crate) const BCO_COMPILE_PUBLISHED: usize = 9;
pub(crate) const BCO_COMPILE_REFUSED: usize = 10;
pub(crate) const BCO_ATTEMPTS_SPENT: usize = 11;

/// The reason each [`JitCounterBlock::bytecode_only_census`] slot names.
const BYTECODE_ONLY_CENSUS_REASONS: [&str; 12] = [
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
pub(crate) fn note_bytecode_only_outcome(row: usize) {
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
const REFLECT_ACCESSOR_CLASS: &str = "jdk/internal/reflect/DirectMethodHandleAccessor";

/// `DirectMethodHandleAccessor.invokeImpl(Object, Object[])`'s specialized
/// `invokeExact` sites (JDK 25, `paramCount` 0 to 3), by the accessor's
/// parameter count: the receiver word, then one word per argument, all
/// `Object`, returning `Object`. The spreading arm (`(Object, Object[])`, more
/// than 3 parameters) and the caller-sensitive arms (a trailing `Class`) are
/// not among them.
const REFLECT_ACCESSOR_SITES: [&str; 4] = [
    "(Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
];

/// The accessor parameter count a site descriptor stands for, or `None` for
/// any other descriptor ([`REFLECT_ACCESSOR_SITES`]).
fn reflect_accessor_site_params(desc: &str) -> Option<usize> {
    REFLECT_ACCESSOR_SITES.iter().position(|s| *s == desc)
}

/// A method descriptor's parameter descriptors (at most `out.len()`, written
/// into `out`) and its return type's first byte; `None` when malformed or
/// wider than `out`. No allocation.
fn reflect_accessor_split<'a>(desc: &'a str, out: &mut [&'a str; 3]) -> Option<(usize, u8)> {
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
const REFLECT_WRAPPER_ORDER: [u8; 8] = *b"IJZBSCFD";

/// REFLECT21-4 (JIT round 21 wave 1, lane reflect): the class id `cell`
/// caches for the JDK class `name` (one definition per VM), filed on first
/// use from the class manager's name index; `None` while no class of that
/// name is loaded (nothing is filed then).
fn reflect_link_class_id(
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
fn reflect_accessor_wrapper(p: u8) -> Option<&'static str> {
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
fn reflect_accessor_arg(
    vm: &SharedVm,
    ctx: &crate::vm::NativeContextImpl<'_>,
    param: &str,
    word: i64,
) -> Option<i64> {
    let pb = param.as_bytes();
    let Some(wrapper) = pb
        .first()
        .copied()
        .filter(|_| pb.len() == 1)
        .and_then(reflect_accessor_wrapper)
    else {
        if word == 0 || param == "Ljava/lang/Object;" {
            return Some(word);
        }
        let name = param.strip_prefix('L')?.strip_suffix(';')?;
        let obj = mh_link_object(vm, word)?;
        let cid = jit_dispatch_class_validated(vm, obj).class_id()?;
        if cid.as_u32() >= 0x8000_0000 {
            return None;
        }
        let cratonvm_native_api::NameLookup::Unique(target) = ctx.classify_class_name(name) else {
            return None;
        };
        return crate::runtime::interpreter::class_is_subtype(vm, cid, target).then_some(word);
    };
    let obj = mh_link_object(vm, word)?;
    let cid = jit_dispatch_class_validated(vm, obj).class_id()?;
    // REFLECT21-4: the wrapper's id, filed per VM, not its name per call.
    let wrapper_index = REFLECT_WRAPPER_ORDER.iter().position(|b| *b == pb[0])?;
    let expected = reflect_link_class_id(
        ctx,
        &vm.jit.reflect_link_ids.wrappers[wrapper_index],
        wrapper,
    )?;
    if cid.as_u32() != expected {
        return None;
    }
    let slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, cid, "value")?;
    if slot >= vm.mem.heap.num_fields(obj) {
        return None;
    }
    match (pb[0], vm.mem.heap.get_field(obj, slot)) {
        (b'I' | b'Z' | b'B' | b'S' | b'C', Value::Int(v)) => Some(i64::from(v)),
        (b'J', Value::Long(v)) => Some(v),
        (b'F', Value::Float(v)) => Some(i64::from(v.to_bits())),
        (b'D', Value::Double(v)) => Some(v.to_bits() as i64),
        _ => None,
    }
}

/// The compiled call [`reflect_accessor_link_redirect`] makes: the reflected
/// method's interned info, its words (written into `words`) and their count,
/// and the member's return type's first byte. Pure reads (no Java allocation,
/// no GC point), so every word is the caller's value at the helper's entry.
fn reflect_accessor_link_target(
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
    if n != params || !matches!(ret, b'V' | b'I' | b'L' | b'[') {
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
pub(crate) fn reflect_link_hidden_latch_enabled(vm: &SharedVm) -> bool {
    *vm.jit.reflect_link_ids.hidden_latch_switch.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_REFLECT_LINK_HIDDEN_LATCH")
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
pub(crate) fn reflect_link_hidden_standins_enabled(vm: &SharedVm) -> bool {
    *vm.jit.reflect_link_ids.hidden_standins_switch.get_or_init(|| {
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_REFLECT_LINK_HIDDEN_STANDINS")
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
fn reflect_link_declines_for_hidden_frames(vm: &SharedVm) -> bool {
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
pub(crate) fn reflect_link_forget_classes(vm: &SharedVm, unloaded: &dyn Fn(u32) -> bool) {
    vm.jit
        .reflect_link_ids
        .adapters
        .write()
        .retain(|_, row| !unloaded(row.declaring_class_id) && !unloaded(row.adapter_class_id));
}

fn reflect_link_note_adapter(
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
/// opt-in; probe `tools/probes/jit/r21/R21ReflectInvoke.java`): the compiled
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
unsafe fn reflect_accessor_link_redirect(
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
    let mut words = [0i64; 4];
    let (leaf, count, ret) = {
        let (thread, _guard) = jit_thread_mut()?;
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        reflect_accessor_link_target(vm, &mut ctx, info, params, args, &mut words)?
    };
    // As the linker arm: the door this replaces re-anchored the
    // compiled-recursion budget for its nested call (`L3W79NestedReflectDepth`).
    let budget = rebase_self_call_stack_floor(None);
    let rc = mh_bind_direct_call(vm_ptr, leaf, words.as_ptr() as i64, count as i64);
    restore_self_call_stack_floor(budget);
    // The exception / deopt sentinel `i64::MIN` goes back as is (no `int`
    // result and no `void` call's `0` can equal it).
    Some(match ret {
        _ if rc == i64::MIN => rc,
        b'V' => 0,
        b'I' => jit_integer_value_of_direct_body(vm_ptr, i64::from(rc as i32)),
        _ => rc,
    })
}

/// The class whose compiled `invokeExact` site [`reflect_ctor_link_redirect`]
/// serves.
const REFLECT_CTOR_ACCESSOR_CLASS: &str = "jdk/internal/reflect/DirectConstructorHandleAccessor";

/// `DirectConstructorHandleAccessor.invokeImpl(Object[])`'s specialized
/// `invokeExact` sites (JDK 25, `paramCount` 0 to 3), by parameter count.
const REFLECT_CTOR_SITES: [&str; 4] = [
    "()Ljava/lang/Object;",
    "(Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
];

/// What [`reflect_ctor_link_redirect`] calls: the `<init>`'s interned info
/// (an `invokespecial`), the class to allocate and its slot count, and the
/// converted argument words after the receiver slot (`words[0]` is left `0`).
/// Pure reads, as [`reflect_accessor_link_target`].
fn reflect_ctor_link_target(
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
    let num_fields = {
        let cm = vm.classes.class_manager.read();
        let class = cm.get_class(instance)?;
        if class.is_abstract() || class.is_interface() {
            return None;
        }
        class.num_total_fields.max(1)
    };
    let mut ptypes = [""; 3];
    let (n, ret) = reflect_accessor_split(leaf.descriptor, &mut ptypes)?;
    if n != params || ret != b'V' {
        return None;
    }
    words[0] = 0;
    for (i, p) in ptypes[..n].iter().enumerate() {
        words[1 + i] = reflect_accessor_arg(vm, ctx, p, *args.get(1 + i)?)?;
    }
    reflect_link_note_adapter(vm, ctx, leaf, handle);
    Some((leaf, instance, num_fields, n + 1))
}

/// JIT round 21 wave 1, lane reflect (REFLECT21-3,
/// `CRATONVM_JIT_REFLECT_CTOR_LINK`, opt-in; probe
/// `tools/probes/jit/r21/R21ReflectInvoke.java` `reflect-ctor`): the compiled
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
unsafe fn reflect_ctor_link_redirect(
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
    let mut words = [0i64; 4];
    let (leaf, instance, num_fields, count, pins) = {
        let (thread, _guard) = jit_thread_mut()?;
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let (leaf, instance, num_fields, count) =
            reflect_ctor_link_target(vm, &mut ctx, info, params, args, &mut words)?;
        // From here on the call is this helper's: the allocation below is a
        // GC point, so the argument words are pinned across it.
        let pins = CompileArgPins::pin_raw(vm, &mut *thread, leaf, &words[..count]);
        (leaf, instance, num_fields, count, pins)
    };
    let obj = jit_new_object_body(vm_ptr, i64::from(instance.as_u32()), num_fields as i64);
    let Some((thread, _guard)) = jit_thread_mut() else {
        // JIT round 22, lane sync: as `mh_hidden_static_call`'s arm.
        set_jit_deopt_pending();
        return Some(i64::MIN);
    };
    if let Some(moved) = pins.release_raw(&mut *thread, &words[..count]) {
        words[..count].copy_from_slice(&moved);
    }
    if obj == 0 {
        // The allocation's throwable (`OutOfMemoryError`) is pending.
        return Some(i64::MIN);
    }
    words[0] = obj;
    let pins = CompileArgPins::pin_raw(vm, &mut *thread, leaf, &words[..count]);
    drop(_guard);
    let budget = rebase_self_call_stack_floor(None);
    let rc = mh_bind_direct_call(vm_ptr, leaf, words.as_ptr() as i64, count as i64);
    restore_self_call_stack_floor(budget);
    let Some((thread, _guard)) = jit_thread_mut() else {
        // JIT round 22, lane sync: as `mh_hidden_static_call`'s arm.
        set_jit_deopt_pending();
        return Some(i64::MIN);
    };
    let now = pins
        .release_raw(&mut *thread, &words[..count])
        .map_or(words[0], |moved| moved[0]);
    Some(if rc == i64::MIN { rc } else { now })
}

#[cfg(test)]
mod r20_prefl_mh_compiled_link_tests {
    use super::*;

    fn info(class_name: &'static str, method_name: &'static str, kind: u8) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name,
            method_name,
            descriptor: "()V",
            num_jit_args: 1,
            return_type: b'V',
            invoke_kind: kind,
            declaring_class_id: 0,
            owner_class_id: 0,
        }
    }

    #[test]
    fn only_the_three_signature_polymorphic_shapes_are_linked() {
        let mh = "java/lang/invoke/MethodHandle";
        assert_eq!(
            mh_link_shape(&info(mh, "invokeExact", 0)),
            Some(MhLinkShape::Linker)
        );
        assert_eq!(
            mh_link_shape(&info(mh, "invokeBasic", 0)),
            Some(MhLinkShape::Basic)
        );
        // Interpreter round i5 wave 109, lane L2: `linkToSpecial` has the
        // shape too; the redirect links it only under
        // `CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL`.
        for name in [
            "linkToStatic",
            "linkToVirtual",
            "linkToInterface",
            "linkToSpecial",
        ] {
            assert_eq!(mh_link_shape(&info(mh, name, 3)), Some(MhLinkShape::LinkTo));
        }
        // `invoke` (asType), `linkToNative`, other kinds and other classes
        // keep their doors.
        assert_eq!(mh_link_shape(&info(mh, "invoke", 0)), None);
        assert_eq!(mh_link_shape(&info(mh, "linkToSpecial", 0)), None);
        assert_eq!(mh_link_shape(&info(mh, "linkToNative", 3)), None);
        assert_eq!(mh_link_shape(&info(mh, "invokeBasic", 3)), None);
        assert_eq!(
            mh_link_shape(&info("java/lang/invoke/VarHandle", "invokeExact", 0)),
            None
        );
    }

    #[test]
    fn basic_signatures_erase_to_the_lambda_form_types() {
        assert_eq!(
            mh_link_basic_sig("(ZBSCIJFD[ILjava/lang/String;[[Ljava/lang/Object;)Z"),
            Some((b"IIIIIJFDLLL".to_vec(), b'I'))
        );
        assert_eq!(mh_link_basic_sig("()V"), Some((Vec::new(), b'V')));
        assert_eq!(mh_link_basic_sig("(V)V"), None);
        assert_eq!(mh_link_basic_sig("(Ljava/lang/Object)V"), None);
        assert_eq!(mh_link_basic_sig("(I)Vx"), None);
    }

    #[test]
    fn the_words_must_line_up_with_the_member_in_basic_types() {
        // invokeExact (Object,Object,Object)Object -> linker (L,L,L,L,L)L.
        let site = "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
        let linker = "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
        assert!(mh_link_basic_agree(site, linker, 1, 0, 0, 1));
        assert!(!mh_link_basic_agree(site, linker, 1, 0, 0, 0));
        // invokeBasic (I)I on an entry (Object,int)int.
        assert!(mh_link_basic_agree(
            "(I)I",
            "(Ljava/lang/Object;I)I",
            1,
            0,
            0,
            0
        ));
        // A boxed word where the entry takes an int is the door's (it unboxes).
        assert!(!mh_link_basic_agree(
            "(Ljava/lang/Object;)I",
            "(Ljava/lang/Object;I)I",
            1,
            0,
            0,
            0
        ));
        // linkToStatic (II MemberName)I -> add(II)I.
        assert!(mh_link_basic_agree(
            "(IILjava/lang/invoke/MemberName;)I",
            "(II)I",
            0,
            0,
            1,
            0
        ));
        // linkToVirtual (receiver, I, MemberName)I -> scale(I)I.
        assert!(mh_link_basic_agree(
            "(Ljava/lang/Object;ILjava/lang/invoke/MemberName;)I",
            "(I)I",
            0,
            1,
            1,
            0
        ));
        // Interpreter round i5 wave 109, lane L2: linkToSpecial (receiver,
        // MemberName)I -> String.length()I, and a constructor form's
        // (receiver, I, MemberName)V -> <init>(I)V, lay out as linkToVirtual.
        assert!(mh_link_basic_agree(
            "(Ljava/lang/Object;Ljava/lang/invoke/MemberName;)I",
            "()I",
            0,
            1,
            1,
            0
        ));
        assert!(mh_link_basic_agree(
            "(Ljava/lang/Object;ILjava/lang/invoke/MemberName;)V",
            "(I)V",
            0,
            1,
            1,
            0
        ));
        // A primitive where the receiver / MemberName word stands is refused.
        assert!(!mh_link_basic_agree("(IIJ)I", "(I)I", 0, 1, 1, 0));
        // Returns must agree.
        assert!(!mh_link_basic_agree(
            "(IILjava/lang/invoke/MemberName;)J",
            "(II)I",
            0,
            0,
            1,
            0
        ));
    }
}

/// The cold half of [`mh_constant_bind_redirect`]: ask the admission about the
/// handle at `raw` and (re)bind the site. Holds the JIT thread only for the
/// admission's reads.
///
/// Round 16 wave 15 (lane mh6; MHB-1): when the exact admission refuses and
/// `CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING` is on, the converting admission
/// (`lang_invoke::mh_compiled_bind_converting_target`) is asked next, and its
/// site records the [`MhBindConv`] the warm path applies.
#[cold]
unsafe fn mh_constant_bind_fill(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    site_key: usize,
    raw: u64,
    vm_id: usize,
    epoch: u64,
) -> Option<MhBindHit> {
    // A re-fill within the epoch of the last one is churn (another handle at
    // this site); one after a collection is the same site re-validating its
    // moved handle, which must not count towards giving up.
    // gcj j3/rt3: only this site's own row carries its budget forward; a row
    // left by another site at a re-issued info address starts it again.
    let identity = MhSiteIdentity::of(info);
    let retire_epoch = identity.retire_epoch.get();
    // JIT round 21 wave 19 (lane mhbind2): and the previous real-bound shape,
    // of this VM, for `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE_TWO_CLASSES`.
    let (prev_fills, prev_epoch, prev_real_shape) = MH_BIND_SITES.with(|sites| {
        let sites = sites.borrow();
        sites
            .get(&site_key)
            .filter(|s| s.identity.names(info, retire_epoch, mh_site_identity_enabled))
            .map_or((0, u64::MAX, None), |s| {
                (s.fills, s.epoch, s.real_shape.filter(|_| s.vm == vm_id))
            })
    });
    if prev_fills > MH_BIND_MAX_FILLS {
        return None;
    }
    let fills = if prev_epoch == epoch {
        prev_fills + 1
    } else {
        prev_fills
    };
    let obj = vm.mem.heap.is_object_address(raw as usize)?;
    // MH7-3 (JIT round 17 wave 2, lane mh2;
    // `CRATONVM_JIT_MH_CONSTANT_IMMUTABLE_CHECK`, default ON): a handle the
    // compile doors may call as its member must not have been rewritten in
    // place since its admission. Read here, on the cold fill.
    use crate::runtime::interpreter::jit_bridge as mh_memo;
    let compile_memo = mh_memo::mh_constant_compile_enabled(vm);
    let check_words = compile_memo && mh_memo::mh_constant_immutable_check_enabled();
    if check_words {
        mh_memo::mh_constant_guard_verify(vm, raw, epoch, || mh_constant_handle_words(vm, obj));
        if mh_memo::mh_constant_guard_sweep_dbg() {
            mh_memo::mh_constant_guard_sweep(vm, epoch, &|h: u64| {
                mh_constant_handle_words(vm, ObjectRef::from_raw(h as *mut u8))
            });
        }
    }
    let (target, conversion, real, real_receiver, real_conversion) = {
        let (thread, _guard) = jit_thread_mut()?;
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let real = cratonvm_native_builtins::lang_invoke::mh_is_real_handle(&ctx, obj);
        let bind = match cratonvm_native_builtins::lang_invoke::mh_compiled_bind_target(
            &mut ctx,
            obj,
            info.descriptor,
        ) {
            Some(target) => (Some(target), None, real),
            // MHB-1 (round 16 wave 15, lane mh6): read here, on the cold fill.
            None if !real && mh_constant_bind_converting_flag() => {
                match cratonvm_native_builtins::lang_invoke::mh_compiled_bind_converting_target(
                    &mut ctx,
                    obj,
                    info.descriptor,
                    info.method_name == "invoke",
                ) {
                    Some((target, conversion)) => (Some(target), Some(conversion), real),
                    None => (None, None, real),
                }
            }
            None => (None, None, real),
        };
        // THROWS18-4 (round 18 wave 11, lane mhresid): an interface member,
        // behind the per-call receiver type guard. Read here, on the cold fill.
        let bind = if bind.0.is_none()
            && cratonvm_native_builtins::lang_invoke::mh_constant_bind_interface_enabled()
        {
            match cratonvm_native_builtins::lang_invoke::mh_compiled_bind_interface_target(
                &mut ctx,
                obj,
                info.descriptor,
            ) {
                Some(target) => (Some(target), None, real),
                None => bind,
            }
        } else {
            bind
        };
        // Interpreter round i5 wave 109, lane L2
        // (`CRATONVM_JIT_MH_COMPILED_LINK_SPECIAL`, default OFF): a real
        // `$Special` handle, or a plain one over a private method, as an
        // `invoke_kind` 1 (`invokespecial`) call; its null receiver is screened
        // per call by the redirect. Read here, on the cold fill.
        let bind = if bind.0.is_none()
            && real
            && cratonvm_native_builtins::lang_invoke::jit_mh_compiled_link_special_enabled()
        {
            match cratonvm_native_builtins::lang_invoke::mh_compiled_bind_special_target(
                &mut ctx,
                obj,
                info.descriptor,
            ) {
                Some(target) => (Some(target), None, real),
                None => bind,
            }
        } else {
            bind
        };
        // j27 wave 5 (lane MHASTYPE;
        // `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING`, opt-in): a REAL
        // `asType` result every arm above refused. Read here, on the cold fill.
        let real_converting = if bind.0.is_none()
            && real
            && cratonvm_native_builtins::lang_invoke::mh_constant_bind_real_converting_enabled()
        {
            cratonvm_native_builtins::lang_invoke::mh_real_converting_compiled_bind(
                &mut ctx,
                obj,
                info.descriptor,
            )
        } else {
            None
        };
        let (bind, real_conversion) = match real_converting {
            Some((target, c)) => ((Some(target), None, real), Some(c)),
            None => (bind, None),
        };
        // MH6-3 (JIT round 21 wave 17, lane mhbind): a REAL `bindTo` result's
        // bound value, the call's receiver (its `argL1`, a `final` field).
        let real_receiver = match &bind.0 {
            Some(t) if real && t.bound => {
                cratonvm_native_builtins::lang_invoke::mh_real_bound_receiver(&ctx, obj)
                    .map(|r| r.as_ptr() as u64)
            }
            _ => None,
        };
        (bind.0, bind.1, bind.2, real_receiver, real_conversion)
    };
    let conv = conversion.and_then(|c| mh_bind_conv_of(vm, obj, &c));
    // j27 wave 5 (lane MHASTYPE): a real `asType` result's wrapper slots.
    let real_conv = real_conversion.and_then(|c| mh_real_conv_of(vm, &c));
    // A conversion whose slots do not resolve keeps the door.
    let target = if (conversion.is_some() && conv.is_none())
        || (real_conversion.is_some() && real_conv.is_none())
    {
        None
    } else {
        target
    };
    // JIT round 17 wave 2 (lane mh2;
    // `r17-mh-constant-bind-admission-is-loader-blind-FIXED-20261005.md`): the
    // admission compared descriptor STRINGS. The compiled caller's loader
    // (`declaring_class_id`, the class the site lives in) and the member's
    // must see every class the site names as one, the compile door's rule
    // (`CRATONVM_JIT_MH_CONSTANT_LOADER_CHECK`); otherwise the door, which
    // judges the call against the caller's classes, keeps the site.
    let target = target.filter(|t| {
        crate::runtime::interpreter::jit_bridge::mh_constant_bind_loaders_agree(
            vm,
            info.declaring_class_id,
            t.class_id,
            info.descriptor,
        )
    });
    let leaf = target.as_ref().and_then(|t| mh_bind_leaf_info(vm_id, t));
    let bound = target.as_ref().is_some_and(|t| t.bound);
    // THROWS18-4: the interface whose implementors the warm path admits.
    let iface = match (&target, leaf) {
        (Some(t), Some(_)) if t.invoke_kind == 2 => t.class_id,
        _ => 0,
    };
    // MH16-8 (round 16 wave 10, lane mhfloor): read here, on the cold fill.
    // JIT round 19 wave 1 (lane mhiface): an interface site only with
    // `CRATONVM_JIT_MH_CONSTANT_BIND_DIRECT_INTERFACE`.
    let direct = leaf.is_some()
        && mh_constant_bind_direct_flag()
        && (iface == 0 || mh_constant_bind_direct_interface_flag());
    let mut guard = [0u64; 7];
    guard[0] = raw;
    // A real handle has none of these slots (the redirect's warm path takes its
    // address and epoch as the whole guard, `MhBindHit::real`).
    if leaf.is_some() && !real {
        let slots = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_GUARD_SLOTS;
        for (i, slot) in slots.iter().enumerate() {
            guard[i + 1] = mh_bind_slot_key(vm.mem.heap.get_field(obj, *slot));
        }
    }
    // MH6-3 (JIT round 21 wave 17, lane mhbind): a REAL `bindTo` result's
    // bound value stands where a carrier's `MH_BOUND` guard word does, which
    // is where the redirect reads a bound call's receiver. Its `argL1` is
    // `final`, so within this epoch the handle's address answers it. No value
    // leaves the word `0`, which the redirect hands to the door.
    let mut real_shape = None;
    if leaf.is_some() && real && bound {
        if let Some(receiver) = real_receiver {
            guard[1 + cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_BOUND_GUARD] =
                receiver;
            // MHB-3: read here, on the cold fill.
            if mh_constant_bind_real_shape_flag() {
                real_shape = mh_real_bound_shape_of(vm, obj, receiver);
                // JIT round 21 wave 19 (lane mhbind2): a second bound-value
                // class of the same shape, same epoch.
                if let (Some(now), Some(prev)) = (real_shape.as_mut(), prev_real_shape) {
                    let same_but_class = MhRealBoundShape {
                        receiver_class: now.receiver_class,
                        receiver_class_alt: 0,
                        ..prev
                    } == *now;
                    if prev_epoch == epoch
                        && same_but_class
                        && prev.receiver_class != now.receiver_class
                        && mh_constant_bind_real_shape_two_classes_flag()
                    {
                        now.receiver_class_alt = prev.receiver_class;
                    }
                }
            }
        }
    }
    // MHF-1 (round 16 wave 11, lane mhbind): read here, on the cold fill. A
    // converting site records no shape (its guard is address-keyed only).
    let shape = if leaf.is_some() && !real && conv.is_none() && mh_constant_bind_shape_flag() {
        mh_bind_shape_of(vm, obj)
    } else {
        None
    };
    let conv = if leaf.is_some() { conv } else { None };
    let real_conv = if leaf.is_some() { real_conv } else { None };
    // MH16-8 at compile time (round 16 wave 17, lane mh7): an exact direct
    // STATIC handle is recorded for the compile doors, which call it as its
    // member when a `static final` field holds it
    // (`jit_bridge::CpResolvers::mh_constant`). Read here, on the cold fill.
    // JIT round 17 wave 3 (lane mh4; MHCONST17-2): an unbound VIRTUAL handle
    // too, under `CRATONVM_JIT_MH_CONSTANT_VIRTUAL` (`mh_constant_records_kind`).
    if let Some(t) = target.as_ref().filter(|t| {
        leaf.is_some()
            && conv.is_none()
            // j27 wave 5 (lane MHASTYPE): a converting real handle is no
            // compile row (the compile door would call the member unconverted).
            && real_conv.is_none()
            && !t.bound
            && crate::runtime::interpreter::jit_bridge::mh_constant_records_kind(t.invoke_kind)
    }) {
        if compile_memo {
            // MH7-3: the words the compile-time bind relies on, as admitted.
            if check_words {
                if let Some(words) = mh_constant_handle_words(vm, obj) {
                    crate::runtime::interpreter::jit_bridge::note_mh_constant_guard(
                        vm, raw, epoch, words,
                    );
                }
            }
            crate::runtime::interpreter::jit_bridge::note_mh_constant_target_kind(
                vm,
                raw,
                epoch,
                info.descriptor,
                (&t.class, &t.name, &t.desc),
                t.class_id,
                t.invoke_kind,
            );
        }
    }
    // MHBIND21-1 (JIT round 21 wave 19, lane mhbind2;
    // `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND`): a REAL `bindTo` result of a
    // virtual member, bound above with its `argL1` as the receiver, is
    // recorded for the compile doors with that field's slot and the bound
    // value's exact class, so a `static final` field holding it compiles as a
    // plain `invokevirtual` on the bound value (`invoke_kind 4`). Read here,
    // on the cold fill.
    if let (Some(t), Some(receiver)) = (target.as_ref(), real_receiver) {
        if compile_memo
            && leaf.is_some()
            && conv.is_none()
            && real
            && t.bound
            && t.invoke_kind == 0
            && crate::runtime::interpreter::jit_bridge::mh_constant_compile_bound_enabled()
        {
            if let Some((slot, class)) = mh_real_bound_compile_row(vm, obj, receiver) {
                if mh_constant_bind_dbg() {
                    eprintln!(
                        "[mh-bind] compile row {} -> {}.{}{} bound, argL1 slot {slot}, class {class}",
                        info.descriptor,
                        t.class,
                        t.name,
                        t.desc,
                    );
                }
                crate::runtime::interpreter::jit_bridge::note_mh_constant_target_bound(
                    vm,
                    raw,
                    epoch,
                    info.descriptor,
                    (&t.class, &t.name, &t.desc),
                    t.class_id,
                    slot,
                    class,
                );
            }
        }
    }
    // j27 wave 6 (lane T1LAM2; `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`,
    // default ON since 2026-10-28, j27 WARNING flip; =0 restores): a REAL `bindTo` result of a STATIC member (MH16-7's bind, its
    // bound value the member's leading argument) is recorded for the compile
    // doors with its `argL1` slot, so a `static final` field holding it
    // compiles as `invokestatic member(argL1, a*)` (`invoke_kind 6`). Never
    // as a plain kind 3 (the `!t.bound` gate above): that call would drop
    // the bound argument. Read here, on the cold fill.
    if let (Some(t), Some(value)) = (target.as_ref(), real_receiver) {
        if mh_bound_static_compile_row_wanted(
            compile_memo && leaf.is_some() && real,
            conv.is_none() && real_conv.is_none(),
            t.bound,
            t.invoke_kind,
        ) && crate::runtime::interpreter::jit_bridge::mh_constant_compile_bound_static_enabled()
        {
            if let Some((slot, _)) = mh_real_bound_compile_row(vm, obj, value) {
                if mh_constant_bind_dbg() {
                    eprintln!(
                        "[mh-bind] compile row {} -> {}.{}{} bound-static, argL1 slot {slot}",
                        info.descriptor, t.class, t.name, t.desc,
                    );
                }
                crate::runtime::interpreter::jit_bridge::note_mh_constant_target_bound_static(
                    vm,
                    raw,
                    epoch,
                    info.descriptor,
                    (&t.class, &t.name, &t.desc),
                    t.class_id,
                    slot,
                );
            }
        }
    }
    // MHBIND2-1 (JIT round 21 wave 20, lane mhfilter;
    // `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER`): a REAL filter species every
    // bind refused is recorded for the compile doors, which compile a `static
    // final` field holding it as two plain calls (`invoke_kind 5`). Read here,
    // on the cold fill.
    if target.is_none()
        && real
        && compile_memo
        && crate::runtime::interpreter::jit_bridge::mh_constant_compile_filter_enabled()
    {
        mh_constant_filter_compile_fill(vm, info, obj, raw, epoch);
    }
    if mh_constant_bind_dbg() {
        mh_constant_bind_dbg_line(info, leaf, bound, conv.as_ref());
        if let (Some(l), Some(c)) = (leaf, real_conv.as_ref()) {
            // j27 wave 5 (lane MHASTYPE): the engagement record of
            // `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING`.
            eprintln!(
                "[mh-bind] MethodHandle.{}{} -> {}.{}{} real-converting unbox={:?} box={:?}",
                info.method_name,
                info.descriptor,
                l.class_name,
                l.method_name,
                l.descriptor,
                &c.unbox[..l.num_jit_args.min(MH_BIND_MAX_SPLICED_ARGS)],
                c.box_return
            );
        }
    }
    MH_BIND_SITES.with(|sites| {
        let mut sites = sites.borrow_mut();
        if sites.len() < MH_BIND_MAX_SITES || sites.contains_key(&site_key) {
            sites.insert(
                site_key,
                MhBindSite {
                    vm: vm_id,
                    epoch,
                    guard,
                    leaf,
                    bound,
                    direct,
                    shape,
                    conv,
                    fills,
                    real,
                    iface,
                    iface_seen: std::cell::Cell::new(0),
                    customized_seen: std::cell::Cell::new(false),
                    identity,
                    real_shape,
                    real_conv,
                },
            );
        }
    });
    leaf.map(|leaf| MhBindHit {
        leaf,
        guard,
        bound,
        direct,
        conv,
        real,
        iface,
        iface_seen: 0,
        customized_seen: false,
        real_conv,
    })
}

/// The interface bind's per-call receiver type guard (JIT round 18 wave 11,
/// lane mhresid; THROWS18-4, `CRATONVM_JIT_MH_CONSTANT_BIND_INTERFACE`): is
/// the receiver word `raw` an object whose class implements the interface
/// `iface`? `seen` is the site's last accepted class (one compare for a
/// monomorphic site); a newly accepted class is remembered on the site. An
/// array, an unreadable word or a class that does not implement it is `false`
/// (the door's call). Reads only; the subtype test takes the class manager's
/// read lock only on a memo miss.
fn mh_bind_interface_receiver_ok(
    vm: &SharedVm,
    site_key: usize,
    iface: u32,
    seen: u32,
    raw: i64,
) -> bool {
    let bits = raw as u64;
    if bits == 0 || (bits & 0x7) != 0 || bits >= (1u64 << 48) {
        return false;
    }
    let Some(obj) = vm.mem.heap.is_object_address(bits as usize) else {
        return false;
    };
    let Some(class) = jit_dispatch_class_validated(vm, obj).class_id() else {
        return false;
    };
    if class.as_u32() == seen {
        return true;
    }
    if !jit_is_subclass_of_cached(vm, class, ClassId::new(iface)) {
        return false;
    }
    MH_BIND_SITES.with(|sites| {
        if let Some(site) = sites.borrow().get(&site_key) {
            site.iface_seen.set(class.as_u32());
        }
    });
    true
}

/// `CRATONVM_MH_COMPILED_SITE_LINK` -- correctness, default ON, `=0` restores
/// round 17 wave 2's behaviour (JIT round 17 wave 3, lane mh4; item 1 of
/// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`): see
/// [`mh_compiled_site_link`]. Read only on a site's first compiled call per
/// thread.
fn mh_compiled_site_link_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_MH_COMPILED_SITE_LINK")
}

/// Link a COMPILED `MethodHandle.invokeExact` / `invoke` call site the way
/// the interpreter links one at its first execution
/// (`jit_bridge::mh_site_link_caller_types`; JIT round 17 wave 3, lane mh4):
/// every class the site's descriptor names that the caller's loader has not
/// seen yet is resolved through that loader (HotSpot resolves the site's
/// `MethodType` with the caller's loader, JVMS §5.4.3.6, which loads), so
/// the door's `invokeExact` loader check and the constant bind's loader check
/// see the caller's own copy. A method compiled before its first call used to
/// reach the door through this helper only, unlinked (`R17MhLoaders
/// cross-loader-null` compiled first kept the old answer).
///
/// The caller is the site's class (`JitInvokeInfo::declaring_class_id`; `0`,
/// the bootstrap loader, links nothing). The loads run Java code and may
/// collect: the argument buffer's references are pinned across them
/// ([`CompileArgPins`]) and their current addresses written back into the
/// buffer, which the dispatch reads after this. `true` when anything was
/// loaded (the caller re-reads the handle and the epoch). Called on a site's
/// first compiled call per thread ([`mh_constant_bind_redirect`]).
///
/// JIT round 18 wave 11 (lane mhresid; item 4 of
/// `r17-mh-door-loader-residuals-after-mh3-FIXED-20261006.md`): under
/// `CRATONVM_MH_SITE_LINK_NCDFE` (default ON, shared with the interpreted
/// link, `invoke.rs` `mh_site_link_checked`) the first failed load is
/// PROPAGATED, as HotSpot's `find_method_handle_type` does: it is converted
/// into its throwable through the helpers' one conversion
/// ([`stash_helper_failure`]) while the arguments are still pinned, stashed
/// as the pending exception, and [`MhCompiledSiteLink::Threw`] tells the
/// redirect to return the sentinel. With the switch off a failure is
/// ignored, as before.
///
/// # Safety
/// `args_ptr` / `num_args` are the dispatch helper's argument buffer, as
/// compiled code passed it; no `jit_thread_mut` borrow may be live.
#[cold]
unsafe fn mh_compiled_site_link(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_ptr: i64,
    num_args: i64,
) -> MhCompiledSiteLink {
    use crate::runtime::interpreter::invoke::mh_site_link_access_refusal;
    use crate::runtime::interpreter::jit_bridge;
    if info.declaring_class_id == 0 || num_args < 1 || !mh_compiled_site_link_enabled() {
        return MhCompiledSiteLink::Nothing;
    }
    let caller = ClassId::new(info.declaring_class_id);
    let mut missing =
        jit_bridge::mh_site_link_missing_names(vm, info.method_name, info.descriptor, caller);
    // Interpreter round i4 wave 99, lane L4: a `java/` name no loader has
    // loaded yet is loaded too (`invoke.rs`
    // `mh_site_link_add_missing_java_names`), so the access half below judges
    // it (a package-private `java.*` class is HotSpot's `IllegalAccessError`).
    crate::runtime::interpreter::invoke::mh_site_link_add_missing_java_names(
        vm,
        info.method_name,
        info.descriptor,
        caller,
        &mut missing,
    );
    // Interpreter round i4 wave 97, lane L4: the link's access half
    // (`invoke.rs` `mh_site_link_access_refusal`, HotSpot's
    // `check_klass_accessibility` for each class the site's type names), which
    // the interpreted link runs and this one did not: a compiled site naming a
    // package-private class of another runtime package reached the door, whose
    // own copy is asked only for a name two loaders define. Pure reads; a
    // refusal is stashed like a failed load and is not recorded as linked.
    // Only for a USER loader's caller (`invoke.rs`
    // `mh_site_link_caller_is_user_loader`: a built-in loader's answer here
    // comes from the flat store and may be a class it cannot see; w101,
    // `L4W79BootSiteType boot-site`).
    let check_access =
        crate::runtime::interpreter::invoke::mh_site_link_caller_is_user_loader(vm, caller);
    if missing.is_empty() {
        if !check_access {
            return MhCompiledSiteLink::Nothing;
        }
        let Some(refusal) =
            mh_site_link_access_refusal(vm, info.method_name, info.descriptor, caller, &[])
        else {
            return MhCompiledSiteLink::Nothing;
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            return MhCompiledSiteLink::Nothing;
        };
        let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
        let pins = CompileArgPins::pin_raw(vm, thread, info, args);
        let threw = stash_helper_failure(
            vm,
            thread,
            refusal,
            &"MethodHandle call site link failed",
            true,
            true,
        );
        let moved = pins.release_raw(thread, args);
        if let Some(moved) = moved {
            let out = args_ptr as *mut i64;
            for (i, word) in moved.iter().enumerate().take(num_args as usize) {
                *out.add(i) = *word;
            }
        }
        return if threw {
            MhCompiledSiteLink::Threw
        } else {
            MhCompiledSiteLink::Loaded
        };
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        return MhCompiledSiteLink::Nothing;
    };
    let propagate = crate::runtime::interpreter::invoke::mh_site_link_ncdfe_enabled();
    let mut threw = false;
    let moved = {
        let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
        let pins = CompileArgPins::pin_raw(vm, thread, info, args);
        let mut resolved: Vec<(&str, ClassId)> = Vec::new();
        for name in &missing {
            let loaded = jit_bridge::mh_site_link_load(
                vm,
                thread,
                info.method_name,
                info.descriptor,
                caller,
                name,
                " (compiled)",
            );
            match loaded {
                Ok(id) => resolved.push((name.as_str(), id)),
                Err(failed) => {
                    if propagate {
                        // Built while the arguments are pinned: the conversion
                        // allocates.
                        threw = stash_helper_failure(
                            vm,
                            thread,
                            failed,
                            &"MethodHandle call site link failed",
                            true,
                            true,
                        );
                        break;
                    }
                }
            }
        }
        // Wave 97: the access half, after the loads (see above).
        if !threw && check_access {
            if let Some(refusal) = mh_site_link_access_refusal(
                vm,
                info.method_name,
                info.descriptor,
                caller,
                &resolved,
            ) {
                threw = stash_helper_failure(
                    vm,
                    thread,
                    refusal,
                    &"MethodHandle call site link failed",
                    true,
                    true,
                );
            }
        }
        pins.release_raw(thread, args)
    };
    if let Some(moved) = moved {
        let out = args_ptr as *mut i64;
        for (i, word) in moved.iter().enumerate().take(num_args as usize) {
            *out.add(i) = *word;
        }
    }
    if threw {
        MhCompiledSiteLink::Threw
    } else {
        MhCompiledSiteLink::Loaded
    }
}

/// What [`mh_compiled_site_link`] did (JIT round 18 wave 11, lane mhresid).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MhCompiledSiteLink {
    /// Nothing to load (or the link is off): the handle and epoch stand.
    Nothing,
    /// Something was loaded (Java ran, a collection may have moved the
    /// arguments): the caller re-reads the handle and the epoch.
    Loaded,
    /// A load failed and its throwable is pending: the caller returns the
    /// `i64::MIN` sentinel.
    Threw,
}

/// The words of the handle at `obj` that a compile-time constant bind relies
/// on (JIT round 17 wave 2, lane mh2; MH7-3): the guard words of
/// `MH_COMPILED_BIND_GUARD_SLOTS`, then its `type`'s. `None` when the object
/// is too narrow for any of them (never an admitted handle). Pure reads; the
/// caller guarantees `obj` is the object at that address in this epoch.
pub(crate) fn mh_constant_handle_words(vm: &SharedVm, obj: ObjectRef) -> Option<[u64; 7]> {
    let heap = &vm.mem.heap;
    let width = heap.num_fields(obj);
    let slots = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_GUARD_SLOTS;
    if slots.iter().any(|slot| *slot >= width) {
        return None;
    }
    let type_slot =
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, heap.class_id_of(obj), "type")?;
    if type_slot >= width {
        return None;
    }
    let mut words = [0u64; 7];
    for (i, slot) in slots.iter().enumerate() {
        words[i] = mh_bind_slot_key(heap.get_field(obj, *slot));
    }
    words[6] = mh_bind_slot_key(heap.get_field(obj, type_slot));
    Some(words)
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING` -- default ON (see below; built
/// opt-in in JIT round 16 wave 15, lane mh6; proposal MHB-1 of `jit-r16-mhbind-proposals-RETIRED-20261006.md`): a
/// compiled `invokeExact` / `invoke` site whose handle converts `Integer` /
/// `Number` / `Object` arguments into the member's `int` parameters, or its
/// `int` return into such a reference, binds like an exact one; the helper
/// unboxes and boxes around the member's call ([`mh_constant_bind_call`]).
/// Default ON since round 16 wave 17 (w15a A/B: exact-astyped 2950 -> 424 ns,
/// invoke-box 2738 -> 380; 20/20 loaded); `=0` turns it off. Read on the cold
/// fill only ([`mh_constant_bind_fill`]); the answer rides on the bound site,
/// so this getter holds no static.
fn mh_constant_bind_converting_flag() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING")
}

/// `CRATONVM_DBG_JIT_MH_BIND=1` (JIT round 16 wave 15, lane mh6): one stderr
/// line per cold fill of a `MethodHandle` site ([`mh_constant_bind_dbg_line`]),
/// the engagement record of every constant-bind switch. Read on the cold fill
/// only.
pub(crate) fn mh_constant_bind_dbg() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_JIT_MH_BIND")
}

/// The fill's verdict, as `[mh-bind] MethodHandle.<name><site> -> <verdict>`:
/// `door`, or the member and how it is called (`exact`, `bound`, or
/// `converting unbox=<mask> box=<bool>`).
fn mh_constant_bind_dbg_line(
    info: &JitInvokeInfo,
    leaf: Option<&'static JitInvokeInfo>,
    bound: bool,
    conv: Option<&MhBindConv>,
) {
    let verdict = match (leaf, conv) {
        (None, _) => "door".to_string(),
        (Some(l), Some(c)) => format!(
            "{}.{}{} converting unbox={:#x} box={}",
            l.class_name, l.method_name, l.descriptor, c.unbox_int, c.box_int_return
        ),
        (Some(l), None) => format!(
            "{}.{}{} {}",
            l.class_name,
            l.method_name,
            l.descriptor,
            if bound {
                "bound"
            } else if l.invoke_kind == 2 {
                // THROWS18-4 (round 18 wave 11, lane mhresid).
                "interface-guarded"
            } else {
                "exact"
            }
        ),
    };
    eprintln!(
        "[mh-bind] MethodHandle.{}{} -> {verdict}",
        info.method_name, info.descriptor
    );
}

/// The warm-path form of a converting admission's answer (MHB-1): the slots of
/// the handle's `type` and of `Integer.value`, both resolved by name as
/// `get_field_by_name` resolves them, and the handle's `type` word now. `None`
/// when either slot does not resolve inside its object.
fn mh_bind_conv_of(
    vm: &SharedVm,
    obj: ObjectRef,
    conversion: &cratonvm_native_builtins::lang_invoke::MhBindConversion,
) -> Option<MhBindConv> {
    let heap = &vm.mem.heap;
    let class_id = heap.class_id_of(obj);
    let type_slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, "type")?;
    if type_slot >= heap.num_fields(obj) {
        return None;
    }
    let integer = ClassId::new(conversion.integer_class_id);
    let value_slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, integer, "value")?;
    Some(MhBindConv {
        type_slot,
        type_word: mh_bind_slot_key(heap.get_field(obj, type_slot)),
        unbox_int: conversion.unbox_int,
        box_int_return: conversion.box_int_return,
        integer_class_id: conversion.integer_class_id,
        value_slot,
    })
}

/// The warm-path form of a real converting admission's answer (j27 wave 5,
/// lane MHASTYPE): the `value` slot of each loaded wrapper, resolved by name as
/// [`mh_bind_conv_of`] resolves `Integer.value`. `None` when a loaded
/// wrapper's slot does not resolve (the site keeps the door).
fn mh_real_conv_of(
    vm: &SharedVm,
    conversion: &cratonvm_native_builtins::lang_invoke::MhRealConversion,
) -> Option<MhRealConv> {
    let mut wrappers = [(0u32, 0usize); 5];
    for (k, id) in conversion.wrapper_class_ids.iter().enumerate() {
        if *id != 0 {
            let slot =
                crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, ClassId::new(*id), "value")?;
            wrappers[k] = (*id, slot);
        }
    }
    Some(MhRealConv {
        unbox: conversion.unbox,
        wrappers,
        box_return: conversion.box_return,
    })
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_SHAPE` -- default ON since its A/B (`=0` off; JIT round 16
/// wave 11, lane mhbind; proposal MHF-1 of `jit-r16-mhfloor-proposals-RETIRED-20261006.md`): a
/// bound `invokeExact` site also takes a handle at ANOTHER address whose shape
/// is the bound one's ([`mh_bind_shape_matches`]), so a site that sees a fresh
/// `bindTo` result of one source per call (`R16MhFloor bind-each`) stays bound
/// instead of re-filling until it gives up. `1`/`true`/`on`/`yes` turns it on.
/// Read on the cold fill only ([`mh_constant_bind_fill`]); the answer rides on
/// the bound site, so this getter holds no static.
fn mh_constant_bind_shape_flag() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_SHAPE")
}

/// The shape of the handle at `obj` (validated by the fill's
/// `is_object_address`), or `None` when its class has no `type` slot inside
/// it.
fn mh_bind_shape_of(vm: &SharedVm, obj: ObjectRef) -> Option<MhBindShape> {
    let heap = &vm.mem.heap;
    let class_id = heap.class_id_of(obj);
    if class_id.as_u32() == 0 {
        return None;
    }
    let width = heap.num_fields(obj);
    let type_slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, "type")?;
    let extra = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_SHAPE_SLOTS;
    if type_slot >= width || extra.iter().any(|slot| *slot >= width) {
        return None;
    }
    let mut words = [0u64; 5];
    for (i, slot) in extra.iter().enumerate() {
        words[i] = mh_bind_slot_key(heap.get_field(obj, *slot));
    }
    words[4] = mh_bind_slot_key(heap.get_field(obj, type_slot));
    Some(MhBindShape {
        class_id: class_id.as_u32(),
        width,
        type_slot,
        words,
    })
}

/// Is the handle at `obj` -- at another address than the site's bound one, in
/// the same VM and collection epoch -- of the bound one's shape (MHF-1)? Its
/// class (read through `class_id_of`, which validates the address as an
/// object start: the word is a compiled caller's `MethodHandle` reference, but
/// this is its first header read), its slot count, the guard words of
/// `MH_COMPILED_BIND_GUARD_SLOTS` and the words of `shape`. Every slot the
/// admission (`mh_compiled_bind_target`) reads is among them, and the objects
/// they name are immutable or are the same objects (an equal address in an
/// equal epoch), so the admission would answer this handle as it answered the
/// bound one. A bound receiver is the `MH_BOUND` guard word: the same object.
fn mh_bind_shape_matches(
    vm: &SharedVm,
    obj: ObjectRef,
    guard: &[u64; 7],
    shape: &MhBindShape,
) -> bool {
    let heap = &vm.mem.heap;
    if heap.class_id_of(obj).as_u32() != shape.class_id || heap.num_fields(obj) != shape.width {
        return false;
    }
    let slots = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_GUARD_SLOTS;
    if !slots
        .iter()
        .enumerate()
        .all(|(i, slot)| mh_bind_slot_key(heap.get_field(obj, *slot)) == guard[i + 1])
    {
        return false;
    }
    let extra = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_SHAPE_SLOTS;
    extra
        .iter()
        .enumerate()
        .all(|(i, slot)| mh_bind_slot_key(heap.get_field(obj, *slot)) == shape.words[i])
        && mh_bind_slot_key(heap.get_field(obj, shape.type_slot)) == shape.words[4]
}

/// What the redirect's warm path checks on a handle at another address than
/// the site's bound one: a carrier's shape (MHF-1) or a real `bindTo`
/// result's (MHB-3, JIT round 21 wave 17, lane mhbind).
#[derive(Clone, Copy)]
enum MhOtherAddress {
    Carrier(MhBindShape),
    RealBound(MhRealBoundShape),
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE` -- perf, default ON, `=0` off
/// (JIT round 21 wave 17, lane mhbind; MHB-3 of the r18 backlog, page
/// `docs/known-issues/jit/jr21-mhbind-constant-bound-and-bindto-20261015.md`):
/// a site bound to a REAL `bindTo` result
/// (`CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND`) also takes a fresh `bindTo`
/// result of the same direct handle with a bound value of the same exact
/// class ([`mh_real_bound_shape_receiver`]), so
/// `h.bindTo(x).invokeExact(..)` in a loop stays bound instead of re-filling
/// until the site gives up and every call walks the link chain. Read on the
/// cold fill only ([`mh_constant_bind_fill`]); the answer rides on the site.
fn mh_constant_bind_real_shape_flag() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE")
}

/// `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE_TWO_CLASSES` -- perf, default ON,
/// `=0` off (JIT round 21 wave 19, lane mhbind2; page
/// `docs/known-issues/jit/jr21-mhbind-constant-bound-and-bindto-20261015.md`,
/// "Round 21 wave 19"): a real-bound site's [`MhRealBoundShape`] keeps a
/// SECOND bound-value class. It kept one, so `h.bindTo(x).invokeExact(..)`
/// over values of two alternating classes (`R21MhbindRows bind-each-mixed`)
/// missed the shape on every call, re-filled (the full real-bound admission)
/// until the site gave up, and walked the link chain for good. A fill whose
/// new shape equals the site's previous one in everything but the bound
/// value's class, in the same VM and epoch, keeps the previous class as the
/// alternate: the admission's only question about the bound value is that it
/// is a non-array instance of the member's class, which a value of either
/// admitted exact class answers as the admitted one did. Read on the cold
/// fill only.
fn mh_constant_bind_real_shape_two_classes_flag() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_MH_CONSTANT_BIND_REAL_SHAPE_TWO_CLASSES",
    )
}

/// The [`MhRealBoundShape`] of the real `bindTo` result at `obj` (validated by
/// the fill's `is_object_address`) whose bound value is the word `receiver`,
/// or `None` when a field does not resolve inside the instance or the bound
/// value is not that object. Cold: the fill's.
fn mh_real_bound_shape_of(
    vm: &SharedVm,
    obj: ObjectRef,
    receiver: u64,
) -> Option<MhRealBoundShape> {
    let heap = &vm.mem.heap;
    let class_id = heap.class_id_of(obj);
    if class_id.as_u32() == 0 {
        return None;
    }
    let width = heap.num_fields(obj);
    let slot = |name: &str| {
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, name)
            .filter(|s| *s < width)
    };
    let target_slot = slot("argL0")?;
    let receiver_slot = slot("argL1")?;
    let form_slot = slot("form")?;
    let type_slot = slot("type")?;
    if receiver == 0 || mh_bind_slot_key(heap.get_field(obj, receiver_slot)) != receiver {
        return None;
    }
    let robj = heap.is_object_address(receiver as usize)?;
    let receiver_class = jit_dispatch_class_validated(vm, robj).class_id()?;
    Some(MhRealBoundShape {
        class_id: class_id.as_u32(),
        target_slot,
        receiver_slot,
        form_slot,
        type_slot,
        target_word: mh_bind_slot_key(heap.get_field(obj, target_slot)),
        form_word: mh_bind_slot_key(heap.get_field(obj, form_slot)),
        type_word: mh_bind_slot_key(heap.get_field(obj, type_slot)),
        receiver_class: receiver_class.as_u32(),
        receiver_class_alt: 0,
    })
}

/// The `(argL1 slot, bound value's exact class id)` the compile doors need
/// for the REAL `bindTo` result at `obj` (validated by the fill's
/// `is_object_address`), whose bound value is the word `receiver` (MHBIND21-1,
/// JIT round 21 wave 19, lane mhbind2): the slot must lie inside the instance
/// and hold that very word, and the value's class must read. Reads only; on
/// the cold fill.
fn mh_real_bound_compile_row(vm: &SharedVm, obj: ObjectRef, receiver: u64) -> Option<(u32, u32)> {
    let heap = &vm.mem.heap;
    let class_id = heap.class_id_of(obj);
    if class_id.as_u32() == 0 || receiver == 0 {
        return None;
    }
    let slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, "argL1")
        .filter(|s| *s < heap.num_fields(obj))?;
    if mh_bind_slot_key(heap.get_field(obj, slot)) != receiver {
        return None;
    }
    let robj = heap.is_object_address(receiver as usize)?;
    let class = jit_dispatch_class_validated(vm, robj).class_id()?;
    Some((u32::try_from(slot).ok()?, class.as_u32()))
}

/// j27 wave 6 (lane T1LAM2): does the cold fill record a bound STATIC compile
/// row (`invoke_kind 6`)? Only for a REAL handle the bind admitted with a leaf
/// and the compile memo on (`eligible`), no conversion of either kind
/// (`unconverted`: the compile door calls the member as it is), bound, and a
/// static member (`invoke_kind 3`). The virtual bound row (kind 4) is the
/// MHBIND21-1 block's.
fn mh_bound_static_compile_row_wanted(
    eligible: bool,
    unconverted: bool,
    bound: bool,
    invoke_kind: u8,
) -> bool {
    eligible && unconverted && bound && invoke_kind == 3
}

/// MHBIND2-1 (JIT round 21 wave 20, lane mhfilter;
/// `CRATONVM_JIT_MH_CONSTANT_COMPILE_FILTER`): the cold fill's record of a
/// REAL filter species at `obj` (validated by the fill's `is_object_address`,
/// word `raw`, collection epoch `epoch`) that every bind refused, for the
/// compile doors (`invoke_kind 5`,
/// `jit_bridge::note_mh_constant_target_filter`). Admitted by
/// `lang_invoke::mh_real_filter_compiled_target`; then, as the bind's own
/// loader rule (`mh_constant_bind_loaders_agree`), the caller must see every
/// class the site names as the target's class does, and every class the
/// filter's own descriptor names as the filter's class does. The row's member
/// is the MAIN call (the target of an argument filter, the filter of a return
/// filter), the other the PRE call. Holds the JIT thread only for the
/// admission's reads.
fn mh_constant_filter_compile_fill(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    obj: ObjectRef,
    raw: u64,
    epoch: u64,
) {
    use crate::runtime::interpreter::jit_bridge as memo;
    let parts = {
        // SAFETY: called only from `mh_constant_bind_fill` on the JIT helper's
        // own thread, after the fill's admission block dropped its guard, so no
        // other `&mut JvmThread` is live; the guard is held for this block.
        let Some((thread, _guard)) = (unsafe { jit_thread_mut() }) else {
            return;
        };
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        cratonvm_native_builtins::lang_invoke::mh_real_filter_compiled_target(
            &mut ctx,
            obj,
            info.descriptor,
        )
    };
    let Some(parts) = parts else {
        return;
    };
    if !memo::mh_constant_bind_loaders_agree(
        vm,
        info.declaring_class_id,
        parts.target.class_id,
        info.descriptor,
    ) || !memo::mh_constant_bind_loaders_agree(
        vm,
        info.declaring_class_id,
        parts.filter.class_id,
        &parts.filter.desc,
    ) {
        return;
    }
    // The species' `argL0` slot, holding the target's direct handle: the
    // frame a filtered receiver's null guard resumes on. `0`: unknown.
    let heap = &vm.mem.heap;
    let target_word = parts.target_handle.as_ptr() as u64;
    let target_slot = Some(heap.class_id_of(obj))
        .filter(|c| c.as_u32() != 0)
        .and_then(|c| crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, c, "argL0"))
        .filter(|s| *s < heap.num_fields(obj))
        .filter(|&s| mh_bind_slot_key(heap.get_field(obj, s)) == target_word)
        .and_then(|s| u32::try_from(s).ok())
        .unwrap_or(0);
    let (main, pre) = match parts.position {
        Some(_) => (&parts.target, &parts.filter),
        None => (&parts.filter, &parts.target),
    };
    if mh_constant_bind_dbg() {
        eprintln!(
            "[mh-bind] compile row {} -> {}.{}{} filter ({}), pre {}.{}{}, argL0 slot {target_slot}",
            info.descriptor,
            main.class,
            main.name,
            main.desc,
            match parts.position {
                Some(p) => format!("argument {p}"),
                None => "return".to_string(),
            },
            pre.class,
            pre.name,
            pre.desc,
        );
    }
    memo::note_mh_constant_target_filter(
        vm,
        raw,
        epoch,
        info.descriptor,
        (&main.class, &main.name, &main.desc),
        main.class_id,
        memo::MhConstantFilter::new(
            parts.position,
            main.invoke_kind,
            (&pre.class, &pre.name, &pre.desc),
            pre.invoke_kind,
            pre.class_id,
            target_slot,
        ),
    );
}

/// The bound value's word of the real `bindTo` result at `obj` -- at another
/// address than the site's bound one, in the same VM and collection epoch --
/// when it is of the site's [`MhRealBoundShape`] (MHB-3, JIT round 21 wave
/// 17, lane mhbind): the same species class (read through `class_id_of`, which
/// validates the address as an object start, as [`mh_bind_shape_matches`]
/// does), the same `argL0`, `form` and `type` objects, and a non-null bound
/// value of the admitted value's exact class. `None` otherwise (the fill
/// decides the handle). Reads only; no allocation, no GC point.
fn mh_real_bound_shape_receiver(
    vm: &SharedVm,
    obj: ObjectRef,
    shape: &MhRealBoundShape,
) -> Option<u64> {
    let heap = &vm.mem.heap;
    if heap.class_id_of(obj).as_u32() != shape.class_id
        || mh_bind_slot_key(heap.get_field(obj, shape.target_slot)) != shape.target_word
        || mh_bind_slot_key(heap.get_field(obj, shape.form_slot)) != shape.form_word
        || mh_bind_slot_key(heap.get_field(obj, shape.type_slot)) != shape.type_word
    {
        return None;
    }
    let Value::Object(Some(receiver)) = heap.get_field(obj, shape.receiver_slot) else {
        return None;
    };
    // A field's reference, not an address just proved by `is_object_address`:
    // the validating header reads.
    let class = jit_dispatch_class(vm, receiver).class_id()?.as_u32();
    // JIT round 21 wave 19 (lane mhbind2): or the shape's second class.
    (class == shape.receiver_class
        || (shape.receiver_class_alt != 0 && class == shape.receiver_class_alt))
        .then_some(receiver.as_ptr() as u64)
}
/// Count one native dispatch on a JIT fast-path edge (§4 census).
///
/// One `Option` test plus one relaxed `fetch_add` on an id resolved at
/// cache-fill time. No allocation, no hashing, no lock, no string comparison —
/// all of that was hoisted to the cold admission above. The acceptance
/// criterion for `--jdk-only` is *zero* `SyntheticStub` invocations through
/// **any** path, and an uncounted path is an unverifiable one.
#[inline]
fn count_jit_native_dispatch(vm: &SharedVm, id: Option<cratonvm_native_api::NativeMethodId>) {
    if let Some(id) = id {
        vm.natives.native_methods.record_invocation(id);
    }
}

// ---------------------------------------------------------------------------
// Invoke dispatch helpers
// ---------------------------------------------------------------------------

/// Per-info-pointer JIT dispatch state: caches a compiled callee's entry point
/// so that repeated calls from the same JIT call site skip the JIT cache lookup.
struct DispatchCache {
    entry: usize,
    needs_context: bool,
    /// Owns JIT code while this thread-local raw entry remains published.
    ///
    /// [`cratonvm_jit::RetainedCode`], not a bare `Arc`, because this map is
    /// evicted by the very thread that dispatches through it: a generation
    /// flush (`flush_raw_entry_dispatch_caches`), a class-identity flush, a
    /// replacement `insert`, or thread exit. Any of those can run while this
    /// thread is *inside* the body it names — `try_mic_rust_cached_entry` reads
    /// only `entry`/`needs_context` out of the map and calls the raw pointer, so
    /// the map entry is the sole owner across the call, and a nested dispatch
    /// from the callee re-enters the flush. Dropping a bare `Arc` there unmaps
    /// the code under this thread's own return address; measured on
    /// `BasicErrorControllerIntegrationTests` at
    /// `active_jit_executions` = 1. The wrapper releases through
    /// `defer_jit_owner`, which retains until no thread is in compiled code.
    ///
    /// Read as well as held since 2026-08-17: the dispatch sites clone it and
    /// hand it to `try_call_compiled_entry_reentrant_owned` instead of making
    /// that function re-derive the same `Arc` out of the code-range registry.
    ///
    /// Behind an `Rc` since round 9 wave 3 (vmhelpers3). The per-call clone
    /// that keeps the body alive across the call (the map entry may be evicted
    /// by a nested dispatch while this thread is inside it) used to be a
    /// `RetainedCode` clone — at the time an `Arc` increment plus a holder-count
    /// increment, and the matching two decrements plus the last-holder test on
    /// drop: four locked RMWs per cached dispatch, 6-7% of the mutual-recursion
    /// probe (`perf-mutual-recursion-forces-every-call-through-invoke-dispatch`).
    /// The maps are thread-local, so a non-atomic `Rc` gives the same
    /// guarantee: the ONE `RetainedCode` lives until the map entry AND every
    /// in-flight clone are gone, and its own drop still routes through the
    /// retirement queue exactly as before.
    owner: Option<std::rc::Rc<cratonvm_jit::RetainedCode>>,
    /// The callee declares an exception table, so the sentinel it returns can
    /// RE-RUN it with this call's arguments, and the call roots its reference
    /// arguments across the compiled call ([`call_compiled_then_route`]).
    /// Answered once, when the entry is filled; `false` costs a call nothing.
    rerun_pins: bool,
}

impl DispatchCache {
    /// The body this entry names was replaced by a later publication of its
    /// key (`CompiledMethod::superseded`) or invalidated (`retired`). Either
    /// way the site must re-resolve, and the entry is dropped at the hit.
    ///
    /// This is what lets `JitCache::put` move only the publish half of the
    /// generation word on a replacement instead of flushing every thread's
    /// positive memos (round 11 wave 11). A replaced body is still correct
    /// code, so a hit that raced the flag ran correct code; the entry's own
    /// `RetainedCode` kept it mapped. `retired` is checked as well although
    /// every invalidation also moves the retire half: a memo must not wait for
    /// its thread's next flush to stop entering withdrawn code.
    ///
    /// An entry with no owner names nothing this can ask about. Every
    /// production insertion carries one (an unowned raw entry is never
    /// cached); the unowned form exists only in tests.
    #[inline]
    fn names_a_stale_body(&self) -> bool {
        use std::sync::atomic::Ordering::Acquire;
        self.owner.as_ref().is_some_and(|o| {
            let cm = o.arc();
            cm.superseded.load(Acquire) || cm.retired.load(Acquire)
        })
    }
}

#[cfg(test)]
mod r11w11_superseded_body_memo_tests {
    use super::DispatchCache;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    fn body() -> Arc<cratonvm_jit::CompiledMethod> {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
        buf.emit(&[0xC3]); // RET
        Arc::new(cratonvm_jit::CompiledMethod::new(buf))
    }

    fn memo_on(owner: &Arc<cratonvm_jit::CompiledMethod>) -> DispatchCache {
        DispatchCache {
            entry: owner.entry_ptr() as usize,
            needs_context: false,
            owner: Some(std::rc::Rc::new(cratonvm_jit::RetainedCode::new(
                Arc::clone(owner),
            ))),
            rerun_pins: false,
        }
    }

    /// A fresh body is served; a superseded one and an invalidated one are
    /// both dropped at the hit. An unowned entry (tests only) names nothing.
    #[test]
    fn a_memo_is_stale_once_its_body_is_superseded_or_retired() {
        let fresh = body();
        assert!(!memo_on(&fresh).names_a_stale_body());

        let replaced = body();
        replaced.superseded.store(true, Ordering::Release);
        assert!(memo_on(&replaced).names_a_stale_body());

        let invalidated = body();
        invalidated.retired.store(true, Ordering::Release);
        assert!(memo_on(&invalidated).names_a_stale_body());

        let unowned = DispatchCache {
            entry: 0x1000,
            needs_context: false,
            owner: None,
            rerun_pins: false,
        };
        assert!(!unowned.names_a_stale_body());
    }

    /// The whole contract end to end: a replacing `put` marks the body it
    /// displaced, so a memo that still names it goes stale while a memo on the
    /// new body does not, and the displaced body is NOT `retired` (which six
    /// readers take to mean "withdrawn by an invalidation").
    #[test]
    fn a_replacing_put_makes_the_old_bodys_memo_stale_and_leaves_it_unretired() {
        let cache = cratonvm_jit::JitCache::new();
        let cid = cratonvm_types::ClassId::new(7);
        let publish = |cache: &cratonvm_jit::JitCache| {
            let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
            buf.emit(&[0xC3]);
            cache.put(
                "r11w11/Memo".into(),
                "hot".into(),
                "()V".into(),
                cid,
                cratonvm_jit::CompiledMethod::new(buf),
            );
            cache
                .get("r11w11/Memo", "hot", "()V", cid)
                .expect("the publication is reachable")
        };
        let first = publish(&cache);
        let old_memo = memo_on(&first);
        assert!(!old_memo.names_a_stale_body());
        let second = publish(&cache);
        assert!(
            !Arc::ptr_eq(&first, &second),
            "the second put replaced the first"
        );
        assert!(first.superseded.load(Ordering::Acquire));
        assert!(!first.retired.load(Ordering::Acquire));
        assert!(old_memo.names_a_stale_body());
        assert!(!memo_on(&second).names_a_stale_body());
    }
}

#[derive(Clone, Copy)]
struct NativeDispatchCache {
    receiver_class_id: u32,
    callback: cratonvm_native_api::NativeCallback,
    kind: ObjectNativeKind,
    /// §4 census handle for the registry slot this entry dispatches, resolved
    /// once by [`admit_jit_fast_native`] at cache-fill time. `None` only when
    /// the registry has no slot for the triple, which under `JdkOnly` cannot
    /// happen (admission refuses that case) and under `Compatible` just means
    /// the dispatch is uncountable, exactly as it was before.
    native_id: Option<cratonvm_native_api::NativeMethodId>,
}

#[derive(Clone, Copy)]
enum ObjectNativeKind {
    HashMap,
    Matcher,
    /// Registered `java/lang/StringBuilder` natives (the `append(I)/(C)/
    /// (String)` family + `toString`/`length`). Exact-receiver-guarded like
    /// the other kinds; resolution consults the native registry ONCE at
    /// cache-fill time instead of a 3-string hash per call.
    StringBuilder,
}

#[derive(Clone, Copy)]
enum IntegerNativeKind {
    ValueOf,
    IntValue,
}

#[derive(Clone, Copy)]
struct IntegerNativeDispatchCache {
    kind: IntegerNativeKind,
    callback: cratonvm_native_api::NativeCallback,
    /// §4 census handle — see [`NativeDispatchCache::native_id`].
    native_id: Option<cratonvm_native_api::NativeMethodId>,
}

/// A JIT call site resolved to a **leaf** native — see
/// [`cratonvm_native_api::NativeMethodRegistry::set_leaf`].
///
/// This is the compiled-code half of the funnel bypass the interpreter already
/// has for `Thread.onSpinWait` / `Thread.currentThread`. Measured on
/// `probes/NativeShapeProbe.java`, a no-argument static native from compiled
/// code cost ~560 ns against ~8 ns for an ordinary Java call, and essentially
/// none of it was the native's own body: it was `jit_invoke_dispatch`'s tail
/// (`invoke_or_native`, a ~27-gate string cascade ending in a three-string
/// registry hash) plus `safe_native_call`'s GC bookkeeping. This entry removes
/// both — the cascade at cache-fill time, the funnel via
/// `safe_native_call_leaf`.
/// What a resolved leaf site actually does on a hit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeafNativeKind {
    /// Call the registered callback through `safe_native_call_leaf`.
    Callback,
    /// `Thread.currentThread()` — answered from `thread.java_thread_obj`
    /// WITHOUT calling the native at all, exactly as the interpreter's inline
    /// cache does (`dispatch_static.rs`, `InterpIntrinsic::ThreadCurrentThread`).
    ///
    /// It cannot be an ordinary leaf, because its registered body
    /// (`native_thread_current_thread` → `NativeContext::current_thread_object`)
    /// **allocates** when this thread's mirror has not been built yet — it
    /// loads `java/lang/Thread`, allocates the mirror, builds the field holder
    /// and the thread group. That is a real violation of leaf contract item 1,
    /// so the claim is not made at registration; the site instead serves only
    /// the warm case and falls through to the ordinary dispatcher (which runs
    /// that allocating slow path) when the mirror is absent.
    ///
    /// Worth its own arm because of how often the JDK calls it: **twice per
    /// uncontended `ReentrantLock.lock()`/`unlock()` pair**, censused with
    /// `--dump-native-registry` in `probes/LockNativeCensusProbe.java`, plus
    /// every AQS ownership check and every thread-local lookup. The interpreter
    /// took it from 306 ns to 136 ns; compiled code was still paying 564 ns
    /// because none of the compiled loop's calls reach the interpreter's cache.
    ThreadCurrentThread,
    /// `ArrayList.get(int)` — the site resolved to
    /// `cratonvm_native_collections::native_al_get`. A hit on an ordinary
    /// `ArrayList` with an in-range index is answered by
    /// `jit_arraylist_get` (one fused field read and one array-element read)
    /// WITHOUT the `safe_native_call_*` wrapper or the argument decode. Every
    /// other receiver shape — views, wrappers, an out-of-range index, the
    /// `CRATONVM_DBG_ALTRACE` trace — falls through to the ordinary
    /// [`LeafNativeKind::Callback`] path of the same entry, which calls the
    /// same native. Classified once in `resolve_native_site`
    /// (`collection_leaf_kind`); r9w4 vm4, per `NOTES-w3-collections3.md`.
    ArrayListGet,
    /// `ArrayList.size()` — the site resolved to `native_al_size`; served by
    /// `jit_arraylist_size` on the same terms as [`LeafNativeKind::ArrayListGet`].
    ArrayListSize,
    /// `ConcurrentHashMap.get(Object)` / `ConcurrentMap.get` -- the site resolved to
    /// `cratonvm_native_collections::native_chm_get`. A primitive-wrapper or `String` key is
    /// answered by `jit_chm_get` (in-place, allocation-free, never-blocking chain walk)
    /// WITHOUT the argument decode or the `safe_native_call_*` wrapper; every other key, a
    /// null key (the native's NPE) and any decline fall through to the ordinary callback
    /// path, which runs the same native. r9w11 chm11.
    ChmGet,
    /// `Integer.hashCode()I` (and the `Character`/`Byte`/`Short` twins, which
    /// register the same body) -- the site resolved to
    /// `lang_math::native_wrapper_int_hash_code`. Answered as that body does
    /// (field 0 as an `int`, else 0) WITHOUT the argument decode or the funnel.
    ///
    /// Round 11 wave 11, lane `hashmap`: `HashMap.hash` calls `key.hashCode()`
    /// on every `put` and `get`, and an inline cache can never hold a native,
    /// so with `Integer` keys every one of those calls missed into
    /// `jit_invoke_virtual_mic` and then paid `safe_native_call_prevalidated_objects`
    /// (these wrapper natives are not registered leaf). `CratonBench hashmap`
    /// measured 29 999 550 such `mic_site_native` calls in one phase.
    IntWrapperHashCode,
    /// `Integer.equals(Object)Z` (and its int-stored twins) -- the site resolved
    /// to `lang_math::native_wrapper_int_equals`. Only the same-class,
    /// both-plain-object case is answered here; every other shape (a null or
    /// differently-classed argument, an array, a wrapper whose field did not
    /// decode as an `int`) declines to the ordinary callback path, which runs
    /// the same native. `HashMap.getNode` calls it once per `get`.
    IntWrapperEquals,
    /// `Object.getClass()` on a plain object whose class already has its mirror
    /// and a memoised "report the stamped class" display verdict. SBR round 2,
    /// lane CALLER: every `Field.get*` crosses this native once through the
    /// funnel (`Field.checkAccess`'s `obj.getClass()`); answered here WITHOUT
    /// the argument decode or the funnel. Any other shape (array, unmemoised or
    /// aliased display, no mirror yet) declines to the callback, which runs the
    /// same native. `CRATONVM_CALLER_LEAF=0` turns the arm off.
    ObjectGetClass,
    /// `Reference.refersTo(Object)Z` (and the three subclasses'): the referent
    /// slot compared with the argument, as `native_ref_refers_to` does. Crossed
    /// once per `Field` access by `AccessibleObject.isAccessChecked`.
    /// `CRATONVM_CALLER_LEAF=0` turns the arm off.
    ReferenceRefersTo,
    /// `Class.isInstance(Object)Z` (JIT round 20 wave 17, lane pAtom2;
    /// `CRATONVM_JIT_CLASS_IS_INSTANCE_INLINE`): a null argument is `false`,
    /// and a plain object whose class's mirror IS the receiver is `true`
    /// ([`class_is_instance_exact_hit`], which also publishes that class's
    /// compiled-`getClass()` word for the optimizing tier's prefix). Every
    /// other shape (an array, a subclass or interface instance) declines to the
    /// callback, which runs the same native. The field updaters'
    /// `accessCheck` crosses it on every access.
    ClassIsInstance,
}

/// `CRATONVM_CALLER_LEAF` (default on): the `ObjectGetClass` and
/// `ReferenceRefersTo` site arms. `0`/`off` restores the funnel for both.
fn caller_leaf_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_CALLER_LEAF").as_deref(),
            Ok("0") | Ok("off") | Ok("false")
        )
    })
}

/// Which collection fast path, if any, a resolved native site may serve
/// before its ordinary callback. Keyed on the RESOLVED callback's identity —
/// not on names — so a triple re-registered to a different body (or a
/// receiver class whose override is some other native) is never answered by
/// the ArrayList probe; and on the CALL SITE descriptor, so the raw argument
/// slot and return encoding are the ones the probe assumes. Only virtual and
/// interface sites qualify: a static site has no receiver, and an
/// `invokespecial` (`super.get(i)` from an `ArrayList` subclass) keeps the
/// plain callback path.
fn collection_leaf_kind(
    info: &JitInvokeInfo,
    callback: cratonvm_native_api::NativeCallback,
    poly: bool,
) -> LeafNativeKind {
    if !matches!(info.invoke_kind, 0 | 2) || poly {
        return LeafNativeKind::Callback;
    }
    let cb = callback as usize;
    if cb == cratonvm_native_collections::native_al_get as usize
        && info.descriptor == "(I)Ljava/lang/Object;"
    {
        LeafNativeKind::ArrayListGet
    } else if cb == cratonvm_native_collections::native_al_size as usize && info.descriptor == "()I"
    {
        LeafNativeKind::ArrayListSize
    } else if cb == cratonvm_native_collections::native_chm_get as usize
        && info.descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
    {
        LeafNativeKind::ChmGet
    } else if info.descriptor == "()I"
        && cratonvm_native_builtins::intrinsics::integer::is_int_wrapper_hash_code_native(callback)
    {
        LeafNativeKind::IntWrapperHashCode
    } else if info.descriptor == "(Ljava/lang/Object;)Z"
        && cratonvm_native_builtins::intrinsics::integer::is_int_wrapper_equals_native(callback)
    {
        LeafNativeKind::IntWrapperEquals
    } else if info.descriptor == "()Ljava/lang/Class;"
        && caller_leaf_enabled()
        && cratonvm_native_builtins::is_object_get_class_native(callback)
    {
        LeafNativeKind::ObjectGetClass
    } else if info.descriptor == "(Ljava/lang/Object;)Z"
        && caller_leaf_enabled()
        && cratonvm_native_builtins::reference::is_refers_to_native(callback)
    {
        LeafNativeKind::ReferenceRefersTo
    } else if info.descriptor == "(Ljava/lang/Object;)Z"
        && cratonvm_jit::class_is_instance_inline_enabled()
        && cratonvm_native_builtins::is_class_is_instance_native(callback)
    {
        LeafNativeKind::ClassIsInstance
    } else {
        LeafNativeKind::Callback
    }
}

/// FUNNEL3-19-2 (JIT round 19 wave 6, lane mega6;
/// `CRATONVM_JIT_LEAF_PROBE_VALIDATED_ARGS`, default OFF): the site-cache
/// door's (Route S) validated reference argument for a leaf-kind probe, from
/// the forwarding pass's mask (`forward_jit_reference_args_validated`, under
/// `CRATONVM_JIT_NATIVE_WALK_ONCE`).
///
/// Only on a `warm` call: the mask's contract is "no GC point since the
/// forwarding pass", and a resolution in this call (a cold fill, a refill)
/// can load and initialize classes, which runs Java. Only for the kinds that
/// take a reference argument (`lean_leaf_kind_takes_reference`). `None`
/// otherwise, and the probe walks the word as before.
#[inline]
fn site_leaf_probe_validated_arg(
    vm: &SharedVm,
    kind: LeafNativeKind,
    warm: bool,
    prevalidated: u64,
    args_slice: &[i64],
) -> Option<ObjectRef> {
    if !warm
        || prevalidated == 0
        || !lean_leaf_kind_takes_reference(kind)
        || !vm.natives.native_methods.jit_leaf_probe_validated_args()
    {
        return None;
    }
    prevalidated_jit_ref(prevalidated, 1, *args_slice.get(1)?)
}

/// Serve an [`LeafNativeKind::ArrayListGet`] / [`LeafNativeKind::ArrayListSize`]
/// site hit from the `native-collections` no-wrapper probes (and, since round 11
/// wave 11, the `ChmGet` probe's siblings [`LeafNativeKind::IntWrapperHashCode`]
/// and [`LeafNativeKind::IntWrapperEquals`], which re-state their natives'
/// read-only bodies).
///
/// `Some(bits)` is exactly what the site's native would have returned, in the
/// JIT return encoding (`size()` sign-extended; `get` an object pointer or 0);
/// `None` means "run the entry's ordinary callback path", which is also what
/// happens for every receiver or index the probe does not recognise. The
/// probes have the same no-allocation / no-Java / no-throw contract as
/// `jit_overlay_hashmap_get`, which `jit_hashmap_get_direct` already calls
/// without the funnel.
///
/// `validated_arg` (JIT round 19 wave 6, lane mega6; FUNNEL3-19-2,
/// `CRATONVM_JIT_LEAF_PROBE_VALIDATED_ARGS`): the object `args_slice[1]` names,
/// already validated (and forwarded) by the calling door, or `None`. When
/// `Some`, the `ChmGet` / `IntWrapperEquals` / `ReferenceRefersTo` arms use it
/// instead of walking the word again; every other check is unchanged (a null
/// word still answers before it). The callers pass `None` with the switch off.
fn try_jit_arraylist_leaf(
    vm: &SharedVm,
    thread: &mut JvmThread,
    kind: LeafNativeKind,
    receiver: ObjectRef,
    args_slice: &[i64],
    validated_arg: Option<ObjectRef>,
) -> Option<i64> {
    match kind {
        LeafNativeKind::ArrayListGet => {
            // The `int` argument travels as the raw 64-bit slot; the low 32
            // bits are the Java value, exactly as the argument decode reads it.
            let index = *args_slice.get(1)? as i32;
            let probe = {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                cratonvm_native_collections::jit_arraylist_get(&ctx, receiver, index)
            };
            match probe? {
                Value::Object(Some(obj)) => {
                    // Object-return handoff root, same contract as every other
                    // JIT native fast path (see `jit_integer_value_of_direct`).
                    thread.native_pending_return = Some(obj);
                    Some(obj.as_ptr() as i64)
                }
                Value::Object(None) => Some(0),
                // `elementData` is a reference array; anything else is not a
                // shape the probe should have answered — let the native decide.
                _ => None,
            }
        }
        LeafNativeKind::ArrayListSize => {
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            cratonvm_native_collections::jit_arraylist_size(&ctx, receiver).map(|n| n as i64)
        }
        LeafNativeKind::ChmGet => {
            // The key travels as a raw reference slot. Screen it exactly as
            // `jit_concurrent_hashmap_get_direct` does; a null key is the
            // native's NPE, so decline and let the callback raise it.
            let raw = *args_slice.get(1)?;
            if raw == 0 || (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
                return None;
            }
            let key = match validated_arg {
                Some(key) => key,
                None => vm.mem.heap.is_object_address(raw as usize)?,
            };
            let probe = {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                cratonvm_native_collections::jit_chm_get(
                    &mut ctx,
                    receiver,
                    Value::Object(Some(key)),
                )
            };
            match probe? {
                Value::Object(Some(obj)) => {
                    thread.native_pending_return = Some(obj);
                    Some(obj.as_ptr() as i64)
                }
                Value::Object(None) => Some(0),
                _ => None,
            }
        }
        LeafNativeKind::IntWrapperHashCode => {
            // `native_wrapper_int_hash_code` line for line: field 0 when it
            // decodes as an `int`, else 0. `get_field` forwards the receiver
            // exactly as it does for the native.
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            match ctx.get_field(receiver, 0) {
                Value::Int(v) => Some(i64::from(v)),
                _ => Some(0),
            }
        }
        LeafNativeKind::IntWrapperEquals => {
            // `native_wrapper_int_equals`: a null argument is `false`.
            let raw = *args_slice.get(1)?;
            if raw == 0 {
                return Some(0);
            }
            if (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
                return None;
            }
            let other = match validated_arg {
                Some(other) => other,
                None => vm.mem.heap.is_object_address(raw as usize)?,
            };
            let receiver = vm.mem.heap.load_and_forward(receiver);
            let other = vm.mem.heap.load_and_forward(other);
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            // Only the same-class, both-plain-object case. The native also
            // compares class NAMES when the ids differ (a second loader's copy)
            // and refuses arrays; both go to the callback path, which runs it.
            if ctx.heap_kind_of(receiver) != cratonvm_types::ObjectKind::Object
                || ctx.heap_kind_of(other) != cratonvm_types::ObjectKind::Object
                || ctx.class_id_of_object(receiver) != ctx.class_id_of_object(other)
            {
                return None;
            }
            let a = match ctx.get_field(receiver, 0) {
                Value::Int(v) => v,
                _ => return Some(0),
            };
            let b = match ctx.get_field(other, 0) {
                Value::Int(v) => v,
                _ => return Some(0),
            };
            Some(i64::from(a == b))
        }
        LeafNativeKind::ObjectGetClass => {
            let (plain, cid) = {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                let plain = ctx.heap_kind_of(receiver) == cratonvm_types::ObjectKind::Object;
                (plain, ctx.class_id_of_object(receiver))
            };
            if !plain
                || !cratonvm_native_builtins::getclass_display_is_cached_stamp(vm.vm_identity, cid)
            {
                return None;
            }
            // nio20 L3: publishes the compiled-`getClass()` word too.
            let mirror = stamped_class_mirror_publishing_word(vm, cid)?;
            thread.native_pending_return = Some(mirror);
            Some(mirror.as_ptr() as i64)
        }
        LeafNativeKind::ReferenceRefersTo => {
            let raw = *args_slice.get(1)?;
            let receiver = vm.mem.heap.load_and_forward(receiver);
            let referent = {
                let ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                // zt t23/zb8: the non-keep-alive read, as the native.
                ctx.get_field_no_keepalive(
                    receiver,
                    cratonvm_native_builtins::reference::REFERENT_SLOT,
                )
            };
            let Value::Object(referent) = referent else {
                return None;
            };
            if raw == 0 {
                return Some(i64::from(referent.is_none()));
            }
            if (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
                return None;
            }
            let other = match validated_arg {
                Some(other) => other,
                None => vm.mem.heap.is_object_address(raw as usize)?,
            };
            let other = vm.mem.heap.load_and_forward(other);
            Some(i64::from(referent == Some(other)))
        }
        LeafNativeKind::ClassIsInstance => {
            // `native_class_is_instance`: a null argument is `false` (the
            // receiver, the mirror, is non-null at a virtual site).
            let raw = *args_slice.get(1)?;
            if raw == 0 {
                return Some(0);
            }
            if (raw as u64 & 0x7) != 0 || (raw as u64) >= (1u64 << 48) {
                return None;
            }
            let obj = match validated_arg {
                Some(obj) => obj,
                None => vm.mem.heap.is_object_address(raw as usize)?,
            };
            let obj = vm.mem.heap.load_and_forward(obj);
            // An array's header holds its COMPONENT's id: the native's array
            // arms decide it.
            if vm.mem.heap.kind_of(obj) != cratonvm_types::ObjectKind::Object {
                return None;
            }
            let mirror = vm.mem.heap.load_and_forward(receiver);
            let cid = vm.mem.heap.class_id_of(obj);
            class_is_instance_exact_hit(vm, cid, mirror).then_some(1)
        }
        LeafNativeKind::Callback | LeafNativeKind::ThreadCurrentThread => None,
    }
}

/// JIT round 20 wave 17, lane pAtom2 (`LeafNativeKind::ClassIsInstance`): is
/// `mirror` the mirror of `class_id` (`class_mirrors[class_id]`)? Then
/// `mirror.isInstance(obj)` is `true` for a plain object of that class, which
/// is what `native_class_is_instance` answers by its id compare (and HotSpot
/// by `obj.getClass() == mirror`). No allocation, no Java, no GC point.
///
/// On a hit, while the `class_mirrors` read lock is held, the class's
/// compiled-`getClass()` word is published as `stamped_class_mirror_publishing_word`
/// publishes it, so the optimizing tier's `Class.isInstance` prefix (and its
/// `getClass()` prefix) answer the next call with no CALL. The word's contract
/// is that `getClass()` of every instance of the class answers this mirror,
/// so it is published only when that holds: the class's display verdict is
/// memoised as its stamp, or no `getClass()` remap arm names it
/// (`getclass_reports_only_itself`, by name: conservative). Only when the
/// table exists and the word is still unpublished; a busy class manager (the
/// non-blocking read) skips the publish, never the answer. The class manager
/// is asked with no `class_mirrors` guard held, and the mirror is read again
/// under the guard the publish happens under.
fn class_is_instance_exact_hit(
    vm: &SharedVm,
    class_id: crate::classloading::ClassId,
    mirror: ObjectRef,
) -> bool {
    if vm.classes.class_mirrors.read().get(&class_id).copied() != Some(mirror) {
        return false;
    }
    let words = &vm.classes.getclass_mirror_words;
    if words.is_allocated() && words.get(class_id) == 0 {
        let stamp =
            cratonvm_native_builtins::getclass_display_is_cached_stamp(vm.vm_identity, class_id)
                || vm.classes.class_manager.try_read().is_some_and(|cm| {
                    cm.get_class(class_id).is_some_and(|class| {
                        cratonvm_native_builtins::getclass_reports_only_itself(&class.name)
                    })
                });
        if stamp {
            let mirrors = vm.classes.class_mirrors.read();
            if let Some(current) = mirrors.get(&class_id).copied() {
                words.publish(class_id, current);
            }
        }
    }
    true
}

/// JIT round 20 wave 19, lane zAssign (`CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE`;
/// `NativeClassAccess::note_class_mirror_id`, called by the
/// `Class.isAssignableFrom` / `Class.isInstance` natives with a pair they just
/// resolved through `class_id_from_mirror`): publish the mirror hint the
/// optimizing tier's arms read (`cratonvm_jit::ir_lower::
/// class_mirror_hint_resolve`), so the next compiled call of the pair needs
/// no call. Never changes an answer: a hint is only a guess that compiled
/// code proves against the class word before using it.
///
/// * Steady state (this mirror's hint names `class_id` and its class word is
///   the mirror): two loads, nothing else.
/// * Only while the compiled-`getClass()` table exists (a compile asked for
///   it), for a class-band id (no lambda proxy), with the switch on, and for a
///   class `kind` gives a hint (`0` for an array or primitive-named class:
///   their pairs keep the native's array / primitive arms).
/// * The class word is published first if it is not, under exactly
///   [`class_is_instance_exact_hit`]'s rule -- the class's display verdict is
///   memoised as its stamp, or no `getClass()` remap arm names it -- since a
///   word also answers compiled `getClass()` for the class's instances. (An
///   interface's or abstract class's word is never read by a `getClass()`
///   prefix: no instance's header names it.) A remapped class gets no word,
///   hence no hint, and its pairs keep the call.
/// * A refused class (an array, a primitive name, a remapped `getClass()`) is
///   remembered in the slot (`MIRROR_HINT_DECLINED`, no present bit), so its
///   later calls stop at the steady-state test.
/// * The hint is written under the `class_mirrors` read lock, after reading
///   `class_mirrors[class_id] == mirror` and the class word equal to it there
///   (both writers' lock; the GC's re-key / forget hold the WRITE lock).
/// * On a hint's first fill, the class's supers display is published if it
///   has none (non-blocking: a busy class manager skips it), so a class used
///   as a SOURCE before its `<clinit>` completed is served too.
///
/// No allocation, no Java, no GC point: the natives that call it are leaf.
pub(crate) fn class_mirror_id_hint_fill(
    vm: &SharedVm,
    mirror: ObjectRef,
    class_id: ClassId,
    kind: &dyn Fn() -> u8,
) {
    let words = &vm.classes.getclass_mirror_words;
    if !words.is_allocated() {
        return;
    }
    let raw = class_id.as_u32();
    if usize::try_from(raw).map_or(true, |i| {
        i >= cratonvm_jit::ir_lower::GETCLASS_WORDS_CLASS_CAPACITY
    }) {
        return;
    }
    // Cast: the mirror's address, as compiled code compares it.
    let addr = mirror.as_ptr() as usize;
    let present = usize::try_from(cratonvm_jit::ir_lower::MIRROR_HINT_PRESENT).unwrap_or(0);
    let declined = usize::try_from(cratonvm_jit::ir_lower::MIRROR_HINT_DECLINED).unwrap_or(0);
    let hint = words.mirror_hint(addr);
    // Cast: the low 32 bits of a hint word are its class id.
    let same_class = (hint as u32) == raw;
    let names_it = hint & present != 0 && same_class;
    if names_it && words.get(class_id) == addr {
        return;
    }
    // A class this filler already refused (below): nothing to do. Only a
    // remembered refusal -- compiled code reads no present bit there.
    if hint & declined != 0 && same_class {
        return;
    }
    if !words.mirror_hints_enabled() {
        return;
    }
    // Remember a refusal in the slot (no present bit: compiled code declines
    // as for an empty slot), so the next call of this mirror stops above.
    let refuse = || {
        let word = cratonvm_jit::ir_lower::MIRROR_HINT_DECLINED | u64::from(raw);
        words.publish_mirror_hint(addr, usize::try_from(word).unwrap_or(0));
    };
    let k = kind();
    if k == 0 {
        refuse();
        return;
    }
    if words.get(class_id) == 0 {
        let faithful =
            if cratonvm_native_builtins::getclass_display_is_cached_stamp(vm.vm_identity, class_id)
            {
                Some(true)
            } else {
                // `None`: a busy class manager -- ask again on a later call.
                vm.classes.class_manager.try_read().map(|cm| {
                    cm.get_class(class_id).is_some_and(|class| {
                        cratonvm_native_builtins::getclass_reports_only_itself(&class.name)
                    })
                })
            };
        match faithful {
            Some(true) => {}
            Some(false) => {
                refuse();
                return;
            }
            None => return,
        }
        let mirrors = vm.classes.class_mirrors.read();
        if mirrors.get(&class_id).copied() == Some(mirror) {
            words.publish(class_id, mirror);
        }
    }
    {
        let mirrors = vm.classes.class_mirrors.read();
        if mirrors.get(&class_id).copied() != Some(mirror) || words.get(class_id) != addr {
            return;
        }
        let word = cratonvm_jit::ir_lower::mirror_hint_word(raw, k == 2);
        words.publish_mirror_hint(addr, usize::try_from(word).unwrap_or(0));
    }
    if !names_it && vm.jit.supers_displays.enabled() && !vm.jit.supers_displays.is_published(raw) {
        vh_const_publish_display(vm, class_id, false);
    }
}

#[cfg(test)]
mod r9w4_vm4_collection_leaf_kind_tests {
    use super::*;

    fn info(kind: u8, desc: &'static str) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: "java/util/ArrayList",
            method_name: "get",
            descriptor: desc,
            num_jit_args: 2,
            return_type: b'L',
            invoke_kind: kind,
            declaring_class_id: 0,
            owner_class_id: 0,
        }
    }

    /// The classification is by the RESOLVED callback's identity plus the
    /// call-site descriptor, and never for a static site or a
    /// signature-polymorphic entry.
    #[test]
    fn arraylist_sites_classify_by_callback_identity() {
        let get: cratonvm_native_api::NativeCallback = cratonvm_native_collections::native_al_get;
        let size: cratonvm_native_api::NativeCallback = cratonvm_native_collections::native_al_size;
        for k in [0u8, 2] {
            assert!(
                collection_leaf_kind(&info(k, "(I)Ljava/lang/Object;"), get, false)
                    == LeafNativeKind::ArrayListGet
            );
            assert!(
                collection_leaf_kind(&info(k, "()I"), size, false) == LeafNativeKind::ArrayListSize
            );
            // Callback/descriptor mismatch: not served by the probe.
            assert!(collection_leaf_kind(&info(k, "()I"), get, false) == LeafNativeKind::Callback);
            assert!(
                collection_leaf_kind(&info(k, "(I)Ljava/lang/Object;"), size, false)
                    == LeafNativeKind::Callback
            );
            // Poly entries keep their own return-unboxing path.
            assert!(
                collection_leaf_kind(&info(k, "(I)Ljava/lang/Object;"), get, true)
                    == LeafNativeKind::Callback
            );
        }
        // A static site has no receiver, and an `invokespecial` keeps the
        // plain callback path.
        for k in [1u8, 3] {
            assert!(
                collection_leaf_kind(&info(k, "(I)Ljava/lang/Object;"), get, false)
                    == LeafNativeKind::Callback
            );
        }
    }

    /// r9w4 vm4 (exception-subclass-constructor page, option 1): the site
    /// cache now admits `invokespecial` by default, and still refuses any kind
    /// the dispatcher does not know.
    #[test]
    fn site_cache_admits_invokespecial_by_default() {
        if std::env::var_os("CRATONVM_JIT_SITE_CACHE_SPECIAL").is_some() {
            return; // an explicit setting in the test environment wins
        }
        for k in [0u8, 1, 2, 3] {
            assert!(site_cache_admits_invoke_kind(k), "kind {k}");
        }
        assert!(!site_cache_admits_invoke_kind(4));
        assert!(!site_cache_admits_invoke_kind(255));
    }
}

#[derive(Clone, Copy)]
struct NativeSiteCache {
    kind: LeafNativeKind,
    /// `poly_call_site::vh_value_site_classes(info.descriptor)` for a
    /// signature-polymorphic (`poly`) entry, `0` otherwise. Published before
    /// the native runs, so a `VarHandle` store-mode native can apply
    /// `asType`'s static refusals (round 11 wave 16,
    /// `r11w16-vh-jit-site-cache-arm-vh-value-site-patch-FIXED-20260925`).
    /// Since round 15 wave 3 (lane vh3; VH1-3) the whole
    /// `poly_call_site::vh_param_site_classes` word (coordinates included,
    /// read modes too), armed with `arm_vh_param_site`.
    vh_value_site: u64,
    /// Whether the resolved slot claims the leaf contract, and may therefore go
    /// through `safe_native_call_leaf` instead of the full funnel.
    ///
    /// `false` entries are the majority, and they are the reason this cache is
    /// worth having for more than leaves: **skipping `invoke_or_native` is a
    /// bigger win than skipping the funnel.** Measured on a quiet host with
    /// `probes/AqsAttributionProbe.java`:
    ///
    /// | | ns/op |
    /// |---|---:|
    /// | ordinary Java call | 7.9 |
    /// | `AtomicInteger.get` — leaf, site-cached | 233 |
    /// | `AtomicInteger.compareAndSet` — non-leaf | 806 |
    /// | `setExclusiveOwnerThread` — non-leaf, 2 object args | 821 |
    ///
    /// The ~570 ns between the leaf and non-leaf rungs is not the funnel — it
    /// is the dispatcher's tail re-running `invoke_or_native`'s ~27-gate string
    /// cascade and its three-string registry hash **on every call**. Resolving
    /// the site once removes that for both flavours; the leaf claim then
    /// decides only which call wrapper runs.
    leaf: bool,
    callback: cratonvm_native_api::NativeCallback,
    /// §4 census handle — see [`NativeDispatchCache::native_id`].
    native_id: Option<cratonvm_native_api::NativeMethodId>,
    /// Exact receiver class id this entry is valid for, or `None` for an
    /// `invokestatic` site (no receiver to guard).
    ///
    /// A virtual/interface site is resolved on the RECEIVER's runtime class,
    /// not on `info.class_name` (the constant-pool owner), for the same reason
    /// [`NativeDispatchCache`] is keyed that way: an interface method resolves
    /// to whatever the receiver overrides. The guard is re-tested on every hit
    /// so a site that goes polymorphic falls out to the generic dispatcher
    /// rather than calling the wrong body.
    receiver_class_id: Option<u32>,
    /// The entry was resolved through rule 4 — a signature-polymorphic
    /// `VarHandle` access mode, whose native is registered with an erased
    /// `Object[]`/`Object` signature that has nothing to do with the call
    /// site's own descriptor.
    ///
    /// The dispatch side must then unbox the erased result against the CALL
    /// SITE descriptor (`unbox_poly_return_checked`), exactly as
    /// `invoke_on_class_shared_inner` does. `coerce_native_return` — what
    /// every other entry uses — would hand a boxed `Integer` back into an
    /// `int` return slot.
    poly: bool,
    /// `capability::classify_native`'s answer for this site's RECEIVER class,
    /// resolved once with the rest of the entry.
    ///
    /// `Some` means the dispatch must run
    /// `vm_exec::check_native_dispatch_capability_for_slot` before calling the
    /// callback — the same gate, at the same point in the call, as
    /// `invoke_or_native`'s "dispatch site 1 of 3". `None` for every native
    /// that is not process spawn, library load, `Unsafe`, Panama, or
    /// file/socket I/O, which is nearly all of them; those pay one
    /// predicted-not-taken branch.
    ///
    /// It is the KIND and not a flag because the dispatch must not re-derive
    /// it: only this resolution saw the receiver's runtime class. See the long
    /// note at the classification site in `resolve_native_site` for why this is
    /// carried at all and no longer a refusal.
    capability_kind: Option<cratonvm_native_api::CapabilityKind>,
    /// This site's descriptor, decoded once — see [`NativeSiteDescriptor`].
    desc: NativeSiteDescriptor,
    /// `Some((is_static, declaring class))` when the native answers an
    /// `ACC_SYNCHRONIZED` method under `native_sync_enabled`
    /// (`interpreter::native_sync_for_id`, the funnel's own memo): the
    /// dispatch then takes the full funnel under that monitor
    /// (`safe_native_call_synchronized`) and none of the leaf / collection /
    /// `VarHandle` shortcuts. `None` — nearly every entry — costs one
    /// not-taken branch.
    sync: Option<(bool, ClassId)>,
    /// The ANCESTOR of the receiver's class whose registered native this
    /// entry serves (`resolve_native_owner_for_receiver`'s walk), with that
    /// class's redefine generation at fill; `None` for the receiver's own
    /// native, a static or `invokespecial` site (their class is the one the
    /// hit's other redefine tests ask), a carrier entry and the
    /// signature-polymorphic tail. Round 15 wave 4, lane walk2
    /// (`r15w3-walk-helpers-site-cache-owner-redefine-patch-FIXED-20260930.md`): the
    /// hit asked the site's class and the receiver's, never this one, so a
    /// JVMTI redefinition of the owner after the fill kept its native over
    /// the woven body wherever no retire flushed this memo.
    ///
    /// Round 15 wave 4 (lane walk3): with
    /// `NativeMethodRegistry::jit_site_cache_owner_redefine` on, a rule-1
    /// entry (the receiver's own native, or a static site's) names its owner
    /// here too (`site_native_owner_scope`).
    native_owner: Option<(ClassId, u32)>,
    /// The VM's redefine epoch (`JitCache::redefine_epoch`) read before
    /// `native_owner`'s generation. While it holds, nothing was redefined
    /// since the fill and the hit asks nothing more.
    fill_redefine_epoch: u32,
    /// `native_owner` is the ONE class whose redefinition can stale this
    /// entry, and the fill applied the redefine rule to it
    /// (`site_native_owner_scope`): the hit skips its permanent "was the
    /// receiver / the constant-pool class ever redefined" tests and asks the
    /// owner once per redefine epoch. `false` (a carrier, rule-4 or
    /// unscoped entry, and every entry under
    /// `CRATONVM_JIT_SITE_CACHE_OWNER_REDEFINE=0`) keeps those tests. Round 15
    /// wave 4, lane walk3
    /// (`r15w4-walk2-jit-site-cache-declines-forever-after-a-redefinition-FIXED-20261001.md`);
    /// default ON since round 16 (lane redefine).
    owner_scoped: bool,
    /// A redefined-owner refusal (`site_refusal` slot 12) for the ONE receiver
    /// class in `receiver_class_id`, not a site-wide one: every call with that
    /// receiver declines (the woven body, or the funnel's per-call walk,
    /// decides), and the first call with another receiver re-resolves the site
    /// once (`native_site_refill_for_receiver`). The rest of the entry is a
    /// placeholder that is never dispatched (`NativeSiteCache::refusal_for_receiver`).
    /// Round 16 (lane redefine; backlog sweepB S4, WK4-6): a site-wide
    /// `(generation, None)` refused every later receiver of a polymorphic site
    /// whose FIRST receiver's owner was woven. Only with
    /// `NativeMethodRegistry::jit_site_cache_owner_redefine`.
    refused_for_receiver: bool,
}

impl NativeSiteCache {
    /// The receiver-keyed slot-12 refusal (see `refused_for_receiver`): a
    /// virtual/interface site whose receiver `guard` resolved to a redefined
    /// owner. Its callback is never called (the hit returns before any
    /// dispatch for a `refused_for_receiver` entry). Fill time only.
    fn refusal_for_receiver(guard: u32, info: &JitInvokeInfo, fill_redefine_epoch: u32) -> Self {
        fn never_dispatched(
            _ctx: &mut dyn cratonvm_native_api::NativeContext,
            _args: &[Value],
        ) -> crate::error::MethodCallResult {
            Ok(None)
        }
        NativeSiteCache {
            kind: LeafNativeKind::Callback,
            leaf: false,
            callback: never_dispatched,
            native_id: None,
            receiver_class_id: Some(guard),
            capability_kind: None,
            desc: NativeSiteDescriptor::decode(info.descriptor),
            poly: false,
            vh_value_site: 0,
            sync: None,
            native_owner: None,
            fill_redefine_epoch,
            owner_scoped: false,
            refused_for_receiver: true,
        }
    }
}

/// Round 16 (lane redefine; backlog sweepB S4): must the cached `entry` be
/// re-resolved for a call whose receiver class is `receiver`? Only a
/// receiver-keyed refusal (`NativeSiteCache::refused_for_receiver`) for ANOTHER
/// receiver: a positive entry keeps the monomorphic contract (another receiver
/// declines without re-resolving, as before), and a site-wide refusal is
/// answered before the receiver is known. Pure; the hit asks it.
fn native_site_refill_for_receiver(entry: Option<&NativeSiteCache>, receiver: Option<u32>) -> bool {
    entry.is_some_and(|e| e.refused_for_receiver && e.receiver_class_id != receiver)
}

/// Round 16 (lane redefine): what a re-resolution for a second receiver
/// (`native_site_refill_for_receiver`) caches. A second receiver-keyed refusal
/// becomes the site-wide refusal (`None`): two receivers with woven owners
/// would otherwise re-resolve the site on every alternation, so a site pays
/// at most one extra resolution per registry generation.
fn native_site_refill_outcome(resolved: Option<NativeSiteCache>) -> Option<NativeSiteCache> {
    resolved.filter(|e| !e.refused_for_receiver)
}

/// `CRATONVM_JIT_SITE_CACHE_CAPABILITY` — default-ON, `=0` restores the
/// blanket refusal of capability-classified triples in `resolve_native_site`.
///
/// One binary, two arms, so "serve `Unsafe` from the site cache and run the
/// gate on dispatch" can be priced against "refuse the site and pay
/// `invoke_or_native` for every call" without a rebuild — and switched off
/// on the spot if a capability question is ever raised against it.
fn site_cache_capability_gate_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_SITE_CACHE_CAPABILITY")
            .map_or(true, |v| v != "0" && !v.eq_ignore_ascii_case("false"))
    })
}

/// A call site's descriptor, decoded once and carried on the cache entry the
/// dispatch already probes.
///
/// A site-cached native dispatch used to parse `info.descriptor` twice per
/// call: `decode_dispatch_values_into` walked it to type each argument, and
/// `coerce_native_return` scanned it again for the return tag. Measured on
/// this file's `jit_native_dispatch_profile`, that argument decode cost
/// 12.4-14.7 ns for a receiver-plus-one-object site against 9.5-11.9 ns with
/// the tags already in hand.
///
/// It rides on [`NativeSiteCache`] rather than in a memo of its own because
/// that entry is already fetched — the probe is 0.8 ns and is paid whether or
/// not this is here. A separate `JitSiteKey`-keyed map was tried first and
/// measured 14 ns per lookup, i.e. more than the parsing it removed.
///
/// Validity needs no argument beyond the one the entry already carries: the
/// descriptor belongs to the `JitInvokeInfo` this entry was resolved against,
/// and every path that can invalidate the entry (registry generation, receiver
/// class, class redefinition, the site-memo flush on a JIT generation change)
/// invalidates the decode with it.
#[derive(Clone, Copy)]
struct NativeSiteDescriptor {
    /// One descriptor tag per PARAMETER, receiver excluded.
    params: [u8; INLINE_JIT_NATIVE_ARGS],
    /// Parameters decoded into `params`, or `u8::MAX` when the site declares
    /// more than `INLINE_JIT_NATIVE_ARGS` of them — then `params` is unusable
    /// and the caller parses the descriptor exactly as before.
    params_len: u8,
    /// What `cratonvm_jit::return_type` answers for this descriptor.
    ret: u8,
}

impl NativeSiteDescriptor {
    fn decode(descriptor: &str) -> Self {
        let mut params = [0u8; INLINE_JIT_NATIVE_ARGS];
        let mut count = 0usize;
        for tag in DescriptorParamIter::new(descriptor) {
            if count < INLINE_JIT_NATIVE_ARGS {
                params[count] = tag;
            }
            count += 1;
        }
        Self {
            params,
            params_len: if count > INLINE_JIT_NATIVE_ARGS {
                u8::MAX
            } else {
                count as u8
            },
            ret: cratonvm_jit::return_type(descriptor),
        }
    }

    /// The decoded tags, or `None` for a site with more parameters than this
    /// can hold.
    fn params(&self) -> Option<&[u8]> {
        if self.params_len == u8::MAX {
            None
        } else {
            Some(&self.params[..self.params_len as usize])
        }
    }
}

// Thread-local map from [`JitSiteKey`] -> cached JIT entry.
// Using a thread-local avoids synchronization on the hot path.
// T10.9.B: FxHashMap — pointer values are internal; this is touched on every
// JIT-dispatched invoke.
//
// The key is `(vm_identity, JitInvokeInfo pointer)`, NOT the pointer alone —
// see [`JitSiteKey`] for why the pointer alone aliases across VMs and what a
// cross-VM hit would execute. A thread reaches two VMs via JNI
// `AttachCurrentThread`, or by being reused across `SharedVm`s in one test
// process; thread-local is not per-VM.
site_keyed_memos! {
    retire_only {
        /// Statically bound site -> the published callee body it enters. The
        /// body is found by exact key (`JitCache::get`), so the only event
        /// that can change the answer is a replacement or withdrawal of that
        /// key. A withdrawal is a retire; a replacement is publish-only and
        /// marks the old body `superseded`, which the hit checks
        /// (`DispatchCache::names_a_stale_body`). `VIRTUAL_DISPATCH_CACHE`
        /// below is held to the same check at both of its hits.
        DISPATCH_CACHE: JitSiteKey => DispatchCache;
        /// Virtual/interface call sites are keyed by their JIT site key AND the
        /// receiver's actual class id. A static CP owner is not sound here:
        /// an interface method may resolve to a receiver override.
        VIRTUAL_DISPATCH_CACHE: (JitSiteKey, u32) => DispatchCache;
        /// `(JitSiteKey, receiver ClassId or 0) -> ` the compiled callee whose own
        /// exception handler a dispatch helper resumes (`resolve_callee_cached`).
        ///
        /// Every serviced callee exception used to re-resolve it under the class
        /// manager lock and rebuild the template — a copy of the whole method body,
        /// its exception table and three `Arc<str>`s — per THROW, so a loop whose
        /// compiled callee throws and catches paid that on every iteration. Only
        /// successful resolutions are stored (a callee with a non-empty table that
        /// is not `synchronized`), so a hit also answers
        /// `callee_has_exception_table` without the lock. Keyed on the receiver
        /// class for kinds 0/2 (it selects the override), on the site alone
        /// otherwise; the `RedefineGate` is re-tested per hit, and the memo is on
        /// the class-identity flush because the static kinds resolve by NAME.
        CALLEE_HANDLER_TEMPLATE_CACHE: (JitSiteKey, u32) => (std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>, crate::classloading::resolution::RedefineGate);
        /// `(JitSiteKey, receiver ClassId) -> CachedDispatchTarget`.
        ///
        /// Keyed exactly like `VIRTUAL_DISPATCH_CACHE`. Array receivers and
        /// `ClassId(0)` never reach it — an array header carries its COMPONENT
        /// class id, so `(site, class id)` does not identify one.
        VIRTUAL_TARGET_CACHE: (JitSiteKey, u32) => CachedDispatchTarget;
        /// `JitSiteKey -> (redefine epoch, verdict)`: whether the class this
        /// site names was ever redefined, as of that epoch of the site's VM
        /// ([`site_class_was_redefined`]). No publication can change which
        /// class a site names.
        QUIESCE_VERDICT: JitSiteKey => (u32, bool);
    }
    publication_sensitive {
        DISPATCH_COUNTER: JitSiteKey => u32;
        OBJECT_NATIVE_DISPATCH_CACHE: JitSiteKey => NativeDispatchCache;
        INTEGER_NATIVE_DISPATCH_CACHE: JitSiteKey => Option<IntegerNativeDispatchCache>;
        /// Leaf-native resolution per JIT call site — see
        /// [`NativeSiteCache`]. `None` is a cached refusal ("this site is
        /// not a leaf native"), which is what keeps the ~27-gate `invoke_or_native`
        /// probe off every OTHER site's steady state; it is keyed on the registry
        /// generation stored beside it so a lazy `register_*` pass that appears
        /// later is still seen.
        NATIVE_SITE_CACHE: JitSiteKey => (u32, Option<NativeSiteCache>);
        /// `(JitSiteKey, receiver ClassId) -> ` the native-site resolution of
        /// a virtual/interface site's second receiver class, stamped with the
        /// registry generation like `NATIVE_SITE_CACHE` (JIT round 19 wave 3,
        /// lane funnel3; FUNNEL2-19-1, `native_site_second_receiver_entry`).
        /// Empty unless `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY` is on.
        NATIVE_SITE_CACHE_ALT: (JitSiteKey, u32) => (u32, Option<NativeSiteCache>);
        VIRTUAL_DISPATCH_COUNTER: (JitSiteKey, u32) => u32;
        /// `(JitSiteKey, receiver ClassId) -> declined-probe count`.
        ///
        /// A compile probe that declined this callee will decline it again. On
        /// netty's `AdaptiveByteBufAllocatorTest` `probe_returned_none` read
        /// **6 592 571** — every single entryless MIC hit in the run — against
        /// `cyc_compile_probe` of 9.16e9 cycles, i.e. ~3.5 s of a ~430 s run spent
        /// re-asking a question that had already been answered on the first call.
        ///
        /// Throttled rather than permanent: a refusal can be transient (another
        /// thread holding the compile, a tier budget), and a never-retry memo would
        /// pin such a site to the helper for the life of the process. One probe per
        /// [`MIC_COMPILE_PROBE_RETRY`] calls keeps the retry while removing 1023 of
        /// every 1024 probes. Being a `site_keyed_memos!` member is what makes it
        /// safe: the same generation and class-identity flushes that drop every
        /// other dispatch memo drop this one too.
        MIC_COMPILE_DECLINED: (JitSiteKey, u32) => u32;
        /// `JitSiteKey -> ` the interpreter frame template for an `invokestatic`
        /// callee the JIT did not compile, or `None` as a cached refusal.
        ///
        /// Keyed by the call site alone: a static call has no receiver, so the
        /// target is a pure function of the site. Being a `site_keyed_memos!`
        /// member is what makes it safe — the same generation and class-identity
        /// flushes that drop every other dispatch memo drop this one too.
        STATIC_BYTECODE_CALLEE_CACHE: JitSiteKey => Option<(std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>, crate::classloading::resolution::RedefineGate)>;
        /// `(JitSiteKey, receiver ClassId) -> ` the interpreter frame template for
        /// an `invokevirtual`/`invokeinterface` callee the JIT did not compile, or
        /// `None` as a cached refusal.
        ///
        /// Keyed by the RECEIVER CLASS as well as the site, because that pair is
        /// what selects the override: the same site reached with a different
        /// receiver class gets its own entry rather than reusing one resolved for
        /// another class. That is also why no separate receiver guard is re-tested
        /// per hit — the key IS the guard. Array receivers and `ClassId(0)` never
        /// reach it, for the reason `VIRTUAL_TARGET_CACHE` records.
        VIRTUAL_BYTECODE_CALLEE_CACHE: (JitSiteKey, u32) => Option<(std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>, crate::classloading::resolution::RedefineGate)>;
        /// `JitSiteKey -> ` the interpreter frame template for an `invokespecial`
        /// callee the JIT did not compile, or `None` as a cached refusal.
        ///
        /// Keyed by the call site ALONE, unlike its virtual twin, and that is the
        /// definition of `invokespecial`: the target is chosen by the resolved
        /// constant-pool class, never by the receiver's runtime class. A
        /// receiver-keyed memo here would be modelling a retarget the opcode
        /// explicitly does not perform — the one that turns a super-call into a
        /// self-call and recurses forever (see the `invoke_kind == 1` arm).
        SPECIAL_BYTECODE_CALLEE_CACHE: JitSiteKey => Option<(std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>, crate::classloading::resolution::RedefineGate)>;
        /// `JitSiteKey -> ()`: this statically bound site's published callee
        /// needs a wrapped entry (`requires_wrapped_entry`), so the by-name
        /// `jit_cache` probe in `jit_invoke_dispatch` can only refuse it again.
        /// Without this memo nothing was cached for such a site and every call
        /// repeated `class_manager.read()`, `jit_cache.read()` and a
        /// string-keyed lookup (round 11).
        ///
        /// A refusal, so it is publication-sensitive (round 13 wave 6, lane
        /// sync3; it was `retire_only` until then): since round 13 wave 4 a
        /// synchronized INSTANCE method can publish a SELF-LOCKING body
        /// (`CompiledMethod::self_locks_monitor`, not wrapped) over a wrapped
        /// one it was compiled after -- a replacement, which is publish-only --
        /// and a `retire_only` memo kept an `invokespecial` of it (private or
        /// `super.`) on the interpreter route for the life of the thread.
        WRAPPED_STATIC_CALLEE: JitSiteKey => ();
    }
}

thread_local! {
    /// Real `java/lang/Integer` class discovered from the first ordinary
    /// `valueOf` result in each VM, as `(vm_identity, class id)`. A different
    /// VM identity invalidates the entry.
    static INTEGER_WRAPPER_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
    /// Exact real-JDK `java/util/regex/Matcher` class id, discovered once per
    /// VM for the virtual-MIC native fast path. `(vm_identity, class id)`.
    static MATCHER_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
}

// ===========================================================================
// BUG-1 fix: native-stack recursion guard for the JIT→JIT dispatch path.
//
// A recursive Java method that has been JIT-compiled (e.g. `binaryTrees(18)`'s
// deep `make(int)` self-recursion) never re-enters `interpreter::execute`, so
// the interpreter's `EXEC_DEPTH` / `EXEC_DEPTH_CEILING` guard (which throws a
// *catchable* `StackOverflowError`) never fires. Each compiled recursion level
// instead stacks a large native Rust frame through
// `jit_invoke_dispatch` / `jit_invoke_virtual_mic` → compiled entry → … with
// nothing checking remaining OS stack, so deep recursion overflows the guard
// page → uncatchable rc=127 abort.
//
// We mirror the interpreter's guard with a dedicated thread-local JIT-dispatch
// depth counter, incremented at the top of BOTH dispatch helpers under an RAII
// Drop-decrement, compared against the per-thread ceiling
// `interpreter::jit_dispatch_depth_ceiling()` (derived from the thread's REAL
// native stack size, using a generous per-level budget because the JIT
// dispatch frame is much larger than an interpreter level). On overflow we do
// NOT recurse: we stash a catchable `java/lang/StackOverflowError` in
// `JIT_PENDING_EXCEPTION` and return the `i64::MIN` deopt sentinel, exactly
// like the array-NPE / AIOOBE helpers, so the interpreter's post-JIT drain
// routes it through the method's exception table.
// ===========================================================================
thread_local! {
    /// Re-entrant JIT-dispatch depth for the current thread. Incremented on
    /// entry to `jit_invoke_dispatch` / `jit_invoke_virtual_mic` and
    /// decremented (RAII) on return. Compared against
    /// `interpreter::jit_dispatch_depth_ceiling()`.
    static JIT_DISPATCH_DEPTH: Cell<u32> = const { Cell::new(0) };

    static DISPATCH_CACHE_JIT_GENERATION: Cell<u64> = const { Cell::new(0) };
}

/// RAII guard that decrements [`JIT_DISPATCH_DEPTH`] when dropped. Constructed
/// by [`enter_jit_dispatch`] only AFTER the depth has been incremented and the
/// ceiling check passed, so every successful entry has exactly one matching
/// decrement on every return path (including the compiled callee unwinding).
struct JitDispatchDepthGuard;
impl Drop for JitDispatchDepthGuard {
    fn drop(&mut self) {
        JIT_DISPATCH_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// t1 wave 5 lane E2 (stage 2 of
/// `i94-L6-proposal-run-a-jdk-continuation-slice-on-its-carrier-os-thread`):
/// replace this OS thread's re-entrant JIT-dispatch depth, answering the old
/// one. A continuation slice run on its carrier's OS thread below carrier
/// compiled frames starts from 0, as on a runner, so its `StackOverflowError`
/// budget is not shrunk by the carrier's levels (the real-stack floor,
/// `jit_native_stack_exhausted`, still sees every carrier frame). Every guard
/// the slice takes is dropped inside the slice, so restoring cannot unbalance
/// the carrier's count.
pub(crate) fn replace_jit_dispatch_depth(v: u32) -> u32 {
    JIT_DISPATCH_DEPTH.try_with(|d| d.replace(v)).unwrap_or(0)
}

/// Only an instance constructor's first argument is an uninitialized receiver.
fn constructor_receiver(info: &JitInvokeInfo, args: &[i64]) -> i64 {
    if info.invoke_kind == 1 && info.method_name == "<init>" {
        args.first().copied().unwrap_or(0)
    } else {
        0
    }
}

/// Enter a JIT dispatch level: bump the thread-local depth and check it against
/// the per-thread native-stack ceiling.
///
/// On success returns `Ok(guard)` — the caller binds it (e.g. `let _g = ...`)
/// so the level is released on return. On overflow returns `Err(sentinel)`:
/// the depth has already been rolled back, a catchable
/// `java/lang/StackOverflowError` has been stashed in `JIT_PENDING_EXCEPTION`
/// (when constructible), and the caller must immediately `return` the contained
/// `i64::MIN` deopt sentinel WITHOUT recursing further. If the throwable cannot
/// be constructed (heap too exhausted) the sentinel is still `i64::MIN` so the
/// JIT caller deopts rather than continuing with corrupt state.
#[must_use]
fn enter_jit_dispatch(vm: &SharedVm, ctor_receiver: i64) -> Result<JitDispatchDepthGuard, i64> {
    let ceiling = crate::runtime::interpreter::jit_dispatch_depth_ceiling();
    let depth = JIT_DISPATCH_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v + 1
    });
    if depth > ceiling && !dispatch_ceiling_yields_to_stack_floor() {
        // Roll back the increment we just made — we are NOT entering a level.
        JIT_DISPATCH_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        // Interpreter round i1 wave 40, lane L2: name the bound that tripped
        // (the depth ceiling here, the stack floor below), so a probe's `soe`
        // row says which one to scale. Cold: once per raised error.
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] jit-dispatch depth ceiling TRIP: depth={depth} ceiling={ceiling}"
            );
        }
        crate::runtime::interpreter::withdraw_compiled_constructor_at_entry(vm, ctor_receiver);
        return Err(raise_jit_stack_overflow(vm));
    }
    // Round 13 wave 2 (lane mega, `r13w1-crash-ic-recursion-stack-guard-patch`
    // patch 2): the depth above counts only helper levels, so a recursion
    // whose other levels are raw inline-cache hits arrives with a small count
    // and a nearly exhausted native stack. Both dispatch helpers enter here --
    // `jit_invoke_dispatch` is also the slow path of a PIC-only or cache-less
    // site -- so one check covers every inline-cache guard's low-stack edge.
    if jit_native_stack_exhausted() {
        JIT_DISPATCH_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        if crate::runtime::env_cache::dbg_jitc() {
            // Interpreter round i1 wave 41, lane L2: the stack the helper
            // levels above the first compiled entry used, per level -- the
            // floor sits `SELF_CALL_STACK_BUDGET` (plus a requested stack's
            // growth) below that entry when that term binds, so this is an
            // estimate of the real per-level cost the ceiling's 8 KiB
            // guesses at (an over-estimate when the thread's own stack low
            // end bound the floor instead).
            let probe = 0u8;
            let sp_now = &probe as *const u8 as usize;
            let floor = JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get());
            let above = floor
                .saturating_add(current_self_call_budget().bytes())
                .saturating_sub(sp_now);
            eprintln!(
                "[cratonvm-jitc] jit-dispatch native stack floor TRIP: depth={depth} ceiling={ceiling} \
                 approx_bytes_per_level={}",
                above / usize::try_from(depth.max(1)).unwrap_or(1)
            );
        }
        crate::runtime::interpreter::withdraw_compiled_constructor_at_entry(vm, ctor_receiver);
        return Err(raise_jit_stack_overflow(vm));
    }
    Ok(JitDispatchDepthGuard)
}

/// Kill switch for [`dispatch_ceiling_yields_to_stack_floor`] (interpreter
/// round i1 wave 41, lane L2;
/// `docs/feature-designs/native-dispatch-architecture.md`).
/// `false` restores the depth ceiling as a hard bound.
const DISPATCH_CEILING_YIELDS_TO_STACK_FLOOR: bool = true;

/// Past the per-thread JIT-dispatch depth ceiling, does the real stack floor
/// decide instead? The ceiling is an ESTIMATE of how many helper levels fit
/// (half the carrier at 8 KiB a level: 512 on a default 8 MiB thread, where
/// the interpreter admits 8192 frames and HotSpot thousands of compiled ones),
/// while [`jit_native_stack_exhausted`], asked at the same helper entry right
/// after it, compares the real stack pointer with the compiled-recursion
/// floor, which keeps at least the self-call margin (1 MiB on a default
/// carrier) above the thread's own stack end -- the bound a direct compiled
/// self call already lives by. So while that check is on
/// (`CRATONVM_JIT_IC_STACK_GUARD`, default on) the estimate only ever threw
/// early: a recursion through the helpers stopped at 512 levels that the
/// stack held. With the guard switched off the ceiling binds as before.
///
/// Reached only past the ceiling, so the flag read is off the common path.
#[cold]
fn dispatch_ceiling_yields_to_stack_floor() -> bool {
    DISPATCH_CEILING_YIELDS_TO_STACK_FLOOR
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IC_STACK_GUARD")
}

/// Stash a catchable `java/lang/StackOverflowError` for the JIT caller to route
/// through its exception table, and return the `i64::MIN` deopt sentinel.
///
/// Mirrors `jit_newarray_oom`: obtain the current `&mut JvmThread`, construct
/// the throwable via `create_exception_object`, and `set_jit_pending_exception`.
/// Cold path — only hit at pathological recursion depth.
#[cold]
fn raise_jit_stack_overflow(vm: &SharedVm) -> i64 {
    // SAFETY: called from inside a JIT dispatch helper, on the thread that set
    // the JIT thread pointer; no other `&mut JvmThread` is live at this point
    // (we are above any compiled-callee invocation).
    let mut stashed = false;
    if let Some((thread, _guard)) = unsafe { jit_thread_mut() } {
        // Round 13 wave 2 (lane mega): the constructor below runs Java
        // (`Throwable.<init>` -> `fillInStackTrace()`, an inline-cache call)
        // at a stack pointer that is BELOW the floor by definition, so every
        // compiled stack check it passes trips again and raises a nested
        // `StackOverflowError`, whose constructor trips again... each level one
        // more `execute`, until the interpreter's re-entry cap fails the whole
        // chain into a bare deopt sentinel that callers answer by re-running
        // the callee interpreted: the C2-arm hang of `R13CrashIcRecursionSoe`.
        // Suspend the floor while the error is built (HotSpot disables its
        // yellow zone for the same reason); the margin below it is at least
        // three 64 KiB strides, far more than one constructor needs.
        let _suspended = SuspendedStackFloor::enter();
        if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
            vm,
            thread,
            "java/lang/StackOverflowError",
            None,
        ) {
            set_jit_pending_exception(thread, exc);
            stashed = true;
        }
    }
    // Round 12 wave 4: nothing constructible -- raise the out-of-band signal
    // (the `jit_monitor_enter_body` idiom) so the bare sentinel is never taken
    // as a value with no exception pending.
    if !stashed && helper_failure_conversion_on() {
        set_jit_deopt_pending();
    }
    i64::MIN
}

/// The compiled-recursion stack floor, suspended for the life of the value
/// (round 13 wave 2, lane mega): [`raise_jit_stack_overflow`] builds its error
/// with every compiled stack check disarmed, then puts the floor back.
///
/// Suspension stores `0` in the thread's floor cell, which every check reads
/// as "far above": the self-call guard and the IR inline-cache sample
/// (`jit_self_call_stack_guard`), [`jit_native_stack_exhausted`], and the
/// prologue fetch that single-pass frames entered meanwhile cache
/// (`jit_native_stack_floor`). Those frames all belong to the construction
/// and return before the restore. `CRATONVM_JIT_SOE_BUILD_SUSPENDS_FLOOR=0`
/// leaves the floor armed (the pre-fix behaviour).
struct SuspendedStackFloor(Option<usize>);

impl SuspendedStackFloor {
    fn enter() -> Self {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SOE_BUILD_SUSPENDS_FLOOR")
        {
            return Self(None);
        }
        let saved = JIT_SELF_CALL_STACK_FLOOR.with(|f| {
            let v = f.get();
            // Unset (`usize::MAX`) or already suspended (`0`, a nested raise
            // under a construction): nothing to suspend or restore here.
            if v == usize::MAX || v == 0 {
                None
            } else {
                f.set(0);
                Some(v)
            }
        });
        Self(saved)
    }
}

impl Drop for SuspendedStackFloor {
    fn drop(&mut self) {
        if let Some(v) = self.0 {
            JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(v));
        }
    }
}

/// Interpreter round i3 wave 79, lane L3: re-anchor this thread's
/// compiled-recursion budget ([`SELF_CALL_STACK_BUDGET`] below the first
/// observed stack pointer) for a native that re-enters Java -- a served
/// `Method.invoke` / `Constructor.newInstance` -- and return the value to hand
/// back to [`restore_self_call_stack_floor`] when the native returns.
///
/// The budget bounds how much stack COMPILED recursion may burn (the JIT's
/// `-Xss` analogue for the interpreter's frame cap). A reflective level's Rust
/// frames (the door, `execute`, the helpers) are not compiled Java frames, but
/// were charged to it: nested reflective calls under the JIT stopped at about
/// 300 levels on the 128 MiB main thread with a `StackOverflowError`
/// (`L3W79NestedReflectDepth` `depth-400`; HotSpot and `--nojit` run it).
///
/// The common case is free (wave-79 follow-up: unsetting the floor on every
/// call made each nested call's first compiled query ask the OS for the
/// stack bounds, +35-70% on `L3W77ReflectFactsBench` under the JIT): only a
/// call whose stack pointer is already within [`REFLECTIVE_REBASE_MARGIN`] of
/// the cached floor moves it, to [`SELF_CALL_STACK_BUDGET`] below that
/// pointer, never below the thread's own guard floor
/// ([`compute_self_call_stack_floor_in`] over `bounds`, the stack bounds the
/// caller caches; `None` asks the OS, once per rebase). A shallow call is one
/// TLS read and a compare and returns [`FLOOR_NOT_REBASED`]. An unset floor
/// (`usize::MAX`, nothing compiled asked yet) or a suspended one (`0`, a
/// `StackOverflowError` under construction) is left as it is.
pub(crate) fn rebase_self_call_stack_floor(bounds: Option<(usize, usize)>) -> usize {
    let probe = 0u8;
    let sp_now = std::hint::black_box(&probe) as *const u8 as usize;
    let floor = JIT_SELF_CALL_STACK_FLOOR.with(std::cell::Cell::get);
    if !floor_wants_rebase(sp_now, floor) {
        return FLOOR_NOT_REBASED;
    }
    rebase_self_call_stack_floor_cold(sp_now, floor, bounds)
}

/// [`rebase_self_call_stack_floor`]'s move, off the common path.
#[cold]
#[inline(never)]
fn rebase_self_call_stack_floor_cold(
    sp_now: usize,
    floor: usize,
    bounds: Option<(usize, usize)>,
) -> usize {
    let bounds = bounds.or_else(thread_stack_bounds);
    let budget = current_self_call_budget();
    let rebased = compute_self_call_stack_floor_in(sp_now, budget, bounds);
    if rebased >= floor {
        // The guard floor binds already: nothing lower to move to.
        return FLOOR_NOT_REBASED;
    }
    JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(rebased));
    floor
}

/// What [`rebase_self_call_stack_floor`] returns when it moved nothing;
/// [`restore_self_call_stack_floor`] then does nothing. Never a floor: a
/// floor is `0` (suspended) or an address `SELF_CALL_STACK_BUDGET` below a
/// stack pointer.
pub(crate) const FLOOR_NOT_REBASED: usize = usize::MAX;

/// How close to the cached floor a reflective door's stack pointer must be
/// before the door moves it: half the budget, so a level re-anchors once per
/// ~2 MiB of nested native frames and a shallow call never.
const REFLECTIVE_REBASE_MARGIN: usize = SELF_CALL_STACK_BUDGET / 2;

/// Does a reflective door at `sp_now` move the cached `floor`? Only an armed
/// floor (not unset, not suspended) within [`REFLECTIVE_REBASE_MARGIN`].
fn floor_wants_rebase(sp_now: usize, floor: usize) -> bool {
    floor != 0 && floor != usize::MAX && sp_now <= floor.saturating_add(REFLECTIVE_REBASE_MARGIN)
}

/// Undo [`rebase_self_call_stack_floor`]: the floor the native's caller had
/// (nothing when it moved none).
pub(crate) fn restore_self_call_stack_floor(saved: usize) {
    if saved != FLOOR_NOT_REBASED {
        JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(saved));
    }
}

#[cfg(test)]
mod i79_l3_rebase_self_call_floor_tests {
    use super::*;

    /// Only a door near the floor moves it; unset and suspended floors stay.
    #[test]
    fn only_a_door_near_the_floor_rebases() {
        let floor = 0x4000_0000usize;
        assert!(
            !floor_wants_rebase(floor + (8 << 20), floor),
            "shallow: free"
        );
        assert!(floor_wants_rebase(floor + (1 << 20), floor));
        assert!(floor_wants_rebase(floor + REFLECTIVE_REBASE_MARGIN, floor));
        assert!(!floor_wants_rebase(
            floor + REFLECTIVE_REBASE_MARGIN + 1,
            floor
        ));
        assert!(!floor_wants_rebase(16, 0), "suspended");
        assert!(!floor_wants_rebase(16, usize::MAX), "unset");
    }

    /// A rebase moves the floor `SELF_CALL_STACK_BUDGET` below the door, never
    /// below the guard floor of the bounds it is given, and the restore gives
    /// the caller's floor back; a shallow door changes and restores nothing.
    #[test]
    fn a_reflective_door_rebases_and_restores_the_floor() {
        let before = JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get());
        let probe = 0u8;
        let sp = std::hint::black_box(&probe) as *const u8 as usize;
        // Bounds far below this frame: the guard floor does not bind.
        let bounds = Some((sp - (64 << 20), sp + (1 << 20)));
        // A cached floor just below this frame: the door is near it.
        let near = sp - (64 << 10);
        JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(near));
        let saved = rebase_self_call_stack_floor(bounds);
        assert_eq!(saved, near);
        let moved = JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get());
        // JIT r23 wave 6: the budget is `SelfCallBudget::bytes` (16 MiB for
        // the default Java stack, 4 MiB with the switch at `0`).
        assert!(
            moved < near && moved >= sp - SelfCallBudget::default().bytes() - (1 << 20),
            "moved {moved:#x} near {near:#x}"
        );
        restore_self_call_stack_floor(saved);
        assert_eq!(JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get()), near);
        // Far below this frame: shallow, untouched.
        let far = sp - (32 << 20);
        JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(far));
        let saved = rebase_self_call_stack_floor(bounds);
        assert_eq!(saved, FLOOR_NOT_REBASED);
        restore_self_call_stack_floor(saved);
        assert_eq!(JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get()), far);
        JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(0));
        assert_eq!(rebase_self_call_stack_floor(bounds), FLOOR_NOT_REBASED);
        assert_eq!(
            JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get()),
            0,
            "suspension kept"
        );
        JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(before));
    }
}

#[cfg(test)]
mod r13w2_mega_suspended_floor_tests {
    use super::*;

    /// Suspended while held (nested holds included), restored exactly once by
    /// the outermost, and an unset floor is left unset.
    #[test]
    fn the_floor_is_suspended_while_building_and_restored_after() {
        let ok = std::thread::spawn(|| {
            let get = || JIT_SELF_CALL_STACK_FLOOR.with(|f| f.get());
            {
                let _unset = SuspendedStackFloor::enter();
                assert_eq!(get(), usize::MAX, "an unset floor stays unset");
            }
            JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(0x1000_0000));
            {
                let _outer = SuspendedStackFloor::enter();
                assert_eq!(get(), 0);
                {
                    let _inner = SuspendedStackFloor::enter();
                    assert_eq!(get(), 0);
                }
                assert_eq!(get(), 0, "the nested hold does not restore early");
            }
            assert_eq!(get(), 0x1000_0000);
        })
        .join()
        .is_ok();
        assert!(ok);
    }
}

#[cfg(test)]
mod r13w4_mega3_floor_word_tests {
    use super::*;
    use crate::ThreadId;

    /// Build w4c regression: a publish made while `raise_jit_stack_overflow`
    /// has the floor suspended (TLS `0`) must not copy the `0` into the
    /// thread's floor word -- nothing would re-arm it afterwards, and every
    /// compiled inline-cache check of the thread would pass forever.
    #[test]
    fn a_publish_under_a_suspended_floor_keeps_the_armed_word() {
        let ok = std::thread::spawn(|| {
            let mut thread = Box::new(JvmThread::new(ThreadId(13), "r13w4-mega3-floor-word"));
            JIT_SELF_CALL_STACK_FLOOR.with(|f| f.set(0x1000_0000));
            let scope = set_jit_thread(&mut thread);
            restore_jit_thread(scope);
            assert_eq!(thread.jit_stack_floor, 0x1000_0000, "armed by the publish");
            {
                let _suspended = SuspendedStackFloor::enter();
                // The error's constructor re-enters compiled code.
                let scope = set_jit_thread(&mut thread);
                restore_jit_thread(scope);
            }
            assert_eq!(
                thread.jit_stack_floor, 0x1000_0000,
                "a suspended publish disarmed the floor word"
            );
        })
        .join()
        .is_ok();
        assert!(ok);
    }
}

#[cfg(test)]
mod r16w17_mega5_blind_late_probe_tests {
    //! Round 16 wave 17, lane mega5: the late probe sets a blind key to probe
    //! on its next call, once per memo generation, and the blind arm's count
    //! does not see the mark.
    use super::*;

    const THRESHOLD: u32 = 500;
    /// A fake site address: the memo is keyed by value, and a test `static`
    /// would move the per-VM statics ratchet.
    const SITE: usize = 0x5170_0000;

    fn key(receiver: u32) -> (JitSiteKey, u32) {
        (jit_site_key(0x51_7e, SITE), receiver)
    }

    fn raw(key: (JitSiteKey, u32)) -> Option<u32> {
        VIRTUAL_DISPATCH_COUNTER.with(|dc| dc.borrow().get(&key).copied())
    }

    #[test]
    fn a_key_is_set_to_probe_once_per_generation() {
        let k = key(0x9001);
        VIRTUAL_DISPATCH_COUNTER.with(|dc| dc.borrow_mut().insert(k, 3));
        assert!(blind_late_probe_rewind_counter(k, THRESHOLD));
        let set = raw(k).expect("entry");
        assert_eq!(set & !BLIND_LATE_PROBE_MARK, THRESHOLD - 1);
        // The blind arm's next increment reads exactly the threshold.
        assert_eq!(set.wrapping_add(1) & !BLIND_LATE_PROBE_MARK, THRESHOLD);
        // Marked: a second stand-down in the same generation does nothing.
        assert!(!blind_late_probe_rewind_counter(k, THRESHOLD));
        assert_eq!(raw(k), Some(set));
        // A publication clears the memo, and with it the mark.
        clear_publication_sensitive_site_memos();
        assert_eq!(raw(k), None);
        assert!(blind_late_probe_rewind_counter(k, THRESHOLD));
        clear_publication_sensitive_site_memos();
    }

    #[test]
    fn a_key_never_counted_is_created_set_and_a_zero_threshold_is_inert() {
        let k = key(0x9002);
        clear_publication_sensitive_site_memos();
        assert!(blind_late_probe_rewind_counter(k, THRESHOLD));
        assert_eq!(
            raw(k).map(|r| r & !BLIND_LATE_PROBE_MARK),
            Some(THRESHOLD - 1)
        );
        let other = key(0x9003);
        assert!(!blind_late_probe_rewind_counter(other, 0));
        assert_eq!(raw(other), None);
        clear_publication_sensitive_site_memos();
    }

    /// Off by default, and a MIC-side call never touches the memo.
    #[test]
    fn the_switch_and_the_blind_flag_gate_it() {
        let k = key(0x9004);
        clear_publication_sensitive_site_memos();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_BLIND_DISPATCH_LATE_PROBE", None)],
            || blind_dispatch_late_probe_rewind(k, true),
        );
        assert_eq!(raw(k), None);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_BLIND_DISPATCH_LATE_PROBE", Some("1"))],
            || blind_dispatch_late_probe_rewind(k, false),
        );
        assert_eq!(raw(k), None);
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_BLIND_DISPATCH_LATE_PROBE", Some("1"))],
            || blind_dispatch_late_probe_rewind(k, true),
        );
        assert!(raw(k).is_some_and(|r| r & BLIND_LATE_PROBE_MARK != 0));
        clear_publication_sensitive_site_memos();
    }
}

#[cfg(test)]
mod r16w8_mega_blind_dispatch_hint_tests {
    //! Round 16 wave 8, lane mega (shape 6, MG2-4): the blind dispatch arm's
    //! table cue fires only below the threshold, only for a way the table
    //! holds, and only under `CRATONVM_JIT_MEGA_BLIND_DISPATCH_HINT`; when it
    //! fires it spends the key's one probe. A spent key re-probes only under
    //! `CRATONVM_JIT_BLIND_DISPATCH_REPROBE`.
    use super::*;
    use crate::config::VmConfig;

    const SWITCH: &str = "CRATONVM_JIT_MEGA_BLIND_DISPATCH_HINT";
    const RECEIVER: u32 = 0x7781;
    const THRESHOLD: u32 = 500;

    static INFO: JitInvokeInfo = JitInvokeInfo {
        class_name: "r16w8/Visitor",
        method_name: "visit",
        descriptor: "(I)I",
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    };

    fn ret_body() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // RET
        cratonvm_jit::CompiledMethod::new(buf)
    }

    fn key(shared: &SharedVm) -> (JitSiteKey, u32) {
        (
            // Cast: the site's info address, as the helper keys it.
            jit_site_key(shared.vm_identity, &INFO as *const JitInvokeInfo as usize),
            RECEIVER,
        )
    }

    fn counter(key: (JitSiteKey, u32)) -> Option<u32> {
        VIRTUAL_DISPATCH_COUNTER.with(|dc| dc.borrow().get(&key).copied())
    }

    /// Publish a body for `RECEIVER` and its way under `INFO`'s selector into
    /// `shared`'s table, made under `switch`. Returns the body (held).
    fn publish_way(
        shared: &SharedVm,
        switch: Option<&str>,
    ) -> std::sync::Arc<cratonvm_jit::CompiledMethod> {
        let table = cratonvm_types::flags::with_thread_overrides(&[(SWITCH, switch)], || {
            shared.jit.jit_cache.mega_dispatch_table()
        });
        assert_eq!(table.blind_dispatch_hint_enabled(), switch != Some("0"));
        let selector = table
            .selector_id_in_context(
                INFO.class_name,
                INFO.method_name,
                INFO.descriptor,
                INFO.invoke_kind,
                mega_selector_context(shared, &INFO),
                INFO.owner_class_id,
            )
            .expect("interned");
        let cid = ClassId::new(RECEIVER);
        shared.jit.jit_cache.put(
            std::sync::Arc::from("r16w8/Impl"),
            std::sync::Arc::from("visit"),
            std::sync::Arc::from("(I)I"),
            cid,
            ret_body(),
        );
        let body = shared
            .jit
            .jit_cache
            .get("r16w8/Impl", "visit", "(I)I", cid)
            .expect("published");
        // Cast: an entry address.
        let entry = body.entry_ptr() as u64;
        assert!(
            table
                .install_with_class_slot(RECEIVER, selector, None, entry, false, false)
                .hashed
        );
        body
    }

    #[test]
    fn on_it_cues_the_probe_for_a_held_way_below_the_threshold_only() {
        let ok = std::thread::spawn(|| {
            let shared = SharedVm::new(VmConfig::default());
            let key = key(&shared);
            // SAFETY: no class cell is read (no column: the receiver has no
            // vtable), so the thread contract is moot here.
            let hint = |count| unsafe {
                blind_dispatch_table_hint(&shared, &INFO, key, RECEIVER, count, THRESHOLD)
            };
            assert!(!hint(1), "no table, nothing held");
            let body = publish_way(&shared, Some("1"));
            assert!(!hint(THRESHOLD), "the threshold probe owns its own count");
            assert!(!hint(THRESHOLD + 1), "a spent key is not probed again");
            assert_eq!(counter(key), None);
            assert!(hint(3));
            assert_eq!(counter(key), Some(THRESHOLD), "the cue spent the probe");
            // A receiver the table does not hold is not cued.
            let other = (key.0, RECEIVER + 1);
            // SAFETY: as above.
            assert!(!unsafe {
                blind_dispatch_table_hint(&shared, &INFO, other, RECEIVER + 1, 1, THRESHOLD)
            });
            assert_eq!(counter(other), None);
            VIRTUAL_DISPATCH_COUNTER.with(|dc| dc.borrow_mut().clear());
            shared.jit.jit_cache.mega_dispatch_table().clear_entries();
            drop(body);
        })
        .join()
        .is_ok();
        assert!(ok);
    }

    /// The spent-key re-probe fires only past the threshold, only at its
    /// doublings, and only under its switch.
    #[test]
    fn a_spent_key_reprobes_at_doublings_of_the_threshold_only_when_on() {
        const REPROBE: &str = "CRATONVM_JIT_BLIND_DISPATCH_REPROBE";
        let due = |switch: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(&[(REPROBE, switch)], || {
                (1..=17 * THRESHOLD)
                    .filter(|&count| blind_dispatch_spent_reprobe(count, THRESHOLD))
                    .collect::<Vec<u32>>()
            })
        };
        assert_eq!(due(None), due(Some("1")), "on by default");
        assert!(due(Some("0")).is_empty());
        assert_eq!(
            due(Some("1")),
            vec![2 * THRESHOLD, 4 * THRESHOLD, 8 * THRESHOLD, 16 * THRESHOLD],
            "never at or below the threshold, whose own probe runs there"
        );
        let zero = || blind_dispatch_spent_reprobe(4, 0);
        let on = [(REPROBE, Some("1"))];
        assert!(!cratonvm_types::flags::with_thread_overrides(&on, zero));
    }

    #[test]
    fn switched_off_it_cues_nothing_even_for_a_held_way() {
        let ok = std::thread::spawn(|| {
            let shared = SharedVm::new(VmConfig::default());
            let key = key(&shared);
            let body = publish_way(&shared, Some("0"));
            // SAFETY: as in the test above.
            assert!(!unsafe {
                blind_dispatch_table_hint(&shared, &INFO, key, RECEIVER, 1, THRESHOLD)
            });
            assert_eq!(counter(key), None, "the off arm counts as before");
            shared.jit.jit_cache.mega_dispatch_table().clear_entries();
            drop(body);
        })
        .join()
        .is_ok();
        assert!(ok);
    }
}

#[cold]
fn throwable_class_name(vm: &SharedVm, obj: ObjectRef) -> Option<String> {
    let cid = vm.mem.heap.class_id_of(obj);
    vm.classes
        .class_manager
        .read()
        .get_class(cid)
        .map(|c| c.name.to_string())
}

/// The S111r12 rescues' trigger: did the by-name dispatch on `dispatch_class`
/// fail with ITS OWN untranslated linkage miss (`VmError::Linkage`
/// `NoSuchMethodError` naming exactly that class and method), so that the
/// rescue may resend the call through the site's constant-pool class?
///
/// Structured only. Until interpreter round i5 wave 100 (lane L2) a second
/// arm also accepted a THROWN `NoSuchMethodError` whose detail message read
/// `C.m(desc)`; the by-name terminal has written HotSpot's `'ret C.m(params)'`
/// since 2026-09-23, so that arm had matched nothing since then and was
/// removed rather than re-keyed on the new text. The miss that arm was for --
/// a by-name dispatch on the receiver's class failing where the site's own
/// reference does not resolve -- is the site's resolution failure, which
/// HotSpot raises naming the SITE's class without running anything:
/// [`jit_site_reference_failure`] answers it in
/// [`handle_jit_dispatch_error`], for every door, from the class store rather
/// than from message text. The raw arm stays live for the dispatch classes the
/// by-name route refuses without building a throwable (the VM's `<...>`
/// placeholder names) and for a throwable that could not be materialized.
/// The rescue is a no-op for a correctly typed receiver: by-name lookup from a
/// subclass of the site's class finds every method the site's reference
/// resolves to.
#[cold]
fn is_dispatch_no_such_method_miss(
    err: &crate::error::MethodCallFailed,
    dispatch_class: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    use crate::error::{LinkageError, MethodCallFailed, VmError};

    match err {
        MethodCallFailed::InternalError(VmError::Linkage(LinkageError::NoSuchMethodError {
            class_name,
            method_name: err_method,
            method_descriptor,
        })) => {
            class_name == dispatch_class
                && err_method == method_name
                && method_descriptor == descriptor
        }
        _ => false,
    }
}

/// The class a compiled `invokevirtual` / `invokeinterface` site NAMED, when
/// its constant-pool reference itself provably does not resolve (JVMS 5.4.3.3
/// / 5.4.3.4 find nothing), so HotSpot's `LinkResolver` throws
/// `NoSuchMethodError` naming that class (`'int HJ.m()'`, `'int UGS.gone()'`)
/// before any selection, on every execution, and no method runs. Two shapes:
///
/// * the only declaration is a STATIC method of a superinterface (static
///   interface methods are not inherited;
///   [`crate::runtime::resolve::selection::reference_names_only_an_uninherited_static_interface_method`];
///   interpreter round i4 wave 99, lane L7, `L7W98StaticShadowDispatch` s4);
/// * nothing at all declares it -- separate compilation removed it
///   ([`crate::runtime::resolve::selection::member_provably_absent`], real class
///   bytes only) and no native is registered under the reference's own triple
///   (interpreter round i5 wave 100, lane L2, `L2W100UnresolvedSiteWarm`;
///   behind `CRATONVM_UNRESOLVABLE_METHOD_REF_THROWS`, default ON, as the
///   interpreter's check in `resolve_method_metadata_noting`).
///
/// `None` unless the site is kind 0 or 2 with an owner loaded in the site's
/// namespace ([`jit_site_owner`]) whose kind matches the invoke (a `Methodref`
/// naming an interface or an `InterfaceMethodref` naming a class is the
/// `IncompatibleClassChangeError` of step 1, which comes first). Under the
/// caller's class-manager guard; asked on a failed dispatch
/// ([`jit_site_reference_failure`]) and where a (site, receiver class) target
/// is resolved and memoized ([`virtual_dispatch_target_cached`],
/// [`jit_selection_errs`]), never per call on a cached path.
fn jit_site_unresolvable_owner(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    info: &JitInvokeInfo,
) -> Option<String> {
    if !matches!(info.invoke_kind, 0 | 2) {
        return None;
    }
    let owner = jit_site_owner(cm, info)?;
    let owner_class = cm.get_class(owner)?;
    if owner_class.is_interface() != (info.invoke_kind == 2) {
        return None;
    }
    let store = cm.class_store();
    // The common answer first: the reference resolves (one walk up the
    // owner's hierarchy, on a memo miss).
    if !crate::runtime::resolve::selection::member_provably_absent(
        store,
        owner,
        info.method_name,
        info.descriptor,
    ) {
        return None;
    }
    let unresolvable = crate::runtime::resolve::selection::reference_names_only_an_uninherited_static_interface_method(
        store,
        owner,
        info.method_name,
        info.descriptor,
    ) || (vm
        .natives
        .native_methods
        .find(info.class_name, info.method_name, info.descriptor)
        .is_none()
        && crate::runtime::interpreter::invoke::unresolvable_method_ref_throws_enabled());
    unresolvable.then(|| owner_class.name.to_string())
}

/// The error a compiled `invokevirtual` / `invokeinterface` site raises when
/// its constant-pool reference itself does not resolve
/// ([`jit_site_unresolvable_owner`]): HotSpot's `NoSuchMethodError` naming the
/// class the SITE named, never the receiver's class.
///
/// Every compiled dispatch door retargets onto the receiver's class before it
/// calls by name, so the by-name terminal in `invoke_on_class_shared_inner`
/// names the receiver (`'int HC1.m()'`). The S111r12 rescue that would have
/// resent the call through the site's class matched the old `C.m(desc)`
/// message text only, and its thrown arm was removed in wave 100
/// ([`is_dispatch_no_such_method_miss`]). Rewriting here, in the one sink every
/// door's failure passes through, keeps the answer the site's whatever door the
/// call took (the by-name tails, the MIC hit/miss tails, the register-overflow
/// bail, a selection failure, the bytecode-callee doors). Interpreter round i4
/// wave 99, lane L7 (the static-interface shape); wave 100, lane L2 (every
/// unresolvable reference, and the kind guard).
///
/// `None` (the error stays as it is) unless the failure is a linkage error
/// (`VmError::Linkage`, or a thrown `NoSuchMethodError` /
/// `AbstractMethodError` / `IncompatibleClassChangeError`) and
/// [`jit_site_unresolvable_owner`] names the site's class. Then no callee can
/// have been selected at all, so the failure can only be the site's own.
/// Cold: asked only on a failed dispatch.
#[cold]
fn jit_site_reference_failure(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: &crate::error::MethodCallFailed,
    info: &JitInvokeInfo,
) -> Option<crate::error::MethodCallFailed> {
    use crate::error::{MethodCallFailed, VmError};
    if !matches!(info.invoke_kind, 0 | 2) {
        return None;
    }
    let is_linkage = match err {
        MethodCallFailed::InternalError(VmError::Linkage(_)) => true,
        MethodCallFailed::ExceptionThrown(exc) => matches!(
            throwable_class_name(vm, *exc).as_deref(),
            Some(
                "java/lang/NoSuchMethodError"
                    | "java/lang/AbstractMethodError"
                    | "java/lang/IncompatibleClassChangeError"
            )
        ),
        _ => false,
    };
    if !is_linkage {
        return None;
    }
    let owner_name = {
        let cm = vm.classes.class_manager.read();
        jit_site_unresolvable_owner(vm, &cm, info)?
    };
    Some(jit_site_unresolvable_error(vm, thread, owner_name, info))
}

/// The `NoSuchMethodError` an unresolvable compiled site raises, naming
/// `owner_name` (from [`jit_site_unresolvable_owner`]).
#[cold]
fn jit_site_unresolvable_error(
    vm: &SharedVm,
    thread: &mut JvmThread,
    owner_name: String,
    info: &JitInvokeInfo,
) -> crate::error::MethodCallFailed {
    crate::runtime::exceptions::throw_linkage_error(
        vm,
        thread,
        crate::error::LinkageError::NoSuchMethodError {
            class_name: owner_name,
            method_name: info.method_name.to_string(),
            method_descriptor: info.descriptor.to_string(),
        },
    )
}

/// S112r9 — JIT dispatch error handler. When a JIT-dispatched callee returns
/// an error, route it through `JIT_PENDING_EXCEPTION` so the interpreter's
/// post-JIT exception-routing path can find a handler (or propagate to the
/// top of the JVM with a printable message).
///
/// Previously only `MethodCallFailed::ExceptionThrown` was captured, and
/// `MethodCallFailed::InternalError` was silently dropped — the JIT helper
/// returned 0/null to the JIT caller, which would proceed as if the call
/// returned a benign null. That was the root cause of Spring Boot 3 fat-jars
/// exiting silently with rc=0 between `prepareEnvironment` and `printBanner`:
/// some downstream invoke produced an `InternalError` ("method has no Code
/// attribute" or similar linkage gap), the JIT swallowed it, the JIT'd
/// `prepareEnvironment` continued with corrupt state and returned, then the
/// caller `run()` returned cleanly without ever reaching `printBanner`.
///
/// Wrapping the InternalError in a Java `java/lang/InternalError` gives the
/// VM a real Throwable to walk through exception tables. If the heap is
/// exhausted or the class can't be loaded, we fall back to leaving the
/// error unstored — the original "swallow and return 0" behaviour. That
/// keeps this purely additive: it never makes a previously-working scenario
/// worse, only converts silent rc=0 into a visible stack trace.
/// Route a failed JIT dispatch into the thread-local pending-exception
/// slot and return the value the dispatch helper should hand back to its
/// JIT caller: `i64::MIN` (the deopt sentinel) when a pending Java
/// exception was successfully stashed, or `0` if the failure could not be
/// turned into a throwable (legacy silent-drop fallback).
///
/// Returning `i64::MIN` makes the JIT caller's post-invoke exception guard
/// (`emit_post_invoke_exception_check` in `jit/src/x64.rs`) fire and deopt
/// out, so the interpreter routes the real exception through the method's
/// exception table — instead of the JIT running on with a bogus `0` and
/// masking the true failure with a downstream secondary error.
#[must_use]
fn handle_jit_dispatch_error(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
    info: &JitInvokeInfo,
) -> i64 {
    use crate::error::{ClassFileError, MethodCallFailed, RuntimeError, VmError};
    // The SITE's resolution failure, whichever door the call took (the
    // by-name tails, the MIC hit/miss tails, the register-overflow bail, a
    // selection failure): see `jit_site_reference_failure`.
    let site_err = jit_site_reference_failure(vm, thread, &err, info);
    let err = match site_err {
        Some(site_err) => site_err,
        None => err,
    };
    match err {
        MethodCallFailed::ExceptionThrown(exc) => {
            set_jit_pending_exception(thread, exc);
        }
        // A native callee that returns `Err(RuntimeError::X)` is, by the
        // exception model, asking the VM to throw the Java exception that
        // `X` maps to (e.g. `NoSuchMethodException`, `NullPointerException`,
        // `ClassCastException`). The `From<RuntimeError>` conversion wraps
        // these as `InternalError(VmError::Runtime(..))`, which is *not*
        // an internal VM bug — it is a catchable Java throwable.
        //
        // The interpreter's per-instruction post-processing already does
        // this conversion (`interpreter.rs`: "Convert RuntimeErrors from
        // native methods into catchable Java exceptions"), but the JIT
        // dispatch path previously skipped it and wrapped the runtime
        // error in a *fatal* `java/lang/InternalError`. That turned an
        // ordinary catchable exception into an uncatchable abort — e.g.
        // Netty's `Class.getDeclaredConstructor(...)` probe for the
        // legacy `DirectByteBuffer(long,int)` constructor (absent on
        // JDK 25) threw `NoSuchMethodException`, which Netty catches and
        // falls back from; under the JIT it surfaced as a fatal
        // `InternalError: ... NoSuchMethodException: <init>`.
        //
        // Mirror the interpreter: route `VmError::Runtime` through
        // `throw_runtime_error` so the proper Java exception object is
        // built and caught by the caller's exception table. `NotImplemented`
        // stays a hard error; the stack-overflow shapes take the next arm.
        MethodCallFailed::InternalError(VmError::Runtime(rt_err))
            if !matches!(
                rt_err,
                RuntimeError::NotImplemented { .. } | RuntimeError::StackOverflowError
            ) =>
        {
            match crate::runtime::exceptions::throw_runtime_error(vm, thread, rt_err) {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(thread, exc);
                }
                MethodCallFailed::InternalError(vm_err2) => {
                    // Exception-object construction failed — fall back to
                    // the legacy `InternalError` wrap so the failure is
                    // still visible rather than silently dropped.
                    let msg = format!(
                        "JIT dispatch into {}.{}{} failed: {}",
                        info.class_name, info.method_name, info.descriptor, vm_err2,
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(thread, exc);
                    }
                }
            }
        }
        // Round 12 wave 3 (hfix): a stack overflow detected under this
        // dispatch (the interpreter's `EXEC_DEPTH_CEILING`, a value-stack
        // push) is a catchable `java.lang.StackOverflowError`. The
        // interpreter excludes it from its FIRST conversion only because its
        // catch-all `VmError::Runtime` arm converts it a few lines later; the
        // arm above copied the exclusion without that second arm, so the
        // overflow fell to the catch-all below and surfaced as an
        // `InternalError` ("JIT dispatch into ... failed: runtime error:
        // StackOverflowError") that no `catch (StackOverflowError)` sees
        // (`R12HuntSoeRecover` phase 5).
        MethodCallFailed::InternalError(VmError::Runtime(rt_err))
            if dispatch_error_is_stack_overflow(&rt_err)
                && cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JIT_DISPATCH_SOE_THROWS",
                ) =>
        {
            match crate::runtime::exceptions::throw_runtime_error(
                vm,
                thread,
                RuntimeError::StackOverflowError,
            ) {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(thread, exc);
                }
                MethodCallFailed::InternalError(vm_err2) => {
                    let msg = format!(
                        "JIT dispatch into {}.{}{} failed: {}",
                        info.class_name, info.method_name, info.descriptor, vm_err2,
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(thread, exc);
                    }
                }
            }
        }
        // `VmError::Linkage` variants are catchable Java errors in the
        // `java.lang.LinkageError` hierarchy (NoClassDefFoundError,
        // NoSuchFieldError, NoSuchMethodError, …). The interpreter
        // already converts them at opcode boundaries via
        // `convert_class_not_found` (`runtime/exceptions.rs`); the JIT
        // dispatch path previously fell through to the catch-all and
        // wrapped them in `java/lang/InternalError`, breaking
        // `catch (NoClassDefFoundError)` / `catch (LinkageError)` in
        // application code — notably JUnit Platform's test-execution
        // error handler in `EngineExecutionOrchestrator.execute`, which
        // expects `NoClassDefFoundError` from a JIT-compiled callee to
        // propagate as-is so the orchestrator can record the test as
        // failed instead of aborting the whole JVM.
        //
        // Mirror the interpreter: build the matching Java throwable so
        // the caller's exception table can find a handler.
        MethodCallFailed::InternalError(VmError::Linkage(linkage_err)) => {
            let converted =
                crate::runtime::exceptions::throw_linkage_error(vm, thread, linkage_err);
            match converted {
                MethodCallFailed::ExceptionThrown(exc) => {
                    set_jit_pending_exception(thread, exc);
                }
                MethodCallFailed::InternalError(vm_err2) => {
                    let msg = format!(
                        "JIT dispatch into {}.{}{} failed: {}",
                        info.class_name, info.method_name, info.descriptor, vm_err2,
                    );
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/InternalError",
                        Some(&msg),
                    ) {
                        set_jit_pending_exception(thread, exc);
                    }
                }
            }
        }
        // A class-resolution miss in the resolver surfaces as
        // `VmError::ClassFile(ClassNotFound)`. The interpreter maps this
        // to `NoClassDefFoundError` (see `raise_no_class_def_found`); do
        // the same here so JIT-dispatched callees behave identically.
        MethodCallFailed::InternalError(VmError::ClassFile(ClassFileError::ClassNotFound {
            ref class_name,
        })) => {
            // `CRATONVM_DBG_LINKAGE_BT=1` -- the third place a
            // `NoClassDefFoundError` reaches Java (see the matching hooks in
            // `runtime::exceptions`). Only the Rust backtrace names the JIT
            // dispatch site that could not resolve the class.
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LINKAGE_BT") {
                let bt = std::backtrace::Backtrace::force_capture();
                eprintln!("[DBG_LINKAGE_BT] jit NoClassDefFoundError {class_name}\n{bt}");
            }
            // Interpreter round i5 wave 100, lane L2: the interpreter's
            // conversion (`convert_class_not_found_for`), with the SITE's class
            // as the initiating one. A user-loader site's error carries
            // HotSpot's `ClassNotFoundException` cause (`p.Missing`); this arm
            // built a bare `NoClassDefFoundError` with no cause
            // (`L2W100CompiledSiteMissingClass`).
            // A synthetic site (`declaring_class_id == 0`: the thin direct
            // natives, the indy bridge) has no initiating class and keeps the
            // bare error.
            let missing = class_name.clone();
            let converted = (info.declaring_class_id != 0).then(|| {
                crate::runtime::exceptions::convert_class_not_found_for(
                    vm,
                    thread,
                    Some(ClassId::new(info.declaring_class_id)),
                    &missing,
                    MethodCallFailed::InternalError(VmError::ClassFile(
                        ClassFileError::ClassNotFound {
                            class_name: missing.clone(),
                        },
                    )),
                )
            });
            match converted {
                Some(MethodCallFailed::ExceptionThrown(exc)) => {
                    set_jit_pending_exception(thread, exc);
                }
                Some(MethodCallFailed::InternalError(_)) | None => {
                    if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                        vm,
                        thread,
                        "java/lang/NoClassDefFoundError",
                        Some(&missing),
                    ) {
                        set_jit_pending_exception(thread, exc);
                    }
                }
            }
        }
        MethodCallFailed::InternalError(vm_err) => {
            // Format a message that points at the failing dispatch site so
            // the user can see WHICH callee blew up. This is the difference
            // between a silent rc=0 and a visible "Exception in thread main"
            // for Spring Boot.
            let msg = format!(
                "JIT dispatch into {}.{}{} failed: {}",
                info.class_name, info.method_name, info.descriptor, vm_err,
            );
            // Try to wrap in a Java `InternalError`; on any allocation /
            // load failure, fall through to the legacy silent drop so we
            // never make things worse than before this fix.
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm,
                thread,
                "java/lang/InternalError",
                Some(&msg),
            ) {
                set_jit_pending_exception(thread, exc);
            }
        }
    }
    // Return the deopt sentinel iff a pending exception was actually
    // stashed; otherwise `0` (legacy silent-drop — exception construction
    // itself failed, nothing for the caller to route).
    //
    // Read the field through `thread` rather than `jit_pending_exception_is_set()`:
    // that helper resolves the thread from the raw `JIT_THREAD` pointer, and we
    // are holding a live `&mut JvmThread` for the same thread, so going through
    // the pointer here would manufacture an aliasing reference for no reason.
    let pending = thread.jit_pending_exception.is_some();
    // Interpreter round i5 wave 106, lane L2: the re-posted throwable has left
    // `info`'s method; say so instead of leaving it unstamped.
    note_dispatch_error_escape(pending, info.method_name, info.descriptor);
    if pending {
        i64::MIN
    } else {
        0
    }
}

/// The two runtime-error shapes [`handle_jit_dispatch_error`] raises as a
/// `java.lang.StackOverflowError`: the variant itself, and the operand-stack
/// overflow `NotImplemented` both interpreter conversion points normalize to it.
fn dispatch_error_is_stack_overflow(err: &crate::error::RuntimeError) -> bool {
    use crate::error::RuntimeError;
    match err {
        RuntimeError::StackOverflowError => true,
        RuntimeError::NotImplemented { feature } => feature == "operand stack overflow",
        _ => false,
    }
}

/// Round 12 wave 4 (exc2): whether the non-dispatch JIT helpers (class-init
/// guards of `new` / `getstatic` / `putstatic`, the monitor helpers) convert a
/// `VmError::Linkage` into its Java throwable through [`stash_helper_failure`].
/// `CRATONVM_JIT_HELPER_FAILURE_CONVERSION=0` restores their pre-wave-4
/// `java/lang/InternalError` wrap.
fn helper_failure_conversion_on() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_HELPER_FAILURE_CONVERSION")
}

/// The `MethodCallFailed` -> pending-Java-throwable conversion the JIT helpers
/// outside [`handle_jit_dispatch_error`] share: `ExceptionThrown` as is;
/// `VmError::Runtime` (when `convert_runtime`) through `throw_runtime_error`,
/// the stack-overflow shapes included under `CRATONVM_JIT_DISPATCH_SOE_THROWS`
/// exactly as the dispatch arm does (both interpreter conversion points throw
/// them); `VmError::Linkage` (when `convert_linkage`) through
/// `throw_linkage_error`; anything else, and a conversion that itself failed,
/// as `java/lang/InternalError("{context}: {err}")`.
///
/// JVMS 5.5 step 11: an `Error` raised while initializing a class is rethrown
/// as itself, so a `<clinit>` entered at the interpreter's depth ceiling from
/// compiled code must surface as `StackOverflowError`, not `InternalError`.
///
/// Returns whether a throwable is now pending.
#[cold]
fn stash_helper_failure(
    vm: &SharedVm,
    thread: &mut JvmThread,
    err: crate::error::MethodCallFailed,
    context: &dyn std::fmt::Display,
    convert_runtime: bool,
    convert_linkage: bool,
) -> bool {
    use crate::error::{MethodCallFailed, RuntimeError, VmError};
    let err = match err {
        MethodCallFailed::InternalError(VmError::Runtime(rt)) if convert_runtime => {
            if dispatch_error_is_stack_overflow(&rt) {
                if cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JIT_DISPATCH_SOE_THROWS",
                ) {
                    crate::runtime::exceptions::throw_runtime_error(
                        vm,
                        thread,
                        RuntimeError::StackOverflowError,
                    )
                } else {
                    MethodCallFailed::InternalError(VmError::Runtime(rt))
                }
            } else if matches!(rt, RuntimeError::NotImplemented { .. }) {
                MethodCallFailed::InternalError(VmError::Runtime(rt))
            } else {
                crate::runtime::exceptions::throw_runtime_error(vm, thread, rt)
            }
        }
        MethodCallFailed::InternalError(VmError::Linkage(le)) if convert_linkage => {
            crate::runtime::exceptions::throw_linkage_error(vm, thread, le)
        }
        other => other,
    };
    match err {
        MethodCallFailed::ExceptionThrown(exc) => set_jit_pending_exception(thread, exc),
        MethodCallFailed::InternalError(vm_err) => {
            let msg = format!("{context}: {vm_err}");
            if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                vm,
                thread,
                "java/lang/InternalError",
                Some(&msg),
            ) {
                set_jit_pending_exception(thread, exc);
            }
        }
    }
    thread.jit_pending_exception.is_some()
}

#[cfg(test)]
mod r12w3_hfix_dispatch_stack_overflow_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::error::{MethodCallFailed, RuntimeError, VmError};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    #[test]
    fn only_the_stack_overflow_shapes_are_recognised() {
        assert!(dispatch_error_is_stack_overflow(
            &RuntimeError::StackOverflowError
        ));
        assert!(dispatch_error_is_stack_overflow(
            &RuntimeError::NotImplemented {
                feature: "operand stack overflow".to_string(),
            }
        ));
        assert!(!dispatch_error_is_stack_overflow(
            &RuntimeError::NotImplemented {
                feature: "invokedynamic".to_string(),
            }
        ));
        assert!(!dispatch_error_is_stack_overflow(
            &RuntimeError::NegativeArraySizeException { size: -1 }
        ));
    }

    /// R12HuntSoeRecover phase 5: an overflow reported under a compiled
    /// caller's dispatch must reach Java as `StackOverflowError`, not as an
    /// `InternalError` wrapping its text.
    #[test]
    fn a_dispatch_stack_overflow_is_raised_as_stack_overflow_error() {
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "r12w3-hfix-soe");
        if crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/StackOverflowError",
            None,
        )
        .is_err()
        {
            return; // the unit VM cannot build the throwable at all
        }
        for err in [
            RuntimeError::StackOverflowError,
            RuntimeError::NotImplemented {
                feature: "operand stack overflow".to_string(),
            },
        ] {
            thread.jit_pending_exception = None;
            let got = handle_jit_dispatch_error(
                &shared,
                &mut thread,
                MethodCallFailed::InternalError(VmError::Runtime(err)),
                &INTEGER_VALUE_OF_INFO,
            );
            let pending = thread.jit_pending_exception.take();
            assert_eq!(got, i64::MIN);
            assert_eq!(
                pending
                    .and_then(|exc| throwable_class_name(&shared, exc))
                    .as_deref(),
                Some("java/lang/StackOverflowError")
            );
        }
    }
}

#[cfg(test)]
mod r12w4_exc2_helper_failure_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::error::{LinkageError, MethodCallFailed, RuntimeError, VmError};
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    fn pending_class(
        shared: &SharedVm,
        thread: &mut JvmThread,
        err: MethodCallFailed,
        convert: bool,
    ) -> Option<String> {
        thread.jit_pending_exception = None;
        let stashed = stash_helper_failure(shared, thread, err, &"ctx", convert, convert);
        let pending = thread.jit_pending_exception.take();
        assert_eq!(stashed, pending.is_some());
        pending.and_then(|exc| throwable_class_name(shared, exc))
    }

    /// The class-init guards of `new` / `getstatic` / `putstatic` and the
    /// monitor helpers used to wrap a Java-mappable failure (a `<clinit>`
    /// entered at the depth ceiling, a linkage error) in `InternalError`.
    #[test]
    fn helper_failures_become_their_java_throwables() {
        let shared = SharedVm::new(VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "r12w4-exc2-helper-failure");
        if crate::runtime::exceptions::create_exception_object(
            &shared,
            &mut thread,
            "java/lang/InternalError",
            None,
        )
        .is_err()
        {
            return; // the unit VM cannot build a throwable at all
        }
        let cases: [(MethodCallFailed, &str); 4] = [
            (
                MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::StackOverflowError)),
                "java/lang/StackOverflowError",
            ),
            (
                MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NotImplemented {
                    feature: "operand stack overflow".to_string(),
                })),
                "java/lang/StackOverflowError",
            ),
            (
                MethodCallFailed::InternalError(VmError::Linkage(
                    LinkageError::NoClassDefFoundError {
                        class_name: "r12w4/Missing".to_string(),
                    },
                )),
                "java/lang/NoClassDefFoundError",
            ),
            (
                MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NotImplemented {
                    feature: "invokedynamic".to_string(),
                })),
                "java/lang/InternalError",
            ),
        ];
        for (err, want) in cases {
            // Skip a throwable class the unit VM cannot build at all.
            if crate::runtime::exceptions::create_exception_object(
                &shared,
                &mut thread,
                want,
                Some("probe"),
            )
            .is_err()
            {
                continue;
            }
            let got = pending_class(&shared, &mut thread, err, true);
            assert_eq!(got.as_deref(), Some(want));
        }
        // Conversion off: every internal error keeps the legacy wrap.
        let got = pending_class(
            &shared,
            &mut thread,
            MethodCallFailed::InternalError(VmError::Linkage(LinkageError::NoClassDefFoundError {
                class_name: "r12w4/Missing".to_string(),
            })),
            false,
        );
        assert_eq!(got.as_deref(), Some("java/lang/InternalError"));
    }
}

/// The loader-faithful owner of a compiled `invokestatic` — but ONLY when it is
/// a different class from the one the flat global binary-name map would hand
/// back. `None` means "the by-name answer is already right; take the path this
/// arm has always taken".
///
/// Returning `None` for the agreeing case is not an optimization, it is the
/// safety property: `invoke_or_native` carries a large amount of behaviour that
/// the on-class form does not reproduce (the native-registry override order, the
/// `SyntheticStub`-yields-to-real-bytecode rule, the `DowncallHandle` and
/// signature-polymorphic intercepts). Diverting every static dispatch through a
/// second path to fix the case where they disagree would be trading a rare wrong
/// answer for a common one.
///
/// The gates are ordered cheapest-first, because this sits on every compiled
/// static call:
///
/// 1. no call-site class (`0`) — synthetic call sites naming JDK classes that
///    have exactly one definition, per `JitInvokeInfo::declaring_class_id`;
/// 2. no user-defined loader has ever defined ANY class in this process — one
///    relaxed atomic, and it is the precondition for a name to have two
///    definitions at all;
/// 3. this NAME has one definition — `classify_loaded_name` is O(1) against the
///    definition-count index, and `Unique`/`Absent` both mean the by-name answer
///    cannot be the wrong copy.
///
/// Only past all three does it ask the caller's own loader, via the same
/// `find_class_by_name_for_class` the direct-call path's `callee_compiler`
/// already uses to pick a callee — a pure lookup.
///
/// EXCEPT for a caller a user-defined loader defined. There the question is
/// what THAT loader resolves the name to, which is known only once it has
/// defined or initiated it (`class_resolved_without_loading`); gate 3 is no
/// answer, because a child-first loader that has not been asked yet is about
/// to define a SECOND copy of a name that has one definition today. So such a
/// caller compares the known answer with the by-name one, and when nothing is
/// known yet drives the caller's own loader exactly as the interpreter's
/// `invokestatic` would at the same program point -- `thread` is the calling
/// thread, which is about to run the callee's Java anyway.
///
/// AND EXCEPT, before any of those gates, a hidden caller naming ITSELF
/// ([`jit_hidden_self_owner`]; JIT round 21 wave 14, lane hidden-static): its
/// class-file name has no definition (or an ordinary class's), and the
/// by-name tail would load the class path's copy and call that.
fn jit_static_owner_override(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
) -> Option<ClassId> {
    if info.declaring_class_id == 0 {
        return None;
    }
    if let Some(owner) =
        jit_hidden_self_owner(vm, ClassId::new(info.declaring_class_id), info.class_name)
    {
        return Some(owner);
    }
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&vm.loader_identity) {
        return None;
    }
    let caller = ClassId::new(info.declaring_class_id);
    let decided = {
        let cm = vm.classes.class_manager.read();
        if matches!(
            cm.get_loader_id(caller),
            Some(cratonvm_types::ClassLoaderId::UserDefined(_))
        ) {
            // A JDK-global name the caller's loader has not answered yet
            // drives it below, as the interpreter's `invokestatic` does
            // (interpreter round i1 wave 37, lane L2). The `Methodref`'s
            // `CONSTANT_Class` entry's record first: it is what the
            // interpreter's `invokestatic` of the entry answers, where the
            // memo keeps a later drive's answer (round 16, lane classsite,
            // CS2-2; R2 of `r15w5-classsite2-review-findings-FIXED-20261001.md`).
            crate::runtime::interpreter::jit_bridge::jit_recorded_class_entry_in(
                vm,
                &cm,
                caller,
                None,
                info.class_name,
            )
            .or_else(|| {
                crate::runtime::interpreter::class_resolved_without_loading(
                    vm,
                    &cm,
                    caller,
                    info.class_name,
                )
                .and_then(|id| {
                    crate::runtime::interpreter::jit_bridge::loader_faithful_global_name_answer(
                        vm,
                        &cm,
                        caller,
                        info.class_name,
                        id,
                    )
                })
            })
        } else if let Some(owner) = loader_faithful_static_owner(&cm, caller, info.class_name) {
            Some(owner)
        } else if builtin_caller_sole_copy_is_user_defined(vm, &cm, caller, info.class_name) {
            // The only definition is another loader's: drive the caller's own
            // loader below, as the interpreter's `invokestatic` does.
            None
        } else {
            return None;
        }
    };
    let owner = match decided {
        Some(owner) => owner,
        // Through the entry for an isolated loader's caller, so the drive
        // is recorded once for the entry (CS2-2).
        None => jit_bridged_owner_entry(vm, thread, caller, info.class_name)?,
    };
    if vm
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(info.class_name)
        == Some(owner)
    {
        return None;
    }
    // DIAG (`CRATONVM_DBG=coerce`, the loader-split diagnostic this shares a
    // subject with): a silent fix cannot be told from a fix that never fires.
    // This line is what proves the arm ran on the run that went green.
    if crate::runtime::env_cache::dbg_coerce() {
        let by_name = vm
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(info.class_name);
        eprintln!(
            "[DBG_COERCE] jit invokestatic: {}.{}{} from cid={:?} -- by-name owner {by_name:?} \
is NOT the caller's loader's copy {owner:?}; dispatching on the caller's",
            info.class_name, info.method_name, info.descriptor, caller,
        );
    }
    Some(owner)
}

/// The class a statically bound compiled call site of `caller` means by the
/// constant-pool owner `class_name` when that owner is `caller` ITSELF and
/// `caller` is a hidden class, or `None` (JIT round 21 wave 14, lane
/// hidden-static, `CRATONVM_JIT_HIDDEN_SELF_BY_ID`; page
/// `docs/feature-designs/class-resolution-and-caching.md`).
///
/// A hidden class is stored as `<class-file name>/0x<hex>` and is in no
/// loader's namespace, but its constant pool still names it by the class-file
/// name, so every self-naming `Methodref` reaches `jit_invoke_dispatch` with a
/// `class_name` no name lookup answers. The interpreter's `invokestatic`
/// answers it from the frame's own class (`dispatch_static`'s
/// `self_class_id`, `constants::is_self_class_reference`); the compiled static
/// tail went by name (`invoke_static_or_native_prepared`), whose last resort
/// LOADS the name: the class path's ordinary copy of the bytes, a third class
/// with its own statics, whose method it then ran. Every by-name probe
/// (`get_loaded_class_id`) answered that copy from then on.
///
/// `R21Fz3HiddenAccess1`, arm `HN` (the bytes defined twice, `H` and `HN`):
/// by the count, the payload's own `loop` runs OSR-compiled from its 2000th
/// back edge in the `HN` pass, and each of its remaining 437 `selfCtor()`
/// calls ran the copy's `selfCtor`, whose reflective `newInstance`
/// incremented the COPY's `sCount`: `fact hot HN outer.privStaticField =
/// 1910` against HotSpot's 2347 (worked out from the code, not measured; the
/// page's recipe checks it).
///
/// Cost (review item 6 of `r21-review-w15-native-findings-FIXED-20261020.md`): for
/// a caller that is not hidden -- every site but a hidden class's own -- one
/// load of this VM's lock-free hidden-class bitmap
/// (`ClassRealm::hidden_classes`, set at each hidden define) and no lock. Only
/// a hidden caller (or, once a hidden class lives past the bitmap, an id there) takes the
/// class-manager read to compare its class-file name with `class_name`. No
/// process global is read. The switch is read only for a site that is such a
/// reference.
pub(crate) fn jit_hidden_self_owner(
    vm: &SharedVm,
    caller: ClassId,
    class_name: &str,
) -> Option<ClassId> {
    if caller == ClassId::new(0)
        || !crate::runtime::interpreter::constants::hidden_class_may_refer_to_itself(vm, caller)
    {
        return None;
    }
    // A hidden caller's site naming ANOTHER class (a lambda proxy calling its
    // target), ruled out from this VM's lock-free table of recorded
    // class-file name hashes, takes no class-manager read
    // (`CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER`, opt-in; JIT round 21 wave
    // 22, lane hidden-resid; item 6 of `r21-review-w15-native-findings-FIXED-20261020.md`).
    if hidden_self_prefilter_rules_out(&vm.classes.hidden_classes, caller, class_name) {
        return None;
    }
    let is_self = {
        let cm = vm.classes.class_manager.read();
        crate::runtime::interpreter::constants::hidden_self_reference_in(&cm, caller, class_name)
    };
    hidden_self_owner_decision(
        is_self,
        caller,
        crate::runtime::interpreter::constants::jit_hidden_self_by_id_enabled,
    )
}

/// The opt-in lock-free exit of [`jit_hidden_self_owner`]: `true` only when
/// `CRATONVM_JIT_HIDDEN_SELF_NAME_PREFILTER` is on for this VM and its table
/// proves `class_name` is not `caller`'s recorded class-file name
/// (`HiddenClassBits::self_name_rules_out`). The switch is asked first, so
/// `=0` (the default) reads nothing else.
#[inline]
fn hidden_self_prefilter_rules_out(
    bits: &cratonvm_classloading::HiddenClassBits,
    caller: ClassId,
    class_name: &str,
) -> bool {
    bits.self_name_prefilter() && bits.self_name_rules_out(caller, class_name)
}

/// The decision of [`jit_hidden_self_owner`] from its facts: the caller's own
/// id for a hidden self-reference while the switch (asked only then) is on.
fn hidden_self_owner_decision(
    is_self: bool,
    caller: ClassId,
    enabled: impl FnOnce() -> bool,
) -> Option<ClassId> {
    (is_self && enabled()).then_some(caller)
}

/// `true` when a statically-bound call from `caller` to `class_name` must NOT
/// be served a compiled body found or compiled BY NAME: resolved in
/// `caller`'s own loader, `class_name` is a different class from the one the
/// flat binary-name map answers.
///
/// Also `true` for a hidden caller's reference to ITSELF
/// ([`jit_hidden_self_owner`]; JIT round 21 wave 14, lane hidden-static): the
/// name finds no class, or an ordinary class of the same class file, never
/// the caller. Asked before the loader gates, which say nothing about it.
///
/// Every by-name shortcut -- `try_jit_compile_callee`, and a `JitCache` probe
/// keyed by `get_loaded_class_id(name)` -- hands such a site the OTHER
/// loader's body. MEASURED 2026-09-23 on H2 `TestUpgrade`, which loads H2
/// 1.2.120 through `ClassLoader(null)` beside the current H2: both define
/// `org.h2.compress.CompressLZF`, the old one with INSTANCE `first/next/hash`
/// and the new one with STATIC methods of the same names and descriptors. An
/// OSR compile of the old `compress` baked the new, static `first` in as a
/// direct `invokespecial` target, so every argument arrived one slot over
/// (`this` as the `byte[]`, the array's address as the index): an
/// `ArrayIndexOutOfBoundsException: Index <pointer bits> out of bounds for
/// length 0`, or a SIGSEGV. `--nojit` and `CRATONVM_JIT_OSR=0` were clean.
///
/// Such a site keeps the dispatch helper's interpreter route, which resolves
/// the callee in the caller's loader (`invoke_special_shared_on_class` /
/// [`jit_static_owner_override`]). The gates are the ones that function uses,
/// in the same cheapest-first order, so a process with one definition of every
/// name never gets past the relaxed atomic.
pub(crate) fn jit_callee_name_is_another_loaders_copy(
    vm: &SharedVm,
    caller: ClassId,
    class_name: &str,
) -> bool {
    if caller == ClassId::new(0) {
        return false;
    }
    if jit_hidden_self_owner(vm, caller, class_name).is_some() {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] callee-guard caller={} callee={class_name}: hidden \
                 self-reference, no by-name body",
                caller.as_u32()
            );
        }
        return true;
    }
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&vm.loader_identity) {
        return false;
    }
    let cm = vm.classes.class_manager.read();
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] callee-guard caller={} callee={class_name} sole={:?} caller_loader={:?}",
            cm.get_class(caller).map(|c| c.name.to_string()).unwrap_or_default(),
            cm.find_unique_class_by_name(class_name).map(|c| c.as_u32()),
            cratonvm_native_builtins::classloader::defining_loader_for(&vm.loader_identity, caller.as_u32()).is_some(),
        );
    }
    if loader_faithful_static_owner(&cm, caller, class_name).is_some() {
        return true;
    }
    // A class a USER-DEFINED caller's loader has not answered for yet. A name with
    // one definition (the application loader's) is exactly the case
    // `loader_faithful_static_owner` waves through, and for a child-first loader
    // (Spring Boot's `ModifiedClassPathClassLoader`) that has not been asked yet
    // it is about to define a SECOND copy: the by-name callee probe then compiles
    // and binds the application loader's body for it, and the instances that body
    // allocates are not the caller's `TypeDescription`s
    // (`NoSpringWebFilterRegistrationBeanTests`: `TypeDescription$ArrayProjection.of`
    // allocating the application copy under a Mockito mock of
    // `jakarta.servlet.ServletContext`; `ClassCastException` from a JIT-compiled
    // `MethodDescription$AbstractBase.isBridgeCompatible`, `--nojit` clean).
    // `jit_static_owner_override` asks the same question of the same loader at
    // run time and drives it; this keeps the compiled site on that path until the
    // loader's answer is known. Cheap: one class-manager read and, only for a
    // user-defined caller, the recorded-answer probes.
    //
    // The mirror image: a caller of a BUILT-IN loader whose callee name has ONE
    // definition, and that definition is a USER-DEFINED loader's. The built-in
    // loader cannot see it -- it is about to define (or find) its own copy -- but
    // the by-name probe binds the only one there is. A compiled
    // `LoggingSystemFactory.lambda$fromSpringFactories$0` of the application
    // loader bound a `ModifiedClassPathClassLoader` fork's `SpringFactoriesLoader`
    // (the only definition after an earlier fork test), whose
    // `AnnotationAwareOrderComparator` then read none of the application's `@Order`
    // annotations: `LoggingSystem.get` answered `JavaLoggingSystem`
    // (`LoggingSystemTests.logbackIsTheDefaultLoggingSystem`, JIT only).
    if builtin_caller_sole_copy_is_user_defined(vm, &cm, caller, class_name) {
        return true;
    }
    if matches!(
        cm.get_loader_id(caller),
        Some(cratonvm_types::ClassLoaderId::UserDefined(_))
    ) && !class_name.starts_with('[')
    {
        let known = crate::runtime::interpreter::jit_bridge::jit_recorded_class_entry_in(
            vm, &cm, caller, None, class_name,
        )
        .or_else(|| {
            crate::runtime::interpreter::class_resolved_without_loading(vm, &cm, caller, class_name)
        });
        return known.is_none();
    }
    false
}

/// The decision inside [`jit_static_owner_override`], without the two
/// process-wide pre-gates.
///
/// Split out so it can be tested: the gates it drops are a `0` call-site class
/// and the `any_defining_loader_registered` latch, both of which are pure
/// short-circuits onto the `None` this returns anyway. The latch in particular
/// is a process-global that a unit test would have to arm for the whole test
/// binary, which is a worse trade than testing the decision it guards. That
/// the wrapper still consults them is a source-level fact, and
/// `jit_invokestatic_owner_override_is_gated_before_it_is_consulted` is what
/// holds it.
fn loader_faithful_static_owner(
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    class_name: &str,
) -> Option<ClassId> {
    // A name with one definition cannot resolve to the wrong copy, and this is
    // O(1) against the definition-count index — so it runs before the two
    // lookups below rather than after them.
    if !matches!(
        cm.classify_loaded_name(class_name),
        cratonvm_classloading::NameResolution::Ambiguous { .. }
    ) {
        return None;
    }
    let owner = cm.find_class_by_name_for_class(class_name, caller)?;
    // The caller's loader agrees with the global map: nothing to correct, and
    // the previous path is the one with all the special cases.
    if cm.get_loaded_class_id(class_name) == Some(owner) {
        return None;
    }
    Some(owner)
}

/// A caller of a BUILT-IN loader (no defining loader) whose callee name has
/// exactly ONE definition, and that definition is a USER-DEFINED loader's: the
/// built-in loader cannot see it and is about to define (or find) its own copy,
/// but every by-name shortcut binds the only one there is.
fn builtin_caller_sole_copy_is_user_defined(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    class_name: &str,
) -> bool {
    if class_name.starts_with('[')
        || cratonvm_native_builtins::classloader::defining_loader_for(
            &vm.loader_identity,
            caller.as_u32(),
        )
        .is_some()
    {
        return false;
    }
    cm.find_unique_class_by_name(class_name)
        .is_some_and(|sole| {
            cratonvm_native_builtins::classloader::defining_loader_for(
                &vm.loader_identity,
                sole.as_u32(),
            )
            .is_some()
        })
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// info_ptr must point to a live JitInvokeInfo (heap-allocated, outlives this call).
// args_ptr/num_args form a valid i64 slice of JIT-encoded arguments.
// Transmutes within this function convert cached JIT entry pointers to function pointers
// with known signatures matching the compiled method's calling convention.
#[allow(clippy::too_many_arguments)]
/// Consecutive declines after which a compiled call site stops consulting
/// the offload hook.
///
/// The hook is not free: a decline costs ~0.5 us, against ~10 ns for the
/// compiled call it is standing in front of. A site whose arrays are
/// always below `--gpu-min-work` would pay that forever, which would
/// trade the 40x this whole change removes for a new one on the kernel
/// call itself.
///
/// Shares `CRATONVM_GPU_MIN_WORK_GIVEUP` with the interpreter's
/// per-site promotion, because it is the same policy question about the
/// same kind of site. `0` never retires. Unlike the interpreter's
/// version this needs no pc-keying caveat: a `JitInvokeInfo` IS one call
/// site, so retiring one cannot deoptimise its siblings.
#[cfg(feature = "gpu-offload")]
fn compiled_offload_giveup_after() -> u32 {
    static N: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_GPU_MIN_WORK_GIVEUP")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(256)
    })
}

/// What one compiled call site is, as far as offload is concerned.
///
/// Keyed by `JitInvokeInfo` address — which is one call site *while that
/// address is live*, so a decision here cannot leak to another call of the
/// same method elsewhere.
///
/// RT-6. That sentence used to end "…which IS the site", full stop, and it is
/// false after retirement. The address is a `Box<JitInvokeInfo>` inside
/// `CompiledMethod::_jit_invoke_infos`; when the artifact retires the box is
/// freed and the allocator is free to hand the same address to an unrelated
/// compile's `JitInvokeInfo`. A stale `Active` would only cost a wasted
/// `try_dispatch` probe, because the target identity is re-derived from the
/// CURRENT `info` further down — but a stale `NotKernel` or `Retired` at a
/// recycled address permanently disables offload for a genuine kernel site,
/// which is the exact failure [`CompiledOffloadSite::Unresolved`] was added
/// to fix, arriving by a different road.
///
/// [`CompiledOffloadSiteEntry`] closes it: every entry carries the identity it
/// was resolved FOR, and a lookup that finds a different identity at the same
/// address treats the entry as absent and resolves afresh.
#[cfg(feature = "gpu-offload")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompiledOffloadSite {
    /// Not a kernel call. The overwhelming majority, and the reason this
    /// is resolved ONCE per site rather than per call.
    NotKernel,
    /// A kernel call that has declined `n` times in a row.
    Active(u32),
    /// Declined past the cap; the hook is never consulted here again.
    Retired,
    /// The target's declaring class was not loaded when this site was first
    /// resolved, so neither the registry nor the gate could answer. `n` is how
    /// many times we have come back; see [`OFFLOAD_SITE_UNRESOLVED_TRIES`].
    ///
    /// AUDIT 2026-09-06. Without this the first execution of a site decided
    /// `NotKernel` FOREVER, using a registry that could not yet know — and the
    /// class that same dispatch was about to resolve was very often the
    /// kernel's own. That is the "forward references" limitation in
    /// `runtime::offload_jit_gate`'s module docs, and it cost the device for
    /// the life of the process on any caller that got hot before its kernel
    /// branch was first taken.
    Unresolved(u8),
}

/// How many times a site whose target class is still unloaded is re-asked
/// before it is written off as [`CompiledOffloadSite::NotKernel`].
///
/// One retry would do in the ordinary case: the dispatch this helper falls
/// through to resolves and initializes the class, so the SECOND execution of
/// the site has a real answer. The cap exists for the case where that dispatch
/// throws — a site whose every call ends in `NoClassDefFoundError` must not
/// re-run the gate query forever.
#[cfg(feature = "gpu-offload")]
const OFFLOAD_SITE_UNRESOLVED_TRIES: u8 = 4;

/// Per-site offload state.
///
/// AUDIT 2026-09-04. This started as a name lookup on every compiled
/// dispatch -- `offload_hook::is_kernel(class, method, descriptor)`,
/// which allocates three `Box<str>` to build its key. Measured on
/// `GpuHookOverheadBench`, that put `small_ns_per_call` at **214 ns**
/// against 13.8 for the same loop without `--gpu`: the string work, not
/// the hook, and paid by every compiled static call in the program.
///
/// The name lookup now happens once, the first time a site is seen, and
/// every later call is one `usize` hash. The retirement streak lives in
/// the same entry, so a retired site costs that one lookup and nothing
/// else.
#[cfg(feature = "gpu-offload")]
fn compiled_offload_sites(
    // addr-keyed: not a heap address -- `JitInvokeInfo`, identity-checked per read.
) -> &'static parking_lot::RwLock<rustc_hash::FxHashMap<usize, CompiledOffloadSiteEntry>> {
    static T: std::sync::OnceLock<
        // addr-keyed: not a heap address -- `JitInvokeInfo`, identity-checked per read.
        parking_lot::RwLock<rustc_hash::FxHashMap<usize, CompiledOffloadSiteEntry>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// Everything about a `JitInvokeInfo` that says WHICH call site it is, taken
/// as plain machine words so it stays comparable after the `info` it came from
/// has been freed.
///
/// RT-6. The three name fields of a `JitInvokeInfo` are `&'static str`s that
/// point into the owning `CompiledMethod`'s storage. This captures their
/// ADDRESS and LENGTH and never their bytes: a stored identity is compared, at
/// most, against a live `info`'s fat-pointer fields, so nothing here ever
/// dereferences a pointer that may have been freed. Comparing the text instead
/// would be a use-after-free on precisely the path this exists to make safe.
///
/// What a pointer-identity comparison can and cannot tell apart:
///
/// * a DIFFERENT site at a recycled address — caught, because two compiles do
///   not produce the same three string addresses by accident. This is the
///   failure the entry-identity check exists for;
/// * the SAME method recompiled, whose `JitInvokeInfo` landed at the same
///   address and reuses the same interned names — not caught, and it does not
///   need to be: the decision (`NotKernel`, or a decline streak) is about that
///   method's target, which has not changed.
#[cfg(feature = "gpu-offload")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CompiledOffloadSiteIdentity {
    class_name: (usize, usize),
    method_name: (usize, usize),
    descriptor: (usize, usize),
    invoke_kind: u8,
    declaring_class_id: u32,
    /// The VM the decision was reached in (`SharedVm::vm_identity`, never
    /// reissued; 0 from [`Self::of`] alone). [`compiled_offload_sites`] is one
    /// map for the process, and a `JitInvokeInfo` address — and, through the
    /// synthetic direct-dispatch sites, a whole identity — can recur in another
    /// VM, whose registry, gate and class ids answered nothing here. Part of
    /// the identity, so another VM's row is treated as absent, like a recycled
    /// address's (interpreter round i1 wave 38, lane L6: per-VM state).
    vm_identity: usize,
}

#[cfg(feature = "gpu-offload")]
impl CompiledOffloadSiteIdentity {
    fn of(info: &JitInvokeInfo) -> Self {
        Self {
            class_name: (info.class_name.as_ptr() as usize, info.class_name.len()),
            method_name: (info.method_name.as_ptr() as usize, info.method_name.len()),
            descriptor: (info.descriptor.as_ptr() as usize, info.descriptor.len()),
            invoke_kind: info.invoke_kind,
            declaring_class_id: info.declaring_class_id,
            vm_identity: 0,
        }
    }

    /// [`Self::of`], in the VM `vm_identity`.
    fn of_in(vm_identity: usize, info: &JitInvokeInfo) -> Self {
        Self {
            vm_identity,
            ..Self::of(info)
        }
    }
}

/// One row of [`compiled_offload_sites`]: the decision, plus the identity it
/// was reached for.
///
/// The identity is not part of the KEY because the key has to stay one `usize`
/// hash — that is the whole point of the memo, which replaced a three-`Box<str>`
/// name lookup that cost 200 ns on every compiled static call. It is checked on
/// the hit instead: four word comparisons on a path that was already touching
/// this cache line.
#[cfg(feature = "gpu-offload")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CompiledOffloadSiteEntry {
    identity: CompiledOffloadSiteIdentity,
    state: CompiledOffloadSite,
}

/// Write one site's decision, stamped with the identity it was reached for.
///
/// The only writer. Going through it rather than `insert`ing at each of the
/// four update sites is what makes "an entry always carries the identity its
/// state was decided under" a property of the code rather than of four
/// remembered call sites — which is the shape of mistake RT-6 is.
#[cfg(feature = "gpu-offload")]
fn record_compiled_offload_site(
    site: usize,
    identity: CompiledOffloadSiteIdentity,
    state: CompiledOffloadSite,
) {
    compiled_offload_sites()
        .write()
        .insert(site, CompiledOffloadSiteEntry { identity, state });
}

/// Decode the JIT argument slots of a STATIC call into `Value`s.
///
/// One slot per declared parameter, including `long`/`double` -- the JIT
/// flattens its outgoing argument array rather than using JVM slot
/// widths, which is the same assumption `forward_jit_reference_args`
/// makes when it walks this descriptor.
///
/// `None` on any descriptor this cannot read exactly. Guessing would
/// hand the marshaller an argument list that does not match the kernel
/// signature, and the failure would be a wrong ANSWER on the device
/// rather than a refusal.
#[cfg(feature = "gpu-offload")]
///
/// # SAFETY
///
/// A non-zero reference slot is treated as a live heap pointer. That is
/// the same contract `forward_jit_reference_args` runs under one line
/// earlier in the caller, and these are the arguments it just forwarded.
unsafe fn decode_static_args(descriptor: &str, args: &[i64]) -> Option<Vec<cratonvm_types::Value>> {
    use cratonvm_types::{ObjectRef, Value};
    let bytes = descriptor.as_bytes();
    let mut p = bytes.iter().position(|&b| b == b'(')? + 1;
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0usize;
    while p < bytes.len() && bytes[p] != b')' {
        let raw = *args.get(i)?;
        let v = match bytes[p] {
            b'I' | b'Z' | b'B' | b'S' | b'C' => {
                p += 1;
                Value::Int(raw as i32)
            }
            b'J' => {
                p += 1;
                Value::Long(raw)
            }
            b'F' => {
                p += 1;
                Value::Float(f32::from_bits(raw as u32))
            }
            b'D' => {
                p += 1;
                Value::Double(f64::from_bits(raw as u64))
            }
            b'L' => {
                while p < bytes.len() && bytes[p] != b';' {
                    p += 1;
                }
                p = p.checked_add(1)?;
                if raw == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(raw as *mut u8)))
                }
            }
            b'[' => {
                while p < bytes.len() && bytes[p] == b'[' {
                    p += 1;
                }
                if p < bytes.len() && bytes[p] == b'L' {
                    while p < bytes.len() && bytes[p] != b';' {
                        p += 1;
                    }
                }
                p = p.checked_add(1)?;
                if raw == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(raw as *mut u8)))
                }
            }
            _ => return None,
        };
        out.push(v);
        i += 1;
    }
    // Every declared parameter consumed exactly one slot, and there were
    // exactly that many. A mismatch means this walk and the JIT disagree
    // about the argument layout, which is not something to offload on.
    if i != args.len() {
        return None;
    }
    Some(out)
}

/// Try to run this compiled `invokestatic` on the GPU.
///
/// `Some(ret)` when the device ran it and `ret` is what the compiled
/// caller should receive; `None` to fall through to the ordinary
/// dispatch below, which is always safe -- `try_dispatch` leaves the
/// operand stack and locals untouched on every refusal and on every
/// failure.
///
/// # SAFETY
///
/// `info` and `args` are the dispatch helper's own, already validated
/// and GC-forwarded. The thread borrow is the standard `jit_thread_mut`
/// scoped one.
#[cfg(feature = "gpu-offload")]
/// What one compiled static call site is, as far as offload is concerned.
///
/// The cheap answer first: `offload_hook`'s registry, populated as a side
/// effect of `offload_jit_gate` scanning CALLERS. A hit is the common path and
/// costs the three `Box<str>` `is_kernel` allocates, once per site.
///
/// AUDIT 2026-09-06: a MISS used to mean `NotKernel`, permanently. That is
/// wrong whenever the registry could not have known — the gate skips any call
/// target whose declaring class is not loaded at the moment the caller is
/// scanned (its "forward references" limitation), and the first execution of a
/// site is very often the thing that loads that very class. A caller that got
/// hot before its kernel branch was first taken therefore lost the device for
/// the rest of the run, silently, with the census printing zeroes.
///
/// So a miss now asks the gate itself:
///
/// * [`descriptor_could_ever_dispatch`] first, off `info.descriptor` alone —
///   no lock, no allocation, no bytecode. `try_dispatch` launches only `)V`, or
///   `)I`/`)J` for a reduction, and above `--gpu-min-work` 0 it needs an array
///   parameter. Nearly every compiled static site in a program fails this and
///   stops here.
/// * [`target_is_dispatchable_kernel`] for the few that pass — the identical
///   question `judge_target` asks during a caller scan. `None` means the class
///   is STILL unloaded, and the site is left [`CompiledOffloadSite::Unresolved`]
///   so the next call re-asks rather than being written off.
/// * A kernel it finds is `note_kernel`ed, which is what repairs the
///   compile-time doors too: `jit/src/lib.rs` and `jit/src/x64/bytecode_walk.rs`
///   read the same registry to decide whether to bind a static call directly or
///   inline it, and both of those are one-way once taken.
/// Does a registry miss get to ask the gate, or is it written off on the spot?
///
/// `CRATONVM_GPU_JIT_GATE_LATE_REGISTER=0` restores the pre-2026-09-06
/// behaviour, in which the FIRST execution of a compiled static call site
/// decided `NotKernel` permanently from a registry that could not yet know
/// about a forward-referenced kernel. Kept as a switch and not just as history:
/// it is the control arm for measuring what the fix is worth, on ONE binary --
/// see `test_classes/gpu/GpuForwardRef.java`, whose two arms differ only in
/// whether the caller gets hot before or after the kernel's class is loaded.
#[cfg(feature = "gpu-offload")]
fn offload_site_late_register() -> bool {
    // j27 wave 7 (lane T1IC): read per call (was a process `OnceLock`); a
    // site resolution is cold.
    cratonvm_types::flags::runtime_var("CRATONVM_GPU_JIT_GATE_LATE_REGISTER")
        .ok()
        .as_deref()
        != Some("0")
}

#[cfg(feature = "gpu-offload")]
fn resolve_offload_site(vm: &SharedVm, info: &JitInvokeInfo, tries: u8) -> CompiledOffloadSite {
    // This VM's kernels only (interpreter round i1 wave 40, lane L2).
    if cratonvm_jit::offload_hook::is_kernel(
        vm.vm_identity,
        info.class_name,
        info.method_name,
        info.descriptor,
    ) {
        return CompiledOffloadSite::Active(0);
    }
    if !offload_site_late_register() {
        return CompiledOffloadSite::NotKernel;
    }
    if !crate::runtime::offload_jit_gate::descriptor_could_ever_dispatch(
        info.descriptor,
        vm.config.gpu_min_work,
    ) {
        return CompiledOffloadSite::NotKernel;
    }
    match crate::runtime::offload_jit_gate::target_is_dispatchable_kernel(
        vm,
        info.class_name,
        info.method_name,
        info.descriptor,
    ) {
        Some(true) => {
            cratonvm_jit::offload_hook::note_kernel(
                vm.vm_identity,
                info.class_name,
                info.method_name,
                info.descriptor,
            );
            cratonvm_types::gpu_jit_gate_census::note_late_registered(format!(
                "{}.{}{}",
                info.class_name, info.method_name, info.descriptor
            ));
            CompiledOffloadSite::Active(0)
        }
        Some(false) => CompiledOffloadSite::NotKernel,
        None if tries + 1 >= OFFLOAD_SITE_UNRESOLVED_TRIES => CompiledOffloadSite::NotKernel,
        None => CompiledOffloadSite::Unresolved(tries + 1),
    }
}

#[cfg(feature = "gpu-offload")]
unsafe fn try_compiled_offload(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    site: usize,
    args: &[i64],
) -> Option<i64> {
    use crate::runtime::offload::DispatchOutcome;
    use cratonvm_types::Value;

    let cap = compiled_offload_giveup_after();

    // One `usize` hash on the hot path. Everything below the hit runs once per
    // SITE, not once per call.
    //
    // RT-6: a hit is believed only when the entry's recorded identity still
    // matches this `info`. `site` is a `Box<JitInvokeInfo>` address inside a
    // retired artifact's `_jit_invoke_infos`, so the allocator can hand it to
    // an unrelated compile; without this check an inherited `NotKernel` or
    // `Retired` would permanently disable offload for a genuine kernel at the
    // new site. A mismatch is treated exactly like a miss — resolve afresh and
    // overwrite, which also keeps the map's size tied to live addresses rather
    // than to every address the process ever used.
    let identity = CompiledOffloadSiteIdentity::of_in(vm.vm_identity, info);
    let state = {
        let known = compiled_offload_sites()
            .read()
            .get(&site)
            .copied()
            .filter(|e| e.identity == identity)
            .map(|e| e.state);
        match known {
            Some(CompiledOffloadSite::Unresolved(tries)) => {
                // Come back: the dispatch that followed our last visit
                // resolved the class, so the gate can answer now.
                let st = resolve_offload_site(vm, info, tries);
                record_compiled_offload_site(site, identity, st);
                st
            }
            Some(st) => st,
            None => {
                let st = resolve_offload_site(vm, info, 0);
                record_compiled_offload_site(site, identity, st);
                st
            }
        }
    };
    match state {
        CompiledOffloadSite::NotKernel
        | CompiledOffloadSite::Retired
        | CompiledOffloadSite::Unresolved(_) => return None,
        CompiledOffloadSite::Active(_) => {}
    }

    cratonvm_types::gpu_compiled_offload_census::note_considered();

    let Some(values) = (unsafe { decode_static_args(info.descriptor, args) }) else {
        cratonvm_types::gpu_compiled_offload_census::note_undecodable();
        return None;
    };
    let Some((thread, _guard)) = jit_thread_mut() else {
        cratonvm_types::gpu_compiled_offload_census::note_undecodable();
        return None;
    };

    // `frame_idx` reaches only a `tracing::debug!` inside `try_dispatch`,
    // and `tracing` is built with `max_level_info`, so it is compiled out
    // of every release binary. There is no compiled frame index to give.
    let outcome = crate::runtime::offload::try_dispatch(
        vm,
        thread,
        0,
        info.class_name,
        info.method_name,
        info.descriptor,
        &values,
    );
    let Ok(outcome) = outcome else {
        cratonvm_types::gpu_compiled_offload_census::note_undecodable();
        return None;
    };

    match outcome {
        DispatchOutcome::Handled => {
            cratonvm_types::gpu_compiled_offload_census::note_offloaded();
            record_compiled_offload_site(site, identity, CompiledOffloadSite::Active(0));
            // A `)V` kernel: the results reached Java through the D2H
            // writeback into the output array, and the caller expects
            // nothing on its operand stack.
            Some(0)
        }
        DispatchOutcome::HandledWithValue(v) => {
            cratonvm_types::gpu_compiled_offload_census::note_offloaded();
            record_compiled_offload_site(site, identity, CompiledOffloadSite::Active(0));
            match v {
                Value::Int(i) => Some(i as i64),
                Value::Long(l) => Some(l),
                // Only `)I`/`)J` reductions are transparently dispatched,
                // so anything else here means the dispatcher and this
                // decode disagree. Fall through rather than invent a
                // return value.
                _ => None,
            }
        }
        _ => {
            cratonvm_types::gpu_compiled_offload_census::note_declined();
            if cap > 0 {
                let CompiledOffloadSite::Active(n) = state else {
                    return None;
                };
                let n = n.saturating_add(1);
                let next = if n > cap {
                    cratonvm_types::gpu_compiled_offload_census::note_site_retired();
                    CompiledOffloadSite::Retired
                } else {
                    CompiledOffloadSite::Active(n)
                };
                record_compiled_offload_site(site, identity, next);
            }
            None
        }
    }
}

#[cfg(all(test, feature = "gpu-offload"))]
mod compiled_offload_site_tests {
    use super::*;

    fn info(class: &'static str, method: &'static str, desc: &'static str) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: class,
            method_name: method,
            descriptor: desc,
            num_jit_args: 1,
            return_type: b'V',
            invoke_kind: 3,
            declaring_class_id: 0,
            owner_class_id: 0,
        }
    }

    /// RT-6's whole point. A `JitInvokeInfo` lives in a `Box` inside the
    /// artifact's `_jit_invoke_infos`; when the artifact retires that box is
    /// freed and the allocator may hand the same address to an unrelated
    /// compile. The memo is keyed on the address, so the new site inherits the
    /// old one's decision — and a stale `NotKernel` or `Retired` permanently
    /// disables offload for a genuine kernel, which is the exact failure the
    /// `Unresolved` retry was added to fix, arriving by a different road.
    ///
    /// The identity is what tells the two apart, and it must do so without
    /// ever dereferencing the old site's strings, which is why it holds
    /// addresses and lengths.
    #[test]
    fn two_different_sites_never_share_an_identity() {
        let a = info("com/example/A", "kernel", "([I)V");
        let b = info("com/example/B", "kernel", "([I)V");
        assert_ne!(
            CompiledOffloadSiteIdentity::of(&a),
            CompiledOffloadSiteIdentity::of(&b),
            "two sites with different declaring classes must not be mistaken \
             for one another at a recycled address"
        );

        // Same three names, different dispatch kind: still a different site,
        // because `invoke_kind` selects a different resolution entirely.
        let mut c = info("com/example/A", "kernel", "([I)V");
        c.invoke_kind = 0;
        assert_ne!(
            CompiledOffloadSiteIdentity::of(&a),
            CompiledOffloadSiteIdentity::of(&c)
        );

        // And the loader-disambiguating field, which is the one a name-only
        // comparison would miss: two loaders can each define the same class.
        let mut d = info("com/example/A", "kernel", "([I)V");
        d.declaring_class_id = 7;
        assert_ne!(
            CompiledOffloadSiteIdentity::of(&a),
            CompiledOffloadSiteIdentity::of(&d)
        );

        // And the VM: the map is one per process, and one `JitInvokeInfo`
        // (a synthetic site's, or a recycled address) can meet two VMs.
        assert_ne!(
            CompiledOffloadSiteIdentity::of_in(1, &a),
            CompiledOffloadSiteIdentity::of_in(2, &a),
            "one VM's offload decision must not answer for another VM"
        );
    }

    /// The other direction, which is what keeps the memo a memo: re-deriving
    /// the identity of the SAME live site must give the same answer, or every
    /// call would miss and pay the three-`Box<str>` name lookup this cache
    /// exists to have removed (214 ns/call against 13.8 without it).
    #[test]
    fn one_site_keeps_its_identity_across_lookups() {
        let a = info("com/example/A", "kernel", "([I)V");
        assert_eq!(
            CompiledOffloadSiteIdentity::of(&a),
            CompiledOffloadSiteIdentity::of(&a)
        );
    }

    /// A recycled address must not carry a decision forward, and the check
    /// that stops it is on the READ. Recorded under one identity, read back
    /// under another: the entry is present at that key and must still be
    /// treated as absent.
    #[test]
    fn a_recycled_address_does_not_inherit_the_old_sites_decision() {
        // A key no other test can collide with: this map is process-global.
        let site = 0x5254_3600_usize;
        let old = info("com/example/Retired", "kernel", "([I)V");
        let new = info("com/example/Fresh", "kernel", "([I)V");
        let old_id = CompiledOffloadSiteIdentity::of(&old);
        let new_id = CompiledOffloadSiteIdentity::of(&new);

        record_compiled_offload_site(site, old_id, CompiledOffloadSite::Retired);

        let read_back = |want: CompiledOffloadSiteIdentity| {
            compiled_offload_sites()
                .read()
                .get(&site)
                .copied()
                .filter(|e| e.identity == want)
                .map(|e| e.state)
        };

        assert_eq!(
            read_back(old_id),
            Some(CompiledOffloadSite::Retired),
            "the site that recorded the decision must still see it"
        );
        assert_eq!(
            read_back(new_id),
            None,
            "a different site at the same recycled address must see a MISS, \
             not the retired decision it never made"
        );
    }
}

/// The class a statically bound (`invoke_kind == 1`) site's `class_name`
/// names when the compile SUBSTITUTED that name for the constant-pool class
/// (`JitInvokeInfo::owner_class_id`: the declaring class of a private or
/// `final` target, a super call's selection start): the recorded id, while it
/// still names a loaded class called `class_name`. `None` for every other
/// site, and for an id the store no longer holds under that name; such a site
/// resolves `class_name` in the caller's loader as before.
///
/// Why the id and not the name: the substituted name is not one the caller's
/// constant pool references, so resolving it through the caller's loader asks
/// that loader for a class its bytecode never names
/// (`java/lang/AbstractStringBuilder` for `StringBuilder.length()`), and can
/// answer another loader's copy of the ancestor
/// (`interpreter-L2-substituted-owner-dispatch-resolves-the-declaring-class-by-name`).
///
/// JIT round 21 lane reflect4 (REFLECT3-21-2): also an `invoke_kind == 3`
/// site with an `owner_class_id`, which only the hidden-owner leaf of a
/// compiled `invokeBasic` / `linkToStatic` link carries
/// ([`mh_bind_hidden_leaf_info`]; no compile mints a static site with one): a
/// hidden class (a spun `LambdaForm$MH`) is never found by name, so every
/// service of the compiled call that asks this first resolves it by id.
fn jit_substituted_owner(vm: &SharedVm, info: &JitInvokeInfo) -> Option<ClassId> {
    if !matches!(info.invoke_kind, 1 | 3) || info.owner_class_id == 0 {
        return None;
    }
    let owner = ClassId::new(info.owner_class_id);
    let cm = vm.classes.class_manager.read();
    cm.get_class(owner)
        .is_some_and(|class| &*class.name == info.class_name)
        .then_some(owner)
}

/// [`jit_substituted_owner`] for the hidden-owner static leaf only
/// (`invoke_kind == 3` with an `owner_class_id`; JIT round 21 lane reflect4).
/// Every ordinary static site answers `None` at the first compare.
fn jit_hidden_static_owner(vm: &SharedVm, info: &JitInvokeInfo) -> Option<ClassId> {
    if info.invoke_kind != 3 || info.owner_class_id == 0 {
        return None;
    }
    jit_substituted_owner(vm, info)
}

/// [`jit_callee_name_is_another_loaders_copy`] for a site with a substituted
/// owner: `true` when the flat binary-name map answers a different class for
/// `class_name` than the one the substitution found, so a compiled body found
/// or compiled BY NAME would be the other copy's. Same cheapest-first gates.
fn jit_substituted_owner_is_another_copy(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    if info.invoke_kind != 1
        || info.owner_class_id == 0
        || !cratonvm_native_builtins::classloader::any_defining_loader_registered(
            &vm.loader_identity,
        )
    {
        return false;
    }
    substituted_owner_differs_from_by_name(&vm.classes.class_manager.read(), info)
}

/// The decision inside [`jit_substituted_owner_is_another_copy`], without the
/// process-wide latch (split out for the same reason as
/// [`loader_faithful_static_owner`]).
fn substituted_owner_differs_from_by_name(
    cm: &crate::classloading::ClassManager,
    info: &JitInvokeInfo,
) -> bool {
    if info.invoke_kind != 1 || info.owner_class_id == 0 {
        return false;
    }
    if !matches!(
        cm.classify_loaded_name(info.class_name),
        cratonvm_classloading::NameResolution::Ambiguous { .. }
    ) {
        return false;
    }
    cm.get_loaded_class_id(info.class_name) != Some(ClassId::new(info.owner_class_id))
}

/// nio21 wave 20 L1: the dispatch helper's door to
/// [`crate::runtime::exceptions::jit_new_throwable_direct`] for an
/// `invokespecial <init>` site with `args` (receiver first, forwarded). `true`
/// when the constructor's effect is complete; `false` (nothing changed) sends
/// the call down the ordinary route. A cheap textual screen first: only the
/// `(String)V` and `()V` constructors are candidates.
unsafe fn try_throwable_init_direct_at_dispatch(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> bool {
    let expected_args = match info.descriptor {
        "(Ljava/lang/String;)V" => 2,
        "()V" => 1,
        _ => return false,
    };
    if args.len() != expected_args || args[0] == 0 {
        return false;
    }
    // Cast: a JIT argument word holding an object reference is its address.
    let receiver = ObjectRef::from_raw(args[0] as usize as *mut u8);
    let message = args
        .get(1)
        .copied()
        .filter(|&m| m != 0)
        // Cast: as above.
        .map(|m| ObjectRef::from_raw(m as usize as *mut u8));
    let Some((thread, _guard)) = jit_thread_mut() else {
        return false;
    };
    crate::runtime::exceptions::jit_new_throwable_direct(
        vm,
        thread,
        info.class_name,
        info.descriptor,
        receiver,
        message,
    )
}

/// gci i12/mf (`gci-i8dm-jni-native-call-sites-re-resolve-by-name-every-call-FIXED-20261017`,
/// the JIT's virtual arms): the hit half of the compiled virtual / interface
/// JNI call-site rows (`vm_exec`'s "gci i12/mf" block), asked by the three
/// by-name tails a compiled virtual call of a table-bound JNI native reaches:
/// `jit_invoke_dispatch_body`'s `0 | 2` arm and `jit_invoke_virtual_mic_body`'s
/// entryless-hit and miss arms. `None` for an array or class-0 receiver (no
/// row holds one), with the memo off (one byte compare), and for a receiver
/// class other than the row's.
#[inline]
fn jit_jni_vsite_lookup(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_ptr: usize,
    receiver: Option<ClassId>,
) -> Option<crate::vm::vm_exec::JniJitVSiteHit> {
    let receiver = receiver.filter(|cid| cid.as_u32() != 0)?;
    crate::vm::vm_exec::jni_jit_vsite_memo_lookup(
        vm,
        thread,
        ClassId::new(info.declaring_class_id),
        crate::vm::vm_exec::jni_jit_vsite(info_ptr),
        (info.class_name, info.method_name, info.descriptor),
        receiver,
    )
}

/// gci i12/mf: a compiled virtual call-site hit, as the tail it stands in for
/// ends: the door on the receiver's class, then the tail's S111r12 rescue on a
/// `NoSuchMethodError` miss named for `rescue_class` (the blind arm asks the
/// receiver's class name, the MIC arms the dispatch target's) when that is not
/// the site's class, with the arguments re-read from pins as the tail keeps them;
/// every other error goes to `handle_jit_dispatch_error`.
fn jit_jni_vsite_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    hit: &crate::vm::vm_exec::JniJitVSiteHit,
    values: &mut [Value],
    rescue_class: &str,
) -> i64 {
    let rescue_differs = !info.class_name.is_empty() && rescue_class != info.class_name;
    let result = match crate::vm::vm_exec::jni_jit_vsite_memo_call(
        vm,
        thread,
        hit,
        values,
        rescue_differs,
    ) {
            Ok(v) => v,
            Err(e)
                if rescue_differs
                    && is_dispatch_no_such_method_miss(
                        &e,
                        rescue_class,
                        info.method_name,
                        info.descriptor,
                    ) =>
            {
                match crate::vm::invoke_or_native(
                    vm,
                    thread,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    values,
                ) {
                    Ok(v) => v,
                    Err(e2) => return handle_jit_dispatch_error(vm, thread, e2, info),
                }
            }
            Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
        };
    match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    }
}

/// JIT round 20 (lane stash;
/// `r19-lean-native-door-runs-outside-the-helper-panic-scope-FIXED-20261012.md`): a
/// door an `extern "C"` dispatch helper asks BEFORE its `contain`ed body (the
/// SBR5 lean native door, the FFM direct door) runs under the same panic scope.
/// A panic in a door (a header guard, the return coercion, building the
/// throwable in `handle_jit_dispatch_error`) used to unwind out of the
/// `extern "C"` shell and abort the process; contained, it becomes the
/// helper's pending `InternalError` and the `i64::MIN` sentinel, as a panic in
/// the body does. The non-panicking path costs the scope counter's two
/// thread-local updates (`contain_compile_panic`); `catch_unwind` itself is
/// free on x64. Correctness, unconditional.
#[inline(always)]
fn contain_door(
    name: &'static str,
    vm_ptr: i64,
    door: impl FnOnce() -> Option<i64>,
) -> Option<i64> {
    contain(name, OnPanic::Throw { vm_ptr }, Some(i64::MIN), door)
}

thread_local! {
    /// `(vm_identity, sender, receiver class)` of the last pair
    /// [`iface_special_receiver_guard`] checked and passed on this thread. A
    /// class's supertypes never change, so a pass is permanent for the pair.
    static IFACE_SPECIAL_GUARD_PASSED: Cell<(usize, u32, u32)> = const { Cell::new((0, 0, 0)) };
    /// `(vm_identity, sender)` of the last statically bound site's holder the
    /// guard found to be a CLASS (never checked; the verifier proves its
    /// receivers).
    static IFACE_SPECIAL_GUARD_CLASS_SENDER: Cell<(usize, u32)> = const { Cell::new((0, 0)) };
}

/// Interpreter round i7 wave 136, lane L2
/// (`CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, performance, default OFF;
/// page `i135-L6-trait-static-accessors-run-uncached-and-uncompiled`): the
/// compiled twin of the interpreter's receiver check for an `invokespecial`
/// whose sender is an interface (`invoke::interface_special_receiver_refusal`,
/// HotSpot's `LinkResolver::runtime_resolve_special_method`). With the switch
/// on, a method-entry door compiles such a site whose receiver is not proven
/// to be `this` (Scala's `static T.m$(T)`, `aload_0; invokespecial T.m`) as a
/// call through `jit_invoke_dispatch` (`invoke_kind == 1`, no direct bind, no
/// splice: `jit_bridge::iface_special_guarded_target`); this is that call's
/// check. `Some(i64::MIN)` with the `IllegalAccessError` pending when the
/// receiver's class is neither the sender nor a subtype of it; `None` to go
/// on dispatching.
///
/// The sender is `info.declaring_class_id` (the class whose method holds the
/// site). Asked only for a site that names a member some unproven
/// `invokespecial` of the sender names
/// (`jit_bridge::interface_special_receivers_proven`), so a devirtualized
/// kind-1 `invokevirtual` (`Object.getClass()` from a default method) is never
/// checked. A class sender is remembered per thread and costs one compare; a
/// passing `(sender, receiver class)` pair likewise.
///
/// # Safety
/// Called from [`jit_invoke_dispatch_body`] with its forwarded argument words;
/// `jit_thread_mut` is that helper's own thread contract.
unsafe fn iface_special_receiver_guard(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<i64> {
    let message = iface_special_receiver_refusal_message(vm, info, args)?;
    let (thread, _guard) = jit_thread_mut()?;
    // Interpreter round i7 wave 137, lane L2: built as the other compiled
    // throws are, with the compiled frames snapshotted HERE (the sending
    // `T.m$` is the innermost one, at its call's published safepoint), LIVE:
    // every frame the snapshot names is still on the machine stack and nothing
    // ran since. The capture then takes it in place of its own compiled walk,
    // so the trace's top frame is the sender's, as HotSpot's (the IAE is
    // raised AT the `invokespecial`, `LinkResolver::runtime_resolve_special_method`).
    note_trap_snapshot_bound(vm);
    let snapshot = if trap_frame_snapshot_enabled() {
        let frames = trap_snapshot_walk();
        (!frames.is_empty()).then_some(frames)
    } else {
        None
    };
    match crate::runtime::exceptions::create_exception_object_with_trap_frames_live(
        vm,
        thread,
        "java/lang/IllegalAccessError",
        Some(&message),
        snapshot,
        true,
    ) {
        Ok(exc) => set_jit_pending_exception(thread, exc),
        // The check FAILED: never fall through to the arms that call the
        // callee (the w136 form's `.ok()?` did, on an allocation failure).
        // The construction's own error (an `OutOfMemoryError`, a linkage
        // error) is what the call raises instead.
        Err((err, _)) => return Some(handle_jit_dispatch_error(vm, thread, err, info)),
    }
    // The call failed before the callee was entered, as a linkage error the
    // helper raises does: its handlers do not apply
    // ([`note_dispatch_error_escape`], which `handle_jit_dispatch_error`
    // writes for its own re-posts).
    note_dispatch_error_escape(true, info.method_name, info.descriptor);
    Some(i64::MIN)
}

/// [`iface_special_receiver_guard`]'s question without the raise: the
/// `IllegalAccessError` message when the receiver word `args[0]` fails the
/// interface sender's receiver check, `None` when it passes or is not checked.
/// Interpreter round i7 wave 137, lane L6 (review of wave 136 L2): also asked
/// by the bound-site inline cache's publication
/// ([`publish_bound_site_entry`]), which publishes BEFORE its dispatch call
/// runs the guard, keyed by the receiver's class; a refused receiver class
/// published there would hit the cache on its next call and skip the guard.
///
/// # Safety
/// `args` holds the call's forwarded argument words, receiver first.
unsafe fn iface_special_receiver_refusal_message(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<String> {
    let sender = info.declaring_class_id;
    if sender == 0
        || info.method_name == "<init>"
        || !crate::runtime::interpreter::invoke::interface_special_receiver_check_enabled(vm)
    {
        return None;
    }
    let vm_id = vm.vm_identity;
    if IFACE_SPECIAL_GUARD_CLASS_SENDER.with(|c| c.get()) == (vm_id, sender) {
        return None;
    }
    let raw = *args.first()? as u64;
    // The null receiver was answered (NPE) before this guard; anything that
    // is not an object address is left to the arms below, as the object
    // native cache leaves it.
    if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let receiver = vm.mem.heap.is_object_address(raw as usize)?;
    let receiver_class = vm.mem.heap.class_id_of(receiver);
    // Interpreter round i7 wave 136, lane L6 (cross-lane): an ARRAY receiver's
    // header holds its COMPONENT's class id, so it never reads or fills the
    // passed memo (a `C[]` would hit the entry `C` filled), and it is checked
    // through the object form below, which asks the array class itself.
    let is_array = vm.mem.heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array;
    if !is_array
        && IFACE_SPECIAL_GUARD_PASSED.with(|c| c.get()) == (vm_id, sender, receiver_class.as_u32())
    {
        return None;
    }
    let sender_id = ClassId::new(sender);
    let checked = {
        let cm = vm.classes.class_manager.read();
        match cm.get_class(sender_id) {
            None => false,
            Some(class) if !class.is_interface() => {
                IFACE_SPECIAL_GUARD_CLASS_SENDER.with(|c| c.set((vm_id, sender)));
                return None;
            }
            Some(_) => !crate::runtime::interpreter::jit_bridge::interface_special_receivers_proven(
                &cm,
                sender_id,
                info.class_name,
                info.method_name,
                info.descriptor,
            ),
        }
    };
    if !checked {
        return None;
    }
    let Some(message) =
        crate::runtime::interpreter::invoke::interface_special_receiver_refusal(
            vm, sender_id, receiver,
        )
    else {
        if !is_array {
            IFACE_SPECIAL_GUARD_PASSED.with(|c| c.set((vm_id, sender, receiver_class.as_u32())));
        }
        return None;
    };
    Some(message)
}

/// Does [`iface_special_receiver_guard`] check the calls of site `info` (an
/// `invokespecial` whose sender is an interface and whose receiver is not
/// proven to be `this`), with the receiver left out? Asked by the caches that
/// answer a kind-1 site without a receiver-class guard and AHEAD of the
/// guard (the lean native door, [`lean_native_site_fill`]), which must not
/// file such a site. Undecidable (the class manager busy) answers `true`, so
/// the site stays off the cache. Interpreter round i7 wave 138, lane L2. Cold.
fn iface_special_site_is_guarded(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    if info.invoke_kind != 1 || !vm.config.jit_iface_special_receiver_guard {
        return false;
    }
    let sender = info.declaring_class_id;
    if sender == 0
        || info.method_name == "<init>"
        || !crate::runtime::interpreter::invoke::interface_special_receiver_check_enabled(vm)
    {
        return false;
    }
    let Some(cm) = vm.classes.class_manager.try_read() else {
        return true;
    };
    let sender_id = ClassId::new(sender);
    match cm.get_class(sender_id) {
        Some(class) if class.is_interface() => {
            !crate::runtime::interpreter::jit_bridge::interface_special_receivers_proven(
                &cm,
                sender_id,
                info.class_name,
                info.method_name,
                info.descriptor,
            )
        }
        _ => false,
    }
}

/// `(vm_identity, info address)` of a compiled call site
/// [`jit_site_access_guard`] has judged. An address alone does not name a
/// site: a retired body's `JitInvokeInfo` and name strings go back to the
/// allocator and a recompiled (or redefined) caller can get the same
/// addresses back for ANOTHER reference
/// (`docs/internal/fixed-bugs/t1-V1-site-access-memo-keys-alias-after-reuse-FIXED-20261026.md`), so every
/// memo hit is confirmed against the reference's TEXT ([`JitSiteAccessEntry`]).
type JitSiteAccessKey = (usize, usize);

/// What an admitted site named: the caller class id, the invoke kind and the
/// reference's text. Equal entries name the same symbolic reference of the
/// same class (a redefinition keeps the class id and its loader, and one
/// loader answers one name with one class, JVMS 5.3.4), so they share one
/// verdict whatever body or class version carries the site.
#[derive(Clone, Debug, PartialEq, Eq)]
struct JitSiteAccessEntry {
    caller: u32,
    kind: u8,
    class_name: Box<str>,
    method_name: Box<str>,
    descriptor: Box<str>,
}

impl JitSiteAccessEntry {
    fn of(info: &JitInvokeInfo) -> Self {
        Self {
            caller: info.declaring_class_id,
            kind: info.invoke_kind,
            class_name: info.class_name.into(),
            method_name: info.method_name.into(),
            descriptor: info.descriptor.into(),
        }
    }

    fn names(&self, info: &JitInvokeInfo) -> bool {
        self.caller == info.declaring_class_id
            && self.kind == info.invoke_kind
            && *self.class_name == *info.class_name
            && *self.method_name == *info.method_name
            && *self.descriptor == *info.descriptor
    }
}

thread_local! {
    /// The sites of this thread [`jit_site_access_guard`] admitted.
    static JIT_SITE_ACCESS_PASSED: std::cell::RefCell<
        std::collections::HashMap<JitSiteAccessKey, JitSiteAccessEntry>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    /// `(code-cache generation, key)` of the last admitted (or confirmed)
    /// site: the common repeat costs one atomic load and one compare. Valid
    /// only within the generation it was set in: freeing a `JitInvokeInfo`
    /// takes an invalidation, which advances the generation
    /// (`cratonvm_jit::jit_cache_generation`), so a recycled address cannot
    /// match it.
    static JIT_SITE_ACCESS_LAST: Cell<(u64, JitSiteAccessKey)> = const { Cell::new((0, (0, 0))) };
}

/// t1 wave 2, lane P (`CRATONVM_JIT_SITE_ACCESS_CHECK`, correctness, default
/// ON, `=0` kill switch): JVMS §5.4.3 / §5.4.4 for a compiled call site whose
/// method reference the compile door could not judge.
///
/// The door peeks at every invoke site (`jit_bridge::CpResolvers::invoke`)
/// and leaves an `IllegalAccessError` site to the interpreter, but a site
/// whose OWNER no loader had loaded when the method was compiled resolves
/// "uncached, unchecked" (`invoke::resolve_method_metadata_noting`: the check
/// waits for the resolution after the owner's load), and the compiled code
/// then reaches the callee by NAME through this helper or the inline-cache
/// miss, which load the owner and call it without that second resolution.
/// So a class of a child loader calling a package-private class (or a
/// package-private / protected member) of its parent's same-named package ran
/// the call where HotSpot throws `IllegalAccessError: failed to access class
/// T1JiteaRows$Late4 from class T1JiteaRows$Holder (...)`
/// (`tools/probes/t1/T1JiteaRows.java` row `typecheck-loaders`; the focused
/// probe `tools/probes/t1/T1PRuntimePackageAccess.java`, `AccJit.fresh*`).
/// `--nojit` was right: `execute_invokestatic` resolves again after loading.
///
/// Here, at a site's first compiled execution on this thread, the site is
/// resolved as the interpreter resolves it: the caller's constant-pool entry
/// for the site's `(class, name, descriptor)` is found
/// ([`jit_site_cp_method_ref`]), the owner is resolved through the caller's
/// loader for every invoke kind (`resolve_class_entry_loader_aware`, the
/// `CONSTANT_Class` door; `invokestatic` only before wave 3), and the reference is resolved (`resolve_method_ref_settling`, which
/// runs the class and member checks against the owner in the caller's
/// namespace). An `IllegalAccessError` is settled against the entry's record
/// (`settle_method_resolution_failure`: later executions rethrow it, JVMS
/// §5.4.3) and raised at the call; anything else is left to the dispatch as
/// before. A site with no such entry (a synthetic or substituted site) is not
/// judged. The arguments are pinned across the loads and written back.
///
/// `Some(sentinel)` when the call must not run (its exception pending);
/// `None` to dispatch. Admitted sites are remembered per thread, so the steady
/// state is one thread-local compare.
///
/// # Safety
/// The dispatch helpers' contract: `vm_ptr` / `info_ptr` are the live VM and
/// site, `args_ptr` / `num_args` the compiled caller's argument words; no
/// `jit_thread_mut` borrow may be live.
#[inline]
unsafe fn jit_site_access_guard(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> Option<i64> {
    if vm_ptr == 0 || info_ptr == 0 {
        return None;
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    if info.declaring_class_id == 0 || info.owner_class_id != 0 || info.invoke_kind > 3 {
        return None;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let key: JitSiteAccessKey = (vm.vm_identity, info_ptr as usize);
    let generation = cratonvm_jit::jit_cache_generation();
    if JIT_SITE_ACCESS_LAST.with(Cell::get) == (generation, key) {
        return None;
    }
    if jit_site_access_admitted(key, info) {
        JIT_SITE_ACCESS_LAST.with(|last| last.set((generation, key)));
        return None;
    }
    jit_site_access_guard_slow(vm, info, key, args_ptr, num_args)
}

/// [`jit_site_access_guard`]'s repeat test, outside the helpers' panic scope:
/// the site this thread admitted last, in this code-cache generation (one
/// atomic load and one thread-local compare; it cannot panic). Measured in
/// wave 3 (`T1PDispatchBench`, doubling the guard): the guard behind the
/// panic scope cost ~2 ns per `jit_invoke_dispatch` and ~4 ns per inline-cache
/// miss.
#[inline(always)]
unsafe fn jit_site_access_repeat(vm_ptr: i64, info_ptr: i64) -> bool {
    vm_ptr != 0
        && JIT_SITE_ACCESS_LAST.with(Cell::get)
            == (
                cratonvm_jit::jit_cache_generation(),
                (
                    (*(vm_ptr as *const SharedVm)).vm_identity,
                    info_ptr as usize,
                ),
            )
}

/// Does this thread's memo admit `info` at `key`? A hit whose text names
/// another reference (a recycled address) is not an admission.
fn jit_site_access_admitted(key: JitSiteAccessKey, info: &JitInvokeInfo) -> bool {
    JIT_SITE_ACCESS_PASSED.with(|passed| {
        passed
            .borrow()
            .get(&key)
            .is_some_and(|entry| entry.names(info))
    })
}

/// Remember an admitted site ([`jit_site_access_guard`]). Bounded: a thread
/// that has judged a very large number of sites starts over.
fn jit_site_access_admit(key: JitSiteAccessKey, info: &JitInvokeInfo) {
    JIT_SITE_ACCESS_PASSED.with(|passed| {
        let mut passed = passed.borrow_mut();
        if passed.len() >= 1 << 16 {
            passed.clear();
        }
        passed.insert(key, JitSiteAccessEntry::of(info));
    });
    let generation = cratonvm_jit::jit_cache_generation();
    JIT_SITE_ACCESS_LAST.with(|last| last.set((generation, key)));
}

/// The `(Methodref index, its CONSTANT_Class index)` of `caller`'s pool that
/// names `info`'s `(class, name, descriptor)`, or `None` (no such entry: the
/// site's names are not the pool's). Cold: once per site and thread.
fn jit_site_cp_method_ref(
    vm: &SharedVm,
    caller: ClassId,
    info: &JitInvokeInfo,
) -> Option<(u16, u16)> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let cm = vm.classes.class_manager.read();
    let pool = &cm.get_class(caller)?.constant_pool;
    (1..u16::try_from(pool.len()).unwrap_or(u16::MAX)).find_map(|index| {
        let (class_index, nat_index) = match pool.get(index)? {
            ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            }
            | ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            } => (*class_index, *name_and_type_index),
            _ => return None,
        };
        (pool.get_class_name(class_index) == Some(info.class_name)
            && pool.get_name_and_type(nat_index) == Some((info.method_name, info.descriptor)))
        .then_some((index, class_index))
    })
}

/// [`jit_site_access_guard`]'s first look at a site on this thread.
#[cold]
unsafe fn jit_site_access_guard_slow(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    key: JitSiteAccessKey,
    args_ptr: i64,
    num_args: i64,
) -> Option<i64> {
    use crate::error::{LinkageError, MethodCallFailed, VmError};
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SITE_ACCESS_CHECK") {
        jit_site_access_admit(key, info);
        return None;
    }
    let caller = ClassId::new(info.declaring_class_id);
    let Some((cp_idx, class_idx)) = jit_site_cp_method_ref(vm, caller, info) else {
        jit_site_access_admit(key, info);
        return None;
    };
    // A cached resolution passed the checks when it was filled.
    let resolver = crate::runtime::resolve::MemberResolver::new(vm);
    if matches!(
        resolver.probe_method_ref(resolver.scope(caller), cp_idx),
        crate::runtime::resolve::CacheProbe::Hit(_)
    ) {
        jit_site_access_admit(key, info);
        return None;
    }
    let (thread, _guard) = jit_thread_mut()?;
    let args: &[i64] = if args_ptr == 0 || num_args <= 0 {
        &[]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };
    let pins = CompileArgPins::pin_raw(vm, thread, info, args);
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let refusal: Option<MethodCallFailed> = 'judge: {
        if let Some(recorded) =
            crate::runtime::interpreter::constants::recorded_member_owner_failure(
                vm, thread, caller, cp_idx,
            )
        {
            break 'judge Some(recorded);
        }
        // The reference's `CONSTANT_Class` entry is resolved through the
        // caller's loader first, as HotSpot's `LinkResolver::resolve_klass`
        // does for every invoke kind (`execute_invokestatic` loads its owner;
        // the receiver doors drive the caller's loader for an owner it has not
        // answered). Wave 3 (residual 1 of the t1-P page): for an
        // `invokevirtual` / `invokeinterface` / `invokespecial` whose owner the
        // caller's USER loader had not answered yet, the resolution below was
        // "unverified" (never enforced) and the site was admitted for good
        // (`T1PRuntimePackageAccess` `AccRecv.*Call`).
        //
        // A failed load (`t1-V1` page, secondary): under `--jdk-only` it is
        // the resolution's error, recorded against the class entry and raised
        // here, as `execute_invokestatic` records an owner failure; the
        // dispatch would otherwise ask the loader a second time. Under
        // `--compatible` the interpreter falls to the flat route after a
        // class-entry refusal, so the dispatch keeps that route.
        if !info.class_name.starts_with('[') {
            if let Err(failure) =
                crate::runtime::interpreter::constants::resolve_class_entry_loader_aware(
                    vm,
                    thread,
                    (caller, class_idx),
                    info.class_name,
                    fill_as_of,
                )
            {
                if !vm.config.is_jdk_only() {
                    break 'judge None;
                }
                let error = crate::runtime::exceptions::convert_class_not_found(
                    vm,
                    thread,
                    info.class_name,
                    failure,
                );
                break 'judge Some(
                    crate::runtime::interpreter::constants::record_member_owner_failure_as_of(
                        vm, caller, cp_idx, error, fill_as_of,
                    ),
                );
            }
        }
        match crate::runtime::interpreter::invoke::resolve_method_ref_settling(
            vm, thread, caller, cp_idx,
        ) {
            Err(
                e @ MethodCallFailed::InternalError(VmError::Linkage(
                    LinkageError::IllegalAccessError { .. },
                )),
            ) => Some(
                crate::runtime::interpreter::invoke::settle_method_resolution_failure(
                    vm, thread, caller, cp_idx, e, fill_as_of,
                ),
            ),
            _ => None,
        }
    };
    if let Some(error) = refusal {
        pins.release(thread);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] site access refused: class {} -> {}.{}{}",
                caller.as_u32(),
                info.class_name,
                info.method_name,
                info.descriptor
            );
        }
        return Some(handle_jit_dispatch_error(vm, thread, error, info));
    }
    if let Some(moved) = pins.release_raw(thread, args) {
        let out = args_ptr as *mut i64;
        for (i, word) in moved.iter().enumerate().take(args.len()) {
            *out.add(i) = *word;
        }
    }
    // Admitted: the owner is in the caller's namespace now (loaded through
    // its loader above), so the answer was judged; a later call of the site
    // must not pay this resolution again.
    jit_site_access_admit(key, info);
    None
}

#[cfg(test)]
mod t1w2_site_access_guard_tests {
    use super::{
        jit_site_access_admit, jit_site_access_admitted, JitInvokeInfo, JIT_SITE_ACCESS_LAST,
        JIT_SITE_ACCESS_PASSED,
    };

    /// The text of `sig`'s item, up to its closing brace at column 0.
    fn item(sig: &str) -> String {
        let whole = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let start = whole.find(sig);
        assert!(start.is_some(), "`{sig}` not found");
        let body = &whole[start.unwrap_or(0)..];
        body[..body.find("\n}\n").map_or(body.len(), |e| e + 3)].to_string()
    }

    /// Failed before t1 wave 2 (lane P): a compiled `invokestatic` of an owner
    /// no loader had loaded at compile time reached the callee through these
    /// helpers without the JVMS §5.4.4 check the interpreter runs after the
    /// load (`T1PRuntimePackageAccess` `AccJit.freshCls`). The guard must come
    /// before every door that can call the callee, in both helpers.
    #[test]
    fn both_compiled_call_helpers_judge_the_site_before_any_door() {
        for (sig, first_door) in [
            (
                "pub unsafe extern \"C\" fn jit_invoke_dispatch(",
                "try_lean_native_door(",
            ),
            (
                "pub unsafe extern \"C\" fn jit_invoke_virtual_mic(",
                "jit_invoke_bound_site_mic(",
            ),
        ] {
            let body = item(sig);
            let guard = body.find("jit_site_access_guard(vm_ptr, info_ptr, args_ptr, num_args)");
            let door = body.find(first_door);
            assert!(guard.is_some(), "{sig} does not run the site access guard");
            assert!(door.is_some(), "{sig}: `{first_door}` not found");
            assert!(guard < door, "{sig}: the guard runs after `{first_door}`");
        }
    }

    /// The guard settles an `IllegalAccessError` against the class entry's
    /// record (later executions rethrow it) and loads an `invokestatic` owner
    /// through the caller's loader before resolving, as the interpreter does.
    #[test]
    fn the_slow_guard_resolves_like_execute_invokestatic() {
        let body = item("unsafe fn jit_site_access_guard_slow(");
        let load = body.find("resolve_class_entry_loader_aware(");
        let resolve = body.find("resolve_method_ref_settling(");
        assert!(load.is_some() && resolve.is_some() && load < resolve);
        assert!(body.contains("settle_method_resolution_failure("));
        assert!(body.contains("recorded_member_owner_failure("));
        assert!(body.contains("CompileArgPins::pin_raw("));
        assert!(body.contains("\"CRATONVM_JIT_SITE_ACCESS_CHECK\""));
    }

    fn site(class_name: &'static str, method_name: &'static str, caller: u32) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name,
            method_name,
            descriptor: "()I",
            num_jit_args: 0,
            return_type: 0,
            invoke_kind: 3,
            declaring_class_id: caller,
            owner_class_id: 0,
        }
    }

    #[test]
    fn an_admitted_site_is_remembered_and_the_memo_is_bounded() {
        JIT_SITE_ACCESS_PASSED.with(|p| p.borrow_mut().clear());
        let a = site("p/A", "f", 7);
        let key = (1, 0x1000);
        jit_site_access_admit(key, &a);
        assert_eq!(JIT_SITE_ACCESS_LAST.with(std::cell::Cell::get).1, key);
        assert!(jit_site_access_admitted(key, &a));
        for i in 0..(1usize << 16) {
            jit_site_access_admit((2, i), &a);
        }
        assert!(JIT_SITE_ACCESS_PASSED.with(|p| p.borrow().len()) <= 1 << 16);
        JIT_SITE_ACCESS_PASSED.with(|p| p.borrow_mut().clear());
    }

    /// Failed before t1 wave 3 (`t1-V1-site-access-memo-keys-alias-after-reuse`):
    /// the memo was keyed by the info and name-string ADDRESSES plus the caller
    /// id, so a recompiled or redefined caller whose new site got the same
    /// allocations back for another reference inherited "admitted". A hit now
    /// names the same reference text, caller and kind, or it is no admission.
    #[test]
    fn a_recycled_address_naming_another_reference_is_not_admitted() {
        JIT_SITE_ACCESS_PASSED.with(|p| p.borrow_mut().clear());
        let key = (1, 0x1000);
        jit_site_access_admit(key, &site("p/A", "f", 7));
        // Same address, same lengths, another owner (the redefined caller's
        // new reference): judged again.
        assert!(!jit_site_access_admitted(key, &site("p/B", "g", 7)));
        // Same text, another caller class: another site.
        assert!(!jit_site_access_admitted(key, &site("p/A", "f", 8)));
        // Same text, another kind.
        let mut virt = site("p/A", "f", 7);
        virt.invoke_kind = 0;
        assert!(!jit_site_access_admitted(key, &virt));
        // The same reference at the same address (a recompile of the same
        // body) keeps its admission.
        assert!(jit_site_access_admitted(key, &site("p/A", "f", 7)));
        JIT_SITE_ACCESS_PASSED.with(|p| p.borrow_mut().clear());
    }

    /// The one-compare fast path holds only within one code-cache generation:
    /// freeing an info takes an invalidation, which advances it.
    #[test]
    fn the_last_site_fast_path_is_scoped_to_one_cache_generation() {
        let body = item("unsafe fn jit_site_access_guard(");
        assert!(body.contains("let generation = cratonvm_jit::jit_cache_generation();"));
        assert!(body.contains("== (generation, key)"));
        assert!(body.contains("jit_site_access_admitted(key, info)"));
    }

    /// `t1-V1` secondary: under `--jdk-only` a failed owner load is the
    /// resolution's error, recorded against the class entry (later executions
    /// rethrow it) and raised at the call, not left for the dispatch to ask the
    /// loader a second time.
    #[test]
    fn a_failed_owner_load_is_recorded_and_raised_under_jdk_only() {
        let body = item("unsafe fn jit_site_access_guard_slow(");
        let load = body.find("resolve_class_entry_loader_aware(");
        let record = body.find("record_member_owner_failure_as_of(");
        assert!(load.is_some() && record.is_some() && load < record);
        assert!(body.contains("convert_class_not_found("));
        assert!(body.contains("if !vm.config.is_jdk_only()"));
    }
}

pub unsafe extern "C" fn jit_invoke_dispatch(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    // t1 wave 2, lane P: a site the compile door could not judge is judged at
    // its first compiled execution (JVMS §5.4.4).
    if !jit_site_access_repeat(vm_ptr, info_ptr) {
        if let Some(bits) = contain_door("jit_invoke_dispatch", vm_ptr, || {
            jit_site_access_guard(vm_ptr, info_ptr, args_ptr, num_args)
        }) {
            return bits;
        }
    }
    // SBR5 lane ncall: a site already known to be a plain callback native.
    // JIT round 20 (lane stash): under the helper's panic scope, as its body.
    if let Some(bits) = contain_door("jit_invoke_dispatch", vm_ptr, || {
        try_lean_native_door(vm_ptr, info_ptr, args_ptr, num_args, false)
    }) {
        return bits;
    }
    let f = || jit_invoke_dispatch_body(vm_ptr, info_ptr, args_ptr, num_args);
    let bits = contain(
        "jit_invoke_dispatch",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: a native callee's object result is compiled code's now.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_invoke_dispatch`], run under its panic guard.
unsafe fn jit_invoke_dispatch_body(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    // JIT round 14 wave 2 (lane mic, M8-1): an inline-cache quiescent point
    // (no probe in flight on this thread). Must stay first.
    cratonvm_jit::jit_ic_quiescent_point();
    // JIT round 16 (lane mh, MH15-5): a bound constant-handle `invokeExact`
    // site dispatches its target directly (`mh_constant_bind_redirect`).
    let mut mh_link_threw = false;
    if let Some(route) =
        mh_constant_bind_redirect(vm_ptr, info_ptr, args_ptr, num_args, &mut mh_link_threw)
    {
        return mh_constant_bind_call(vm_ptr, route, args_ptr, num_args);
    }
    if mh_link_threw {
        // Round 18 wave 11 (lane mhresid): the site's link stashed its error.
        return i64::MIN;
    }
    // JIT round 20 wave 19, lane pRefl (`CRATONVM_JIT_MH_COMPILED_LINK`): a
    // linked `invokeExact`, an `invokeBasic` or a `linkTo*` calls its Java
    // target as a compiled call, not through its native door.
    if let Some(bits) = mh_compiled_link_redirect(vm_ptr, info_ptr, args_ptr, num_args) {
        return bits;
    }
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Round-7 fix (CRIT, audit §3): SATB safepoint flush. A dispatched
    // call may transitively enter the GC barrier through callee
    // allocations or `Object.wait` paths.
    jit_safepoint_flush_satb(vm_ptr);
    mic_prof::dump_maybe_disp();
    let _cyc_disp = mic_prof::CycGuard::new(&mic_prof::CYC_DISP_TOTAL);
    // WS1 diagnostic: per-call dispatch trace, naming the callee that encloses
    // the lost wall time.
    //
    // This used to ride on `mic_prof::enabled()`, justified by "the dispatch
    // helper is cold enough on the kafka repro — ~15 calls — that an eprintln
    // per call is affordable". On a call-dense workload it is not cold: on
    // `ZipContentTests` the same switch wrote **268 MB of stderr in 73 seconds**
    // and had to be killed, so the COUNTERS — which are the reason to reach for
    // MIC_PROF at all, and are cheap — could not be read on the one workload
    // that needed them. A per-call `eprintln` and a set of atomic counters do
    // not belong on one switch. The trace now has its own.
    let _disp_trace = if mic_prof::trace_enabled() {
        let info = &*(info_ptr as *const JitInvokeInfo);
        struct DispTrace {
            label: String,
            t0: u64,
        }
        impl Drop for DispTrace {
            fn drop(&mut self) {
                eprintln!(
                    "[DISP_TRACE] {} cycles={}",
                    self.label,
                    mic_prof::now().wrapping_sub(self.t0)
                );
            }
        }
        Some(DispTrace {
            label: format!(
                "{}.{}{} kind={}",
                info.class_name, info.method_name, info.descriptor, info.invoke_kind
            ),
            t0: mic_prof::now(),
        })
    } else {
        None
    };
    // SAFETY: vm_ptr and info_ptr originate from JIT code; both point to valid, live objects.
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    disp_census::note_kind(info.invoke_kind);
    // DIAGNOSTIC (gated): record the dispatched callee (restored on return) so
    // a downstream jit_putfield_int miscompile can name the offending method.
    let _callee_guard = if crate::runtime::env_cache::jit_putfield_diag() {
        Some(JitCalleeGuard::new(info))
    } else {
        None
    };
    // Defensive gate: when the user-facing CRATONVM_DISABLE_JIT kill-switch is set,
    // no JIT code should be executing — so this dispatch helper must never run.
    // Reaching it means a JIT entry point bypassed the flag (a real bug). Returning
    // 0 here is preferable to UB from a stale compiled callsite; emit a one-shot
    // warning so the bypass is visible during bisection.
    if crate::runtime::env_cache::disable_jit() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "[cratonvm] WARN: jit_invoke_dispatch reached with CRATONVM_DISABLE_JIT=1 \
                 (callee {}.{}{}). A JIT entry-point bypassed the kill-switch — \
                 returning 0 to avoid undefined behavior.",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        return 0;
    }
    if crate::runtime::env_cache::jit_dispatch_dbg() {
        let p = args_ptr as *const i64;
        let mut buf = String::new();
        if !p.is_null() && num_args > 0 {
            for i in 0..(num_args as usize).min(4) {
                // SAFETY: `p` is non-null (checked above) and `i < num_args`, so
                // `p.add(i)` stays inside the JIT-provided args slice (debug path only).
                let v = unsafe { *p.add(i) };
                buf.push_str(&format!(" arg{}=0x{:x}", i, v));
            }
        }
        eprintln!(
            "[JIT_DISPATCH] {}.{}{} kind={} num_args={}{} info_ptr=0x{:x}",
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
            num_args,
            buf,
            info_ptr,
        );
    }
    if num_args < 0 || (num_args > 0 && (args_ptr as *const i64).is_null()) {
        return 0;
    }
    // SAFETY: args_ptr is non-null (checked above) and num_args >= 0.
    // The JIT caller allocated this array on its own stack frame.
    let args_slice = if num_args == 0 {
        &[] as &[i64]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };

    // The SATB flush above can participate in a moving collection before the
    // helper reads the JIT caller's raw argument array.  Forward every
    // descriptor-declared reference, not only `this`: a compiled callee must
    // not receive a stale byte-array/object argument after a moving GC.
    //
    // FUNNEL17-2 (`CRATONVM_JIT_NATIVE_WALK_ONCE`, JIT round 19 wave 2, lane
    // funnel2): the same pass, also recording which words it validated, so
    // the native-site door below does not walk them again. Nothing between
    // here and that door is a GC point (null tests, memo probes, the
    // object-native cache, whose leaves decline before their native runs).
    let (forwarded_args, prevalidated) = if vm.natives.native_methods.jit_native_walk_once() {
        forward_jit_reference_args_validated(vm, info, args_slice)
    } else {
        (forward_jit_reference_args(vm, info, args_slice), 0)
    };
    let args_slice = forwarded_args.as_deref().unwrap_or(args_slice);
    // jr20 wave 9, lane gcE (gch-h1jp item 2): a forwarded Rust copy of the
    // arguments is in no root set, so every arm below that calls a compiled
    // body with it roots its words across the call and the sentinel service
    // (`CompileArgPins`, read back through `args_now`): a re-run from entry
    // after a moving collection inside the callee must not read the copy's
    // pre-move words. Rare (ZGC's lazy forwarding), so the cost is too.
    let args_forwarded = forwarded_args.is_some();

    // JVMS 6.5: a statically bound INSTANCE site (`invoke_kind == 1`:
    // `invokespecial`, or an `invokevirtual` of a final / private method the
    // compiler devirtualized) with a null receiver raises NPE AT THE INVOKE,
    // before the callee exists. Every compiled-callee arm below
    // (`DISPATCH_CACHE`, the `jit_cache` probe, the tier-up compile) CALLs the
    // callee's entry with `args_slice` as it stands, and a compiled body elides
    // the null check of `this` (`aload_0` of an instance method is a non-null
    // local, `null_check_elim`), so a null receiver ran the callee with
    // `this == null` and its first field read was a bare SIGSEGV
    // (`R16ThrowpathPlain npeCall`: `Holder.get` entered from the blind
    // dispatch of a cold devirtualized `vNullHolder.get()` site once its
    // per-site counter compiled the callee; the GC_FLAGS read `[RAX + 7]` of
    // the compact `getfield` arm with RAX = 0). Raised as the `0 | 2` arm's
    // tail raises it: the pending-NPE action and the sentinel, so the
    // interpreter's drain builds HotSpot's `Cannot invoke "..."` message from
    // the invoke's bci. The direct-call lowerings already test the receiver
    // themselves (`emit_direct_cross_call`, the single-pass direct arm); this
    // is the helper door's copy of that test.
    if info.invoke_kind == 1 && args_slice.first() == Some(&0) {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }

    // Interpreter round i7 wave 136, lane L2
    // (`CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, performance, default OFF):
    // the receiver check of an interface sender's `invokespecial` a
    // method-entry door compiled through this helper instead of refusing the
    // method. After the null test (HotSpot checks a non-null receiver only)
    // and before every arm that could call the callee. One per-VM bool read
    // per statically bound dispatch with the switch off.
    if info.invoke_kind == 1 && vm.config.jit_iface_special_receiver_guard {
        if let Some(bits) = iface_special_receiver_guard(vm, info, args_slice) {
            return bits;
        }
    }

    // nio21 wave 20 L1 (`CRATONVM_JIT_DIRECT_THROW_NEW`): `new X(msg)` of a
    // JDK throwable whose constructor chain is the JDK 25 one, built without
    // interpreting that chain (`exceptions::jit_new_throwable_direct`). The
    // receiver is forwarded and non-null here; `<init>` returns nothing.
    if info.invoke_kind == 1
        && info.method_name == "<init>"
        && try_throwable_init_direct_at_dispatch(vm, info, args_slice)
    {
        return 0;
    }

    // GPU offload, from COMPILED code.
    //
    // Placed here and not later: every probe below this line is a
    // per-callsite memo that ends in a CALL to the callee, so reaching any
    // of them has already decided not to offload. Placed here and not
    // EARLIER because it needs `args_slice` post-forwarding -- a moving
    // collection during the SATB flush above can have relocated the very
    // arrays the kernel is about to marshal.
    #[cfg(feature = "gpu-offload")]
    if info.invoke_kind == 3 && cratonvm_jit::offload_hook::any_kernels() {
        if let Some(ret) = try_compiled_offload(vm, info, info_ptr as usize, args_slice) {
            return ret;
        }
    }

    // JIT dispatch normally calls a custom loader's inherited bytecode
    // directly. ClassLoader's resource methods must throw NPE for a null name
    // before that bytecode runs; the sentinel routes it through Java handlers.
    // ONLY for a class-loader receiver: see
    // `null_resource_name_receiver_is_class_loader`.
    if args_slice.len() == 2
        && args_slice[1] == 0
        && matches!(
            (info.method_name, info.descriptor),
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
        && null_resource_name_receiver_is_class_loader(
            vm,
            args_slice[0],
            info.method_name,
            info.descriptor,
        )
    {
        set_jit_pending_npe();
        return i64::MIN;
    }

    // `(vm_identity, info pointer)`. The VM half is not decoration: the direct
    // helpers below pass the address of a process-global `static JitInvokeInfo`,
    // which is identical in every VM. See [`JitSiteKey`].
    //
    // Revalidate BEFORE the first memo read, not after the compile probes: the
    // native fast paths immediately below are memo consults keyed on this very
    // address, and a `JitInvokeInfo` box is freed with its `CompiledMethod`, so
    // the address can already belong to a different call site. This call was
    // further down (past those probes) and left them serving the previous
    // owner's native. See `flush_raw_entry_dispatch_caches`.
    flush_raw_entry_dispatch_caches();
    let info_key = jit_site_key(vm.vm_identity, info_ptr as usize);
    // Diagnostic (`CRATONVM_DBG_SITE_ALIAS`): does this key still name its site?
    // The gate is here rather than inside, so an all-off run does not make the
    // call at all — it measured 2.2 ns switched off, which is a fifth of the
    // argument decode it precedes.
    if site_alias_detect_enabled() {
        note_site_identity(info_key, info);
    }
    // Cached exact-receiver native fast path — the FIRST per-callsite probe. The
    // resolution/insertion slow path stays further down (after the compile
    // probes); this early block only serves sites the cache has already
    // resolved. Rationale: `HashMap.put/get` and the real-layout Matcher
    // operations are registered natives with no
    // bytecode, so neither the Integer cache below nor the virtual-dispatch
    // machinery can ever serve them — yet every map call paid those probes
    // first. Probing the map cache first costs the (now direct-called on
    // x64, hence rarely dispatched) Integer sites one extra hash lookup and
    // saves one on every cached native operation. The receiver class-id equality
    // check preserves the exact-receiver guard; the `any_class_redefined`
    // gate matches the resolution site below. Runs ahead of the recursion
    // depth guard like the Integer block: the cached callbacks are native
    // leaves that never re-enter JIT code.
    if matches!(info.invoke_kind, 0 | 2) && !args_slice.is_empty() {
        let cached =
            OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| cache.borrow().get(&info_key).copied());
        if let Some(entry) =
            cached.filter(|entry| !class_was_redefined(vm, ClassId::new(entry.receiver_class_id)))
        {
            let receiver_raw = args_slice[0] as u64;
            if receiver_raw != 0 && (receiver_raw & 0x7) == 0 && receiver_raw < (1u64 << 48) {
                crate::jit::helpers::note_membership_walk(3);
                if let Some(receiver) = vm.mem.heap.is_object_address(receiver_raw as usize) {
                    if jit_dispatch_class_validated(vm, receiver)
                        .class_id()
                        .map(|cid| cid.as_u32())
                        == Some(entry.receiver_class_id)
                    {
                        if let Some((thread, _guard)) = jit_thread_mut() {
                            if let Some(result) = call_object_native_raw(
                                vm, thread, info, receiver, args_slice, entry,
                            ) {
                                disp_census::note(disp_census::OUT_OBJECT_NATIVE);
                                return result;
                            }
                        }
                    }
                }
            }
        }
    }
    // Leaf-native fast path. Placed here — after the exact-receiver object
    // native cache above, before the Integer cache and the whole compile /
    // virtual-dispatch machinery below — because a leaf native is by definition
    // a call with nothing to compile and nothing to dispatch: a field read, an
    // atomic, or a constant. Everything after this point is per-call work that
    // such a site was paying for no reason. See `NativeSiteCache`.
    // Unlike the `jit_invoke_virtual_mic` call site, no borrow is live here —
    // the nearest one above is scoped to an `if let` that has already closed —
    // so this one acquires it, and the guard's scope is exactly the call.
    //
    // The borrow is passed as an `Option` rather than being required, so a run
    // with no JIT thread installed still ENTERS the callee and still runs its
    // counted pre-resolution bails; the `?` inside consumes the `None` at the
    // same point the callee's own `jit_thread_mut()?` used to. Requiring it
    // here instead would have skipped those `site_refusal::note_and_decline`
    // counters, which the callee's own comment calls out as the thing that
    // made its first cut unexplainable.
    let mut thread_and_guard = jit_thread_mut();
    let site_native = try_jit_site_cached_native_dispatch(
        vm,
        thread_and_guard.as_mut().map(|(t, _)| &mut **t),
        info,
        info_key,
        args_slice,
        prevalidated,
    );
    drop(thread_and_guard);
    if let Some(result) = site_native {
        disp_census::note(disp_census::OUT_SITE_NATIVE);
        return result;
    }
    // `--jdk-only` (interpreter round i1 wave 38, lane L4): an invoke-exact
    // `VarHandle` at a site the native site cache declined (the cache switched
    // off, a receiver-class change, a refused capability) is judged here, with
    // the site's REAL descriptor, before the tail (`vm_exec`'s
    // signature-polymorphic block, which also serves the thin helpers' cold
    // arms and so cannot judge) converts it. A name test, then the one relaxed
    // load `try_jit_site_cached_native_dispatch` pays. A thin helper's
    // stand-in site is judged against the descriptor its compiled caller's
    // invoke names (wave 49, `varhandle_stand_in_site_descriptor`). Only an
    // access-mode name: `VarHandle$AccessMode.ordinal()` matches the owner
    // test too, and its receiver is no `VarHandle`.
    if crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(info.class_name)
        && crate::vm::vm_exec::is_signature_polymorphic_method_name(info.method_name)
    {
        if let Some(exact_slot) = crate::runtime::interpreter::var_handle_exact_gate(vm) {
            if let Some(error) = jit_var_handle_exact_refusal(vm, info, args_slice, exact_slot) {
                return error;
            }
        }
    }
    // Only a `java/lang/Integer` site can resolve to an entry below (both match
    // arms name it), so every other site is answered by the name test, which
    // for a different-length name is one length compare. Before this, EVERY
    // dispatch paid the redefinition check and a hash probe here to learn
    // "no"; once any class has been redefined (an agent, Mockito) that check
    // takes the class-manager read lock and does a by-name class lookup, per
    // call, on every site.
    if info.class_name == "java/lang/Integer" && !site_class_was_redefined(vm, info, info_key) {
        let cached =
            INTEGER_NATIVE_DISPATCH_CACHE.with(|cache| cache.borrow().get(&info_key).copied());
        let integer_native = match cached {
            Some(entry) => entry,
            None => {
                // Cold, once per call site. The `(kind, callback)` table is
                // unchanged; `admit_jit_fast_native` then asks the registry
                // what `java/lang/Integer.valueOf`/`intValue` are actually
                // registered as and lets `resolve_native_dispatch_wave1`
                // decide. Under `Compatible` it hands the same callback back
                // and adds only the census handle; under `JdkOnly` a refusal
                // caches `None`, so the boxing fast path simply stops existing
                // at this site and every call falls through to the generic,
                // policy-checked tail. No strict-mode branch is added to the
                // dispatch path itself.
                let entry = match (info.class_name, info.method_name, info.descriptor) {
                    ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;") => {
                        admit_jit_fast_native(
                            vm,
                            "java/lang/Integer",
                            info.method_name,
                            info.descriptor,
                            cratonvm_native_builtins::intrinsics::integer::intrinsic_integer_value_of,
                        )
                        .map(|(callback, native_id)| IntegerNativeDispatchCache {
                            kind: IntegerNativeKind::ValueOf,
                            callback,
                            native_id,
                        })
                    }
                    ("java/lang/Integer", "intValue", "()I") => admit_jit_fast_native(
                        vm,
                        "java/lang/Integer",
                        info.method_name,
                        info.descriptor,
                        cratonvm_native_builtins::intrinsics::integer::intrinsic_integer_int_value,
                    )
                    .map(|(callback, native_id)| IntegerNativeDispatchCache {
                        kind: IntegerNativeKind::IntValue,
                        callback,
                        native_id,
                    }),
                    _ => None,
                };
                INTEGER_NATIVE_DISPATCH_CACHE.with(|cache| {
                    cache.borrow_mut().insert(info_key, entry);
                });
                entry
            }
        };
        if let Some(entry) = integer_native {
            if let Some((thread, _guard)) = jit_thread_mut() {
                if let Some(result) = call_integer_native_raw(vm, thread, info, args_slice, entry) {
                    disp_census::note(disp_census::OUT_INTEGER_NATIVE);
                    return result;
                }
            }
        }
    }
    // BUG-1: native-stack recursion guard for the JIT→JIT dispatch path. Bump
    // the per-thread JIT-dispatch depth and check it against the native-stack
    // ceiling BEFORE we recurse into any compiled callee. On overflow this
    // stashes a catchable `StackOverflowError` and returns the `i64::MIN`
    // deopt sentinel instead of overflowing the OS stack. The guard is held
    // for the rest of this call so the level is released on every return path.
    let _jit_dispatch_depth_guard =
        match enter_jit_dispatch(vm, constructor_receiver(info, args_slice)) {
            Ok(g) => g,
            Err(sentinel) => return sentinel,
        };

    // Fast path: check thread-local dispatch cache for a previously-compiled callee.
    // This avoids the JIT cache lock on every call.
    // Virtual/interface dispatch (invoke_kind 0/2) must resolve on the RUNTIME
    // receiver type. The callsite-keyed entry cache below and the
    // `info.class_name` (static CP class) JIT-cache lookup both assume static
    // binding, so reusing them at a polymorphic call site dispatches a
    // SUPERTYPE method — e.g. `Object.equals` (identity `==`) run on boxed
    // `Integer` receivers, so two equal Integers compare unequal and junit
    // `assertEquals` fails on identical values. Only the statically-bound kinds
    // (invokespecial=1, invokestatic=3) may use these fast paths; virtual /
    // interface fall through to the receiver-resolving slow path
    // (`ctx.invoke_virtual`). Hot monomorphic virtual sites are already served
    // by the receiver-guarded MIC helper (`jit_invoke_virtual_mic`).
    let statically_bound = matches!(info.invoke_kind, 1 | 3);
    // Class identity first: a name that no longer resolves to the ClassId a
    // memo recorded it against invalidates every dispatch memo below, and none
    // of the three signals that follow can see that happen.
    flush_class_identity_dispatch_memos(vm);
    let redefine_jit_quiesced = site_quiesced_for_redefinition(vm, info, info_key);
    // Once per redefinition per thread, not once per call. The answer above is
    // true until this flush has run for the current epoch (it was permanently
    // true under `CRATONVM_JIT_QUIESCE_BY_GENERATION=0`, and still is), and this
    // flush empties the WHOLE thread's two caches, so every call through such
    // a site (a mocked class, an agent-woven one) used to throw away every
    // other site's memo on this thread, for the rest of the process: the
    // per-dispatch flush shape the MIC's `redefine_epoch` stamp was written to
    // remove. The quiesced site itself never reads either cache (every consult
    // below is gated on `!redefine_jit_quiesced`), and staleness from the
    // redefinition is `flush_raw_entry_dispatch_caches`' job (the full
    // eviction in `redefineClass` advances the JIT generation). So one flush
    // per epoch keeps what this line was for.
    if redefine_jit_quiesced {
        let stamp = (vm.vm_identity, vm.jit.jit_cache.redefine_epoch());
        if QUIESCED_SITE_FLUSH_STAMP.with(|seen| seen.replace(stamp)) != stamp {
            DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
            VIRTUAL_DISPATCH_CACHE.with(|dc| dc.borrow_mut().clear());
        }
    }
    flush_raw_entry_dispatch_caches();
    // Interpreter-only mode (a JVMTI interpreter-only event, or a JDWP
    // breakpoint / step / suspension; `jvmti_requires_interpreter`): the four
    // compiled-entry arms below stand down with the redefinition quiescence,
    // so a call from a frame that is ALREADY compiled runs its callee through
    // the interpreted tail, where the events and the breakpoints are delivered.
    // The interpreter-side doors only stop NEW compiled frames. Interpreter
    // round i1 wave 5, L7.
    let compiled_entry_forbidden =
        redefine_jit_quiesced || crate::runtime::interpreter::jvmti_requires_interpreter(vm);

    // The argument words as they stand after the synchronous callee compile
    // below, when a collection inside it moved one. Every later reader of
    // `args_slice` — the fresh call, the fallback decode, the slow tail —
    // must see these, not the staging block. See [`CompileArgPins`].
    let mut virtual_compile_moved_args: Option<Vec<i64>> = None;

    // A JIT call-site's CP owner is often an interface. Resolve once on the
    // actual receiver class, then cache and directly enter the compiled
    // concrete body. The generic helper otherwise re-enters invoke_virtual on
    // every element access, rebuilding conservative JIT roots each time.
    if direct_virtual_compiled_callee_entry_enabled()
        && !statically_bound
        && !compiled_entry_forbidden
        && !args_slice.is_empty()
    {
        let raw = args_slice[0] as u64;
        if raw != 0 && (raw & 7) == 0 && raw < (1u64 << 48) {
            let receiver = ObjectRef::from_raw(raw as usize as *mut u8);
            // The cache key below is `(site, class id)`; an array has none
            // (`jit_dispatch_class`). `globally_named` is already false for one
            // (`virtual_dispatch_target_cached`), and the key says so too.
            let receiver_dispatch_cid = jit_dispatch_class(vm, receiver).class_id();
            let receiver_cid = receiver_dispatch_cid.unwrap_or_else(|| ClassId::new(0));
            // Round 12 wave 2 (lane `calls`): the memo consult FIRST, before
            // the target resolution below, as `jit_invoke_virtual_mic` hoisted
            // its own. A hit's key carries the receiver's own class id and was
            // inserted only for a receiver-derived target (the publication
            // condition), so re-deriving the target on a hit (a per-thread memo
            // read and an `Rc<str>` clone per call) cannot change the answer.
            // Only a live entry is taken here; a stale one is left for the
            // consult below, which drops it and re-resolves at once.
            if receiver_dispatch_cid.is_some() && dispatch_virtual_memo_first_enabled() {
                let key = (info_key, receiver_cid.as_u32());
                let fresh = VIRTUAL_DISPATCH_CACHE.with(|dc| {
                    dc.borrow()
                        .get(&key)
                        .filter(|c| !c.names_a_stale_body())
                        .map(|c| (c.entry, c.needs_context, c.owner.clone(), c.rerun_pins))
                });
                if let Some(cached) = fresh {
                    let routed = call_compiled_then_route(
                        vm,
                        info,
                        args_slice,
                        receiver_cid,
                        cached.3 || args_forwarded,
                        || match cached.2.as_ref() {
                            Some(owner) => try_call_compiled_entry_reentrant_owned(
                                owner, cached.0, cached.1, vm_ptr, args_slice,
                            ),
                            None => try_call_compiled_entry_reentrant(
                                cached.0, cached.1, vm_ptr, args_slice,
                            ),
                        },
                    );
                    if let Some(v) = routed {
                        disp_census::note(disp_census::OUT_VIRT_CACHE);
                        return v;
                    }
                }
            }
            // Read before the target is resolved: the shared-table publication
            // on the fill below withdraws itself across a redefinition.
            let dispatch_epoch_seen = vm.jit.jit_cache.redefine_epoch();
            // A `selection_error` target is never `cacheable_receiver`, so it
            // fills nothing below and reaches the by-name tail, which raises it.
            let target = virtual_dispatch_target_cached(vm, receiver, info, info_key);
            // Gated on a receiver-derived target, not on `globally_named`: the
            // compile below is asked for the receiver's OWN class id, so a class
            // name another loader also defines compiles this receiver's copy,
            // and the memo it fills is keyed on that id
            // (`i1-L7-callee-compile-doors-are-loader-blind`). An ARRAY
            // receiver has no dispatch class of its own (round 11 wave 13:
            // `jit_dispatch_class`), so it never fills this cache.
            //
            // Round 17 wave 3 (lane mega3, MEGA2-17-2, under
            // `CRATONVM_JIT_MEGA_CACHE_RETARGETED`): a receiver whose §5.4.6
            // selection moved to a superclass (`selected_cid != 0`, set only
            // under the switch) is receiver-derived too. Its body is compiled
            // for the SELECTED class (`compile_cid`) and memoized/published
            // under the RECEIVER's id, exactly as the MIC arm does: the memo
            // key is (this site, receiver class) and the table key (selector,
            // receiver class), and selection is a function of those alone.
            // Every later reader (`mic_callee_has_exception_table`,
            // `call_compiled_then_route`) resolves the callee through
            // `select_for_jit_receiver` from the receiver's id, so it judges
            // the selected body.
            let receiver_derived = target.publishable() && receiver_dispatch_cid.is_some();
            let compile_cid = ClassId::new(target.compile_cid(receiver_cid.as_u32()));
            // RBC.6 perf follow-up (docs/feature-designs/jit-local-exception-handlers.md)
            // — this used to also require `!mic_callee_has_exception_table(...)`,
            // excluding ANY callee that declares a local exception table from
            // this cache entirely and forcing every such call through the
            // "generic helper" fallback this comment block warns is expensive
            // ("re-enters invoke_virtual on every element access, rebuilding
            // conservative JIT roots each time"). Unlike the INLINE machine-code
            // MIC/PIC cascade (`jit/src/x64.rs`, guarded by the SAME check at its
            // own publish sites — see `BUG-H` comments there — which really does
            // bypass Rust-level exception routing since it CALLs the raw entry
            // pointer directly from compiled machine code), THIS cache is a plain
            // Rust `HashMap` consulted from inside this same Rust function — a hit
            // still calls `try_call_compiled_entry_reentrant` and then
            // `route_implicit_exc_through_callee` below EXACTLY as a cache miss
            // would, so the callee's own exception table is routed identically
            // either way. Excluding it here bought no correctness and cost a real
            // ~450s/round regression for `Response.toAbsolute()` once RBC.6 let it
            // compile (confirmed via the Tomcat suite's `TestResponsePerformance`
            // on the Linux build host).
            if receiver_derived {
                let key = (info_key, receiver_cid.as_u32());
                // An entry naming a superseded or invalidated body is dropped
                // at the hit (`DispatchCache::names_a_stale_body`), and the site
                // re-resolves NOW rather than after re-counting to the
                // threshold: the replacement is already published, so the
                // compile probe below only has to find it.
                let mut stale_dropped = false;
                if let Some(cached) = VIRTUAL_DISPATCH_CACHE.with(|dc| {
                    let mut dc = dc.borrow_mut();
                    if dc.get(&key).is_some_and(DispatchCache::names_a_stale_body) {
                        dc.remove(&key);
                        stale_dropped = true;
                        return None;
                    }
                    dc.get(&key)
                        .map(|c| (c.entry, c.needs_context, c.owner.clone(), c.rerun_pins))
                }) {
                    let routed = call_compiled_then_route(
                        vm,
                        info,
                        args_slice,
                        receiver_cid,
                        cached.3 || args_forwarded,
                        || match cached.2.as_ref() {
                            Some(owner) => try_call_compiled_entry_reentrant_owned(
                                owner, cached.0, cached.1, vm_ptr, args_slice,
                            ),
                            None => try_call_compiled_entry_reentrant(
                                cached.0, cached.1, vm_ptr, args_slice,
                            ),
                        },
                    );
                    if let Some(v) = routed {
                        disp_census::note(disp_census::OUT_VIRT_CACHE);
                        return v;
                    }
                } else {
                    let should_compile = stale_dropped || {
                        let threshold = crate::runtime::env_cache::jit_invocation_threshold();
                        let count = VIRTUAL_DISPATCH_COUNTER.with(|dc| {
                            let mut counts = dc.borrow_mut();
                            let count = counts.entry(key).or_insert(0);
                            *count = count.wrapping_add(1);
                            // Round 16 wave 17 (lane mega5): the late-probe
                            // mark is not part of the count
                            // (`blind_dispatch_late_probe_rewind`).
                            *count & !BLIND_LATE_PROBE_MARK
                        });
                        // Round 16 wave 8 (lane mega): a spent key re-probes
                        // at doublings of the threshold under
                        // `CRATONVM_JIT_BLIND_DISPATCH_REPROBE`, and (shape 6)
                        // a way the shared table already holds cues the probe
                        // below now, under `CRATONVM_JIT_MEGA_BLIND_DISPATCH_HINT`.
                        count == threshold
                            || blind_dispatch_spent_reprobe(count, threshold)
                            || blind_dispatch_table_hint(
                                vm,
                                info,
                                key,
                                receiver_cid.as_u32(),
                                count,
                                threshold,
                            )
                    };
                    if should_compile {
                        let (compiled, moved) =
                            compile_with_ref_args_pinned(vm, info, args_slice, || {
                                // j27 wave 3 (lane DISPATCH2): probe-only
                                // when the manager owns the compile
                                // (`virtual_dispatch_counter_door_miss`).
                                crate::runtime::interpreter::jit_bridge::try_jit_compile_callee_for_class_on_miss(
                                    vm,
                                    // The receiver's own class, or the
                                    // §5.4.6-selected one (MEGA2-17-2).
                                    compile_cid,
                                    &target.class_name,
                                    info.method_name,
                                    info.descriptor,
                                    true,
                                    virtual_dispatch_counter_door_miss(),
                                )
                                // JIT r20 w19 yLadder (DISP20-7): an inherited
                                // method's DECLARING-class body, filled and
                                // called below as a probe answer.
                                .or_else(|| probe_selected_declaring_body(vm, info, receiver_cid))
                            });
                        virtual_compile_moved_args = moved;
                        let args_slice =
                            virtual_compile_moved_args.as_deref().unwrap_or(args_slice);
                        if let Some((_callee_pin, entry, needs_context)) = compiled {
                            // Asked once per (site, receiver class), here on
                            // the cold fill, and memoized with the entry.
                            let rerun_pins = mic_callee_has_exception_table(vm, receiver_cid, info);
                            // `_callee_pin` keeps the callee mapped across the
                            // cache publication and the direct call below.
                            // Only publish a raw entry we can keep alive:
                            // `_owner` is the sole keep-alive for this
                            // thread-local pointer, so caching with `None`
                            // would let a later tier-up `put` unmap the body
                            // under it.
                            if let Some(owner) = cratonvm_jit::pin_jit_entry(entry) {
                                VIRTUAL_DISPATCH_CACHE.with(|dc| {
                                    dc.borrow_mut().insert(
                                        key,
                                        DispatchCache {
                                            entry,
                                            needs_context,
                                            owner: Some(std::rc::Rc::new(owner.into())),
                                            rerun_pins,
                                        },
                                    );
                                });
                            }
                            // Round 12 wave 3 (lane mega2, proposal W2-1 step
                            // 3): and the VM's shared megamorphic table, so the
                            // hashed stubs of every inline-cached site that
                            // resolves this method (same selector) find the
                            // body without a helper round trip. This site is
                            // blind (no PIC), so it only publishes. Admission
                            // is `jit_invoke_virtual_mic`'s for its own PIC: a
                            // receiver-derived target compiled for this
                            // receiver's class id (above), never an
                            // indy-trapping body (the MIC arm's rule for every
                            // machine-called cache), and a table callee only
                            // where that arm would publish one
                            // (`publish_mega_dispatch_entry`).
                            if mega_table_publish_dispatch_enabled()
                                && !_callee_pin.has_indy_trap
                                && (!rerun_pins
                                    || (mega_table_exc_callees_enabled()
                                        && mic_publish_exception_table_callees()))
                            {
                                install_mega_dispatch_way(
                                    vm,
                                    info,
                                    None,
                                    receiver_cid.as_u32(),
                                    entry as u64,
                                    needs_context,
                                    dispatch_epoch_seen,
                                );
                            }
                            let routed = call_compiled_then_route(
                                vm,
                                info,
                                args_slice,
                                receiver_cid,
                                rerun_pins || args_forwarded,
                                || {
                                    try_call_compiled_entry_reentrant(
                                        entry,
                                        needs_context,
                                        vm_ptr,
                                        args_slice,
                                    )
                                },
                            );
                            if let Some(v) = routed {
                                return v;
                            }
                        }
                    }
                }
            }
        }
    }
    let args_slice = virtual_compile_moved_args.as_deref().unwrap_or(args_slice);

    let cached_entry = if direct_static_compiled_callee_entry_enabled()
        && statically_bound
        && !compiled_entry_forbidden
    {
        // A superseded or invalidated body is dropped at the hit; the by-name
        // `jit_cache` probe below then finds the current one on this same
        // call. See `DispatchCache::names_a_stale_body`.
        DISPATCH_CACHE.with(|dc| {
            let mut dc = dc.borrow_mut();
            if dc
                .get(&info_key)
                .is_some_and(DispatchCache::names_a_stale_body)
            {
                dc.remove(&info_key);
                return None;
            }
            dc.get(&info_key)
                .map(|c| (c.entry, c.needs_context, c.owner.clone(), c.rerun_pins))
        })
    } else {
        None
    };
    if let Some((entry, needs_ctx, owner, rerun_pins)) = cached_entry {
        if crate::runtime::env_cache::jit_dispatch_dbg() {
            eprintln!(
                "[JIT_DISPATCH_ARM/dcache] {}.{} entry=0x{:x}",
                info.class_name, info.method_name, entry,
            );
        }
        // SAFETY: entry is a JIT-compiled function pointer cached from a previous successful
        // compilation. `try_call_compiled_entry` selects the correct extern "C" fn signature
        // based on arg count; on overflow it returns None and we bail to the interpreter.
        // CRIT round-5 fix: the previous `_ => 0` arm silently dropped 5+-arg callees;
        // wave-2 changed it to fall through to the slow path, and this wave goes one
        // step further by routing directly through `bail_to_interpreter` so the bail is
        // explicit at the call site (matches the MIC fast-path at `:1722`).
        // BUG-H: if the callee threw an implicit exception its own `catch`
        // should handle, the routing re-runs it in the interpreter through its
        // exception table. Statically bound: no receiver class.
        let routed = call_compiled_then_route(
            vm,
            info,
            args_slice,
            ClassId::new(0),
            rerun_pins || args_forwarded,
            || {
                let rc = match owner.as_ref() {
                    Some(owner) => try_call_compiled_entry_reentrant_owned(
                        owner, entry, needs_ctx, vm_ptr, args_slice,
                    ),
                    None => try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice),
                }?;
                if crate::runtime::env_cache::jit_dispatch_dbg() {
                    eprintln!(
                        "[JIT_DISPATCH_RET/dcache] {}.{}{} ret=0x{:x}",
                        info.class_name, info.method_name, info.descriptor, rc,
                    );
                }
                Some(rc)
            },
        );
        if let Some(v) = routed {
            disp_census::note(disp_census::OUT_DCACHE);
            return v;
        }
        // Overflow: decode args once and hand off to the interpreter.
        if let Some((thread, _guard)) = jit_thread_mut() {
            let bail_args = decode_dispatch_values(vm, info, args_slice);
            return bail_to_interpreter(vm, thread, info, &bail_args);
        }
        return 0;
    }

    // Both shortcuts below find the callee BY NAME. A site whose own loader
    // sees a different class under that name must not take either; it falls
    // to the loader-faithful interpreter route at the tail. Only these two
    // write `DISPATCH_CACHE`, so the cache-hit arm above never serves it.
    // A substituted owner is compared by its recorded id, not looked up in
    // the caller's loader (`jit_substituted_owner`).
    // JIT round 21 lane reflect4: a hidden static owner (`invoke_kind` 3 with
    // an `owner_class_id`, `jit_substituted_owner`) is its id; no loader
    // answers its name for another copy.
    let by_name_is_the_callers_class = !statically_bound
        || if matches!(info.invoke_kind, 1 | 3) && info.owner_class_id != 0 {
            !jit_substituted_owner_is_another_copy(vm, info)
        } else {
            !jit_callee_name_is_another_loaders_copy(
                vm,
                ClassId::new(info.declaring_class_id),
                info.class_name,
            )
        };

    // Check JIT cache for a compiled version of this callee.
    // PERF: `JitCache::get` takes `&str`, so we can pass the static literals from
    // `info` directly. Earlier code wrapped each in `Arc::from(...)` which
    // allocated a fresh heap buffer + atomic header on every dispatch — three
    // wasted allocations per hot call. Deref coercion handles the conversion.
    // Gated on `statically_bound`: the lookup key is the static CP class, which
    // is only the correct dispatch target for invokespecial/invokestatic.
    if direct_static_compiled_callee_entry_enabled()
        && statically_bound
        && !compiled_entry_forbidden
        && by_name_is_the_callers_class
        && !WRAPPED_STATIC_CALLEE.with(|m| m.borrow().contains_key(&info_key))
    {
        // `JitInvokeInfo` carries only the static-CP class NAME baked into this
        // compiled call site at codegen time, not a loader-scoped `ClassId` —
        // extending the raw `extern "C"` JIT ABI to also pass the caller's
        // `ClassId` here would require touching the x64 codegen call-site
        // emission itself. Resolving the name globally preserves this path's
        // existing (already name-based, not loader-aware) behavior unchanged;
        // it does not newly introduce the multi-loader-same-name collision —
        // see `JitKey::declaring_class_id`'s doc comment for the interpreter-
        // side fix this mirrors.
        // JIT round 21 lane reflect4 (correctness of the hidden-owner leaf,
        // unconditional): a hidden class's body is published under its id,
        // which no name lookup answers (`jit_hidden_static_owner`).
        let info_class_id = jit_hidden_static_owner(vm, info).unwrap_or_else(|| {
            vm.classes
                .class_manager
                .read()
                .get_loaded_class_id(info.class_name)
                .unwrap_or(cratonvm_types::ClassId::new(0))
        });
        let jit_cache = vm.jit.jit_cache.read();
        let published = jit_cache.get(
            info.class_name,
            info.method_name,
            info.descriptor,
            info_class_id,
        );
        if published
            .as_ref()
            .is_some_and(|compiled| compiled.requires_wrapped_entry)
        {
            WRAPPED_STATIC_CALLEE.with(|m| {
                m.borrow_mut().insert(info_key, ());
            });
        }
        if let Some(compiled) = published
            // A raw CALL to the entry below supplies no monitor, so an
            // `ACC_SYNCHRONIZED` body must not be served here — it would run
            // unlocked and, worse, be cached in `DISPATCH_CACHE` so every later
            // call at this site runs unlocked too. Falling through leaves the site
            // on the interpreter route, whose `dispatch_static` /
            // `dispatch_virtual` arms take the monitor. See
            // `CompiledMethod::requires_wrapped_entry`.
            .filter(|compiled| !compiled.requires_wrapped_entry)
        {
            let entry = compiled.entry_ptr() as usize;
            let needs_ctx = compiled.needs_context();
            if crate::runtime::env_cache::jit_dispatch_dbg() {
                eprintln!(
                    "[JIT_DISPATCH_ARM/jcache] {}.{} entry=0x{:x}",
                    info.class_name, info.method_name, entry,
                );
            }
            // Asked once per site, on this cold fill, and memoized with the entry.
            let rerun_pins = callee_has_exception_table(vm, info);
            // Cache for future calls
            DISPATCH_CACHE.with(|dc| {
                dc.borrow_mut().insert(
                    info_key,
                    DispatchCache {
                        entry,
                        needs_context: needs_ctx,
                        owner: Some(std::rc::Rc::new(compiled.clone().into())),
                        rerun_pins,
                    },
                );
            });
            // JIT round 16 wave 14 (lane throw4, TP16-20): a method compiled
            // before this callee could bind may be waiting on it.
            crate::runtime::interpreter::jit_bridge::late_bind_site_served(
                vm,
                info.class_name,
                info.method_name,
                info.descriptor,
            );
            // SAFETY: entry was obtained from a CompiledMethod in the JIT cache, whose
            // entry_ptr points to executable memory with the correct extern "C" ABI.
            // CRIT round-5 fix: on >ARG_REGS args, route directly to the interpreter
            // via `bail_to_interpreter` rather than silently returning 0 (the original
            // wave-2 fall-through was already correct; this just makes the bail
            // explicit at the call site to match the MIC fast-path).
            // BUG-H: the routing sends a callee-thrown implicit exception
            // through the callee's own exception table (see dcache site above).
            let routed = call_compiled_then_route(
                vm,
                info,
                args_slice,
                ClassId::new(0),
                rerun_pins || args_forwarded,
                || {
                    let rc =
                        try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice)?;
                    if crate::runtime::env_cache::jit_dispatch_dbg() {
                        eprintln!(
                            "[JIT_DISPATCH_RET/jcache] {}.{}{} ret=0x{:x}",
                            info.class_name, info.method_name, info.descriptor, rc,
                        );
                    }
                    Some(rc)
                },
            );
            if let Some(v) = routed {
                disp_census::note(disp_census::OUT_JCACHE);
                return v;
            }
            if let Some((thread, _guard)) = jit_thread_mut() {
                let bail_args = decode_dispatch_values(vm, info, args_slice);
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
            return 0;
        }
    }

    // Invocation counting — trigger compilation for hot callees. Gated on
    // `statically_bound`: compiling `info` (the static CP-class method) and
    // caching it under the callsite key would re-introduce the supertype
    // miscompile for a virtual/interface site.
    let should_compile = direct_static_compiled_callee_entry_enabled()
        && statically_bound
        && !compiled_entry_forbidden
        && by_name_is_the_callers_class
        && DISPATCH_COUNTER.with(|dc| {
            let mut map = dc.borrow_mut();
            let count = map.entry(info_key).or_insert(0);
            // Saturating: a debug build must not panic past 2^32 calls.
            *count = count.saturating_add(1);
            *count == crate::runtime::env_cache::jit_invocation_threshold()
        })
        // j27 wave 2 (lane NATMISC): asked only at the crossing.
        && static_dispatch_counter_compiles_inline();
    // As `virtual_compile_moved_args`: the arguments after the compile below.
    let mut static_compile_moved_args: Option<Vec<i64>> = None;
    if should_compile {
        // Try to compile the callee and cache it. The compile can run
        // `<clinit>` and collect, so the reference arguments are pinned across
        // it and re-read (r11-irlower-outgoing-arg-staging-refs-unpublished).
        let (compiled, moved) =
            compile_with_ref_args_pinned(vm, info, args_slice, || try_compile_callee(vm, info));
        static_compile_moved_args = moved;
        let args_slice = static_compile_moved_args.as_deref().unwrap_or(args_slice);
        if let Some((_callee_pin, entry, needs_ctx)) = compiled {
            if crate::runtime::env_cache::jit_dispatch_dbg() {
                eprintln!(
                    "[JIT_DISPATCH_ARM/compile] {}.{} entry=0x{:x}",
                    info.class_name, info.method_name, entry,
                );
            }
            // Asked once per site, on this cold fill, and memoized with the entry.
            let rerun_pins = callee_has_exception_table(vm, info);
            // See the virtual-dispatch sibling above: an unowned raw entry
            // must not be cached.
            if let Some(owner) = cratonvm_jit::pin_jit_entry(entry) {
                DISPATCH_CACHE.with(|dc| {
                    dc.borrow_mut().insert(
                        info_key,
                        DispatchCache {
                            entry,
                            needs_context: needs_ctx,
                            owner: Some(std::rc::Rc::new(owner.into())),
                            rerun_pins,
                        },
                    );
                });
            }
            // TP16-20, as at the `jit_cache` fill above.
            crate::runtime::interpreter::jit_bridge::late_bind_site_served(
                vm,
                info.class_name,
                info.method_name,
                info.descriptor,
            );
            // SAFETY: entry was just produced by try_compile_callee, which returns a validated
            // JIT entry pointer. CRIT round-5 fix: bail explicitly to the interpreter on
            // >ARG_REGS args via `bail_to_interpreter` (matches the MIC fast-path).
            // BUG-H: the routing sends a callee-thrown implicit exception
            // through the callee's own exception table (see dcache site above).
            let routed = call_compiled_then_route(
                vm,
                info,
                args_slice,
                ClassId::new(0),
                rerun_pins || args_forwarded,
                || try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice),
            );
            if let Some(v) = routed {
                disp_census::note(disp_census::OUT_COMPILED_NOW);
                return v;
            }
            if let Some((thread, _guard)) = jit_thread_mut() {
                let bail_args = decode_dispatch_values(vm, info, args_slice);
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
            return 0;
        }
    }
    let args_slice = static_compile_moved_args.as_deref().unwrap_or(args_slice);

    disp_census::note(disp_census::OUT_TAIL);
    // Slow path: interpreter fallback
    let (thread, _jit_thread_guard) = match jit_thread_mut() {
        Some(t) => t,
        None => {
            return 0;
        }
    };

    // Registered natives cannot populate DISPATCH_CACHE because they have no
    // compiled entry pointer. Cache the callback, decoder kind, and exact
    // receiver ClassId for the hot monomorphic HashMap and Matcher sites
    // instead of repeating virtual resolution and a failed compile probe on
    // every iteration.
    let object_native_kind = hashmap_native_arg_count(info)
        .map(|_| ObjectNativeKind::HashMap)
        .or_else(|| matcher_native_arg_count(info).map(|_| ObjectNativeKind::Matcher))
        .or_else(|| stringbuilder_native_arg_count(info).map(|_| ObjectNativeKind::StringBuilder));
    if matches!(info.invoke_kind, 0 | 2) && object_native_kind.is_some() && !args_slice.is_empty() {
        let kind = object_native_kind.expect("checked above");
        let receiver_raw = args_slice[0] as u64;
        if receiver_raw != 0 && (receiver_raw & 0x7) == 0 && receiver_raw < (1u64 << 48) {
            crate::jit::helpers::note_membership_walk(3);
            if let Some(receiver) = vm.mem.heap.is_object_address(receiver_raw as usize) {
                // An array (no dispatch class id) is keyed as class 0, which no
                // entry carries and whose name is never `expected_class`, so it
                // neither hits nor fills (`jit_dispatch_class`).
                let receiver_class_id = jit_dispatch_class_validated(vm, receiver)
                    .class_id()
                    .map_or(0, |cid| cid.as_u32());
                if receiver_class_id != 0
                    && !class_was_redefined(vm, ClassId::new(receiver_class_id))
                {
                    let cached = OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| {
                        cache
                            .borrow()
                            .get(&info_key)
                            .copied()
                            .filter(|entry| entry.receiver_class_id == receiver_class_id)
                    });
                    if let Some(entry) = cached {
                        if let Some(result) =
                            call_object_native_raw(vm, thread, info, receiver, args_slice, entry)
                        {
                            return result;
                        }
                    } else {
                        let expected_class = match kind {
                            ObjectNativeKind::HashMap => "java/util/HashMap",
                            ObjectNativeKind::Matcher => "java/util/regex/Matcher",
                            ObjectNativeKind::StringBuilder => "java/lang/StringBuilder",
                        };
                        let is_exact_receiver = {
                            let classes = vm.classes.class_manager.read();
                            classes
                                .get_class(ClassId::new(receiver_class_id))
                                .map(|class| class.name.as_ref() == expected_class)
                                .unwrap_or(false)
                        };
                        if is_exact_receiver {
                            // Cold, once per call site. Each of these three
                            // resolvers now applies JDK-only policy internally
                            // (see `admit_jit_fast_native`) and returns the
                            // §4 census handle alongside the callback. A
                            // `None` under `JdkOnly` means "policy declined
                            // this optimization": nothing is cached, and the
                            // call falls through to the generic
                            // `invoke_or_native` tail below, which is the
                            // single authoritative resolver and raises the
                            // structured `VmError::JdkOnly(..)` if the triple
                            // is genuinely unrunnable. Never a stub, never a
                            // wild jump.
                            let callback = match kind {
                                ObjectNativeKind::HashMap => hashmap_native_callback(vm, info),
                                ObjectNativeKind::Matcher => matcher_native_callback(vm, info),
                                ObjectNativeKind::StringBuilder => {
                                    stringbuilder_native_callback(vm, info)
                                }
                            };
                            if let Some((callback, native_id)) = callback {
                                let entry = NativeDispatchCache {
                                    receiver_class_id,
                                    callback,
                                    kind,
                                    native_id,
                                };
                                OBJECT_NATIVE_DISPATCH_CACHE.with(|cache| {
                                    cache.borrow_mut().insert(info_key, entry);
                                });
                                if let Some(result) = call_object_native_raw(
                                    vm, thread, info, receiver, args_slice, entry,
                                ) {
                                    return result;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Lambda proxies are synthetic and therefore cannot participate in the
    // class-store MIC. Give the erased primitive adapter its own receiver-guarded
    // direct path before allocating decoded Values for the generic fallback.
    //
    // The lambda call site's own cached invoke target goes FIRST and takes any
    // arity — this is the SECOND door into lambda dispatch from compiled code
    // (`jit_invoke_virtual_mic` is the other), and a fast path wired into only
    // one of them is a fast path that a workload can miss entirely for reasons
    // that have nothing to do with its shape.
    if matches!(info.invoke_kind, 0 | 2) && !args_slice.is_empty() {
        crate::jit::helpers::note_membership_walk(3);
        if let Some(proxy) = vm.mem.heap.is_object_address(args_slice[0] as usize) {
            // An array of lambda proxies carries the proxy's id in its
            // header but dispatches on `Object` (`jit_dispatch_class`).
            let proxy_class_id = jit_dispatch_class_validated(vm, proxy)
                .class_id()
                .unwrap_or_else(|| ClassId::new(0));
            if proxy_class_id != ClassId::new(0) && vm.classes.is_lambda_proxy_class(proxy_class_id)
            {
                if let Some(result) = try_lambda_site_direct_call(
                    vm,
                    thread,
                    proxy,
                    proxy_class_id,
                    info,
                    args_slice,
                    vm_ptr,
                    // This door has no inline-cache slot to install into: it is
                    // reached from a call site the codegen did not give one.
                    None,
                ) {
                    return result;
                }
            }
        }
    }
    if matches!(info.invoke_kind, 0 | 2) && args_slice.len() == 2 {
        crate::jit::helpers::note_membership_walk(3);
        if let Some(proxy) = vm.mem.heap.is_object_address(args_slice[0] as usize) {
            // An array of lambda proxies carries the proxy's id in its
            // header but dispatches on `Object` (`jit_dispatch_class`).
            let proxy_class_id = jit_dispatch_class_validated(vm, proxy)
                .class_id()
                .unwrap_or_else(|| ClassId::new(0));
            if proxy_class_id != ClassId::new(0) && vm.classes.is_lambda_proxy_class(proxy_class_id)
            {
                match try_fast_lambda_int_to_double_apply(
                    vm,
                    thread,
                    proxy,
                    proxy_class_id,
                    info,
                    args_slice,
                ) {
                    Ok(Some(result)) => return result,
                    Ok(None) => {}
                    Err(error) => return handle_jit_dispatch_error(vm, thread, error, info),
                }
            }
        }
    }

    // Round-5 CRIT-1 fix: share arg-decoding with the three cache-hit
    // overflow bailouts above via `decode_dispatch_values`.
    // `mut`: the `invokestatic` arm re-reads its reference arguments through
    // pins across a `<clinit>` (`jit_static_owner_init_before_native`).
    let mut values = decode_dispatch_values(vm, info, args_slice);

    let result: Option<Value> = match info.invoke_kind {
        0 | 2 => {
            if values.is_empty() {
                return 0;
            }
            let receiver_ref = match values[0] {
                Value::Object(Some(obj)) => obj,
                // JVM semantics: invokevirtual/invokeinterface on a null
                // receiver throws NullPointerException. Signal it like the
                // array helpers — set the pending-NPE flag and return the
                // i64::MIN sentinel so the interpreter's post-JIT drain builds
                // the real NPE and routes it through the method's exception
                // table. Previously this returned `0`, silently SWALLOWING the
                // NPE: a null deref inside a JIT-compiled method (e.g. a
                // once-called lambda body that reaches this cold dispatch
                // helper before any inline null-check / MIC warm-up) completed
                // normally instead of throwing, so `() -> nullRef.foo()` ran
                // as a no-op under the JIT (the bug only reproduced JIT-on).
                Value::Object(None) => {
                    // `INVOKE_RECEIVER` names the opcode family that trapped
                    // rather than a message: the JEP 358 action half here is
                    // `Cannot invoke "Owner.name(sig)"`, which only the call
                    // site's bytecode knows. This helper is called AT a
                    // published safepoint, so the frame's safepoint-id slot
                    // holds this invoke's bci, and the code lets
                    // `runtime::interpreter::jit_npe_message` corroborate that
                    // bci before reading the opcode there.
                    set_jit_pending_npe_action(
                        crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
                    );
                    return i64::MIN;
                }
                // A non-object receiver slot is a miscompile, not a legitimate
                // null — keep the defensive 0 bail (does not mask a real NPE).
                _ => return 0,
            };
            // A lambda-proxy receiver must be dispatched through the proxy
            // registry, never by name — see `try_lambda_proxy_sam_dispatch`
            // for what resolving one by its functional-interface name runs
            // instead of the lambda body.
            if let Some(result) = try_lambda_proxy_sam_dispatch(vm, thread, info, &values) {
                return match result {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
            // A callee the JIT did not compile: enter it through this
            // (site, receiver class)'s cached interpreter frame template rather
            // than re-resolving it from `info.class_name` on every call. This is
            // the TAIL — `OUT_TAIL` was already counted above and every compiled
            // -callee probe (`DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE`,
            // `JitCache`, the tier-up arm) has already declined — so there is no
            // compiled body being passed over here. Declines (and caches the
            // refusal) for anything the by-name path must still handle.
            let receiver_dispatch_cid = jit_dispatch_class(vm, receiver_ref).class_id();
            // gci i12/mf: first, a (site, receiver class) whose slow call
            // already ended in a table-bound JNI native goes straight to the
            // door (`CRATONVM_JNI_DISPATCH_MEMO`; `vm_exec`'s compiled virtual
            // call-site rows). One byte compare when the memo is off.
            if let Some(hit) =
                jit_jni_vsite_lookup(vm, thread, info, info_key.1, receiver_dispatch_cid)
            {
                return jit_jni_vsite_call(
                    vm,
                    thread,
                    info,
                    &hit,
                    &mut values[..],
                    &hit.receiver_name,
                );
            }
            if let Some(r) = try_jit_virtual_bytecode_callee(
                vm,
                thread,
                info,
                info_key,
                receiver_ref,
                // Class 0 (an array, `jit_dispatch_class`) is refused inside.
                receiver_dispatch_cid.unwrap_or_else(|| ClassId::new(0)),
                &values,
                true,
            ) {
                return match r {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
            // Match the register-overflow bail path: `NativeContext::invoke_virtual`
            // resolves solely from the heap object's class id. That is insufficient
            // for a synthetic/ClassId(0) receiver (common for Lucene iterator
            // adapters): it turns `Iterator.hasNext()` into `Object.hasNext()`.
            // `virtual_dispatch_target_for_receiver` preserves the real receiver
            // class when available and otherwise supplies the CP-resolved class;
            // `invoke_or_native` then applies the VM's interface/abstract retarget.
            //
            // The memoized form (this call ran `flush_class_identity_dispatch_memos`
            // above): the target now carries the §5.4.6 retarget, which costs a
            // walk and an owner lookup, so the per-call tail reads it from the
            // (site, receiver class) memo instead of recomputing it under a lock.
            let dispatch_target = virtual_dispatch_target_cached(vm, receiver_ref, info, info_key);
            if dispatch_target.selection_error {
                if let Some(error) = jit_selection_failure(vm, thread, receiver_ref, info) {
                    return handle_jit_dispatch_error(vm, thread, error, info);
                }
            }
            let jni_vsite_fill_ok = !dispatch_target.selection_error;
            let dispatch_class = dispatch_target.class_name;
            crate::vm::dbg_dispatch_tally(
                "jit_dispatch_virtual_tail",
                &dispatch_class,
                info.method_name,
                info.descriptor,
            );
            // gci i12/mf: the compiled virtual call-site rows' fill probe
            // brackets the by-name tail (armed only with the memo on).
            let jni_vsite_probe = crate::vm::vm_exec::jni_jit_vsite_probe_arm(
                vm,
                thread,
                info.method_name,
                info.descriptor,
            );
            // gch h1/jp (fr review item 3): the rescue below resends `values`,
            // so a failed attempt re-reads it from pins kept across it.
            let virt_result = crate::vm::invoke_or_native_keeping_args_current(
                vm,
                thread,
                &dispatch_class,
                info.method_name,
                info.descriptor,
                &mut values[..],
                !info.class_name.is_empty(),
            );
            if let Some(save) = jni_vsite_probe {
                crate::vm::vm_exec::jni_jit_vsite_probe_finish(
                    vm,
                    thread,
                    save,
                    ClassId::new(info.declaring_class_id),
                    crate::vm::vm_exec::jni_jit_vsite(info_key.1),
                    receiver_dispatch_cid.unwrap_or_else(|| ClassId::new(0)),
                    &dispatch_class,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    virt_result.is_ok() && jni_vsite_fill_ok,
                );
            }
            // The receiver as the failed attempt left it (the rescue reads its
            // class).
            let receiver_ref = match values.first() {
                Some(Value::Object(Some(receiver))) => *receiver,
                _ => receiver_ref,
            };
            match virt_result {
                Ok(v) => v,
                Err(e) => {
                    // S111r12 — JIT virtual-dispatch rescue: when the
                    // receiver's `class_id_of` returns a stub class
                    // (e.g. `java/lang/Comparable` for a malformed
                    // ClassLoader instance) that doesn't declare the
                    // CP-resolved method, `invoke_virtual` raises
                    // `NoSuchMethodError`. The CP method-ref class
                    // carried in `info.class_name` (e.g.
                    // `java/lang/ClassLoader`) is the spec-correct
                    // resolution target — retry the dispatch through
                    // it. Mirrors the S111r10 receiver-walk fallback
                    // for invokeinterface and the S111r8 cid=0 →
                    // CP-class fallback in `execute_invoke`.
                    if !info.class_name.is_empty() {
                        // An array's dispatch class is `java/lang/Object`,
                        // not its header's component (`jit_dispatch_class`).
                        let recv_name_opt = match jit_dispatch_class(vm, receiver_ref) {
                            JitDispatchClass::Class(recv_cid) => {
                                let cm = vm.classes.class_manager.read();
                                cm.get_class(recv_cid).map(|c| c.name.to_string())
                            }
                            JitDispatchClass::JavaLangObject => {
                                Some("java/lang/Object".to_string())
                            }
                        };
                        let cp_differs = recv_name_opt
                            .as_deref()
                            .map(|n| n != info.class_name)
                            .unwrap_or(true);
                        if cp_differs
                            && recv_name_opt.as_deref().is_some_and(|recv_name| {
                                is_dispatch_no_such_method_miss(
                                    &e,
                                    recv_name,
                                    info.method_name,
                                    info.descriptor,
                                )
                            })
                        {
                            let r = crate::vm::invoke_or_native(
                                vm,
                                thread,
                                info.class_name,
                                info.method_name,
                                info.descriptor,
                                &values,
                            );
                            match r {
                                Ok(v) => v,
                                Err(e2) => {
                                    return handle_jit_dispatch_error(vm, thread, e2, info);
                                }
                            }
                        } else {
                            return handle_jit_dispatch_error(vm, thread, e, info);
                        }
                    } else {
                        return handle_jit_dispatch_error(vm, thread, e, info);
                    }
                }
            }
        }
        1 => {
            // invokespecial: dispatch must NOT virtually re-target onto the
            // receiver's runtime class. `invoke_or_native` -> `invoke_on_class_shared`
            // applies the iface/abstract -> receiver-class retarget that
            // `invokevirtual` semantics require, which for invokespecial turns
            // a super-call into a self-call and produces unbounded recursion
            // (e.g. `RunLast.execute` invokespecial-calls `AbstractParseResultHandler.execute`;
            // APRH is abstract, so the retarget bounces back to `RunLast.execute`
            // and we recurse forever — surfaced as `StackOverflowError` inside the
            // picocli `execute` chain on neo4j / keycloak). The `<init>` and
            // `<clinit>` carve-outs that protect that path in
            // `invoke_on_class_shared_inner` do NOT cover ordinary super-calls,
            // so we have to take an invokespecial-aware dispatch path here.
            //
            // `invoke_special_shared` matches the interpreter's invokespecial
            // semantics: walk the hierarchy from the CP-resolved class to the
            // declaring class for the requested method, then invoke through
            // `invoke_on_class_shared_no_retarget` so the virtual retarget never
            // fires. Native-override priority is preserved (same as
            // `invoke_or_native`).
            //
            // BUG-JIT-INVOKESPECIAL-LOADER-20260726: resolve the CP class
            // through the CALLER's loader before dispatching. `info.class_name`
            // is constant-pool text; with two loaders defining the same binary
            // name, `invoke_special_shared`'s own `load_class_concurrent` picks
            // whichever copy the global map holds — see
            // `JitInvokeInfo::declaring_class_id`. `resolve_class_loader_aware`
            // is the same resolver the interpreter's invokespecial uses, and it
            // short-circuits to the global answer whenever no user-defined
            // loader has ever defined a class, so single-loader processes pay
            // one relaxed atomic load.
            // A callee the JIT did not compile: enter it through this site's
            // cached interpreter frame template. Same position in the tail as
            // the static and virtual arms — every compiled-callee probe has
            // already declined by here.
            if let Some(r) = try_jit_special_bytecode_callee(vm, thread, info, info_key, &values) {
                return match r {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
            // gci i12/mf: a statically bound site whose slow call already ended
            // in a table-bound JNI native goes straight to the door on the
            // declaring class (`CRATONVM_JNI_DISPATCH_MEMO`; `vm_exec`'s kind-1
            // rows, refused once anything was redefined). One byte compare when
            // the memo is off.
            if let Some(hit) = crate::vm::vm_exec::jni_jit_special_memo_lookup(
                vm,
                thread,
                ClassId::new(info.declaring_class_id),
                crate::vm::vm_exec::jni_jit_ssite(info_key.1),
                (info.class_name, info.method_name, info.descriptor),
            ) {
                return match crate::vm::vm_exec::jni_jit_special_memo_call(
                    vm, thread, &hit, &values,
                ) {
                    Ok(Some(Value::Int(v))) => v as i64,
                    Ok(Some(Value::Long(v))) => v,
                    Ok(Some(Value::Float(f))) => f.to_bits() as i64,
                    Ok(Some(Value::Double(d))) => d.to_bits() as i64,
                    Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
                    Ok(Some(Value::Object(None)) | None) => 0,
                    Ok(_) => 0,
                    Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
                };
            }
            // An owner the compile substituted for the constant-pool class
            // is taken by its recorded id: its name is not one the caller's
            // loader was ever asked for (`jit_substituted_owner`).
            // Through the caller's `CONSTANT_Class` entry for an isolated
            // loader's caller (round 16, lane classsite, CS2-2).
            let resolved_owner = if let Some(owner) = jit_substituted_owner(vm, info) {
                Some(owner)
            } else if info.declaring_class_id != 0 {
                jit_bridged_owner_entry(
                    vm,
                    thread,
                    ClassId::new(info.declaring_class_id),
                    info.class_name,
                )
            } else {
                None
            };
            // Round 15 wave 5 (lane walk4; F1 of
            // `r15w4-walk3-review-findings-FIXED-20260930.md`): a JVMTI agent wove
            // the owner's own body for this method (a Mockito inline mock's
            // advice) and the native registered on it is not one CratonVM's
            // objects must keep: run that body, as the interpreter's
            // invokespecial and `invoke_or_native` do. (`invoke_special_shared`
            // asks the same question since round 15 wave 6, WK4-2; answering it
            // here runs the body without the door's re-resolution.)
            let woven_owner = resolved_owner
                .filter(|&owner| special_site_runs_woven_body(vm, owner, info, &values));
            let r = match (woven_owner, resolved_owner) {
                (Some(owner), _) => crate::runtime::interpreter::execute(
                    vm,
                    thread,
                    owner,
                    info.method_name,
                    info.descriptor,
                    &values,
                ),
                (None, Some(owner)) => {
                    // gci i12/mf: the kind-1 rows' fill probe brackets the
                    // resolved-owner call (armed only with the memo on).
                    let jni_site_probe = crate::vm::vm_exec::jni_jit_vsite_probe_arm(
                        vm,
                        thread,
                        info.method_name,
                        info.descriptor,
                    );
                    let r = crate::vm::invoke_special_shared_on_class(
                        vm,
                        thread,
                        owner,
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                        &values,
                    );
                    if let Some(save) = jni_site_probe {
                        crate::vm::vm_exec::jni_jit_special_probe_finish(
                            vm,
                            thread,
                            save,
                            ClassId::new(info.declaring_class_id),
                            crate::vm::vm_exec::jni_jit_ssite(info_key.1),
                            owner,
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                            r.is_ok(),
                        );
                    }
                    r
                }
                (None, None) => crate::vm::invoke_special_shared(
                    vm,
                    thread,
                    info.class_name,
                    info.method_name,
                    info.descriptor,
                    &values,
                ),
            };
            match r {
                Ok(v) => v,
                Err(e) => {
                    return handle_jit_dispatch_error(vm, thread, e, info);
                }
            }
        }
        3 => {
            // invokestatic: no receiver, so no dispatch retarget can happen —
            // but the OWNER is still constant-pool text, and a class NAME is
            // not a class identity. This arm used to read "no retarget
            // concern. The historical `invoke_or_native` path is correct
            // here", which was true of the DISPATCH and false of the
            // IDENTITY: `invoke_or_native` resolves `info.class_name` through
            // the flat global binary-name map, so when two loaders define
            // that name it binds the call to whichever copy the map holds.
            // Exactly the defect the `invoke_kind == 1` arm above was fixed
            // for (BUG-JIT-INVOKESPECIAL-LOADER-20260726) — the fix landed on
            // invokespecial and stopped there.
            //
            // Measured 2026-08-10 on `AotIntegrationTests
            // .endToEndTestsForBeanOverrides` (spring-test, under
            // `@CompileWithForkedClassLoader`, which defines its own copy of
            // every `org.springframework.*` class): a compiled static call
            // made from the FORKED copy of `ResolvableType` landed in the
            // APPLICATION copy, so the `ResolvableType` it returned belonged
            // to the other class. `ResolvableType$WildcardBounds.get` then
            // ran `while (…) candidate = candidate.resolveType()` against a
            // `NONE` singleton from the wrong copy, `candidate == NONE` could
            // never hold, and one core spun for as long as the machine was
            // given — the hang this test was known for. The same method
            // passes under `--nojit`, because the interpreter resolves the
            // owner through the caller's own loader.
            //
            // STRICTLY ADDITIVE, by construction: the loader-faithful owner
            // is consulted only behind `jit_static_owner_override`'s gates
            // and used only when it DIFFERS from the global by-name answer,
            // so a process with one copy of the name takes the identical
            // previous path — including every special case
            // `invoke_or_native` carries that the on-class form does not.
            // A callee the JIT did not compile: enter it through this site's
            // cached interpreter frame template rather than re-resolving it
            // from `info.class_name` on every call. Declines (and caches the
            // refusal) for anything the by-name path must still handle.
            //
            // gci i9/cs: first, a site whose slow call already ended in a
            // table-bound JNI native goes straight to the door on the class it
            // resolved (`CRATONVM_JNI_DISPATCH_MEMO`; `vm_exec`'s JNI call-site
            // memo, keyed by this info's address and re-checked against its
            // names). One byte compare when the memo is off.
            if let Some(hit) = crate::vm::vm_exec::jni_site_memo_lookup(
                vm,
                thread,
                ClassId::new(info.declaring_class_id),
                crate::vm::vm_exec::jni_jit_site(info_key.1),
                Some((info.class_name, info.method_name, info.descriptor)),
                crate::vm::vm_exec::JNI_SITE_HIT_JIT,
            ) {
                match crate::vm::vm_exec::jni_site_memo_call(vm, thread, &hit, &values) {
                    Ok(v) => v,
                    Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
                }
            } else if let Some(r) =
                try_jit_static_bytecode_callee(vm, thread, info, info_key, &values)
            {
                match r {
                    Ok(v) => v,
                    Err(e) => return handle_jit_dispatch_error(vm, thread, e, info),
                }
            } else {
                // JIT round 21 lane reflect4: a hidden static owner by its id
                // (`jit_hidden_static_owner`); its name resolves nowhere.
                let owner_override = jit_hidden_static_owner(vm, info)
                    .or_else(|| jit_static_owner_override(vm, thread, info));
                // zt t27/jd1 (`CRATONVM_JIT_STATIC_OWNER_LOADS_FIRST`): an
                // owner no loader holds is loaded first, by the route
                // `execute_invokestatic` takes, so the native-vs-bytecode
                // decision below (`invoke_or_native`'s
                // `registered_native_yields_to_loaded_bytecode`) is made on the
                // loaded declaration, as the interpreter makes it. A
                // loader-faithful override names a loaded owner already.
                if owner_override.is_none() {
                    crate::vm::load_static_owner_before_dispatch(
                        vm,
                        thread,
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                        &mut values,
                    );
                }
                // JVMS §5.5, before a registered native can run: neither
                // `invoke_static_shared_on_class` nor `invoke_or_native`'s
                // registry arms initialize the declaring class.
                if let Err(e) = jit_static_owner_init_before_native(
                    vm,
                    thread,
                    info,
                    owner_override,
                    &mut values,
                ) {
                    return handle_jit_dispatch_error(vm, thread, e, info);
                }
                let r = match owner_override {
                    Some(owner) => crate::vm::invoke_static_shared_on_class(
                        vm,
                        thread,
                        owner,
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                        &values,
                    ),
                    None => {
                        crate::vm::dbg_dispatch_tally(
                            "jit_dispatch_static_tail",
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                        );
                        // The static tail: it initializes the declaring class,
                        // never the symbolic owner of an inherited static
                        // (JVMS §5.5; wave 14). The native prelude ran above.
                        // gci i9/cs: the JNI call-site memo's fill probe
                        // brackets the by-name tail (armed only with the memo
                        // on); the flat by-name owner is the site's owner.
                        let site_probe = crate::vm::vm_exec::jni_site_probe_arm(
                            vm,
                            thread,
                            info.method_name,
                            info.descriptor,
                        );
                        let r = crate::vm::invoke_static_or_native_prepared(
                            vm,
                            thread,
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                            &values,
                        );
                        if let Some(save) = site_probe {
                            crate::vm::vm_exec::jni_site_probe_finish(
                                vm,
                                thread,
                                save,
                                ClassId::new(info.declaring_class_id),
                                crate::vm::vm_exec::jni_jit_site(info_key.1),
                                None,
                                info.class_name,
                                info.method_name,
                                info.descriptor,
                                values.len(),
                                r.is_ok(),
                            );
                        }
                        r
                    }
                };
                match r {
                    Ok(v) => v,
                    Err(e) => {
                        return handle_jit_dispatch_error(vm, thread, e, info);
                    }
                }
            }
        }
        _ => None,
    };

    let ret = match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    };
    if crate::runtime::env_cache::jit_dispatch_dbg() {
        eprintln!(
            "[JIT_DISPATCH_RET] {}.{}{} ret=0x{:x} ({})",
            info.class_name, info.method_name, info.descriptor, ret, ret,
        );
    }
    ret
}

/// Kill switch for the bytecode-callee templates' redefinition handling
/// (`STATIC_` / `VIRTUAL_` / `SPECIAL_BYTECODE_CALLEE_CACHE`; interpreter
/// round i1 wave 18, lane L4). `true`: a redefinition retires per class. A
/// template's key is a compiled caller's `JitInvokeInfo` address, and a
/// redefined caller's bodies are withdrawn with the code-cache flush (their
/// site memos with them), so the caller side needs nothing here. On the
/// target side the template's `RedefineGate` retires it when its declaring
/// class is redefined; a template filled after that is served like any other
/// since wave 40 ([`callee_template_declines_redefined_target`]; before, a
/// callee whose declaring class was EVER redefined kept the by-name path). A redefinition cannot turn a body into a native or back (it
/// refuses a change to a method's access flags), and a template exists only
/// for a `(name, descriptor)` no native anywhere carries, so no shadow
/// decision is cached. `false`: the historical latch — no template is served
/// for the rest of the process after the first redefinition of any class.
const CALLEE_TEMPLATES_SURVIVE_REDEFINITION: bool = true;

/// Whether the bytecode-callee templates are latched off by a redefinition:
/// never, unless the kill switch restores the latch.
#[inline(always)]
fn callee_templates_latched_off() -> bool {
    !CALLEE_TEMPLATES_SURVIVE_REDEFINITION && crate::classloading::any_class_redefined()
}

/// Kill switch for serving a bytecode-callee template whose declaring class
/// was redefined before the template was filled (interpreter round i1 wave
/// 40, lane L2;
/// `docs/feature-designs/profile-guided-inlining.md`).
/// `true`: such a template is served like any other -- counted, nominated
/// and entered compiled when a body exists
/// (`bytecode_callee_compiled_or_nominate`), as the interpreter's cached
/// dispatchers and doors now treat a redefined target
/// (`invoke_fast::REDEFINED_TARGETS_TIER_UP`, waves 39-40). The template's
/// gate is checked first on every hit, so a template filled before the
/// class's latest redefinition is evicted and one filled after it holds the
/// current body, the body the by-name path it replaces would run. `false`:
/// the wave-18 policy, which kept a redefined callee on the by-name path on
/// the ground that the interpreter did not offer it to the JIT.
/// SWITCHED OFF by the wave-40 orchestrator (2026-10-04): with it on, a
/// multi-class redefinition tears for compiled readers
/// (`tools/probes/interp/L3/L3W38RedefineTwoClassesSamePool.java`, 60
/// interleaved runs on the host: `dev` 59/60 and 60/60, on 36/60 and 47/60
/// even with the publication hold, off 60/60 and 60/60). See
/// `docs/feature-designs/class-resolution-and-caching.md`.
/// SWITCHED BACK ON by interpreter round i1 wave 42, lane L2, with the
/// bytecode-read witness of the compile doors (`jit_bridge::try_jit_compile_callee_slow`:
/// a compile of the old bytecode was published after the redefinition); in a
/// commit of its own, so the host can judge the page's table with and without it.
const CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS: bool = true;

/// Is a template whose declaring class has been redefined (its gate's
/// generation, current since `is_stale` passed) declined? Only under
/// [`CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS`]'s kill switch: a constant
/// `false` otherwise, so the three template paths lose the branch.
#[inline(always)]
fn callee_template_declines_redefined_target(gate_generation: u32) -> bool {
    !CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS && gate_generation != 0
}

/// A compiled caller's `invokestatic` to a callee the JIT did NOT compile.
///
/// # The cost this removes
///
/// Without it such a call falls out of `jit_invoke_dispatch`'s fast arms into
/// `crate::vm::invoke_or_native` -> `invoke_on_class_shared_inner`, the fully
/// name-keyed generic dispatch — so a call site that is monomorphic and whose
/// callee never changes re-resolves the owner class from a STRING, re-runs the
/// native-override arbitration, re-clones the `CodeAttribute` and re-pads the
/// bytecode, on every single call. `perf`, steady state, on the denied arm of
/// `probes/XferProbe.java`: `invoke_on_class_shared_inner` 14.72% self, the
/// `(ClassLoaderId, Arc<str>)` by-name class lookup 3.41%, `__memcmp_evex_movbe`
/// 3.04%, the native-override gate 1.93%, and `CodeAttribute::clone` +
/// `padded_bytecode_for_method` + `Arc<[u8]>::drop_slow` 4.50%.
///
/// The interpreter pays none of it — its call sites hold a
/// `CachedInvokeTarget::Bytecode` with a prebuilt `Arc<CachedBytecodeMethod>`.
/// The JIT helper had a cache for a COMPILED callee (`DISPATCH_CACHE`,
/// `VIRTUAL_DISPATCH_CACHE`) and none at all for an interpreted one, which is
/// why compiling the caller and not the callee measured **5x slower than
/// compiling neither** (1242-1902 ns/op against 236-385).
///
/// # Why `invokestatic` first
///
/// It is where the volume is — `CRATONVM_DBG=mic-prof` on
/// `probes/ReactorProbe.java`: `kind_static=2_986_402` of
/// `disp_calls=3_356_461`, 89% — and it has the smallest correctness surface:
/// no receiver, so no virtual retarget, no interface rules and no
/// receiver-class guard to re-test.
///
/// # Why it is safe to take instead of `invoke_or_native`
///
/// Same shape of argument as [`try_jit_site_cached_native_dispatch`]: every
/// obligation is discharged ONCE, at resolution, and a site that cannot
/// discharge them caches a refusal and never asks again. The resolution refuses
/// unless ALL of these hold, so anything unusual keeps the old path verbatim:
///
/// * the invoke kind is `3` and the method name is not one `invoke_or_native`
///   special-cases (`site_name_is_special_cased` — the same gate cascade the
///   leaf-native path defers to);
/// * `jit_static_owner_override` declines, i.e. no loader-faithful owner
///   differs from the global by-name answer. That is what keeps the whole
///   loader-identity surface — BUG-JIT-INVOKESPECIAL-LOADER-20260726 and the
///   `AotIntegrationTests` static-owner hang — on the untouched path;
/// * NO native anywhere has this `(name, descriptor)`
///   (`might_have_method_descriptor`, the same cheap pre-filter
///   `execute_invokevirtual_vtable_fast` uses). That one refusal removes the
///   native-override, `SyntheticStub`-yield and redefine-shadow questions
///   entirely rather than reproducing them — and it subsumes
///   `force_native_over_real_jdk_bytecode`, whose triples name a REGISTERED
///   native that must beat real JDK bytecode: with no registration carrying
///   this `(name, descriptor)` at all, such a gate entry declines and the
///   bytecode runs, which is what this path does;
/// * the owner class is loaded AND the class declaring the callee is already
///   initialised — the by-name tail would run its `<clinit>`, and this path
///   must never be the thing that skips it. Initialisation is monotone, so
///   checking once is checking forever;
/// * `build_lambda_impl_cached` (nothing lambda-specific about it, see its doc)
///   accepts the resolved method: not native, not `synchronized`, not abstract,
///   has a `Code` attribute, no native shadow on the declaring class;
/// * the method really is `static`, and its parameter count matches the decoded
///   argument count.
///
/// Per hit, only what can change is re-tested: the declaring class's
/// `RedefineGate` (see `CALLEE_TEMPLATES_SURVIVE_REDEFINITION` and
/// `CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS`).
///
/// SAFETY: `values` are the caller's already-decoded arguments; `thread` is the
/// JIT helper's own borrow.
unsafe fn try_jit_static_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    values: &[Value],
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    if !crate::runtime::env_cache::jit_static_bytecode_callee() {
        return None;
    }
    if info.invoke_kind != 3 {
        return None;
    }
    // A redefinition retires what it can change per class (the gate below);
    // see `CALLEE_TEMPLATES_SURVIVE_REDEFINITION`.
    if callee_templates_latched_off() {
        return None;
    }
    let cached = match STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&info_key).cloned()) {
        Some(hit) => hit,
        None => {
            let mut init_pending = false;
            let resolved = resolve_static_bytecode_callee(vm, thread, info, &mut init_pending);
            // Not cached while the owner is uninitialised: that refusal ends
            // at `<clinit>`, which moves no epoch these memos are flushed on.
            if !init_pending {
                STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
                    c.borrow_mut().insert(info_key, resolved.clone());
                });
            }
            resolved
        }
    };
    let Some((cached, gate)) = cached else {
        disp_census::note(disp_census::OUT_STATIC_BC_REFUSED);
        return None;
    };
    if gate.is_stale() {
        STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().remove(&info_key);
        });
        return None;
    }
    if callee_template_declines_redefined_target(gate.generation) {
        return None;
    }
    // A static callee takes exactly its declared parameters — no receiver. An
    // arity disagreement means the decode and the template describe different
    // methods, which is a refusal, never a guess.
    if values.len() != cached.num_params as usize {
        return None;
    }
    // Count the invocation the ordinary path would have counted, and stand
    // down if a compiled body now exists — see
    // `bytecode_callee_compiled_or_nominate`. Without this a callee served here
    // is never nominated, so it stays interpreted for the life of the process;
    // and a callee that IS compiled would be run interpreted anyway, which is a
    // straight loss rather than a missed win. MEASURED at 15% on
    // `probes/ExchangeProbe.java` before it was added.
    if crate::runtime::interpreter::bytecode_callee_compiled_or_nominate(vm, thread, &cached) {
        disp_census::note(disp_census::OUT_STATIC_BC_REFUSED);
        return None;
    }
    thread.refill_pools_from_shared(
        &vm.mem.operand_stack_pool,
        &vm.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    // In-place install, not a by-value build: `execute_prebuilt_frame` pushes
    // through `push_frame_and_fire_entry`, which harvests and trims the
    // retired slot, so this helper used to undo frame-slot reuse for the
    // interpreter frames beneath it once per call. See
    // `install_and_run_cached_frame`.
    //
    // The `_monitored` form serves a `static synchronized` template with the
    // doors' uncontended acquire on the class mirror, and declines on
    // contention or while no mirror exists; the caller then continues to
    // `invoke_or_native`. A non-synchronized template is exactly
    // `install_and_run_cached_frame`.
    let served = crate::runtime::interpreter::install_and_run_cached_frame_monitored(
        vm,
        thread,
        cached,
        values,
        Some("jit-static-bc"),
    );
    disp_census::note(if served.is_some() {
        disp_census::OUT_STATIC_BC
    } else {
        disp_census::OUT_STATIC_BC_REFUSED
    });
    served
}

/// The once-per-site half of [`try_jit_static_bytecode_callee`]. Every refusal
/// is cached as `None`, so an ineligible site asks these questions once —
/// except "the callee's declaring class is not initialised yet", which sets
/// `init_pending` and is NOT cached (round 11): no flush follows `<clinit>`,
/// so a site first reached before initialisation stayed on the slow tail for
/// the life of the process.
fn resolve_static_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    init_pending: &mut bool,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    if site_name_is_special_cased(info.method_name) {
        return None;
    }
    // A loader-faithful owner that differs from the global by-name answer is
    // exactly the case this path must not try to reproduce.
    if jit_static_owner_override(vm, thread, info).is_some() {
        return None;
    }
    // No native may be in play for this triple, by either route.
    if vm
        .natives
        .native_methods
        .might_have_method_descriptor(info.method_name, info.descriptor)
    {
        return None;
    }
    let owner = {
        let cm = vm.classes.class_manager.read();
        cm.get_loaded_class_id(info.class_name)
    }?;
    if owner == ClassId::new(0) {
        return None;
    }
    // The by-name tail would run `<clinit>` on the way in. Refuse until it
    // has, rather than becoming the path that skips it. The DECLARING class
    // (`jit_static_init_pending`), not `owner`: the static tail never
    // initializes the symbolic owner of an inherited static (JVMS §5.5,
    // wave 14), so an owner test would keep such a site off this path for good.
    if jit_static_init_pending(vm, owner, info).is_some() {
        *init_pending = true;
        return None;
    }
    // `admit_synchronized`: a `static synchronized` callee is served too;
    // `try_jit_static_bytecode_callee` takes its class-mirror monitor through
    // `install_and_run_cached_frame_monitored`
    // (r11w5-tier-static-synchronized-callee-template-patch). Refusing it sent
    // every compiled call of one down the by-name `invoke_or_native` tail.
    let (cached, gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        owner,
        info.method_name,
        info.descriptor,
        true,
    )?;
    if !cached.is_static {
        return None;
    }
    // The body this template runs must belong to an initialized class, by
    // the template's own resolution as well as by `jit_static_init_pending`'s.
    if !crate::vm::is_class_initialized_via_manager(vm, cached.declaring_class_id) {
        *init_pending = true;
        return None;
    }
    Some((cached, gate))
}

/// The class a compiled `invokestatic` through the symbolic owner `owner`
/// must still initialize (JVMS §5.5), or `None`: the by-name routes' one
/// answer, `crate::vm::static_owner_init_pending` (declaring class, reviewed
/// VM-service receivers left alone under `--jdk-only`). Interpreter round i1
/// wave 12, lane L5; shared with `invoke_static_or_native` since wave 13.
fn jit_static_init_pending(vm: &SharedVm, owner: ClassId, info: &JitInvokeInfo) -> Option<ClassId> {
    crate::vm::static_owner_init_pending(
        vm,
        owner,
        info.class_name,
        info.method_name,
        info.descriptor,
    )
}

/// `true` when the native site cache must not cache a static entry yet: the
/// by-name owner is loaded and its declaring class is not initialized
/// ([`jit_static_init_pending`]). By name, and never loading, because the
/// fill runs with the caller's raw argument words live; the registry the
/// entry is resolved from is name-keyed too. Fill time only.
///
/// An owner no loader holds is not initialized either, and since zt t27/jd1
/// it declines the same way whenever the interpreter's route would load it
/// before deciding (`crate::vm::static_owner_load_pending`): the entry was
/// resolved against the absent owner, where a `Bridge` or an allow-listed
/// `SyntheticStub` cannot yield to the JDK body, and a cached entry kept
/// serving that native after the owner was loaded. The static tail loads the
/// owner (`load_static_owner_before_dispatch`, arguments pinned) and the next
/// call resolves the site on the loaded declaration.
/// `CRATONVM_JIT_STATIC_OWNER_LOADS_FIRST=0`: an absent owner has nothing to
/// initialize and the entry is cached, as before.
fn static_native_site_init_pending(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    let owner = vm
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(info.class_name);
    match owner {
        Some(owner) => jit_static_init_pending(vm, owner, info).is_some(),
        None => crate::vm::static_owner_load_pending(
            vm,
            info.class_name,
            info.method_name,
            info.descriptor,
        )
        .is_some(),
    }
}

/// Initialize the declaring class of a compiled `invokestatic` before the
/// funnel tail can run a registered native for it (JVMS §5.5; the interpreter
/// does so in `execute_invokestatic` since wave 10). `invoke_or_native`'s
/// registry arms and `invoke_static_shared_on_class`'s native override run
/// the native without initializing; a bytecode callee is initialized by the
/// funnel itself, so a triple no native carries (`might_have_method_descriptor`)
/// pays one registry prefilter and nothing else.
///
/// `owner_override` is the loader-faithful owner the tail dispatches on
/// (`jit_static_owner_override`); otherwise the by-name owner. `<clinit>` can
/// collect, and `values` is a Rust copy no collector sees: its references are
/// pinned across the initializer and read back afterwards. Every mode. The
/// body is `crate::vm::init_static_native_owner`, which the by-name static
/// entry `invoke_static_or_native` shares (wave 13).
fn jit_static_owner_init_before_native(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    owner_override: Option<ClassId>,
    values: &mut [Value],
) -> Result<(), crate::error::MethodCallFailed> {
    crate::vm::init_static_native_owner(
        vm,
        thread,
        owner_override,
        info.class_name,
        info.method_name,
        info.descriptor,
        values,
    )
}

/// The `invokevirtual` / `invokeinterface` twin of
/// [`try_jit_static_bytecode_callee`]: enter an UNCOMPILED callee through this
/// `(site, receiver class)`'s own cached interpreter frame template instead of
/// re-resolving it by name on every call.
///
/// `Some(..)` means the call was served here. `None` means this site is not
/// eligible and the caller must continue down its ordinary route — the
/// `invoke_or_native` tail — byte-for-byte as before.
///
/// # Why the static half's argument does not simply carry over
///
/// The static half had no receiver, hence no dispatch retarget, no interface
/// rules and no receiver guard. All three come back here, and the way each is
/// discharged is the whole safety argument:
///
/// * **The retarget IS the resolution.** `build_lambda_impl_cached` resolves
///   through `find_method_recursive` starting at the RECEIVER'S OWN class id —
///   the same walk `invoke_on_class_shared` performs after its retarget, and
///   the same one the MIC's compile probe uses (see the "VIRTUAL DISPATCH FIX"
///   note in `jit_invoke_virtual_mic`). A callee it cannot land on — an
///   abstract declaration, an interface method with no implementation on this
///   receiver — has no `Code` attribute and is refused rather than guessed at.
/// * **The receiver guard is the KEY.** The memo is keyed on
///   `(site, receiver class id)`, so a hit is by construction an answer
///   resolved for exactly this receiver class. A site that goes polymorphic
///   gets one entry per class instead of serving the wrong body.
/// * **Loader identity is not in play at all**, and this is the one place this
///   path is STRICTLY safer than its static sibling. That one resolves its
///   owner through `get_loaded_class_id(info.class_name)` — a class NAME, which
///   two loaders can both define — and had to join
///   `flush_class_identity_dispatch_memos` for it. Here the owner is the
///   receiver's own `ClassId`, taken from its header. No name is consulted, so
///   the `globally_named` term that `virtual_dispatch_target_cached` computes
///   has nothing to decide. The memo joins that flush anyway, because a class
///   id can be retired by unloading and the key would then name a class that no
///   longer exists.
///
/// Every other refusal is the static half's, verbatim and for the same reason:
///
/// * an ARRAY receiver is refused (`ObjectKind::Object` only). An array header
///   carries its COMPONENT class id, so `(site, class id)` does not identify
///   one — the invariant `VIRTUAL_TARGET_CACHE` and the KC26 `array.clone()`
///   note already turn on;
/// * **no native anywhere has this `(name, descriptor)`**
///   (`might_have_method_descriptor`), which removes the native-override,
///   `SyntheticStub`-yield, redefine-shadow and
///   `force_native_over_real_jdk_bytecode` questions rather than reproducing
///   them;
/// * the DECLARING class is already initialised — `invoke_shared` would run
///   `<clinit>` on the way in, and this path must never be the thing that skips
///   it. (For a virtual call the receiver's own class is necessarily
///   initialised, but the resolved body may be declared on a supertype.)
/// * `site_name_is_special_cased`;
/// * the method is NOT `static` — an `invokevirtual` that resolves to a static
///   is a miscompile, not a fast path — and the decoded argument count matches
///   `receiver + declared parameters`.
///
/// Per hit, only what can change is re-tested: the declaring class's
/// `RedefineGate` (see `CALLEE_TEMPLATES_SURVIVE_REDEFINITION` and
/// `CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS`).
///
/// An instance `synchronized` callee IS served (since 2026-09-23): the
/// implicit monitor is taken by the interpreter doors' uncontended acquire and
/// released by the frame's `monitor_on_exit` — see
/// `install_and_run_cached_frame_monitored`. A contended acquire declines to
/// the by-name tail, which blocks properly.
///
/// SAFETY: `values` are the caller's already-decoded arguments with the
/// receiver at index 0; `thread` is the JIT helper's own borrow.
unsafe fn try_jit_virtual_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    receiver_ref: ObjectRef,
    receiver_class_id: ClassId,
    values: &[Value],
    // Round 16 wave 17 (lane mega5): called from `jit_invoke_dispatch`'s blind
    // virtual arm (`blind_dispatch_late_probe_rewind`), not a MIC miss.
    blind: bool,
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    if !crate::runtime::env_cache::jit_virtual_bytecode_callee() {
        return None;
    }
    if !matches!(info.invoke_kind, 0 | 2) {
        return None;
    }
    // A redefinition retires what it can change per class (the gate below);
    // see `CALLEE_TEMPLATES_SURVIVE_REDEFINITION`.
    if callee_templates_latched_off() {
        return None;
    }
    let receiver_cid = receiver_class_id.as_u32();
    if receiver_cid == 0 {
        return None;
    }
    // See the array note in the doc comment above.
    if vm.mem.heap.kind_of(receiver_ref) != cratonvm_types::ObjectKind::Object {
        return None;
    }
    let key = (info_key, receiver_cid);
    let cached = match VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        Some(hit) => hit,
        None => {
            let mut init_pending = false;
            // `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` (wave 132, lane
            // L5b): asked once per (site, receiver class), with the thread the
            // predicate's context needs; no GC point (class-store reads only).
            let user_loader_resource_body =
                compiled_user_loader_resource_body(vm, thread, info, receiver_class_id);
            let resolved = resolve_virtual_bytecode_callee(
                vm,
                info,
                receiver_class_id,
                user_loader_resource_body,
                &mut init_pending,
            );
            // See the static twin: an uninitialised declaring class is not a
            // cacheable refusal.
            if !init_pending {
                VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| {
                    c.borrow_mut().insert(key, resolved.clone());
                });
            }
            resolved
        }
    };
    let Some((cached, gate)) = cached else {
        disp_census::note(disp_census::OUT_VIRTUAL_BC_REFUSED);
        return None;
    };
    if gate.is_stale() {
        VIRTUAL_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().remove(&key);
        });
        return None;
    }
    if callee_template_declines_redefined_target(gate.generation) {
        return None;
    }
    // An instance callee takes the receiver plus its declared parameters. An
    // arity disagreement means the decode and the template describe different
    // methods, which is a refusal, never a guess.
    if values.len() != cached.num_params as usize + 1 {
        return None;
    }
    // Count the invocation the ordinary path would have counted, and stand
    // down if a compiled body now exists — see
    // `bytecode_callee_compiled_or_nominate`. Without this a callee served here
    // is never nominated, so it stays interpreted for the life of the process;
    // and a callee that IS compiled would be run interpreted anyway, which is a
    // straight loss rather than a missed win. MEASURED at 15% on
    // `probes/ExchangeProbe.java` before it was added.
    if crate::runtime::interpreter::bytecode_callee_compiled_or_nominate(vm, thread, &cached) {
        // Round 16 wave 17 (lane mega5): a compiled body exists, so the
        // caller is about to take the by-name tail. At the blind arm, let the
        // key's next call probe for it instead of re-counting to the threshold.
        blind_dispatch_late_probe_rewind(key, blind);
        disp_census::note(disp_census::OUT_VIRTUAL_BC_REFUSED);
        return None;
    }
    // `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` (wave 132, lane L5b):
    // the `Bridge` native this template stands in for counts the
    // `--compatible` `JdkUrlClassLoaders` divergence per call; so does its
    // stand-in, so the alignment census reads the same in both arms. Only an
    // admitted user-loader resource body has this class and triple here (the
    // ordinary resolver refuses those names). Off: one per-VM bool load.
    if vm.config.user_loader_resource_bytecode_compiled
        && !vm.config.is_jdk_only()
        && &*cached.class_name == "java/lang/ClassLoader"
        && is_class_loader_resource_triple(info.method_name, info.descriptor)
    {
        vm.classes
            .note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkUrlClassLoaders);
    }
    thread.refill_pools_from_shared(
        &vm.mem.operand_stack_pool,
        &vm.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    // In-place install, not a by-value build: `execute_prebuilt_frame` pushes
    // through `push_frame_and_fire_entry`, which harvests and trims the
    // retired slot, so this helper used to undo frame-slot reuse for the
    // interpreter frames beneath it once per call. See
    // `install_and_run_cached_frame`.
    //
    // The `_monitored` form serves a `synchronized` template (admitted by
    // `resolve_virtual_bytecode_callee`) with the interpreter doors' own
    // uncontended acquire, and declines on contention -- the caller then
    // continues to `invoke_or_native`, whose acquire blocks properly. A
    // non-synchronized template is exactly `install_and_run_cached_frame`.
    let served = crate::runtime::interpreter::install_and_run_cached_frame_monitored(
        vm,
        thread,
        cached,
        values,
        Some("jit-virtual-bc"),
    );
    disp_census::note(if served.is_some() {
        disp_census::OUT_VIRTUAL_BC
    } else {
        disp_census::OUT_VIRTUAL_BC_REFUSED
    });
    served
}

/// [`site_name_is_special_cased`], minus `apply`.
///
/// That list is a NAME-only filter shared with the leaf-native path, and it is
/// deliberately over-broad there: a leaf native resolves to a callback and
/// cannot afford to reason about which of `invoke_or_native`'s special cases
/// might apply. Deferring to it here cost the single most common interface
/// method in Java. MEASURED, `probes/XferProbe2.java` with
/// `CRATONVM_DBG=mic-prof`, before this narrowing:
///
/// ```text
/// virtual  out_virtual_bc=1048575  out_virtual_bc_refused=0          <- 100% served
/// iface    out_virtual_bc=0        out_virtual_bc_refused=1048575    <- 100% refused
/// ```
///
/// `apply` is the SAM of `java.util.function.Function`, i.e. every reactive
/// operator that is a CLASS rather than a lambda, which is exactly the
/// population the interpreted-callee page exists for. It earns its place on the
/// shared list through ONE rescue: `invoke_or_native` redirects
/// `apply(Ljava/lang/Object;)Ljava/lang/Object;` to
/// `applyAsInt`/`applyAsLong`/`applyAsDouble` when the dispatch class is
/// `java/util/function/To{Int,Long,Double}Function`, because those interfaces
/// declare no `apply` at all and naive dispatch raises `NoSuchMethodError`
/// (Spring/Eureka).
///
/// Narrowed to that rescue's own triple. Two independent things already make it
/// unreachable from here, and the explicit test is so a later change to either
/// does not quietly re-open it: an interface that declares no `apply` has no
/// `Code` for it, so `build_lambda_impl_cached` refuses; and a receiver whose
/// class is an interface DIFFERENT from the call site's never reaches
/// resolution at all, because `virtual_dispatch_target_cached` answers
/// `cacheable_receiver = false` for it.
fn virtual_site_name_is_special_cased(info: &JitInvokeInfo) -> bool {
    if info.method_name == "apply" {
        return info.descriptor == "(Ljava/lang/Object;)Ljava/lang/Object;"
            && matches!(
                info.class_name,
                "java/util/function/ToIntFunction"
                    | "java/util/function/ToLongFunction"
                    | "java/util/function/ToDoubleFunction"
            );
    }
    site_name_is_special_cased(info.method_name)
}

/// The once-per-(site, receiver class) half of
/// [`try_jit_virtual_bytecode_callee`]. Every refusal is cached as `None`, so
/// an ineligible pair asks these questions once.
fn resolve_virtual_bytecode_callee(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    // [`compiled_user_loader_resource_body`]'s verdict for this receiver class.
    user_loader_resource_body: bool,
    init_pending: &mut bool,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    if user_loader_resource_body {
        return resolve_user_loader_resource_body_callee(vm, info, receiver_class_id, init_pending);
    }
    if virtual_site_name_is_special_cased(info) {
        return None;
    }
    // No native may be in play for this triple, by either route.
    if vm
        .natives
        .native_methods
        .might_have_method_descriptor(info.method_name, info.descriptor)
    {
        return None;
    }
    // `build_bytecode_callee_cached` walks from the receiver's class. Where
    // that walk is not §5.4.6's selection (a private hider, a cross-package
    // package-private "override"), refuse: the by-name tail then dispatches
    // from the selected class (`receiver_selection_retarget`). The guard is
    // dropped before the build, which takes its own.
    let selection_moves = {
        let cm = vm.classes.class_manager.read();
        jit_site_owner(&cm, info).is_some_and(|owner| {
            crate::runtime::resolve::selection::receiver_dispatch_moves_to(
                cm.class_store(),
                receiver_class_id,
                owner,
                info.method_name,
                info.descriptor,
            )
            .is_some()
        })
    };
    if selection_moves {
        return None;
    }
    // Nor where selection ends in an error: the by-name tail raises it
    // (`CachedDispatchTarget::selection_error`).
    if jit_selection_errs(
        vm,
        &vm.classes.class_manager.read(),
        receiver_class_id,
        info,
    ) {
        return None;
    }
    // `admit_synchronized`: an instance `synchronized` callee is served too —
    // `try_jit_virtual_bytecode_callee` owns its monitor (see there). Refusing
    // it sent every compiled call of one down the by-name `invoke_or_native`
    // route for the life of the process: the only body the JIT publishes for
    // an `ACC_SYNCHRONIZED` method is a wrapped entry (`requires_wrapped_entry`),
    // which the compile probe above -- a RAW door -- correctly refuses, and the
    // by-name tail then interprets the method. The hot case is
    // `Throwable.fillInStackTrace()`, which the real `Throwable.<init>` calls
    // under `--jdk-only` for every throwable a compiled method constructs. The
    // same-binary A/B is the doors' own kill switch,
    // `CRATONVM_JIT_NO_DOOR_SYNC=1`: the call then takes the by-name tail
    // exactly as before.
    let (cached, gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
        true,
    )?;
    if cached.is_static {
        return None;
    }
    // `invoke_shared` would run `<clinit>` on the way in. Refuse until it has,
    // rather than becoming the path that skips it. The DECLARING class, not the
    // receiver's: the resolved body may live on a supertype.
    if !crate::vm::is_class_initialized_via_manager(vm, cached.declaring_class_id) {
        *init_pending = true;
        return None;
    }
    Some((cached, gate))
}

/// The three instance resource triples of `java/lang/ClassLoader` whose
/// registered `Bridge` natives (`classloader::cl_get_resource_essential` and
/// siblings) route a user-loader receiver to the JDK's own body. The same
/// list as the interpreter's `dispatch_virtual::class_loader_resource_triple`
/// (`pub(super)` there).
fn is_class_loader_resource_triple(method_name: &str, descriptor: &str) -> bool {
    matches!(
        (method_name, descriptor),
        ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
            | ("getResources", "(Ljava/lang/String;)Ljava/util/Enumeration;")
            | ("getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;")
    )
}

/// `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` (interpreter round i6
/// wave 132, lane L5b; the compiled half of
/// `i73-L5-proposal-user-loader-get-resource-without-the-native-hop`, default
/// OFF, effective only with `CRATONVM_USER_LOADER_RESOURCE_BYTECODE`): may a
/// compiled `invokevirtual` of a `ClassLoader` resource triple on a receiver
/// of `receiver_class_id` enter the JDK's own body through this module's
/// frame template?
///
/// Today such a call is refused by NAME here (`site_name_is_special_cased`,
/// `might_have_method_descriptor`) and takes the by-name tail, whose
/// `Bridge` native asks `jdk_only_user_loader_receiver` and, for a
/// user-defined loader or a bare `URLClassLoader`, re-enters exactly that body
/// through `invoke_special_bytecode_only`. The interpreter's virtual cache
/// skips the hop since wave 119/120; compiled callers did not, so the two
/// tiers ran different paths (same answer). The verdict is the interpreter's
/// own (`classloader::user_loader_resource_body_for_class`, a function of the
/// class: `jdk_url_class_loaders()` and the `LoaderKind` computation), asked
/// once per (site, receiver class). It reads the class store only (no
/// allocation, no load), so the caller's unrooted `values` stay valid.
///
/// `invokevirtual` only: `invokespecial` (`super.getResource(name)`) and the
/// static `getSystemResource*` forms keep the native, as in the interpreter
/// half; a null name never reaches here (both helpers raise its NPE before).
fn compiled_user_loader_resource_body(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> bool {
    if !(vm.config.user_loader_resource_bytecode_compiled
        && vm.config.user_loader_resource_bytecode)
    {
        return false;
    }
    if info.invoke_kind != 0 || !is_class_loader_resource_triple(info.method_name, info.descriptor)
    {
        return false;
    }
    let mut ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    cratonvm_native_builtins::classloader::user_loader_resource_body_for_class(
        &mut ctx,
        receiver_class_id,
    )
}

/// [`resolve_virtual_bytecode_callee`] for a receiver class
/// [`compiled_user_loader_resource_body`] admitted. The template is built
/// past the native-shadow refusal (the `ClassLoader` `Bridge` is the native
/// being stood in for), and then admitted only when it is exactly what that
/// native would have re-entered:
///
/// * the selected body is `java/lang/ClassLoader`'s own (a loader class that
///   overrides the method, or `URLClassLoader.getResourceAsStream`, keeps the
///   ordinary refusal and the by-name tail);
/// * NO native is registered for the triple on any class from the receiver up
///   to (not including) `ClassLoader` -- such a native would win the by-name
///   dispatch, so it must keep winning;
/// * the resolver's own selection refusals hold (a §5.4.6 move, a selection
///   error), and the declaring class is initialised.
///
/// Every refusal is cached as `None` by the caller, as for the ordinary
/// resolver, so an ineligible pair asks once.
fn resolve_user_loader_resource_body_callee(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    init_pending: &mut bool,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    let (cached, gate) = crate::runtime::interpreter::build_bytecode_callee_cached_admitting_native(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
    )?;
    if cached.is_static || cached.is_synchronized || &*cached.class_name != "java/lang/ClassLoader"
    {
        return None;
    }
    let selection_moves = {
        let cm = vm.classes.class_manager.read();
        jit_site_owner(&cm, info).is_some_and(|owner| {
            crate::runtime::resolve::selection::receiver_dispatch_moves_to(
                cm.class_store(),
                receiver_class_id,
                owner,
                info.method_name,
                info.descriptor,
            )
            .is_some()
        })
    };
    if selection_moves {
        return None;
    }
    if jit_selection_errs(
        vm,
        &vm.classes.class_manager.read(),
        receiver_class_id,
        info,
    ) {
        return None;
    }
    let reaches_class_loader_without_native = {
        let cm = vm.classes.class_manager.read();
        let store = cm.class_store();
        let mut probe = Some(receiver_class_id);
        let mut reached = false;
        while let Some(cid) = probe {
            if cid == cached.declaring_class_id {
                reached = true;
                break;
            }
            let Some(class) = store.get(cid) else { break };
            if vm
                .natives
                .native_methods
                .find(&class.name, info.method_name, info.descriptor)
                .is_some()
            {
                break;
            }
            probe = class.superclass;
        }
        reached
    };
    if !reaches_class_loader_without_native {
        return None;
    }
    if !crate::vm::is_class_initialized_via_manager(vm, cached.declaring_class_id) {
        *init_pending = true;
        return None;
    }
    Some((cached, gate))
}

/// The `invokespecial` twin. Same memo, same refusals; what differs is where
/// the owner comes from and why the key does not carry a receiver class.
///
/// `invokespecial` resolves against the constant-pool class and must NOT
/// virtually retarget onto the receiver's runtime class — the retarget is what
/// turns `RunLast.execute`'s super-call into a self-call and recurses until the
/// stack ends (the picocli `StackOverflowError` the `invoke_kind == 1` arm was
/// written for). `find_method_recursive` walking UP from the CP class is
/// exactly `invoke_special_shared`'s own "walk the hierarchy from the
/// CP-resolved class to the declaring class", so this path reproduces that
/// dispatch rather than approximating it, and the target is a pure function of
/// the site — hence a site-only key.
///
/// The loader question is answered the same way that arm answers it, and it is
/// the reason the resolver takes a `thread`: `info.class_name` is constant-pool
/// TEXT, and with two loaders defining that binary name a by-name lookup binds
/// the call to whichever copy the global map holds
/// (BUG-JIT-INVOKESPECIAL-LOADER-20260726). This refuses outright unless
/// `declaring_class_id` is present AND `resolve_class_loader_aware` answers, so
/// the owner is a `ClassId` resolved through the CALLER's loader or the memo is
/// never built.
///
/// A `synchronized` instance callee is served only under
/// [`special_bytecode_callee_sync_enabled`] (JIT round 16 wave 8, lane sync),
/// through the virtual twin's monitored door ([`special_bytecode_callee_monitored`]).
///
/// SAFETY: as [`try_jit_virtual_bytecode_callee`].
unsafe fn try_jit_special_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    values: &[Value],
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    if !crate::runtime::env_cache::jit_virtual_bytecode_callee() {
        return None;
    }
    if info.invoke_kind != 1 {
        return None;
    }
    if callee_templates_latched_off() {
        return None;
    }
    let cached = match SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().get(&info_key).cloned()) {
        Some(hit) => hit,
        None => {
            let mut init_pending = false;
            let resolved = resolve_special_bytecode_callee(vm, thread, info, &mut init_pending);
            // See the static twin: an uninitialised owner is not a cacheable
            // refusal.
            if !init_pending {
                SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| {
                    c.borrow_mut().insert(info_key, resolved.clone());
                });
            }
            resolved
        }
    };
    let Some((cached, gate)) = cached else {
        disp_census::note(disp_census::OUT_SPECIAL_BC_REFUSED);
        return None;
    };
    if gate.is_stale() {
        SPECIAL_BYTECODE_CALLEE_CACHE.with(|c| {
            c.borrow_mut().remove(&info_key);
        });
        return None;
    }
    if callee_template_declines_redefined_target(gate.generation) {
        return None;
    }
    // Receiver plus declared parameters, and the receiver must be a live
    // object: `invokespecial` on null is an NPE the by-name path raises, and
    // this fast path must not turn it into a frame with a null `this`.
    if values.len() != cached.num_params as usize + 1 {
        return None;
    }
    if !matches!(values.first(), Some(Value::Object(Some(_)))) {
        return None;
    }
    // Count the invocation the ordinary path would have counted, and stand
    // down if a compiled body now exists — see
    // `bytecode_callee_compiled_or_nominate`. Without this a callee served here
    // is never nominated, so it stays interpreted for the life of the process;
    // and a callee that IS compiled would be run interpreted anyway, which is a
    // straight loss rather than a missed win. MEASURED at 15% on
    // `probes/ExchangeProbe.java` before it was added.
    if crate::runtime::interpreter::bytecode_callee_compiled_or_nominate(vm, thread, &cached) {
        disp_census::note(disp_census::OUT_SPECIAL_BC_REFUSED);
        return None;
    }
    // JIT round 16 wave 8 (lane sync): the resolver admits a `synchronized`
    // template only under `CRATONVM_JIT_SPECIAL_BC_SYNC`; every other template
    // takes the unchanged path below.
    if cached.is_synchronized {
        return special_bytecode_callee_monitored(vm, thread, cached, values);
    }
    disp_census::note(disp_census::OUT_SPECIAL_BC);
    thread.refill_pools_from_shared(
        &vm.mem.operand_stack_pool,
        &vm.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    // In-place install, not a by-value build: `execute_prebuilt_frame` pushes
    // through `push_frame_and_fire_entry`, which harvests and trims the
    // retired slot, so this helper used to undo frame-slot reuse for the
    // interpreter frames beneath it once per call. See
    // `install_and_run_cached_frame`. Interpreter round i7 wave 138, lane L1
    // (`CRATONVM_JVMTI_COMPILED_CALLER_ENTRY_POP`): the compiled caller's
    // form, which re-invokes a callee its `MethodEntry` callback popped.
    Some(
        crate::runtime::interpreter::install_and_run_cached_frame_from_compiled(
            vm,
            thread,
            cached,
            values,
            Some("jit-special-bc"),
        ),
    )
}

/// `CRATONVM_JIT_SPECIAL_BC_SYNC` (JIT round 16 wave 8, lane sync; default
/// ON since the round-16 w8b A/B, `0` refuses): may the `invokespecial` callee door
/// ([`try_jit_special_bytecode_callee`]) serve a `synchronized` INSTANCE
/// template, as the virtual door has since 2026-09-23?
///
/// The arm this covers is not only `super.m()` and private calls: the JIT
/// devirtualizes an `invokevirtual` whose receiver class is `final` (or whose
/// method is) to this kind, so a compiled call of a `synchronized` method on a
/// final class (`StringBuffer`, a `final` user class) whose only published
/// body is a WRAPPED entry -- the self-locking route refuses methods with an
/// exception table or a nested `synchronized` block -- was refused here and
/// went down the by-name `invoke_special_shared` tail for the life of the
/// process: about 9-11 us a call on the Windows box against ~50-100 ns for
/// the unsynchronized twin (`R16SyncDoorCalls` `tc`/`bl`/`th`, base binary,
/// `CRATONVM_DBG=mic-prof` shows `out_special_bc_refused` = every call). The
/// virtual twin's door takes such a callee's monitor with the interpreter
/// doors' uncontended acquire, declines on contention (the tail then blocks
/// properly), and enters its wrapped body under that hold when the body is
/// one the door may enter.
///
/// Read once per site: [`resolve_special_bytecode_callee`]'s answer is
/// memoized in `SPECIAL_BYTECODE_CALLEE_CACHE`.
fn special_bytecode_callee_sync_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SPECIAL_BC_SYNC")
}

/// [`try_jit_special_bytecode_callee`]'s arm for a `synchronized` instance
/// template: `install_and_run_cached_frame_monitored`, the virtual door's
/// serve, with its census. `None` (contention) continues to the by-name tail.
fn special_bytecode_callee_monitored(
    vm: &SharedVm,
    thread: &mut JvmThread,
    cached: std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    values: &[Value],
) -> Option<Result<Option<Value>, crate::error::MethodCallFailed>> {
    thread.refill_pools_from_shared(
        &vm.mem.operand_stack_pool,
        &vm.mem.tag_pool,
        cached.max_locals as usize,
        (cached.max_stack as usize).max(16) + 8,
    );
    let served = crate::runtime::interpreter::install_and_run_cached_frame_monitored(
        vm,
        thread,
        cached,
        values,
        Some("jit-special-bc"),
    );
    disp_census::note(if served.is_some() {
        disp_census::OUT_SPECIAL_BC
    } else {
        disp_census::OUT_SPECIAL_BC_REFUSED
    });
    served
}

#[cfg(test)]
mod r16w8_sync_special_door_tests {
    /// The text of the production function starting at `sig`, up to the next
    /// top-level item.
    fn production_fn(sig: &str) -> String {
        let whole = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let start = whole.find(sig);
        assert!(start.is_some(), "`{sig}` not found");
        let body = &whole[start.unwrap_or(0)..];
        let end = body[1..]
            .find("\nfn ")
            .into_iter()
            .chain(body[1..].find("\nunsafe fn "))
            .chain(body[1..].find("\n#[cfg(test)]"))
            .min()
            .map_or(body.len(), |e| e + 1);
        body[..end].to_string()
    }

    /// `CRATONVM_JIT_SPECIAL_BC_SYNC`: the `invokespecial` door admits a
    /// `synchronized` template only through the switch, and serves one only
    /// through the monitored install (never the unlocked one), as the virtual
    /// twin does. A source witness: the door needs a live VM, a compiled caller
    /// and a final class to be reached at all.
    #[test]
    fn the_special_door_admits_a_synchronized_template_only_monitored() {
        let resolve = production_fn("\nfn resolve_special_bytecode_callee(");
        assert!(resolve.contains("build_bytecode_callee_cached("));
        assert!(resolve.contains("special_bytecode_callee_sync_enabled()"));
        assert!(
            !resolve.contains("build_lambda_impl_cached("),
            "off, the switch must leave exactly `build_lambda_impl_cached`'s answer"
        );
        let door = production_fn("\nunsafe fn try_jit_special_bytecode_callee(");
        let sync_arm = door.find("if cached.is_synchronized");
        // Wave 138 (interpreter i7, L1): the compiled caller's form of the
        // unlocked install.
        let unlocked = door.find("install_and_run_cached_frame_from_compiled(");
        assert!(sync_arm.is_some() && unlocked.is_some());
        assert!(
            sync_arm < unlocked,
            "a synchronized template must leave before the unlocked install"
        );
        let serve = production_fn("\nfn special_bytecode_callee_monitored(");
        assert!(serve.contains("install_and_run_cached_frame_monitored("));
        assert!(!serve.contains("install_and_run_cached_frame("));
    }
}

/// The once-per-site half of [`try_jit_special_bytecode_callee`].
fn resolve_special_bytecode_callee(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    init_pending: &mut bool,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    if site_name_is_special_cased(info.method_name) {
        return None;
    }
    // `<init>` and `<clinit>` are refused by name rather than by consequence.
    // `invoke_on_class_shared_inner` carries explicit carve-outs for both, an
    // `<init>` is where object initialisation and the VM's own synthetic
    // layouts meet, and neither is a throughput path worth opening that surface
    // for.
    if info.method_name.starts_with('<') {
        return None;
    }
    if vm
        .natives
        .native_methods
        .might_have_method_descriptor(info.method_name, info.descriptor)
    {
        return None;
    }
    // Loader-faithful owner or nothing — see the doc comment. A substituted
    // owner is its recorded id (`jit_substituted_owner`).
    if info.declaring_class_id == 0 {
        return None;
    }
    // The caller's `CONSTANT_Class` entry for an isolated loader's caller
    // (round 16, lane classsite, CS2-2).
    let owner = match jit_substituted_owner(vm, info) {
        Some(owner) => owner,
        None => jit_bridged_owner_entry(
            vm,
            thread,
            ClassId::new(info.declaring_class_id),
            info.class_name,
        )?,
    };
    if owner == ClassId::new(0) {
        return None;
    }
    if !crate::vm::is_class_initialized_via_manager(vm, owner) {
        *init_pending = true;
        return None;
    }
    // `admit_synchronized` under `CRATONVM_JIT_SPECIAL_BC_SYNC` only (JIT
    // round 16 wave 8, lane sync): `try_jit_special_bytecode_callee` then
    // owns the monitor, as the virtual twin does. Off, this is exactly
    // `build_lambda_impl_cached`.
    let (cached, gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        owner,
        info.method_name,
        info.descriptor,
        special_bytecode_callee_sync_enabled(),
    )?;
    if cached.is_static {
        return None;
    }
    if !crate::vm::is_class_initialized_via_manager(vm, cached.declaring_class_id) {
        *init_pending = true;
        return None;
    }
    Some((cached, gate))
}

/// Round 15 wave 5 (lane walk4; F1 of `r15w4-walk3-review-findings-FIXED-20260930.md`,
/// W3-4 of `jit-r15w4-walk3-proposals-RETIRED-20261003.md`): must the compiled `invokespecial`
/// tail run `owner`'s own (woven) bytecode for this site instead of the native
/// `invoke_special_shared` would run first?
///
/// `invoke_special_shared_impl` runs the native registered on the exact
/// constant-pool triple before anything else and asks no redefine question,
/// so once a JVMTI agent (a Mockito inline mock) had woven `owner`'s body, a
/// compiled `super.m()` / private call ran the native and skipped the advice,
/// while the interpreter's invokespecial (stackless step 1's
/// `class_redefined`) and `invoke_or_native`'s probe run the woven body.
///
/// Yes iff all hold: something was redefined; the one redefine rule is on
/// (`CRATONVM_ANCESTOR_REDEFINE_RULE`, its kill switch); the method is not a
/// class initializer (`<init>` is admitted since round 15 wave 6, see
/// [`woven_owner_body_wins`]);
/// `owner` is the constant-pool class itself (not a substituted owner, whose
/// name the native is not registered under); a native is registered on that
/// triple and is not a `SyntheticStub` that already yields (the tail runs the
/// bytecode then); `owner` itself declares the method with a body; the rule
/// reads it `Woven` (`owner_redefinition`, W2-1's predicate);
/// and the native is outside the receiver-aware forced immunity, the
/// reading `invoke_or_native` and the stackless door give a single call.
/// Every term is a read; nothing allocates or runs Java, so the caller's
/// decoded arguments stay valid for `execute`. The terms are
/// [`woven_owner_body_wins`]'s.
fn special_site_runs_woven_body(
    vm: &SharedVm,
    owner: ClassId,
    info: &JitInvokeInfo,
    values: &[Value],
) -> bool {
    let receiver = match values.first() {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    };
    woven_owner_body_wins(
        vm,
        owner,
        info.class_name,
        info.method_name,
        info.descriptor,
        receiver,
    )
}

/// Round 15 wave 5 (lane walk4): does a JVMTI agent's woven body of
/// `owner_name.method_name descriptor` (declared by `owner`, whose name it is)
/// win over the native registered on that exact triple? The question every
/// door that is about to run the native REGISTERED ON THE DECLARING CLASS
/// itself must ask: the compiled `invokespecial` tail
/// ([`special_site_runs_woven_body`]) and the bytecode arm of
/// `invoke_on_class_shared_inner` (its `override_cb` re-check, which
/// `invoke_or_native`'s tail and `invoke_special_shared`'s reach; see
/// `r15w5-walk4-woven-owner-native-override-patch-FIXED-20260930.md`).
///
/// Yes iff all hold: something was redefined; the one redefine rule is on
/// (`CRATONVM_ANCESTOR_REDEFINE_RULE`, its kill switch); the method is not
/// `<clinit>`; a native is registered on the triple and
/// is not a `SyntheticStub` that already yields (the caller runs the bytecode
/// then); `owner` is the class named `owner_name` and declares the method with
/// a body; `owner_redefinition` reads it `Woven`; and the native is outside the
/// receiver-aware forced immunity (`receiver` is `None` for a static call),
/// the reading `invoke_or_native`'s probe and the stackless door give a single
/// call. Every term is a read: nothing allocates or runs Java. The class
/// manager is taken with `try_read`; a writer answers `false` (the native, as
/// before).
///
/// `<init>` (WK4-5, round 15 wave 6 lane sweepB; R2 of
/// `r15w5-walk4-review-findings-FIXED-20260930.md`): answered like any method. The
/// interpreter's stackless door already drops a woven owner's non-immune
/// constructor native (its `class_redefined` guard reads every name), and
/// the woven owner's other methods run their bytecode in every door, so
/// keeping the native constructor in compiled code alone mixed a
/// native-built object with bytecode readers, differed between the cold and
/// the compiled calls of one site, and skipped a constructor advice (Mockito's
/// `mockConstruction`) HotSpot runs.
/// `CRATONVM_WOVEN_OWNER_INIT_BODY=0` keeps the native constructor, as
/// before (read only when the answer would be the woven `<init>`).
pub(crate) fn woven_owner_body_wins(
    vm: &SharedVm,
    owner: ClassId,
    owner_name: &str,
    method_name: &str,
    descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    if !crate::classloading::any_class_redefined() || method_name == "<clinit>" {
        return false;
    }
    let registry = &vm.natives.native_methods;
    if !registry.ancestor_redefine_rule()
        || registry
            .resolve_id(owner_name, method_name, descriptor)
            .is_none()
        || crate::runtime::interpreter::synthetic_stub_should_yield_to_real_bytecode(
            vm,
            owner_name,
            method_name,
            descriptor,
        )
    {
        return false;
    }
    {
        let Some(cm) = vm.classes.class_manager.try_read() else {
            return false;
        };
        let declares_body = cm.get_class(owner).is_some_and(|class| {
            class.name.as_ref() == owner_name
                && class
                    .find_method(method_name, descriptor)
                    .is_some_and(|m| !m.is_native() && !m.is_abstract() && m.code().is_some())
        });
        if !declares_body
            || crate::runtime::interpreter::owner_redefinition(
                &cm,
                registry,
                owner,
                method_name,
                descriptor,
            ) != crate::runtime::interpreter::OwnerRedefinition::Woven
        {
            return false;
        }
    }
    !crate::runtime::interpreter::redefine_immune_forced_native_for_receiver(
        vm,
        owner_name,
        method_name,
        descriptor,
        receiver,
    ) && (method_name != "<init>"
        || cratonvm_types::flags::runtime_flag_default_on("CRATONVM_WOVEN_OWNER_INIT_BODY"))
}

/// WK4-2 (round 15 wave 6, lane sweepB; R1/R4 of
/// `r15w5-walk4-review-findings-FIXED-20260930.md`): [`woven_owner_body_wins`] for
/// `invoke_special_shared_impl`'s first arm, which runs the native registered
/// on `class_name` BEFORE it resolves the class -- so every caller of
/// `invoke_special_shared*` (a `findSpecial` handle, a native's
/// `ctx.invoke_special`, the compiled bridge tail, the two-loader static tail
/// `invoke_static_shared_on_class`) gets the answer the compiled
/// `invokespecial` tail already gives. `pre_resolved` is the caller's
/// loader-faithful class; without it the class is looked up among the LOADED
/// classes by name (a class never loaded was never redefined), the same name
/// the impl's own `load_class_concurrent` resolves next. `receiver` is `None`
/// for a static call. One relaxed load when nothing was ever redefined; the
/// class manager is taken with `try_read` (a writer answers `false`: the
/// native, as before). `CRATONVM_ANCESTOR_REDEFINE_RULE=0` answers `false`.
pub(crate) fn special_shared_owner_body_wins(
    vm: &SharedVm,
    pre_resolved: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    receiver: Option<ObjectRef>,
) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    let owner = match pre_resolved {
        Some(owner) => Some(owner),
        None => vm
            .classes
            .class_manager
            .try_read()
            .and_then(|cm| cm.get_loaded_class_id(class_name)),
    };
    owner.is_some_and(|owner| {
        woven_owner_body_wins(vm, owner, class_name, method_name, descriptor, receiver)
    })
}

/// Is the ancestor `owner` that a site-cache entry serves the native of still
/// at the redefine generation the fill read (`owner_generation`)? Asked by the
/// hit once the VM's redefine epoch moved past the entry's stamp (round 15
/// wave 4, lane walk2; `r15w3-walk-helpers-site-cache-owner-redefine-patch-FIXED-20260930.md`).
///
/// The fill applied the one redefine rule (`redefined_ancestor_guard`, door
/// `JitSiteCache`) to the owner as it was then, so the entry stays right until
/// the owner is redefined again. Then (`false`) the entry is dropped and this
/// call declined: the funnel's walk applies the rule to this call, and the
/// next one re-resolves the site. Until then (`true`) the entry is re-stamped
/// with the epoch read here, so its later hits are one atomic load again. A
/// redefinition that ends a publication-sensitive or retire flush drops the
/// memo anyway; this covers the scoped redefinitions (the owner registers no
/// native under any method its class file declares, so
/// `scoped_redefinition_admitted`) and the window between a fill and the
/// flush. Sound by the order `site_class_was_redefined` relies on: the
/// redefinition bumps the class's generation under the class-manager writer
/// before the epoch, and the epoch is read here before the generation. Cold.
#[cold]
#[inline(never)]
fn native_site_owner_still_current(
    vm: &SharedVm,
    info_key: JitSiteKey,
    registry_generation: u32,
    owner: ClassId,
    owner_generation: u32,
) -> bool {
    let epoch = vm.jit.jit_cache.redefine_epoch();
    let Some(current) = vm
        .classes
        .class_manager
        .try_read()
        .map(|cm| cm.class_redefine_generation(owner))
    else {
        // A writer holds the class manager: decline this call, keep the entry.
        return false;
    };
    if current != owner_generation {
        NATIVE_SITE_CACHE.with(|cache| {
            cache.borrow_mut().remove(&info_key);
        });
        return false;
    }
    NATIVE_SITE_CACHE.with(|cache| {
        if let Some((generation, Some(entry))) = cache.borrow_mut().get_mut(&info_key) {
            if *generation == registry_generation {
                entry.fill_redefine_epoch = epoch;
            }
        }
    });
    true
}

/// Round 15 wave 4 (lane walk2): the site cache's owner test.
#[cfg(test)]
mod r15w4_walk2_site_owner_tests {
    use super::*;
    use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};

    fn noop_native(
        _ctx: &mut dyn cratonvm_native_api::NativeContext,
        _args: &[Value],
    ) -> crate::error::MethodCallResult {
        Ok(None)
    }

    /// A version-52 class `name` whose one method `static value()I` is
    /// `ldc #8; ireturn`, `#8` being `CONSTANT_Integer constant`.
    fn ldc_class(name: &str, constant: i32) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, name); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()I"); // #6
        utf8(&mut b, "Code"); // #7
        b.push(3); // #8 Integer
        b.extend_from_slice(&constant.to_be_bytes());
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
        let code = [0x12, 8, 0xac]; // ldc #8; ireturn
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    fn define(shared: &SharedVm, name: &str) -> Option<ClassId> {
        shared
            .classes
            .class_manager
            .write()
            .define_class_with_options(
                name,
                &ldc_class(name, 1),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            )
            .ok()
    }

    /// What `redefine_class_with` does, in its order: the class's generation
    /// under the writer, then the VM's redefine epoch. `false` when this
    /// build's class manager refuses the redefinition.
    fn redefine(shared: &SharedVm, cid: ClassId, name: &str) -> bool {
        let redefined = shared
            .classes
            .class_manager
            .write()
            .redefine_class(cid, ldc_class(name, 2), RedefineOptions::default())
            .is_ok();
        shared.jit.jit_cache.bump_redefine_epoch();
        redefined
    }

    fn stamp_of(key: JitSiteKey) -> Option<u32> {
        NATIVE_SITE_CACHE.with(|c| {
            c.borrow()
                .get(&key)
                .and_then(|(_, entry)| entry.as_ref().map(|e| e.fill_redefine_epoch))
        })
    }

    /// An entry serving an ancestor's native survives (re-stamped) the
    /// redefinition of another class, and is dropped by its owner's.
    #[test]
    fn a_site_serving_an_ancestors_native_notices_the_owners_redefinition() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let (Some(owner), Some(other)) = (
            define(&shared, "r15w4walk2/SiteOwner"),
            define(&shared, "r15w4walk2/SiteOther"),
        ) else {
            // The stripped test VM could not define them: nothing to run.
            return;
        };
        let key = jit_site_key(shared.vm_identity, 0x5157_2400);
        let generation = shared.natives.native_methods.generation();
        let epoch = shared.jit.jit_cache.redefine_epoch();
        let entry = NativeSiteCache {
            kind: LeafNativeKind::Callback,
            leaf: true,
            callback: noop_native,
            native_id: None,
            receiver_class_id: Some(0),
            capability_kind: None,
            desc: NativeSiteDescriptor::decode("()I"),
            poly: false,
            vh_value_site: 0,
            sync: None,
            native_owner: Some((owner, 0)),
            fill_redefine_epoch: epoch,
            owner_scoped: false,
            refused_for_receiver: false,
        };
        NATIVE_SITE_CACHE.with(|c| {
            c.borrow_mut().insert(key, (generation, Some(entry)));
        });
        if !redefine(&shared, other, "r15w4walk2/SiteOther") {
            return;
        }
        let moved = shared.jit.jit_cache.redefine_epoch();
        assert_ne!(moved, epoch, "the redefinition moved the epoch");
        assert!(native_site_owner_still_current(
            &shared, key, generation, owner, 0
        ));
        assert_eq!(stamp_of(key), Some(moved), "the entry was re-stamped");
        if !redefine(&shared, owner, "r15w4walk2/SiteOwner") {
            return;
        }
        assert!(!native_site_owner_still_current(
            &shared, key, generation, owner, 0
        ));
        assert!(
            NATIVE_SITE_CACHE.with(|c| c.borrow().get(&key).is_none()),
            "the owner's redefinition drops the entry"
        );
    }

    /// Round 15 wave 4 (lane walk3): `site_native_owner_scope` names the
    /// class that declares the method as a static site's owner and keeps its
    /// native until that class is redefined (its body wins then); an
    /// ancestor owner is scoped only where the probe lands on it.
    #[test]
    fn the_owner_scope_names_the_declaring_class_and_refuses_its_woven_body() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        if !shared.natives.native_methods.ancestor_redefine_rule() {
            return; // `CRATONVM_ANCESTOR_REDEFINE_RULE=0`: the rule is not applied
        }
        let (Some(cid), Some(other)) = (
            define(&shared, "r15w4walk3/OwnStatic"),
            define(&shared, "r15w4walk3/OwnOther"),
        ) else {
            return;
        };
        let info = |method: &'static str, desc: &'static str| JitInvokeInfo {
            class_name: "r15w4walk3/OwnStatic",
            method_name: method,
            descriptor: desc,
            num_jit_args: 0,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: cid.as_u32(),
            owner_class_id: 0,
        };
        let value = info("value", "()I");
        let mut owner = None;
        assert_eq!(
            site_native_owner_scope(&shared, &value, None, &mut owner),
            Some(true)
        );
        assert_eq!(
            owner,
            Some((cid, 0)),
            "the declaring class, at generation 0"
        );
        // An ancestor owner (preset, as the walk sets it): scoped only where
        // the funnel's probe lands on it.
        let mut ancestor = Some((cid, 0));
        assert_eq!(
            site_native_owner_scope(&shared, &value, None, &mut ancestor),
            Some(true)
        );
        // Round 16 (lane redefine): a landing other than the registering
        // ancestor is the owner (the funnel's probe reads it), judged now.
        let mut elsewhere = Some((other, 0));
        assert_eq!(
            site_native_owner_scope(&shared, &value, None, &mut elsewhere),
            Some(true)
        );
        assert_eq!(elsewhere, Some((cid, 0)), "the landing owns the entry");
        // Declared nowhere: an ancestor's native has nothing to weave over,
        // the start class's own native keeps the permanent tests.
        let absent = info("absent", "()V");
        let mut ancestor = Some((other, 0));
        assert_eq!(
            site_native_owner_scope(&shared, &absent, None, &mut ancestor),
            Some(true)
        );
        let mut own = None;
        assert_eq!(
            site_native_owner_scope(&shared, &absent, None, &mut own),
            Some(false)
        );
        assert_eq!(own, None);
        if !redefine(&shared, cid, "r15w4walk3/OwnStatic") {
            return;
        }
        // Redefined, with a body, not immune: the woven body wins.
        let mut owner = None;
        assert_eq!(
            site_native_owner_scope(&shared, &value, None, &mut owner),
            None
        );
        assert_eq!(owner, None);
    }

    /// Round 15 wave 5 (lane walk4; F1 of
    /// `r15w4-walk3-review-findings-FIXED-20260930.md`): the compiled `invokespecial`
    /// tail leaves the native registered on the constant-pool triple for the
    /// owner's own body once, and only once, an agent could have woven it.
    #[test]
    fn the_special_tail_runs_a_woven_owners_body_over_its_native() {
        let mut shared = SharedVm::new(crate::config::VmConfig::default());
        if !shared.natives.native_methods.ancestor_redefine_rule() {
            return; // `CRATONVM_ANCESTOR_REDEFINE_RULE=0`: the tail is unchanged
        }
        const NAME: &str = "r15w5walk4/SpecialOwner";
        shared
            .natives
            .native_methods
            .with_category(cratonvm_native_api::NativeKind::Bridge, |registry| {
                registry.register(NAME, "value", "()I", noop_native)
            });
        let (Some(cid), Some(other)) = (
            define(&shared, NAME),
            define(&shared, "r15w5walk4/SpecialOther"),
        ) else {
            return;
        };
        let info = |method: &'static str, desc: &'static str| JitInvokeInfo {
            class_name: NAME,
            method_name: method,
            descriptor: desc,
            num_jit_args: 0,
            return_type: b'I',
            invoke_kind: 1,
            declaring_class_id: cid.as_u32(),
            owner_class_id: 0,
        };
        let value = info("value", "()I");
        assert!(
            !special_site_runs_woven_body(&shared, cid, &value, &[]),
            "never redefined: the registered native runs"
        );
        if !redefine(&shared, cid, NAME) {
            return;
        }
        assert!(
            special_site_runs_woven_body(&shared, cid, &value, &[]),
            "redefined, a body, no immunity: the woven body runs"
        );
        // No native on the triple, an initializer, or an owner that is not the
        // constant-pool class (a substituted owner): the tail as before.
        assert!(!special_site_runs_woven_body(
            &shared,
            cid,
            &info("absent", "()V"),
            &[]
        ));
        assert!(!special_site_runs_woven_body(
            &shared,
            cid,
            &info("<init>", "()V"),
            &[]
        ));
        assert!(!special_site_runs_woven_body(&shared, other, &value, &[]));
    }

    /// Round 15 wave 6 (lane sweepB; WK4-2): `invoke_special_shared_impl`'s
    /// native arm asks the compiled tail's question, by the caller's resolved
    /// class or, without one, by the loaded class of that name.
    #[test]
    fn the_special_door_asks_the_woven_owner_with_or_without_a_resolved_class() {
        let mut shared = SharedVm::new(crate::config::VmConfig::default());
        if !shared.natives.native_methods.ancestor_redefine_rule() {
            return; // `CRATONVM_ANCESTOR_REDEFINE_RULE=0`: the door is unchanged
        }
        const NAME: &str = "r15w6sweepB/SpecialDoor";
        const UNLOADED: &str = "r15w6sweepB/SpecialDoorNeverLoaded";
        shared.natives.native_methods.with_category(
            cratonvm_native_api::NativeKind::Bridge,
            |registry| {
                registry.register(NAME, "value", "()I", noop_native);
                registry.register(UNLOADED, "value", "()I", noop_native);
            },
        );
        let (Some(cid), Some(other)) = (
            define(&shared, NAME),
            define(&shared, "r15w6sweepB/SpecialDoorOther"),
        ) else {
            return;
        };
        let wins = |pre: Option<ClassId>, name: &str| {
            special_shared_owner_body_wins(&shared, pre, name, "value", "()I", None)
        };
        assert!(
            !wins(None, NAME),
            "never redefined: the registered native runs"
        );
        assert!(!wins(Some(cid), NAME));
        if !redefine(&shared, cid, NAME) {
            return;
        }
        assert!(
            wins(None, NAME),
            "redefined, found by its loaded name: the body"
        );
        assert!(
            wins(Some(cid), NAME),
            "redefined, the caller's class: the body"
        );
        assert!(
            !wins(Some(other), NAME),
            "a class that is not the one the native is registered under: the native"
        );
        assert!(
            !wins(None, UNLOADED),
            "a class never loaded was never woven"
        );
    }

    /// A version-52 class `name` extending `super_name` that declares nothing.
    fn sub_class(name: &str, super_name: &str) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&5u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, name); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, super_name); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // interfaces, fields, methods, attributes
        b
    }

    /// Round 16 (lane redefine): a native registered on `R` for a method `R`
    /// inherits from `A` is owned by `A` (the funnel's probe reads `A`): the
    /// scope names `A`, keeps the native across `R`'s redefinition and
    /// refuses it once `A`'s body may be woven.
    #[test]
    fn an_inherited_registration_is_owned_by_the_declaring_ancestor() {
        let mut shared = SharedVm::new(crate::config::VmConfig::default());
        if !shared.natives.native_methods.ancestor_redefine_rule() {
            return; // `CRATONVM_ANCESTOR_REDEFINE_RULE=0`: the rule is not applied
        }
        const A: &str = "r16redefine/InhA";
        const R: &str = "r16redefine/InhR";
        shared
            .natives
            .native_methods
            .with_category(cratonvm_native_api::NativeKind::Bridge, |registry| {
                registry.register(R, "value", "()I", noop_native)
            });
        let Some(a) = define(&shared, A) else {
            return;
        };
        let Ok(r) = shared
            .classes
            .class_manager
            .write()
            .define_class_with_options(
                R,
                &sub_class(R, A),
                ClassLoaderId::Application,
                DefineClassOptions::default(),
            )
        else {
            return;
        };
        let info = JitInvokeInfo {
            class_name: R,
            method_name: "value",
            descriptor: "()I",
            num_jit_args: 0,
            return_type: b'I',
            invoke_kind: 3,
            declaring_class_id: r.as_u32(),
            owner_class_id: 0,
        };
        let mut owner = None;
        assert_eq!(
            site_native_owner_scope(&shared, &info, None, &mut owner),
            Some(true)
        );
        assert_eq!(
            owner,
            Some((a, 0)),
            "the declaring ancestor owns R's native"
        );
        let redefined_r = shared
            .classes
            .class_manager
            .write()
            .redefine_class(r, sub_class(R, A), RedefineOptions::default())
            .is_ok();
        shared.jit.jit_cache.bump_redefine_epoch();
        use crate::runtime::interpreter::{owner_redefinition, OwnerRedefinition};
        let rule_of = |cid| {
            owner_redefinition(
                &shared.classes.class_manager.read(),
                &shared.natives.native_methods,
                cid,
                "value",
                "()I",
            )
        };
        if redefined_r {
            let mut owner = None;
            assert_eq!(
                site_native_owner_scope(&shared, &info, None, &mut owner),
                Some(true),
                "R declares nothing an agent could weave"
            );
            assert_eq!(owner, Some((a, 0)));
            // The one rule (S1): R's registration is answered for A.
            let expected = if shared.natives.native_methods.redefine_inherited_owner() {
                OwnerRedefinition::Unchanged
            } else {
                OwnerRedefinition::Woven
            };
            assert_eq!(rule_of(r), expected);
        }
        if !redefine(&shared, a, A) {
            return;
        }
        if shared.natives.native_methods.redefine_inherited_owner() || redefined_r {
            assert_eq!(
                rule_of(r),
                OwnerRedefinition::Woven,
                "A's weave reaches R's registration"
            );
        }
        assert_eq!(rule_of(a), OwnerRedefinition::Woven);
        let mut owner = None;
        assert_eq!(
            site_native_owner_scope(&shared, &info, None, &mut owner),
            None,
            "A's body may carry advice: the woven body wins"
        );
    }

    /// A version-52 interface `name` whose one method is the default
    /// `public value()I` (`iconst_1; ireturn`).
    fn default_iface(name: &str) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&8u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, name); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, "value"); // #5
        utf8(&mut b, "()I"); // #6
        utf8(&mut b, "Code"); // #7
        b.extend_from_slice(&[0x06, 0x01]); // ACC_PUBLIC | ACC_INTERFACE | ACC_ABSTRACT
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
        b.extend_from_slice(&[0, 1]); // methods_count
        b.extend_from_slice(&[0x00, 0x01, 0, 5, 0, 6, 0, 1]); // public value()I
        let code = [0x04, 0xac]; // iconst_1; ireturn
        b.extend_from_slice(&[0, 7]); // "Code"
        b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 1]); // max_stack 1, max_locals 1 (this)
        b.extend_from_slice(&(code.len() as u32).to_be_bytes());
        b.extend_from_slice(&code);
        b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
        b.extend_from_slice(&[0, 0]); // no class attributes
        b
    }

    /// A version-52 class `name` extending `Object`, implementing `iface`,
    /// that declares nothing.
    fn implementing_class(name: &str, iface: &str) -> Vec<u8> {
        fn utf8(out: &mut Vec<u8>, s: &str) {
            out.push(1);
            out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
        b.extend_from_slice(&7u16.to_be_bytes()); // constant_pool_count
        utf8(&mut b, name); // #1
        b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
        utf8(&mut b, "java/lang/Object"); // #3
        b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
        utf8(&mut b, iface); // #5
        b.extend_from_slice(&[7, 0, 5]); // #6 Class #5
        b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
        b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
        b.extend_from_slice(&[0, 1, 0, 6]); // one interface: #6
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // fields, methods, attributes
        b
    }

    /// j27 wave 4 (lane DISPROWS; RD16-4): a native registered on `R` for the
    /// interface default `I.value` that `R` inherits is answered for `I` by
    /// the one rule, as the JIT site scope's landing already was: a weave of
    /// `I.value` reaches `R`'s registration (HotSpot runs the woven default
    /// for every `R` receiver). Before, `inherited_declaring_class` answered
    /// `None` for an interface default and `R`'s generation (0) kept the
    /// native.
    #[test]
    fn an_inherited_interface_default_is_owned_by_the_interface() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        if !shared.natives.native_methods.redefine_inherited_owner() {
            return; // `CRATONVM_REDEFINE_INHERITED_OWNER=0`: the old reading
        }
        const I: &str = "j27disprows/DefI";
        const R: &str = "j27disprows/DefR";
        let define_bytes = |name: &str, bytes: Vec<u8>| {
            shared
                .classes
                .class_manager
                .write()
                .define_class_with_options(
                    name,
                    &bytes,
                    ClassLoaderId::Application,
                    DefineClassOptions::default(),
                )
                .ok()
        };
        let Some(i) = define_bytes(I, default_iface(I)) else {
            return;
        };
        let Some(r) = define_bytes(R, implementing_class(R, I)) else {
            return;
        };
        use crate::runtime::interpreter::{owner_redefinition, OwnerRedefinition};
        let rule_of_r = || {
            owner_redefinition(
                &shared.classes.class_manager.read(),
                &shared.natives.native_methods,
                r,
                "value",
                "()I",
            )
        };
        assert_eq!(rule_of_r(), OwnerRedefinition::Unchanged);
        let redefined_i = shared
            .classes
            .class_manager
            .write()
            .redefine_class(i, default_iface(I), RedefineOptions::default())
            .is_ok();
        if !redefined_i {
            return;
        }
        assert_eq!(
            rule_of_r(),
            OwnerRedefinition::Woven,
            "I's weave reaches R's registration of the inherited default"
        );
    }

    /// Round 16 (lane redefine; backlog sweepB S4): a redefined-owner refusal
    /// of a virtual/interface site is keyed by its receiver. The same receiver
    /// keeps declining; another receiver re-resolves once, and a second
    /// receiver-keyed refusal becomes the site-wide one (no alternation
    /// thrash). A positive entry keeps the monomorphic contract.
    #[test]
    fn a_redefined_owner_refusal_answers_only_its_own_receiver() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let info = JitInvokeInfo {
            class_name: "r16redefine/Poly",
            method_name: "value",
            descriptor: "()I",
            num_jit_args: 1,
            return_type: b'I',
            invoke_kind: 0,
            declaring_class_id: 0,
            owner_class_id: 0,
        };
        let refusal = NativeSiteCache::refusal_for_receiver(7, &info, 3);
        assert!(refusal.refused_for_receiver);
        assert_eq!(refusal.receiver_class_id, Some(7));
        assert_eq!(refusal.native_owner, None);
        assert!(!refusal.owner_scoped);
        assert!(
            !native_site_refill_for_receiver(Some(&refusal), Some(7)),
            "its own receiver"
        );
        assert!(
            native_site_refill_for_receiver(Some(&refusal), Some(8)),
            "another receiver"
        );
        assert!(
            !native_site_refill_for_receiver(None, Some(8)),
            "a site-wide refusal"
        );
        let mut positive = refusal;
        positive.refused_for_receiver = false;
        assert!(
            !native_site_refill_for_receiver(Some(&positive), Some(8)),
            "a positive entry is monomorphic: another receiver declines"
        );
        assert!(
            native_site_refill_outcome(Some(refusal)).is_none(),
            "second refusal: site-wide"
        );
        assert!(native_site_refill_outcome(Some(positive)).is_some());
        assert!(native_site_refill_outcome(None).is_none());
        // Static / special sites (no guard) keep the site-wide refusal.
        assert!(receiver_keyed_owner_refusal(&shared, None, &info, 0).is_none());
        let keyed = receiver_keyed_owner_refusal(&shared, Some(7), &info, 0);
        assert_eq!(
            keyed.is_some(),
            shared
                .natives
                .native_methods
                .jit_site_cache_owner_redefine(),
            "receiver-keyed exactly when the owner scope is on"
        );
    }
}

// ---------------------------------------------------------------------------
// The bound downcall row (JIT round 16 wave 12, lane ffmdoor; FFC16-1,
// `CRATONVM_JIT_FFM_DOWNCALL_BIND`, default ON since wave 12, `=0` off)
// ---------------------------------------------------------------------------
//
// A compiled `MethodHandle.invokeExact` whose handle is a Panama downcall
// carrier reaches the native site cache's `MethodHandle` carrier entry
// (`method_handle_carrier_site`) and runs the erased `invokeExact` native:
// the whole door, then `pe_downcall_invoke`, then a box the caller unboxes
// (`r16-ffm-plain-downcall-costs-microseconds-FIXED-20261007.md`: ~2.4 us per call
// on Linux against HotSpot's ~8-22 ns; the constant bind refuses downcall
// carriers). With the switch on, a site whose handle the native side admits
// (`panama::jit_downcall_bind_target`: a plain carrier at its own exact type,
// where every step of the door is the identity) is bound once per handle and
// collection epoch, and its calls go through the same funnel to
// `panama::jit_downcall_bound_native` -- the plain path -- with the site's
// own argument decode and the same return handling as the carrier entry.

/// One compiled site's downcall bind ([`try_jit_ffm_downcall_bound`]).
struct FfmBindSite {
    epoch: u64,
    /// The handle's address at the fill.
    handle: u64,
    /// The call site's descriptor the admission was asked about: the key is a
    /// site info's address, which a later site may reuse.
    site: Box<str>,
    /// The handle's slot-0 `type` object at the fill (`mh_bind_slot_key`), or
    /// `None` when the admission refused the handle.
    bound_type: Option<u64>,
    /// Fills within one collection epoch for another handle; past
    /// [`FFM_BIND_MAX_CHURN`] the site stays with the door.
    churn: u32,
    /// JIT round 16 wave 16 (lane ffmlean; FFD16-1): the bound handle's
    /// precompiled call (`CRATONVM_JIT_FFM_DOWNCALL_DIRECT`), read at the fill
    /// from the handle the bind validates per call.
    direct: Option<cratonvm_native_builtins::panama::JitDowncallDirect>,
    /// `CRATONVM_DBG_FFM`: this site's first direct call was reported.
    direct_reported: std::cell::Cell<bool>,
    /// JIT round 18 wave 11 (lane ffmret): `CRATONVM_DBG_FFM`, this site's
    /// warm check reported its direct stub (`[ffm-direct] bind`), and its
    /// first declined direct call was reported (`[ffm-direct] declined`).
    bind_reported: std::cell::Cell<bool>,
    decline_reported: std::cell::Cell<bool>,
}

/// JIT round 18 wave 11 (lane ffmret;
/// `r18-ffm-direct-hit-line-prints-before-the-row-serves-20261006`):
/// `CRATONVM_DBG_FFM` lines of the direct row, printed by
/// [`ffm_downcall_direct_call`] once it knows the outcome: `[ffm-direct] hit`
/// after the site's first SERVED direct call, `[ffm-direct] declined ...
/// reason=admits|segment` after its first decline (the bound call then serves
/// it). Once each per site and thread, keyed as [`FFM_BIND_SITES`] is.
#[cold]
fn ffm_direct_outcome_report(
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    stub: &cratonvm_native_builtins::panama::JitDowncallDirect,
    declined: Option<&'static str>,
) {
    let first = FFM_BIND_SITES.with(|sites| {
        let sites = sites.borrow();
        let Some(site) = sites.get(&info_key) else {
            return false;
        };
        match declined {
            None => !site.direct_reported.replace(true),
            Some(_) => !site.decline_reported.replace(true),
        }
    });
    if !first {
        return;
    }
    match declined {
        None => eprintln!(
            "[ffm-direct] hit site {}.{}{} fn=0x{:x} critical={} segments={} returns-object={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            stub.fn_addr(),
            stub.critical(),
            stub.segments(),
            stub.returns_object()
        ),
        Some(reason) => eprintln!(
            "[ffm-direct] declined site {}.{}{} fn=0x{:x} critical={} segments={} reason={reason}",
            info.class_name,
            info.method_name,
            info.descriptor,
            stub.fn_addr(),
            stub.critical(),
            stub.segments()
        ),
    }
}

/// [`ffm_downcall_site_bound`]'s answer.
#[derive(Clone, Copy)]
enum FfmBindAnswer {
    /// Not bound: the door serves the call.
    Door,
    /// Bound, with the handle's direct call when it has one.
    Bound(Option<cratonvm_native_builtins::panama::JitDowncallDirect>),
}

/// Re-fills for another handle within one epoch after which a site gives up.
const FFM_BIND_MAX_CHURN: u32 = 8;
/// Bound (or refused) sites kept per thread.
const FFM_BIND_MAX_SITES: usize = 1024;

thread_local! {
    /// Downcall binds, by the compiled site's key.
    static FFM_BIND_SITES: std::cell::RefCell<rustc_hash::FxHashMap<JitSiteKey, FfmBindSite>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Is `handle` bound at this site ([`FfmBindSite`])? Asks the admission on a
/// miss (the cold fill). [`FfmBindAnswer::Door`]: the door serves the call.
fn ffm_downcall_site_bound(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    handle: ObjectRef,
) -> FfmBindAnswer {
    let epoch = vm.mem.address_epoch();
    let raw = handle.as_ptr() as u64;
    // `Some(answer)` on a warm hit; `None` asks the admission.
    let warm = FFM_BIND_SITES.with(|sites| {
        let sites = sites.borrow();
        let site = sites.get(&info_key)?;
        if site.churn > FFM_BIND_MAX_CHURN {
            return Some(FfmBindAnswer::Door);
        }
        if site.epoch != epoch || site.handle != raw || &*site.site != info.descriptor {
            return None;
        }
        match site.bound_type {
            None => Some(FfmBindAnswer::Door),
            // The handle the fill read, in the fill's epoch: its `type` slot
            // is still the bound one, or the admission is asked again.
            Some(word) => (mh_bind_slot_key(vm.mem.heap.get_field(handle, 0)) == word).then(|| {
                if let Some(direct) = site.direct {
                    // JIT round 18 wave 11 (lane ffmret): this is the bind's
                    // warm check, before the row decides, so the line says
                    // `bind`; `[ffm-direct] hit` / `declined` are printed by
                    // `ffm_downcall_direct_call` once the row has answered.
                    if !site.bind_reported.replace(true)
                        && cratonvm_native_builtins::panama::ffm_dbg_enabled()
                    {
                        eprintln!(
                            "[ffm-direct] bind site {}.{}{} fn=0x{:x} critical={} segments={}",
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                            direct.fn_addr(),
                            direct.critical(),
                            direct.segments()
                        );
                    }
                }
                FfmBindAnswer::Bound(site.direct)
            }),
        }
    });
    if let Some(answer) = warm {
        return answer;
    }
    ffm_downcall_site_fill(vm, thread, info, info_key, handle, epoch)
}

/// The cold half of [`ffm_downcall_site_bound`].
#[cold]
fn ffm_downcall_site_fill(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    handle: ObjectRef,
    epoch: u64,
) -> FfmBindAnswer {
    use cratonvm_native_builtins::panama::{
        jit_downcall_bind_target, jit_downcall_direct_stub, JitDowncallBind,
    };
    let raw = handle.as_ptr() as u64;
    let (answer, direct) = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let answer = jit_downcall_bind_target(&mut ctx, handle, info.descriptor);
        // JIT round 16 wave 16 (lane ffmlean): the bound handle's direct
        // call, read now from the handle the bind then validates per call.
        let direct = match answer {
            JitDowncallBind::Bound(_) => jit_downcall_direct_stub(&mut ctx, handle),
            _ => None,
        };
        (answer, direct)
    };
    let bound_type = match answer {
        // Nothing recorded: the door's call classifies the handle.
        JitDowncallBind::NotYet => return FfmBindAnswer::Door,
        JitDowncallBind::Bound(mt) => Some(mh_bind_slot_key(Value::Object(Some(mt)))),
        JitDowncallBind::Refused => None,
    };
    if cratonvm_native_builtins::panama::ffm_dbg_enabled() {
        eprintln!(
            "[ffm-bind] site {}.{}{} bound={} direct={}",
            info.class_name,
            info.method_name,
            info.descriptor,
            bound_type.is_some(),
            direct.is_some()
        );
    }
    FFM_BIND_SITES.with(|sites| {
        let mut sites = sites.borrow_mut();
        // Another handle within the epoch of the last fill is churn; a fill
        // after a collection is the same handle re-validated.
        let churn = sites.get(&info_key).map_or(0, |prev| {
            if prev.epoch == epoch && prev.handle != raw {
                prev.churn + 1
            } else {
                prev.churn
            }
        });
        if !sites.contains_key(&info_key) && sites.len() >= FFM_BIND_MAX_SITES {
            return;
        }
        sites.insert(
            info_key,
            FfmBindSite {
                epoch,
                handle: raw,
                site: Box::from(info.descriptor),
                bound_type,
                churn,
                direct,
                direct_reported: std::cell::Cell::new(false),
                bind_reported: std::cell::Cell::new(false),
                decline_reported: std::cell::Cell::new(false),
            },
        );
    });
    match bound_type {
        Some(_) => FfmBindAnswer::Bound(direct),
        None => FfmBindAnswer::Door,
    }
}

/// The bound downcall row of the native site cache's `MethodHandle` carrier
/// entry (FFC16-1, `CRATONVM_JIT_FFM_DOWNCALL_BIND`): `Some(bits)` when
/// `handle` is bound at this site and the call was made; `None` hands the call
/// to the carrier entry's door, unchanged.
///
/// The call is the carrier entry's tail with another callback: the site's own
/// argument decode, the same funnel (`safe_native_call_prevalidated_objects`:
/// pins, the native state, a downcall's upcalls and blocking), the erased
/// result through `unbox_poly_return_checked_vh` against the site (a raw
/// primitive stays as is) and `coerce_native_return_typed`, and an error
/// through `handle_jit_dispatch_error`. Not armed: the call-site channel,
/// which only the door reads.
///
/// SAFETY: as `try_jit_site_cached_native_dispatch`: `args_slice` is the
/// compiled caller's forwarded argument buffer, `handle` its validated
/// receiver.
unsafe fn try_jit_ffm_downcall_bound(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    handle: ObjectRef,
    args_slice: &[i64],
    entry: &NativeSiteCache,
) -> Option<i64> {
    // A JVMTI agent that needs the interpreter sees the door's native, as it
    // does for the constant bind's direct call.
    if !cratonvm_native_builtins::panama::jit_downcall_bind_enabled()
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        return None;
    }
    let direct = match ffm_downcall_site_bound(vm, thread, info, info_key, handle) {
        FfmBindAnswer::Door => return None,
        FfmBindAnswer::Bound(direct) => direct,
    };
    // JIT round 16 wave 16 (lane ffmlean; FFD16-1): the precompiled call,
    // when the handle has one (`CRATONVM_JIT_FFM_DOWNCALL_DIRECT`, default
    // off) and this call admits it.
    if let Some(stub) = direct {
        // `r16-ffm-plain-downcall-costs-microseconds` (FFM217-2): the epoch
        // and the handle as the bind just validated them, read before the C
        // call (an upcall may collect: the door then misses and re-binds).
        let epoch = vm.mem.address_epoch();
        if let Some(bits) =
            ffm_downcall_direct_call(vm, thread, info, info_key, args_slice, entry, &stub)
        {
            ffm_direct_door_fill(vm, info_key, handle, epoch, entry, &stub);
            // JIT round 22 (lane memory, FFM22-1): the stub as this handle's
            // compiled call, for the optimizing tier's constant sites.
            crate::runtime::interpreter::jit_bridge::ffm_const_note_fill(
                vm,
                handle,
                epoch,
                &stub,
                info.descriptor,
                entry.native_id,
                entry.desc.ret,
            );
            return Some(bits);
        }
    }
    let mut values = JitDecodedArgs::new();
    decode_dispatch_values_into_shaped(
        vm,
        info,
        args_slice,
        Some(Some(handle)),
        entry.desc.params(),
        0,
        &mut values,
    );
    note_site_cached_native_hit();
    count_jit_native_dispatch(vm, entry.native_id);
    // JIT round 16 wave 13 (lane ffmseg; FFD16-1): a plain call without the
    // funnel, when switched on (`CRATONVM_JIT_FFM_DOWNCALL_LEAN`, default off).
    if jit_ffm_downcall_lean_enabled() {
        if let Some(bits) = ffm_downcall_lean_call(vm, thread, info, &values, entry) {
            return Some(bits);
        }
    }
    let called = crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        cratonvm_native_builtins::panama::jit_downcall_bound_native,
        &values,
    );
    let result = match called {
        Ok(value) => match crate::vm::vm_exec::unbox_poly_return_checked_vh(
            vm,
            thread,
            value,
            info.descriptor,
            info.method_name,
            None,
        ) {
            Ok(unboxed) => crate::vm::coerce_native_return_typed(unboxed, entry.desc.ret),
            Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
        },
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    })
}

/// `CRATONVM_JIT_FFM_DOWNCALL_LEAN` (default OFF, opt-in; JIT round 16 wave
/// 13, lane ffmseg, proposal FFD16-1 of `jit-r16-ffmdoor-proposals-RETIRED-20261006.md`): a
/// bound downcall site whose handle is PLAIN makes its call through
/// [`ffm_downcall_lean_call`] instead of the native funnel. Read once.
fn jit_ffm_downcall_lean_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_FFM_DOWNCALL_LEAN"))
}

/// The lean bound downcall (FFD16-1, [`jit_ffm_downcall_lean_enabled`]):
/// `Some(bits)` when the plain path served the call
/// (`panama::jit_downcall_plain_only`), `None` -- nothing done -- when it
/// declined, and the caller makes the ordinary bound call through the funnel.
///
/// What the funnel (`safe_native_call_prevalidated_objects`) does that a plain
/// downcall does not need: argument forwarding and pins (the only reference is
/// the handle, which the site validated this call and which the plain path
/// does not touch after `ffi_call`), the stop-the-world probe (the compiled
/// caller polls), the collection hooks for natives that allocate (a plain call
/// allocates nothing), the diagnostics mask, the JNI pending-exception drain
/// (a plain FFM callee has no `JNIEnv`) and the debugger's running-native
/// slot. What it keeps: the `NativeRunning` span (the state the
/// stop-the-world census waits for, and the state an upcall from C leaves),
/// `catch_unwind`, the pending-return slot cleared, and the bound row's own
/// return handling. The GC-safe region, when on, is the plain path's.
/// Run an allocating direct native body once. The C call may have side
/// effects, so an allocation refusal is caught locally and never replayed.
fn jit_native_alloc_once(
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    body: impl FnOnce(&mut crate::vm::NativeContextImpl<'_>) -> Option<crate::error::MethodCallResult>,
) -> Option<crate::error::MethodCallResult> {
    let pin_base = ctx.thread.native_pin_roots.len();
    match crate::runtime::native_oom::catch_alloc_oom(|| body(ctx)) {
        Ok(answer) => answer,
        Err(oom) => {
            ctx.thread.native_pin_roots.truncate(pin_base);
            Some(Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Runtime(oom.into_runtime_error()),
            )))
        }
    }
}

fn ffm_downcall_lean_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    values: &[Value],
    entry: &NativeSiteCache,
) -> Option<i64> {
    // A debugger that reports native entries sees the funnel's.
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return None;
    }
    let span = thread_state::enter_native_state("jit::ffm_downcall_lean");
    let served = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            jit_native_alloc_once(&mut ctx, |ctx| {
                cratonvm_native_builtins::panama::jit_downcall_plain_only(ctx, values)
            })
        }))
    };
    span.restore("jit::ffm_downcall_lean:return");
    thread.native_pending_return = None;
    let called = match served {
        Ok(Some(called)) => called,
        Ok(None) => return None,
        // The C call may have run: report, never repeat it through the funnel.
        Err(_) => Err(crate::error::MethodCallFailed::InternalError(
            crate::error::VmError::Internal {
                message: "lean FFM downcall panicked".into(),
            },
        )),
    };
    let result = match called {
        Ok(value) => match crate::vm::vm_exec::unbox_poly_return_checked_vh(
            vm,
            thread,
            value,
            info.descriptor,
            info.method_name,
            None,
        ) {
            Ok(unboxed) => crate::vm::coerce_native_return_typed(unboxed, entry.desc.ret),
            Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
        },
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    })
}

/// The direct bound downcall (JIT round 16 wave 16, lane ffmlean; FFD16-1 /
/// FFS16-4, `CRATONVM_JIT_FFM_DOWNCALL_DIRECT`): `Some(bits)` when the call
/// was made through the handle's direct stub
/// (`panama::jit_downcall_direct_invoke`), `None` -- nothing done -- when
/// `panama::jit_downcall_direct_admits` declines it, and the bound call
/// proceeds as before.
///
/// Against the bound call through the funnel it drops: the `Value` decode
/// (the compiled caller's raw slots go straight into the registers; the bind
/// matched the site's descriptor to the memo's kinds), the funnel's argument
/// forwarding and pins (the handle is the only reference and is not touched
/// after the bind's check), its stop-the-world probe (the compiled caller
/// polls), ordinary funnel allocation dispatch, its JNI pending-exception
/// drain (no `JNIEnv`), five handle field reads, the security-state mutex
/// (read through its exact mirror), the plain path's checks that the fill
/// already made, libffi's `ffi_call`, and the poly-return's strict box checks
/// (primitive results retain checked unboxing/coercion; object returns are
/// materialized after session release with allocation-only reclaim).
///
/// Thread state: a `critical` handle makes no transition at all (HotSpot's
/// critical contract: no upcall, no blocking, no safepoint; the call is a leaf
/// and the thread stays exactly what it was). A non-critical one keeps what
/// the default bound call gives it: the `NativeRunning` span the census waits
/// for and an upcall leaves, the context guard an upcall reads, and -- under
/// `CRATONVM_FFM_DOWNCALL_GC_SAFE` / `_IN_NATIVE_LAZY` -- the memo path's
/// GC-safe region (such a call declines here). `catch_unwind` stays; a panic
/// after the C call is reported, never repeated.
fn ffm_downcall_direct_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    args_slice: &[i64],
    entry: &NativeSiteCache,
    stub: &cratonvm_native_builtins::panama::JitDowncallDirect,
) -> Option<i64> {
    ffm_downcall_direct_call_with(
        vm,
        thread,
        info,
        info_key,
        args_slice,
        entry.native_id,
        entry.desc.ret,
        stub,
        false,
    )
}

/// [`ffm_downcall_direct_call`], with `trim` (JIT round 22, lane memory,
/// FFM22-2, `CRATONVM_JIT_FFM_DOOR_TRIM`; only the FFM direct door passes
/// `true`, after its fill declared the slot's tally a floor): no per-call
/// native invocation tally or site-cache hit counter (a shared `lock xadd`
/// and a thread-local bump per call, read only by the census, which
/// `mark_invocations_incomplete` now labels), and no allocation-refusal frame
/// for a plain stub that answers a primitive (`jit_downcall_direct_invoke`
/// allocates nothing; `catch_unwind` still stands around it).
#[allow(clippy::too_many_arguments)]
fn ffm_downcall_direct_call_with(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    args_slice: &[i64],
    native_id: Option<cratonvm_native_api::NativeMethodId>,
    site_ret: u8,
    stub: &cratonvm_native_builtins::panama::JitDowncallDirect,
    trim: bool,
) -> Option<i64> {
    use cratonvm_native_builtins::panama::{
        jit_downcall_direct_admits, jit_downcall_direct_invoke,
    };
    // A debugger that reports native entries sees the funnel's.
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return None;
    }
    // JIT round 18 wave 11 (lane ffmret): the engagement lines report the
    // row's outcome, not the bind's.
    let dbg = cratonvm_native_builtins::panama::ffm_dbg_enabled();
    // Slot 0 is the handle (the receiver); one slot per C parameter follows.
    let raw = args_slice.get(1..)?;
    let served = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        if !jit_downcall_direct_admits(&ctx, stub, raw.len()) {
            if dbg {
                ffm_direct_outcome_report(info, info_key, stub, Some("admits"));
            }
            return None;
        }
        // A plain stub always serves; a segment stub may still decline, and
        // the bound call then counts the hit itself.
        if !stub.segments() && !trim {
            note_site_cached_native_hit();
            count_jit_native_dispatch(vm, native_id);
        }
        // JIT round 18 wave 11 (lane ffmret; FFMCOST18-2/-3): a stub that
        // answers an object allocates (or calls the allocator, Java) after the
        // C call, so even a critical one runs in the native span the funnel
        // gives every allocating native (the census waits for it).
        let span = (!stub.critical() || stub.returns_object())
            .then(|| thread_state::enter_native_state("jit::ffm_downcall_direct"));
        let allocates = !trim || stub.segments() || stub.returns_object();
        let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if !allocates {
                // SAFETY: as the plain arm below.
                return Some(Ok(Some(unsafe {
                    jit_downcall_direct_invoke(&mut ctx, stub, raw)
                })));
            }
            jit_native_alloc_once(&mut ctx, |ctx| {
            if stub.segments() {
                // JIT round 17 wave 2 (lane ffm2; FFMSEG17-1,
                // `CRATONVM_JIT_FFM_DOWNCALL_DIRECT_SEGMENTS`): a segment
                // stub. Each reference slot is validated as the funnel's
                // decode validates it (`decode_dispatch_values_into_shaped`).
                let mut resolve = |bits: i64| -> Option<ObjectRef> {
                    let bits = bits as u64;
                    if bits == 0 || bits & 0x7 != 0 || bits >= (1u64 << 48) {
                        return None;
                    }
                    crate::jit::helpers::note_membership_walk(2);
                    vm.mem.heap.is_object_address(bits as usize)
                };
                // SAFETY: as below; the segment slots are references the
                // compiled caller holds for this call.
                unsafe {
                    cratonvm_native_builtins::panama::jit_downcall_direct_segment_invoke(
                            ctx,
                        stub,
                        raw,
                        &mut resolve,
                    )
                }
            } else {
                // SAFETY: `raw` is the compiled caller's slots for this site, one
                // per parameter of the descriptor the bind matched against the
                // memo (`admits` checked the count); `stub` was filled from the
                // handle the bind validated this call.
                Some(Ok(Some(unsafe {
                        jit_downcall_direct_invoke(ctx, stub, raw)
                })))
            }
            })
        }));
        if let Some(span) = span {
            span.restore("jit::ffm_downcall_direct:return");
        }
        served
    };
    thread.native_pending_return = None;
    if stub.segments() && !trim && !matches!(served, Ok(None)) {
        note_site_cached_native_hit();
        count_jit_native_dispatch(vm, native_id);
    }
    if dbg {
        match &served {
            Ok(None) => ffm_direct_outcome_report(info, info_key, stub, Some("segment")),
            Ok(Some(Ok(_))) => ffm_direct_outcome_report(info, info_key, stub, None),
            _ => {}
        }
    }
    let value = match served {
        // A segment stub declined before any effect: the bound call serves it.
        Ok(None) => return None,
        Ok(Some(Ok(value))) => value.unwrap_or(Value::Object(None)),
        // An acquire a racing close refused (`Already closed`), raised as the
        // memo path raises it.
        Ok(Some(Err(error))) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
        // The C call may have run: report, never repeat it through the funnel.
        Err(_) => {
            let error =
                crate::error::MethodCallFailed::InternalError(crate::error::VmError::Internal {
                    message: "direct FFM downcall panicked".into(),
                });
            return Some(handle_jit_dispatch_error(vm, thread, error, info));
        }
    };
    // `r16-ffm-plain-downcall-costs-microseconds` (round 19): a primitive the
    // row already produced in the site's own return kind (the bind matched
    // the site's descriptor to the memo) is the answer as it stands; the
    // descriptor parse and the coercion below were ~4 % of a call.
    match (site_ret, value) {
        (b'J', Value::Long(v)) => return Some(v),
        (b'I', Value::Int(v)) => return Some(v as i64),
        (b'D', Value::Double(d)) => return Some(d.to_bits() as i64),
        (b'F', Value::Float(f)) => return Some(f.to_bits() as i64),
        _ => {}
    }
    let unboxed = crate::vm::vm_exec::unbox_poly_return(vm, Some(value), info.descriptor);
    Some(
        match crate::vm::coerce_native_return_typed(unboxed, site_ret) {
            Some(Value::Int(v)) => v as i64,
            Some(Value::Long(v)) => v,
            Some(Value::Float(f)) => f.to_bits() as i64,
            Some(Value::Double(d)) => d.to_bits() as i64,
            Some(Value::Object(Some(obj))) => {
                // JIT round 18 wave 11 (lane ffmret): an object a direct row
                // minted (an `ADDRESS` return's segment, a group return's
                // allocator segment) is handed over with the object-return
                // handoff root, as every other JIT native fast path does.
                thread.native_pending_return = Some(obj);
                obj.as_ptr() as i64
            }
            Some(Value::Object(None)) | None => 0,
            Some(_) => 0,
        },
    )
}

/// JIT round 22 (lane memory, FFM22-1 step (b), `CRATONVM_JIT_IR_FFM_LAZY_CALL`):
/// the thin helper an optimizing-tier `invokeExact` of a constant
/// NON-critical bound downcall calls (`ir_lower::Lowerer::emit_ffm_lazy_call`),
/// with the jit-dispatch ABI: `entry_ptr` is the VM's
/// `jit_bridge::FfmConstEntry` for the field (whose row the compile read),
/// `args_ptr` the staged arguments (the handle, then one slot per C
/// parameter), `info_ptr` the site's `JitInvokeInfo`.
///
/// What it keeps of the door and the bound row: the entry point's quiescent
/// point and pending-return clear, the SATB flush and the dispatch depth
/// guard (an up-call runs Java), the `NativeRunning` span the census waits for
/// and an up-call leaves, and the row's own call
/// (`panama::jit_downcall_direct_invoke`: the context guard, the callee-saved
/// spill and band captured in that frame, the lazy in-native transition, the
/// C call). What it drops, each proven at the row's filing instead: the door's
/// table probe and handle identity test (the field is `static final`; the row
/// is the handle's), its generations (a recompile or a redefinition replaces
/// this compiled body, not the handle), the bind and its per-call admission
/// (the arity and descriptor were matched at the compile, the switches are
/// process latches, the access verdict's two changeable words were tested by
/// the compiled caller), the invocation tally (the slot was declared a floor)
/// and the alloc-refusal frame (a plain primitive stub allocates nothing).
/// A debugger that reports native entries, or no installed JIT thread, takes
/// `jit_invoke_dispatch` with the same words.
///
/// # Safety
///
/// The compiled caller's: a live `SharedVm`, a live entry, `1 + arity` staged
/// words, a live `JitInvokeInfo`.
/// JIT round 22 (lane memory, FFM22-1 step (c),
/// `CRATONVM_JIT_IR_FFM_SEGMENT_CALL`): the thin helper an optimizing-tier
/// `invokeExact` of a constant bound downcall calls when the compiled caller
/// cannot make the call itself -- a SEGMENT stub (`MemorySegment` arguments, a
/// capture-state segment), an `ADDRESS` or by-value group return (a minted
/// segment, a `SegmentAllocator` lead slot) -- with
/// [`jit_ffm_const_lazy_call`]'s ABI. It is the FFM direct door after its
/// lookup: the reference slots forwarded (a relocation in progress moves them
/// without moving the epoch), the capability gate's twin (the row's guard
/// words, tested by the caller), the quiescent point, the SATB flush and
/// depth guard for a call that can reach Java, and the bound row's own call
/// (`ffm_downcall_direct_call_with`, trimmed), which validates every segment,
/// acquires and releases the sessions, mints the result and raises exactly
/// as the door's call does. A segment stub that declines (an unresolved
/// scope, a heap segment, `null`) has done nothing: the site's dispatch
/// helper serves it, as the door's caller would.
///
/// # Safety
///
/// As [`jit_ffm_const_lazy_call`]'s, with `1 + slot_count` staged words.
pub unsafe extern "C" fn jit_ffm_const_direct_call(
    vm_ptr: i64,
    entry_ptr: i64,
    args_ptr: i64,
    info_ptr: i64,
) -> i64 {
    let vm = &*(vm_ptr as *const SharedVm);
    let entry = &*(entry_ptr as *const crate::runtime::interpreter::jit_bridge::FfmConstEntry);
    // Cast: the staged word count, as the dispatch helper takes it.
    let num_args = (usize::from(entry.row.arity) + 1) as i64;
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args);
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
    let forwarded = forward_jit_reference_args(vm, info, args);
    let args = forwarded.as_deref().unwrap_or(args);
    // The thread borrow and the depth guard end with this block: a declined
    // call is handed to the dispatch helper, which takes its own.
    let served = 'served: {
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'served None;
        };
        cratonvm_jit::jit_ic_quiescent_point();
        thread.native_pending_return = None;
        let stub = &entry.stub;
        let _depth_guard = if stub.critical() && !stub.returns_object() {
            None
        } else {
            vm.mem.heap.flush_thread_satb();
            match enter_jit_dispatch(vm, constructor_receiver(info, args)) {
                Ok(g) => Some(g),
                Err(sentinel) => break 'served Some(sentinel),
            }
        };
        let info_key = jit_site_key(vm.vm_identity, info_ptr as usize);
        let bits = ffm_downcall_direct_call_with(
            vm,
            thread,
            info,
            info_key,
            args,
            entry.native_id,
            entry.site_ret,
            stub,
            true,
        );
        // Compiled code holds the result from here
        // (`native_return_handed_to_compiled_code`).
        thread.native_pending_return = None;
        bits
    };
    match served {
        Some(bits) => bits,
        None => jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args),
    }
}

/// JIT round 22 (lane memory, FFM22-3, `CRATONVM_JIT_IR_FFM_SEGMENT_INLINE`):
/// the helper an optimizing-tier `invokeExact` of a constant bound downcall
/// with ONE native segment parameter calls once it has checked that segment
/// itself (`ir_lower::Lowerer::emit_ffm_seg_call`), with step (b)'s ABI. After
/// the `1 + arity` staged arguments the caller staged the
/// `ir_lower::FFM_SEG_STAGE_EXTRA` words it read: the segment's `min` and
/// `length`, and its session -- a `ConfinedSession` it found open and owned by
/// this thread, with the compact offset of its `acquireCount` -- or `0` for the
/// global session (whose `acquire0` / `release0` do nothing).
///
/// What it keeps of step (c)'s call (`jit_downcall_direct_segment_invoke`):
/// the reference forwarding test, the quiescent point, the SATB flush and the
/// depth guard for a call that can reach Java, the arena-call record and the
/// row's address screens (`panama::jit_segment_call_address`), the pin of the
/// session across the call and the acquire before it and release after it,
/// the `NativeRunning` span, the row's call (`jit_downcall_direct_invoke_regs`:
/// the context guard, the spill, the lazy transition), and the result as the
/// row finishes it (`panama::jit_downcall_direct_finish`, after the release).
/// What it drops: the segment and session field reads through the native
/// context (`segment_row_read`, `p67_real_session_observe`: the row's `front`)
/// and the session's slot map. A critical call neither pins nor acquires a
/// confined session, as the row's lean handling (only this thread could close
/// it, and a critical call runs no Java). The count moves by compare-and-swap
/// as the row's does.
///
/// Anything it cannot make -- a forwarded argument, an address a screen
/// refuses, a count at its limit or moved under it, no JIT thread, a debugger
/// that reports native entries -- is handed, before any effect, to step (c)'s
/// helper (`jit_ffm_const_direct_call`) on the same staged arguments, which
/// reads everything again and raises what applies.
///
/// # Safety
///
/// As [`jit_ffm_const_lazy_call`]'s, with `1 + arity + FFM_SEG_STAGE_EXTRA`
/// staged words, the extra ones as above.
pub unsafe extern "C" fn jit_ffm_const_seg_call(
    vm_ptr: i64,
    entry_ptr: i64,
    args_ptr: i64,
    info_ptr: i64,
) -> i64 {
    use std::sync::atomic::{AtomicI32, Ordering};
    let vm = &*(vm_ptr as *const SharedVm);
    let entry = &*(entry_ptr as *const crate::runtime::interpreter::jit_bridge::FfmConstEntry);
    let arity = usize::from(entry.row.arity);
    let num_args = arity + 1;
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return jit_ffm_const_direct_call(vm_ptr, entry_ptr, args_ptr, info_ptr);
    }
    let info = &*(info_ptr as *const JitInvokeInfo);
    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args);
    let extra = std::slice::from_raw_parts(
        (args_ptr as *const i64).add(num_args),
        cratonvm_jit::ir_lower::FFM_SEG_STAGE_EXTRA,
    );
    let row = &entry.row;
    let stub = &entry.stub;
    let seg = usize::from(row.seg_param);
    // The thread borrow, the depth guard and the arena record end with this
    // block: a declined call is handed to step (c)'s helper, which takes its
    // own.
    let served = 'served: {
        // A reference the caller read through moved under it: the row reads
        // everything again from the forwarded words.
        if seg >= arity || forward_jit_reference_args(vm, info, args).is_some() {
            break 'served None;
        }
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'served None;
        };
        let _arena = cratonvm_native_builtins::panama::JitArenaCallScope::open();
        let Some(addr) =
            cratonvm_native_builtins::panama::jit_segment_call_address(extra[0], extra[1])
        else {
            break 'served None;
        };
        // A confined session of a non-critical call: its count, at the offset
        // the caller baked under its layout epoch.
        let count = (extra[2] != 0 && !stub.critical()).then(|| {
            // SAFETY: the caller found this word a compact `ConfinedSession`
            // and the offset its `acquireCount`'s, with no GC point since.
            unsafe { &*((extra[2] + extra[3]) as usize as *const AtomicI32) }
        });
        cratonvm_jit::jit_ic_quiescent_point();
        thread.native_pending_return = None;
        let calls_java = !stub.critical() || stub.returns_object();
        let _depth_guard = if calls_java {
            vm.mem.heap.flush_thread_satb();
            match enter_jit_dispatch(vm, constructor_receiver(info, args)) {
                Ok(g) => Some(g),
                Err(sentinel) => break 'served Some(sentinel),
            }
        } else {
            None
        };
        // The acquire, before any effect: only this thread moves a confined
        // session's count (`ConfinedSession.acquire0`; `release0` from another
        // thread counts `asyncReleaseCount`), so a compare-and-swap that fails
        // or a count at `MAX_FORKS` is handed to the row, which raises.
        if let Some(count) = count {
            let seen = count.load(Ordering::Acquire);
            if !(0..i32::MAX).contains(&seen)
                || count
                    .compare_exchange(seen, seen + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                break 'served None;
            }
        }
        let span = calls_java.then(|| thread_state::enter_native_state("jit::ffm_const_seg"));
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        // SAFETY: as `count` above; pinned for the call, which may collect.
        let session = count.map(|_| unsafe { ObjectRef::from_raw(extra[2] as usize as *mut u8) });
        let pin = session.map(|s| ctx.pin_native_root(s));
        let mut regs = [0u64; cratonvm_native_builtins::panama_libffi::DIRECT_STUB_MAX_ARGS];
        for (i, ((reg, &slot), &code)) in
            regs.iter_mut().zip(&args[1..]).zip(&row.params).enumerate()
        {
            *reg = if i == seg {
                addr
            } else {
                ffm_const_widen(slot as u64, code)
            };
        }
        let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `regs` are the site's `arity` C parameter slots matched
            // to the stub's kinds at the compile, the segment's the screened
            // address of a segment whose session is held for the call.
            cratonvm_native_builtins::panama::jit_downcall_direct_invoke_regs(&mut ctx, stub, &regs)
        }));
        if let (Some(s), Some(pin)) = (session, pin) {
            let now = ctx.read_native_pin(pin, s);
            ctx.unpin_native_roots(pin);
            let offset = extra[3] as usize;
            // SAFETY: the pinned session's current address; the same field.
            let count = unsafe { &*((now.as_ptr() as usize + offset) as *const AtomicI32) };
            let mut seen = count.load(Ordering::Acquire);
            while seen > 0 {
                match count.compare_exchange(seen, seen - 1, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break,
                    Err(moved) => seen = moved,
                }
            }
        }
        let answer = match called {
            Ok(rax) if row.ret != cratonvm_jit::ir_lower::FFM_CONST_REF => {
                // Cast: the register's bits, as the compiled caller's slot
                // holds them.
                Ok(ffm_const_widen(rax, row.ret) as i64)
            }
            // An `ADDRESS` return: the minted segment, after the release.
            Ok(rax) => match jit_native_alloc_once(&mut ctx, |ctx| {
                Some(cratonvm_native_builtins::panama::jit_downcall_direct_finish(ctx, stub, rax))
            }) {
                // Held through the object-return handoff root across the
                // span's end, and read back from it.
                Some(Ok(Some(Value::Object(Some(obj))))) => {
                    ctx.thread.native_pending_return = Some(obj);
                    Ok(-1)
                }
                Some(Ok(_)) | None => Ok(0),
                Some(Err(error)) => Err(error),
            },
            // The C call may have run: report, never repeat it.
            Err(_) => Err(crate::error::MethodCallFailed::InternalError(
                crate::error::VmError::Internal {
                    message: "compiled FFM segment downcall panicked".into(),
                },
            )),
        };
        if let Some(span) = span {
            span.restore("jit::ffm_const_seg:return");
        }
        let bits = match answer {
            Ok(-1) if row.ret == cratonvm_jit::ir_lower::FFM_CONST_REF => thread
                .native_pending_return
                .map_or(0, |obj| obj.as_ptr() as i64),
            Ok(bits) => bits,
            Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
        };
        // Compiled code holds the result from here
        // (`native_return_handed_to_compiled_code`).
        thread.native_pending_return = None;
        Some(bits)
    };
    match served {
        Some(bits) => bits,
        None => jit_ffm_const_direct_call(vm_ptr, entry_ptr, args_ptr, info_ptr),
    }
}

/// One register image widened by an `ir_lower::FfmConstRow` code, as the
/// compiled call's `ffm_const_widen_bytes` widens it (`0` for no value).
#[inline(always)]
fn ffm_const_widen(bits: u64, code: u8) -> u64 {
    use cratonvm_jit::ir_lower as jl;
    match code {
        jl::FFM_CONST_S8 => bits as u8 as i8 as i64 as u64,
        jl::FFM_CONST_S16 => bits as u16 as i16 as i64 as u64,
        jl::FFM_CONST_S32 => bits as u32 as i32 as i64 as u64,
        jl::FFM_CONST_U16 => u64::from(bits as u16),
        jl::FFM_CONST_W64 => bits,
        _ => 0,
    }
}

pub unsafe extern "C" fn jit_ffm_const_lazy_call(
    vm_ptr: i64,
    entry_ptr: i64,
    args_ptr: i64,
    info_ptr: i64,
) -> i64 {
    let vm = &*(vm_ptr as *const SharedVm);
    let entry = &*(entry_ptr as *const crate::runtime::interpreter::jit_bridge::FfmConstEntry);
    let arity = usize::from(entry.row.arity);
    // Cast: the staged word count, as the dispatch helper takes it.
    let num_args = (arity + 1) as i64;
    #[cfg(feature = "experimental-debug")]
    if vm.debug.debugger_gates.method_events_armed() {
        return jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args);
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        return jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args);
    };
    let info = &*(info_ptr as *const JitInvokeInfo);
    let raw = std::slice::from_raw_parts((args_ptr as *const i64).add(1), arity);
    cratonvm_jit::jit_ic_quiescent_point();
    thread.native_pending_return = None;
    vm.mem.heap.flush_thread_satb();
    let _depth_guard = match enter_jit_dispatch(vm, 0) {
        Ok(g) => g,
        Err(sentinel) => return sentinel,
    };
    let span = thread_state::enter_native_state("jit::ffm_const_lazy");
    let served = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread: &mut *thread,
        };
        let mut regs = [0u64; cratonvm_native_builtins::panama_libffi::DIRECT_STUB_MAX_ARGS];
        for ((reg, &slot), &code) in regs.iter_mut().zip(raw).zip(&entry.row.params) {
            *reg = ffm_const_widen(slot as u64, code);
        }
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `regs` are the site's `arity` C parameter slots, matched
            // to the row's kinds at the compile and widened by its codes; the
            // stub is the row's.
            cratonvm_native_builtins::panama::jit_downcall_direct_invoke_regs(
                &mut ctx,
                &entry.stub,
                &regs,
            )
        }))
    };
    span.restore("jit::ffm_const_lazy:return");
    thread.native_pending_return = None;
    match served {
        // Cast: the register's bits, as the compiled caller's slot holds them.
        Ok(rax) => ffm_const_widen(rax, entry.row.ret) as i64,
        // The C call may have run: report, never repeat it.
        Err(_) => {
            let error =
                crate::error::MethodCallFailed::InternalError(crate::error::VmError::Internal {
                    message: "compiled FFM downcall panicked".into(),
                });
            handle_jit_dispatch_error(vm, thread, error, info)
        }
    }
}

// ---------------------------------------------------------------------------
// The FFM direct door (`r16-ffm-plain-downcall-costs-microseconds`, FFM217-2;
// `CRATONVM_JIT_FFM_DIRECT_DOOR`, default ON, `=0` off)
// ---------------------------------------------------------------------------
//
// A bound downcall site with a direct stub (`ffm_downcall_direct_call`) still
// paid, per call, everything in front of the stub: `jit_invoke_virtual_mic`'s
// prologue (the panic scope, the SATB flush, `forward_jit_reference_args`, the
// raw-entry cache flush), the native site cache's probe (a `RefCell` map copy
// of a ~120-byte entry, the receiver's membership walk and class lookup, the
// capability and redefinition tests) and the bind's warm check (a second
// `RefCell` map, a descriptor string compare, the `type` slot read). The SBR5
// lean native door removed the same chain for plain callback natives; this is
// its twin for the direct downcall row.
//
// When a bound site's direct call returns, `ffm_direct_door_fill` files the
// site in a direct-mapped thread-local table keyed by the `JitInvokeInfo`
// address: the handle's address and the collection epoch the bind validated,
// the site cache entry, and the stub. `try_ffm_direct_door` (the first thing
// `jit_invoke_virtual_mic` asks) serves a call only when the receiver is that
// same handle in that same epoch (no collection since the fill, so nothing
// moved and no address was re-used: the bind's own identity test), the JIT
// cache and native registry generations are the fill's, no class was
// redefined and no agent needs the interpreter. It then runs the capability
// gate, the depth guard and SATB flush for a call that can reach Java, and
// the bound row's own `ffm_downcall_direct_call`, which still asks
// `jit_downcall_direct_admits` per call (access verdict, entry call site,
// GC-safe switches) and may decline (a segment stub), in which case nothing
// was done and the ordinary route serves the call. A compiled caller polls
// only at its own poll points, so nothing moves between its argument staging
// and this door (the lean door's argument): the raw slots need no forwarding.

/// Slots of the thread-local FFM door table. A power of two; collisions evict.
const FFM_DIRECT_DOOR_SLOTS: usize = 64;

#[derive(Clone, Copy)]
struct FfmDirectDoorSite {
    /// The `JitInvokeInfo` address.
    key: usize,
    vm_identity: usize,
    jit_gen: u64,
    registry_gen: u32,
    /// `mem.address_epoch()` when the bind validated the handle (gci i2/wr).
    epoch: u64,
    /// The bound handle's address in that epoch.
    handle: u64,
    entry: NativeSiteCache,
    stub: cratonvm_native_builtins::panama::JitDowncallDirect,
}

thread_local! {
    static FFM_DIRECT_DOOR_SITES: std::cell::RefCell<[Option<FfmDirectDoorSite>; FFM_DIRECT_DOOR_SLOTS]> =
        const { std::cell::RefCell::new([None; FFM_DIRECT_DOOR_SLOTS]) };
}

#[inline]
fn ffm_direct_door_slot(info_addr: usize) -> usize {
    (info_addr >> 4) & (FFM_DIRECT_DOOR_SLOTS - 1)
}

/// `CRATONVM_JIT_FFM_DIRECT_DOOR` (default ON, `=0` off): file bound direct
/// downcall sites for [`try_ffm_direct_door`]. Read once per VM
/// (`SharedVm::jit_ffm_direct_door`, gci i14/sx; it was a process latch).
#[inline]
fn ffm_direct_door_enabled(vm: &SharedVm) -> bool {
    vm.jit_ffm_direct_door
}

/// File a bound site whose direct call just returned (the door's fill). The
/// diagnostic modes that print per-site lines or count the MIC keep the
/// ordinary route.
#[inline(never)]
fn ffm_direct_door_fill(
    vm: &SharedVm,
    info_key: JitSiteKey,
    handle: ObjectRef,
    epoch: u64,
    entry: &NativeSiteCache,
    stub: &cratonvm_native_builtins::panama::JitDowncallDirect,
) {
    if !ffm_direct_door_enabled(vm)
        || cratonvm_native_builtins::panama::ffm_dbg_enabled()
        || mic_prof::trace_enabled()
        || mic_prof::enabled()
        || cv_trace_enabled()
        || crate::runtime::env_cache::jit_dispatch_dbg()
        || crate::runtime::env_cache::jit_putfield_diag()
    {
        return;
    }
    // FFM22-2 (`CRATONVM_JIT_FFM_DOOR_TRIM`): the door's calls skip the
    // native invocation tally, so the slot's count is declared a floor, once
    // per fill (`NativeMethodRegistry::mark_invocations_incomplete`'s
    // contract for a bypassing path).
    if vm.jit_ffm_door_trim {
        if let Some(id) = entry.native_id {
            vm.natives.native_methods.mark_invocations_incomplete(id);
        }
    }
    let info_addr = info_key.1;
    let site = FfmDirectDoorSite {
        key: info_addr,
        vm_identity: vm.vm_identity,
        jit_gen: cratonvm_jit::jit_cache_generation(),
        registry_gen: vm.natives.native_methods.generation(),
        epoch,
        handle: handle.as_ptr() as u64,
        entry: *entry,
        stub: *stub,
    };
    FFM_DIRECT_DOOR_SITES.with(|t| {
        if let Ok(mut t) = t.try_borrow_mut() {
            t[ffm_direct_door_slot(info_addr)] = Some(site);
        }
    });
}

/// May [`try_ffm_direct_door`] serve the site at `info_ptr`? Under
/// `CRATONVM_JIT_FFM_DIRECT_DOOR_EXACT_ONLY` (default ON; JIT round 21, lane
/// native, FFL16-1 / FUNNEL2-19-4) only an `invokeExact` site: the door's
/// table is filled from `try_jit_ffm_downcall_bound`, which runs only for
/// `info.method_name == "invokeExact"`, so any other site would miss anyway.
/// `=0` (or a null info) asks the door for every call, as before.
///
/// SAFETY: `info_ptr` is `0` or the extern helper's own live `JitInvokeInfo`.
#[inline(always)]
unsafe fn ffm_direct_door_may_serve(info_ptr: i64) -> bool {
    if info_ptr == 0 || !cratonvm_types::flags().jit.ffm_direct_door_exact_only {
        return true;
    }
    (*(info_ptr as *const JitInvokeInfo)).method_name == "invokeExact"
}

/// The FFM direct door: serve a compiled `invokeExact` of a bound direct
/// downcall from the thread-local table, or answer `None` -- nothing done --
/// and let `jit_invoke_virtual_mic` take its ordinary route.
///
/// SAFETY: the extern helper's own arguments (a live `SharedVm`, a live
/// `JitInvokeInfo`, the caller's argument staging block).
// JIT round 23 wave 2f (lane L1): out of line, so the frame of the helper on every
// compiled-call level (which stays live for the depth of a recursion) does not reserve
// this door's locals; it returns before the call that recurses.
#[inline(never)]
unsafe fn try_ffm_direct_door(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> Option<i64> {
    let info_addr = info_ptr as usize;
    // Copied out before anything runs: a nested call may refill the slot.
    let site = FFM_DIRECT_DOOR_SITES.with(|t| {
        t.try_borrow()
            .ok()
            .and_then(|t| t[ffm_direct_door_slot(info_addr)])
    })?;
    if site.key != info_addr || num_args < 1 || args_ptr == 0 {
        return None;
    }
    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
    let vm = &*(vm_ptr as *const SharedVm);
    if args[0] as u64 != site.handle
        || site.vm_identity != vm.vm_identity
        || site.epoch != vm.mem.address_epoch()
        || site.jit_gen != cratonvm_jit::jit_cache_generation()
        || site.registry_gen != vm.natives.native_methods.generation()
        || crate::classloading::any_class_redefined()
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        return None;
    }
    // Every reference argument forwarded first, as the MIC body forwards them
    // before it reads one (`forward_jit_reference_args`; the bound row's
    // contract is a forwarded block): the epoch above counts finished
    // collections, and a segment row handed a stale address would pin it. A
    // moved handle misses.
    //
    // JIT round 22 (lane memory, FFM22-2, `CRATONVM_JIT_FFM_DOOR_TRIM`): a
    // PLAIN stub's only reference is the handle, which the row never reads
    // (the stub is the fill's copy), and its compare below is the identity
    // test whatever copy of the address the caller holds in this epoch; so
    // only a segment stub, whose row reads its reference slots, walks them.
    let info = &*(info_ptr as *const JitInvokeInfo);
    let forwarded = if vm.jit_ffm_door_trim && !site.stub.segments() {
        None
    } else {
        forward_jit_reference_args(vm, info, args)
    };
    let args = forwarded.as_deref().unwrap_or(args);
    if args[0] as u64 != site.handle {
        return None;
    }
    let (thread, _guard) = jit_thread_mut()?;
    if let Some(kind) = site.entry.capability_kind {
        if crate::vm::check_native_dispatch_capability_for_slot(vm, kind, site.entry.native_id)
            .is_err()
        {
            return None;
        }
    }
    // The extern helper's own entry point: the previous native's returned
    // object stops being a root.
    cratonvm_jit::jit_ic_quiescent_point();
    thread.native_pending_return = None;
    // A callee that may reach Java (a non-critical upcall, an allocator or a
    // minted segment) gets what the MIC prologue gives every such call: the
    // SATB flush and the dispatch depth guard.
    let _depth_guard = if site.stub.critical() && !site.stub.returns_object() {
        None
    } else {
        vm.mem.heap.flush_thread_satb();
        match enter_jit_dispatch(vm, constructor_receiver(info, args)) {
            Ok(g) => Some(g),
            Err(sentinel) => return Some(sentinel),
        }
    };
    let info_key = jit_site_key(vm.vm_identity, info_addr);
    let bits = ffm_downcall_direct_call_with(
        vm,
        thread,
        info,
        info_key,
        args,
        site.entry.native_id,
        site.entry.desc.ret,
        &site.stub,
        vm.jit_ffm_door_trim,
    )?;
    // Compiled code holds the result from here
    // (`native_return_handed_to_compiled_code`).
    thread.native_pending_return = None;
    Some(bits)
}

/// JIT round 20 (lane fixLN): how [`try_jit_site_cached_native_dispatch_mode`]
/// treats a site-cache fill. A fill (`resolve_native_site`) can load and
/// initialise classes, which runs Java: a GC point. The ordinary callers hand
/// the door their own frame words, which the collector keeps current, so they
/// `Serve` across a fill. A caller whose words are a Rust local (the direct
/// lambda arm's native target, `lambda_site_native_call`) must never hold them
/// across one: it asks `WarmOnly` (decline instead of resolving, raising the
/// flag), then `FillOnly` with nothing but the receiver word (resolve, never
/// serve), re-materialises its words from the caller's current ones and asks
/// `WarmOnly` again.
enum NativeSiteDoorMode<'a> {
    Serve,
    WarmOnly(&'a mut bool),
    FillOnly,
}

impl NativeSiteDoorMode<'_> {
    /// `true` (and the `WarmOnly` flag raised) when this call must not run a
    /// resolution.
    fn refuses_resolution(&mut self) -> bool {
        match self {
            Self::WarmOnly(cold) => {
                **cold = true;
                true
            }
            Self::Serve | Self::FillOnly => false,
        }
    }
}

/// [`try_jit_site_cached_native_dispatch_mode`] for a caller whose
/// `args_slice` is its own GC-updated frame buffer (`NativeSiteDoorMode::Serve`).
///
/// SAFETY: as [`try_jit_site_cached_native_dispatch_mode`].
#[inline(always)]
unsafe fn try_jit_site_cached_native_dispatch(
    vm: &SharedVm,
    thread: Option<&mut JvmThread>,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    args_slice: &[i64],
    prevalidated: u64,
) -> Option<i64> {
    try_jit_site_cached_native_dispatch_mode(
        vm,
        thread,
        info,
        info_key,
        args_slice,
        prevalidated,
        NativeSiteDoorMode::Serve,
    )
}

/// The leaf-native fast path for compiled code, probed once per
/// `jit_invoke_dispatch` before any of the compile/dispatch machinery runs.
///
/// `Some(ret)` means the call was fully served here; `None` means this site is
/// not a leaf native and the caller must continue down its ordinary route.
///
/// # Why this is safe to take instead of `invoke_or_native`
///
/// `resolve_native_site` (which runs once per site) is where every
/// obligation is discharged: the invoke kind, the `invoke_or_native` special
/// cases, the `SyntheticStub` arbitration, the capability policy and the
/// `--jdk-only` admission. What is left on this path is the receiver-class
/// guard — re-tested on every hit so a site that goes polymorphic drops out —
/// and the call itself.
///
/// # Redefinition
///
/// The entry is discarded when the target class has been redefined, on the
/// same terms as the Integer cache below it, and re-resolved when the native
/// registry's generation moves (so a lazily-registered native, or one whose
/// re-registration dropped the leaf claim, is picked up rather than memoized
/// wrong — the failure the `NativeCallSite` doc calls out for `OnceLock`).
///
/// SAFETY: `args_slice` is the JIT caller's own stack buffer, already forwarded
/// by `forward_jit_reference_args` in the caller -- or, under
/// `NativeSiteDoorMode::WarmOnly` / `FillOnly`, words no resolution in this
/// call can outlive (see [`NativeSiteDoorMode`]).
unsafe fn try_jit_site_cached_native_dispatch_mode(
    vm: &SharedVm,
    // BORROWED FROM THE CALLER, never re-derived here.
    //
    // This used to do its own `jit_thread_mut()?` at the top of the leaf-native
    // arm. That is sound from `jit_invoke_dispatch`, whose own borrow is scoped
    // to an `if let` that closes before the call — and UNSOUND from
    // `jit_invoke_virtual_mic`, which takes `(thread, _jit_thread_guard)` and
    // holds the guard for the whole function. Two `&mut JvmThread` derived from
    // the same raw pointer were live at once, with no `set_jit_thread` boundary
    // between them to make the inner one a child reborrow — a genuine sibling,
    // which is exactly what `jit_thread_mut`'s debug assertion says when it
    // fires. And the outer reference is USED after the inner one is created
    // (`safe_native_call`, `handle_jit_dispatch_error`, … all take it as
    // `&mut`), so the invalidated borrow is not merely held but dereferenced.
    //
    // The `debug_assert!` is `#[cfg(debug_assertions)]`; the aliasing is not.
    // Release built the same two derivations and simply did not look.
    //
    // `Option`, not `&mut`, and consumed at exactly the point the old
    // `jit_thread_mut()?` stood — several counted bails
    // (`site_refusal::note_and_decline`) run before it, and hoisting the
    // thread requirement above them would silently stop counting a refusal
    // whenever no JIT thread is installed.
    thread: Option<&mut JvmThread>,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    args_slice: &[i64],
    // FUNNEL17-2 (`CRATONVM_JIT_NATIVE_WALK_ONCE`, JIT round 19 wave 2, lane
    // funnel2): the caller's `forward_jit_reference_args_validated` mask for
    // `args_slice`, `0` when the switch is off. A set bit stands for the heap
    // walk this door would otherwise repeat on that word.
    prevalidated: u64,
    // JIT round 20 (lane fixLN): see [`NativeSiteDoorMode`].
    mut mode: NativeSiteDoorMode<'_>,
) -> Option<i64> {
    // One-flag kill switch (`CRATONVM_JIT=-native-site-cache`). Default ON —
    // see `native_site_cache_enabled` for what it is for.
    if !native_site_cache_enabled() {
        return None;
    }
    // JIT round 17 wave 2 (lane funnel): `CRATONVM_DBG_NATIVE_DOOR_SPLIT=1`
    // times a crossing this door serves through a funnel (front / funnel /
    // tail, `NativeMethodRegistry::note_native_door_split`). One field load
    // when off.
    let split_t0 = vm
        .natives
        .native_methods
        .dbg_native_door_split()
        .then(mic_prof::now);
    // Every bail below is counted, including these pre-resolution ones. An
    // uncounted `return None` here is what made the first cut of this path
    // unexplainable: `AtomicInteger.get` showed neither a hit nor a refusal,
    // because it was arriving at `jit_invoke_virtual_mic` — a different entry
    // point — and never reaching this function at all.
    if !site_cache_admits_invoke_kind(info.invoke_kind) {
        return site_refusal::note_and_decline(0);
    }
    // r9w2-vm2: a site this thread already resolved to "not a leaf native" at
    // the current registry generation answers `None` below whatever its
    // receiver (the cached refusal is not receiver-keyed: `entry?` returns
    // before the receiver guard), so answer it BEFORE the conservative
    // receiver validation. That validation is a heap-membership walk, and
    // `jit_invoke_virtual_mic` calls this for EVERY helper-side virtual
    // dispatch, native or not — the overwhelming majority of them sites
    // whose steady state is exactly this cached refusal.
    //
    // r9w5 (typecheck5): the ONE probe of the site's memo on this path. The
    // warm-hit lookup further down used to probe the thread-local map a second
    // time for the same key; nothing between the two can write it (the
    // receiver validation is a heap read), so the copy taken here is the one
    // that lookup would have returned.
    let generation = vm.natives.native_methods.generation();
    let cached = NATIVE_SITE_CACHE.with(|cache| cache.borrow().get(&info_key).copied());
    if matches!(cached, Some((gen, None)) if gen == generation) {
        return None;
    }
    // The receiver's runtime class both selects the override to resolve and
    // guards a warm entry. A static site has neither.
    // The resolved receiver is threaded through to `decode_dispatch_values_into`
    // and to `class_id_of_validated`, so this is the ONLY conservative header
    // validation on the path. It used to be three: here, again inside
    // `VmHeap::class_id_of`, and a third time in the argument decode — 3.2 ns
    // each on a ~100 ns call (`jit_native_dispatch_profile`).
    let mut receiver = None;
    let receiver_class_id = if info.invoke_kind == 3 {
        None
    } else {
        let raw = *args_slice.first()? as u64;
        if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
            return site_refusal::note_and_decline(0);
        }
        let validated = match prevalidated_jit_ref(prevalidated, 0, raw as i64) {
            // FUNNEL17-2: the forwarding pass walked this word already.
            Some(obj) => Some(obj),
            None => {
                crate::jit::helpers::note_membership_walk(1);
                vm.mem.heap.is_object_address(raw as usize)
            }
        };
        match validated {
            Some(obj) => {
                receiver = Some(obj);
                // `invokespecial` validates and decodes its receiver like any
                // instance call, but is statically bound: its entry carries no
                // receiver guard (see `resolve_native_site`), so neither does
                // the key it is compared against.
                if info.invoke_kind == 1 {
                    None
                } else {
                    match jit_dispatch_class_validated(vm, obj) {
                        JitDispatchClass::Class(cid) => Some(cid),
                        // An array's header carries its COMPONENT's class id,
                        // so the class-id key below would take an `Integer[]`
                        // for an `Integer`: resolve `Integer.hashCode` for it,
                        // or hit an entry an `Integer` warmed, and run that
                        // native on the array (round 11 wave 12, probe
                        // `R11IcArrayReceiverAlias`). An array receiver is
                        // served by the general dispatch, which resolves
                        // `Object`'s methods for it. The cached refusal above
                        // is not receiver-keyed, so declining here records
                        // nothing.
                        JitDispatchClass::JavaLangObject => {
                            return site_refusal::note_and_decline(0);
                        }
                    }
                }
            }
            None => return site_refusal::note(0).map(|_| 0),
        }
    };

    // `warm`: served from the memo as it stood on entry, with no resolution in
    // this call (the lean funnel's precondition, `safe_native_call_jit_lean`:
    // a resolution can load and initialize classes, which runs Java).
    let mut warm = true;
    let entry = match cached {
        Some((gen, entry)) if gen == generation => entry,
        _ => {
            if mode.refuses_resolution() {
                return None;
            }
            warm = false;
            let resolved = resolve_native_site(vm, info, receiver_class_id);
            // JVMS §5.5: an `invokestatic` initializes the declaring class
            // before the native runs. A static entry is cached only once that
            // class is initialized (initialization is monotone, so the hit
            // needs no re-check); until then the site is declined WITHOUT
            // caching and the funnel tail initializes it
            // (`jit_static_owner_init_before_native`). A cached refusal would
            // stick until the registry generation moves. Fill time only.
            if info.invoke_kind == 3
                && resolved.is_some()
                && static_native_site_init_pending(vm, info)
            {
                return site_refusal::note_and_decline(11);
            }
            // JIT round 19 wave 6 (lane mega6; FUNNEL3-19-1,
            // `CRATONVM_JIT_NATIVE_SITE_REFUSAL_BY_RECEIVER`): a virtual or
            // interface site's refusal is kept for THIS receiver class only.
            // Off: the site-wide `None`, as before.
            let resolved =
                native_site_refusal_keyed_by_receiver(vm, info, receiver_class_id, resolved);
            NATIVE_SITE_CACHE.with(|cache| {
                cache.borrow_mut().insert(info_key, (generation, resolved));
            });
            resolved
        }
    };
    let receiver_guard = receiver_class_id.map(|cid| cid.as_u32());
    // Round 16 (lane redefine; backlog sweepB S4): a receiver-keyed
    // redefined-owner refusal answers only its own receiver. Another receiver
    // re-resolves the site once; what that caches is final for the generation
    // (`native_site_refill_outcome`). Virtual/interface sites only: a static
    // or `invokespecial` site never caches one. Cold.
    let entry = if native_site_refill_for_receiver(entry.as_ref(), receiver_guard) {
        if mode.refuses_resolution() {
            return None;
        }
        warm = false;
        let resolved = native_site_refill_outcome(resolve_native_site(vm, info, receiver_class_id));
        NATIVE_SITE_CACHE.with(|cache| {
            cache.borrow_mut().insert(info_key, (generation, resolved));
        });
        resolved
    } else {
        entry
    };
    let mut entry = entry?;
    // Re-check the receiver guard on every hit: the entry was resolved against
    // one runtime class, and this site may since have seen another.
    if entry.receiver_class_id != receiver_guard {
        // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-1,
        // `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY`): a second receiver class
        // has its own resolution, so a site alternating between two natives
        // is served for both (and files both in the lean table's set). Off:
        // one field load, then the old `None`.
        // JIT r20 (lane fixLN): `WarmOnly` declines where this would resolve.
        let may_resolve = !matches!(mode, NativeSiteDoorMode::WarmOnly(_));
        let second = native_site_second_receiver_entry(
            vm,
            info,
            info_key,
            generation,
            receiver_class_id,
            &mut warm,
            may_resolve,
        );
        if !warm && mode.refuses_resolution() {
            return None;
        }
        entry = second?;
        if entry.receiver_class_id != receiver_guard {
            return None;
        }
    }
    // JIT round 20 (lane fixLN): the site is settled for this receiver class;
    // a fill-only call serves nothing.
    if matches!(mode, NativeSiteDoorMode::FillOnly) {
        return None;
    }
    // JIT round 20 (lane fixLN): a resolution in this call was a GC point, so
    // the receiver validated before it is re-read from the caller's word,
    // which the collector kept current (`Serve`: the caller's frame buffer;
    // `WarmOnly` never gets here having resolved).
    if !warm {
        if let Some(recv) = receiver.as_mut() {
            *recv = ObjectRef::from_raw(*args_slice.first()? as *mut u8);
        }
    }
    // This receiver's owner was woven (slot 12, counted at fill): the funnel
    // decides every call, as for the site-wide refusal.
    if entry.refused_for_receiver {
        return None;
    }
    // Both of these open with the same process-global `any_class_redefined()`
    // load and answer `false` for every run in which nothing was ever
    // redefined. Hoisting that one test out costs a redefining run nothing and
    // saves every other run a second cross-crate call (1.2 ns each, measured).
    if crate::classloading::any_class_redefined() {
        // Round 15 wave 4 (lane walk3): an owner-scoped entry was judged by
        // the redefine rule at fill and is re-judged by its owner's generation
        // below; these two "ever redefined" tests never turn back to `false`
        // and would decline it forever.
        if !entry.owner_scoped {
            if site_class_was_redefined(vm, info, info_key) {
                return None;
            }
            if let Some(cid) = receiver_class_id {
                if class_was_redefined(vm, cid) {
                    return None;
                }
            }
        }
        // The ancestor that owns the native (round 15 wave 4, lane walk2):
        // one atomic load while nothing was redefined since the fill.
        if let Some((owner, owner_generation)) = entry.native_owner {
            if entry.fill_redefine_epoch != vm.jit.jit_cache.redefine_epoch()
                && vm.natives.native_methods.ancestor_redefine_rule()
                && !native_site_owner_still_current(
                    vm,
                    info_key,
                    generation,
                    owner,
                    owner_generation,
                )
            {
                return None;
            }
        }
    }

    // CAPABILITY GATE. Same gate, same point in the call, as
    // `invoke_or_native`'s "dispatch site 1 of 3": the last place before the
    // native actually runs, because a capability is only exercised by a native
    // that executes. Reached only by the classified triples — process spawn,
    // library load, `Unsafe`, Panama, file and socket I/O — and skipped by a
    // not-taken branch for everything else.
    //
    // A refusal returns `None` rather than propagating the error: this is a
    // fast path whose contract is "served, or not served here", and the funnel
    // it declines to raises the identical error from the identical gate. That
    // costs a refused call one extra traversal and keeps the fast path unable
    // to invent a failure the slow path would not have produced.
    if let Some(kind) = entry.capability_kind {
        if crate::vm::check_native_dispatch_capability_for_slot(vm, kind, entry.native_id).is_err()
        {
            return site_refusal::note_and_decline(3);
        }
    }
    // The old `let (thread, _guard) = jit_thread_mut()?;` stood here. Same
    // position, same early-out, but the reference is the caller's.
    let thread = thread?;
    // `--jdk-only` (interpreter round i1 wave 37, lane L4): an invoke-exact
    // `VarHandle` refuses a call site that is not its access mode type, before
    // the in-funnel fast paths below serve it. One relaxed load and a field
    // read. The thin helpers' synthetic sites carry a stand-in descriptor;
    // they are judged against the one their compiled caller's invoke names
    // (wave 49, `varhandle_stand_in_site_descriptor`).
    if entry.poly {
        if let Some(exact_slot) = crate::runtime::interpreter::var_handle_exact_gate(vm) {
            if let Some(vh) = receiver {
                if crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(info.class_name)
                    && crate::runtime::interpreter::var_handle_receiver_is_exact(vm, vh, exact_slot)
                {
                    if let Some(error) = jit_var_handle_exact_judgement(vm, thread, info, vh) {
                        return Some(handle_jit_dispatch_error(vm, thread, error, info));
                    }
                }
            }
        }
    }
    // JIT round 16 wave 12 (lane ffmdoor; FFC16-1): a `MethodHandle` carrier
    // entry whose handle is a bound plain downcall carrier calls the plain
    // path without the `invokeExact` door (`try_jit_ffm_downcall_bound`;
    // `CRATONVM_JIT_FFM_DOWNCALL_BIND`, default on: one load).
    if entry.poly && entry.sync.is_none() && info.method_name == "invokeExact" {
        if let Some(handle) = receiver {
            if let Some(bits) =
                try_jit_ffm_downcall_bound(vm, thread, info, info_key, handle, args_slice, &entry)
            {
                return Some(bits);
            }
        }
    }
    if entry.kind == LeafNativeKind::ThreadCurrentThread {
        // The mirror is a per-thread GC root the collector remaps, and handing
        // it to the caller roots it again with no allocation in between. When
        // it does not exist yet, fall through: the ordinary dispatcher runs
        // `current_thread_object`'s allocating slow path that builds it.
        let obj = thread.java_thread_obj?;
        note_leaf_native_hit();
        count_jit_native_dispatch(vm, entry.native_id);
        // Object-return handoff root, same contract as every other JIT native
        // fast path (see `jit_integer_value_of_direct`).
        thread.native_pending_return = Some(obj);
        return Some(obj.as_ptr() as i64);
    }
    // r9w4 vm4 (NOTES-w3-collections3.md): `ArrayList.get(int)` / `size()` on
    // an ordinary ArrayList, answered without the argument decode and the
    // `safe_native_call_*` wrapper. A decline falls through to the ordinary
    // callback path below, which runs the same native.
    //
    // JDK-only: nothing to ask, and until 2026-09-22 this stood aside anyway.
    //
    // The entry exists only because `admit_jit_fast_native_resolved` admitted
    // this native under the active policy — that IS the policy question, asked
    // once at cache-fill time, and the probe below is not a second answer to
    // it. `cratonvm_native_collections::jit_arraylist_get` is `native_al_get`'s
    // own hot path, exported: same `al_fast_state`, same bounds test, same
    // `get_array_element`. Standing aside sent the call to `native_al_get`,
    // which ran those exact lines one `safe_native_call_*` wrapper later.
    //
    // That is the shape
    // `docs/feature-designs/jit-ir-optimizations.md`
    // is about — a refusal whose tail runs what it refused — at a third door,
    // and the old comment here cited the thin helpers' guard as its precedent.
    // That guard now asks whether the tail would refuse; this one has no tail
    // question left to ask, because the callback it would fall to is already
    // admitted and is the same code. So the term is gone rather than
    // rephrased.
    //
    // Still not counted in `jit_direct_helper_jdk_only_refusals`, for the
    // reason that counter's doc gives: nothing is refused here in either mode.
    if entry.sync.is_none()
        && matches!(
            entry.kind,
            LeafNativeKind::ArrayListGet | LeafNativeKind::ArrayListSize | LeafNativeKind::ChmGet
        )
    {
        if let Some(recv) = receiver {
            // FUNNEL3-19-2 (lane mega6): `None` with the switch off.
            let validated_arg =
                site_leaf_probe_validated_arg(vm, entry.kind, warm, prevalidated, args_slice);
            if let Some(bits) =
                try_jit_arraylist_leaf(vm, thread, entry.kind, recv, args_slice, validated_arg)
            {
                if entry.leaf {
                    note_leaf_native_hit();
                } else {
                    note_site_cached_native_hit();
                }
                JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.collection_leaf_hits));
                count_jit_native_dispatch(vm, entry.native_id);
                // FUNNEL2-19-2 (lane funnel3): the next call of this site
                // takes the lean door's probe.
                if vm.natives.native_methods.jit_native_site_door_leaf_kinds() {
                    lean_native_site_fill(vm, info, info_key.1, &entry, generation);
                }
                return Some(bits);
            }
        }
    }
    // `Integer.hashCode()` / `Integer.equals(Object)` (round 11 wave 11): the
    // same admitted-entry argument as the collection probes above, for the two
    // natives `HashMap.hash` / `HashMap.getNode` reach on every Integer-keyed
    // `put`/`get`. Not a collection hit, so it does not feed that counter; the
    // site-cached hit and the §4 census count it as before.
    if entry.sync.is_none()
        && matches!(
            entry.kind,
            LeafNativeKind::IntWrapperHashCode
                | LeafNativeKind::IntWrapperEquals
                | LeafNativeKind::ObjectGetClass
                | LeafNativeKind::ReferenceRefersTo
                | LeafNativeKind::ClassIsInstance
        )
    {
        if let Some(recv) = receiver {
            // FUNNEL3-19-2 (lane mega6): `None` with the switch off.
            let validated_arg =
                site_leaf_probe_validated_arg(vm, entry.kind, warm, prevalidated, args_slice);
            if let Some(bits) =
                try_jit_arraylist_leaf(vm, thread, entry.kind, recv, args_slice, validated_arg)
            {
                if entry.leaf {
                    note_leaf_native_hit();
                } else {
                    note_site_cached_native_hit();
                }
                count_jit_native_dispatch(vm, entry.native_id);
                // FUNNEL2-19-2 (lane funnel3), as for the collection kinds.
                if vm.natives.native_methods.jit_native_site_door_leaf_kinds() {
                    lean_native_site_fill(vm, info, info_key.1, &entry, generation);
                }
                return Some(bits);
            }
        }
    }
    // `VarHandle` read modes on an ordinary instance field are a field load
    // wearing a native's clothes. Serving them here rather than through the
    // funnel is what takes `VarHandle.get` off the boxing round trip — see
    // `try_varhandle_instance_field_read`.
    if entry.poly && entry.sync.is_none() {
        // An element of a real `int[]` / `long[]` handle
        // (`AtomicIntegerArray`, `AtomicLongArray`, netty's `MpscIntQueue`):
        // one hardware access instead of the generic VarHandle native.
        if let Some(bits) = try_varhandle_array_element(vm, info, args_slice) {
            note_site_cached_native_hit();
            JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.vh_array_element_served));
            count_jit_native_dispatch(vm, entry.native_id);
            return Some(bits);
        }
        if let Some(bits) = try_varhandle_byte_array_view_read(vm, info, args_slice)
            .or_else(|| try_varhandle_byte_buffer_view_access(vm, info, args_slice, thread))
            .or_else(|| try_varhandle_instance_field_read(vm, info, args_slice, thread))
        {
            note_site_cached_native_hit();
            if info.method_name == "get" {
                JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.vh_field_read_hits));
            }
            count_jit_native_dispatch(vm, entry.native_id);
            return Some(bits);
        }
        // SBR3 lane ffmperf: a real `SegmentVarHandle` over a real native
        // segment (see `try_varhandle_segment_access`).
        if let Some(bits) = try_varhandle_segment_access(vm, info, args_slice, thread) {
            note_site_cached_native_hit();
            count_jit_native_dispatch(vm, entry.native_id);
            return Some(bits);
        }
        // The write direction had no equivalent, which is why a funnelled CAS
        // cost ~182 ns more than a bound call while a funnelled READ cost ~49.
        // See `try_varhandle_instance_field_cas`.
        if varhandle_cas_funnel_fast_enabled() {
            if let Some(bits) = try_varhandle_instance_field_cas(vm, info, args_slice) {
                note_site_cached_native_hit();
                JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.vh_field_cas_served));
                count_jit_native_dispatch(vm, entry.native_id);
                return Some(bits);
            }
            JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.vh_field_cas_declined));
        }
    }
    // SBR5 lane ncall: a plain callback entry that passed every gate above is
    // remembered in the lean door's table, so the NEXT call of this site is
    // served by `try_lean_native_door` at the top of the extern helper.
    lean_native_site_fill(vm, info, info_key.1, &entry, generation);
    // FUNNEL17-2: the mask is good only up to the first possible GC point. A
    // resolution in this call (`warm == false`) can load classes, so the
    // parameters are walked again after one, as before the switch existed.
    let prevalidated = if warm { prevalidated } else { 0 };
    // `JitDecodedArgs::new()` and not `with_capacity(args_slice.len())`: the
    // latter is an outlined call whose ~144-byte return the caller has to
    // materialise, and it measured 10.3 ns for a zero-argument decode that has
    // nothing to do (`jit_native_dispatch_profile`). This is the buffer the
    // decode fills in place.
    let mut values = JitDecodedArgs::new();
    decode_dispatch_values_into_shaped(
        vm,
        info,
        args_slice,
        receiver.map(Some),
        entry.desc.params(),
        prevalidated,
        &mut values,
    );
    if entry.leaf {
        note_leaf_native_hit();
    } else {
        note_site_cached_native_hit();
    }
    count_jit_native_dispatch(vm, entry.native_id);
    // `decode_dispatch_values` heap-validated every object argument, which is
    // exactly the precondition `..._prevalidated_objects` names — and it is the
    // same call `invoke_or_native` would have made after its cascade, so the
    // non-leaf path here differs from the generic route only by what it did
    // NOT re-resolve.
    // Nothing between this and the native call runs Java, so no nested
    // access can consume the cell.
    if entry.vh_value_site != 0 {
        cratonvm_native_api::poly_call_site::arm_vh_param_site(entry.vh_value_site);
    }
    // Round 13 wave 10 (lane callcost4): a `MethodHandle` carrier entry
    // (`method_handle_carrier_site`, the only `poly` entry under these two
    // names) publishes the call-site descriptor its door takes first, as the
    // signature-polymorphic block does (`arms_poly_call_site`).
    if entry.poly && crate::vm::vm_exec::arms_poly_call_site(info.class_name, info.method_name) {
        // Interpreter round i5 wave 107, lane L4 (`CRATONVM_MH_ARMED_CALLER`,
        // default ON in the consumer since the w107 A/B): the site's own class rides along, so
        // a warm before-linker lane need not walk the compiled frames for it.
        cratonvm_native_api::poly_call_site::arm_with_caller(
            info.descriptor,
            info.declaring_class_id,
        );
    }
    // Round 15 wave 2 hand-back (lane vh2; VH2-3): the handle's variable kind
    // for the reference-return cast below, read while `receiver` is current
    // (the native may move it). `None` for every site that is not a narrower-
    // than-`Object` reference read.
    let vh_variable_is_reference = if entry.poly {
        crate::vm::vm_exec::vh_reference_return_variable(
            vm,
            thread,
            info.method_name,
            info.descriptor,
            receiver,
        )
    } else {
        None
    };
    // JIT round 17 wave 2 (lane funnel; RD16-3 = BD14-1 = STR16-P4):
    // `CRATONVM_JIT_NATIVE_LEAN_DOOR=1` sends a plain non-leaf entry served
    // WARM (no resolution in this call, so nothing since the caller's
    // `forward_jit_reference_args` and the decode above was a GC point) through
    // `safe_native_call_jit_lean`, which skips the argument re-forward and the
    // interpreter-only native-array GC hook and is otherwise the funnel.
    // Sync, signature-polymorphic, `VarHandle`-armed and capability-gated
    // entries keep their own route.
    let lean = warm
        && !entry.leaf
        && entry.sync.is_none()
        && !entry.poly
        && entry.vh_value_site == 0
        && entry.capability_kind.is_none()
        && vm.natives.native_methods.jit_native_lean_door();
    // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-3,
    // `CRATONVM_JIT_NATIVE_FRESH_RESULT`): a plain non-leaf native proven to
    // return a fresh live base heals its result by the forwarding read alone.
    let fresh_result = !entry.leaf
        && entry.sync.is_none()
        && !entry.poly
        && entry.vh_value_site == 0
        && vm.natives.native_methods.jit_native_fresh_result()
        && cratonvm_native_builtins::is_fresh_object_result_native(entry.callback);
    let split_t1 = split_t0.map(|_| mic_prof::now());
    let called = if let Some(sync) = entry.sync {
        // `ACC_SYNCHRONIZED`: the full funnel under the method's monitor, as
        // `invoke_or_native` runs it (`by_name_native_call`). No shortcut
        // above served it.
        crate::runtime::interpreter::safe_native_call_synchronized(
            vm,
            thread,
            entry.callback,
            &values,
            Some(sync),
        )
    } else if entry.leaf {
        crate::vm::safe_native_call_leaf(vm, thread, entry.callback, &values)
    } else if fresh_result {
        crate::vm::safe_native_call_jit_fresh_result(vm, thread, entry.callback, &values, lean)
    } else if lean {
        crate::vm::safe_native_call_jit_lean(vm, thread, entry.callback, &values)
    } else {
        crate::vm::safe_native_call_prevalidated_objects(vm, thread, entry.callback, &values)
    };
    let split_t2 = split_t0.map(|_| mic_prof::now());
    // A native that did not take the cell (not an access mode, or a door that
    // served before its take) must not leave it for a later, unarmed native.
    let mut called = called;
    if entry.vh_value_site != 0 {
        cratonvm_native_api::poly_call_site::clear_vh_value_site();
        // An `asType` refusal names this site's descriptor, as the
        // interpreter's doors do (interpreter round i2 wave 53, lane L4).
        called = match called {
            Err(failed @ crate::error::MethodCallFailed::ExceptionThrown(_)) => {
                Err(jit_vh_astype_refusal_site_text(vm, thread, failed, info))
            }
            other => other,
        };
    }
    let result = match called {
        // A signature-polymorphic native returns the erased `Object` the
        // registration promises, so it needs the call site's own descriptor
        // applied — including the `WrongMethodTypeException` rule
        // `unbox_poly_return_checked` owns. That call can itself fail (it
        // raises the exception), so it is folded into the same error arm the
        // native's own failure takes.
        Ok(value) if entry.poly => {
            match crate::vm::vm_exec::unbox_poly_return_checked_vh(
                vm,
                thread,
                value,
                info.descriptor,
                info.method_name,
                vh_variable_is_reference,
            ) {
                Ok(unboxed) => crate::vm::coerce_native_return_typed(unboxed, entry.desc.ret),
                Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
            }
        }
        Ok(value) => crate::vm::coerce_native_return_typed(value, entry.desc.ret),
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    let bits = match result {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    };
    // `CRATONVM_DBG_NATIVE_DOOR_SPLIT`: the three spans of a crossing served
    // here (a throwing native's crossing is not counted). `front` starts at
    // this function's entry, so the dispatch helper's own prologue is not in
    // it (`CRATONVM_DBG_MIC_PROF`'s `cyc_disp_total` / `cyc_mic_total` cover
    // the whole helper).
    if let (Some(t0), Some(t1), Some(t2)) = (split_t0, split_t1, split_t2) {
        vm.natives.native_methods.note_native_door_split(
            lean,
            t1.wrapping_sub(t0),
            t2.wrapping_sub(t1),
            mic_prof::now().wrapping_sub(t2),
        );
    }
    Some(bits)
}

/// FUNNEL2-19-1 (JIT round 19 wave 3, lane funnel3;
/// `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY`): the site cache's answer for a
/// virtual/interface site's SECOND receiver class, when the site's own entry
/// guards another class.
///
/// `NATIVE_SITE_CACHE` holds one resolution per site. A site whose receiver
/// alternates between two classes (`o.hashCode()` over `Integer`s and
/// `String`s, a bimorphic `List.get`) served its first class and sent every
/// call with the other one to the generic dispatcher. This memo, keyed by
/// `(site, receiver class)` and stamped with the registry generation like the
/// site's own, resolves that class once (`resolve_native_site`, the same
/// resolution the site's first call ran) and keeps the answer, a refusal
/// included. `None` when the switch is off, for a static or `invokespecial`
/// site, or a refusal. `warm` is cleared when this call resolved.
#[inline(never)]
fn native_site_second_receiver_entry(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
    generation: u32,
    receiver_class_id: Option<ClassId>,
    warm: &mut bool,
    // JIT round 20 (lane fixLN): `false` declines (clearing `warm`, resolving
    // nothing) where this would resolve (`NativeSiteDoorMode::WarmOnly`).
    may_resolve: bool,
) -> Option<NativeSiteCache> {
    if !vm.natives.native_methods.jit_native_site_door_two_way()
        || !matches!(info.invoke_kind, 0 | 2)
    {
        return None;
    }
    let key = (info_key, receiver_class_id?.as_u32());
    match NATIVE_SITE_CACHE_ALT.with(|cache| cache.borrow().get(&key).copied()) {
        Some((gen, entry)) if gen == generation => entry,
        _ => {
            *warm = false;
            if !may_resolve {
                return None;
            }
            let resolved = resolve_native_site(vm, info, receiver_class_id);
            NATIVE_SITE_CACHE_ALT.with(|cache| {
                cache.borrow_mut().insert(key, (generation, resolved));
            });
            resolved
        }
    }
}

// ---------------------------------------------------------------------------
// SBR5 lane ncall: the lean native door
// ---------------------------------------------------------------------------
//
// A compiled call to a registered native used to cost 190-250 ns of CPU: the
// extern helper's panic scope, SATB flush, `forward_jit_reference_args`' descriptor
// walk, the site-memo generation flush, a hash probe that copies a ~120-byte
// entry out of a `RefCell<HashMap>`, a second heap-membership walk per object
// argument, and only then the call wrapper. All of those are per-SITE facts or
// guards for work a plain callback cannot do (a collection cannot run between
// the compiled caller's argument staging and this call, because the caller
// polls safepoints only at its own poll points).
//
// `try_jit_site_cached_native_dispatch` already decides, once per site and
// registry generation, that a site is a plain callback native. When it serves
// such a site it also files a compact record here (`lean_native_site_fill`):
// a direct-mapped, thread-local table keyed by the `JitInvokeInfo` address.
// `try_lean_native_door` is the hit: it re-tests exactly the guards the memo's
// own hit re-tests (the JIT cache generation that retires an address, the
// native registry generation, the receiver's class for a virtual site, "no class
// was ever redefined") and calls the same wrappers the memo would
// (`safe_native_call_leaf` for a leaf claim, the funnel otherwise). Anything
// else -- a miss, a changed guard, a null/array receiver, an unexpected
// argument count -- answers `None` and the call takes today's route unchanged.
//
// `CRATONVM_JIT=-native-site-cache` (the memo's own kill switch) keeps the table
// empty: one binary, two arms for the A/B.

/// Slots of the thread-local table. A power of two; collisions evict.
///
/// JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-1,
/// `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY`): under that switch the same 256
/// slots are read as 128 two-way sets, `{i, i ^ LEAN_NATIVE_SLOTS / 2}`
/// ([`lean_native_alt_slot`]). The primary slot of a key is the one the
/// direct-mapped table uses, so the switch off probes exactly what it always
/// probed.
const LEAN_NATIVE_SLOTS: usize = 256;

#[derive(Clone, Copy)]
struct LeanNativeSite {
    /// The `JitInvokeInfo` address; `0` is an empty slot.
    key: usize,
    vm_identity: usize,
    /// `cratonvm_jit::jit_cache_generation()` at the fill: an address is only
    /// ever re-issued under a newer one (`flush_raw_entry_dispatch_caches`).
    jit_gen: u64,
    registry_gen: u32,
    /// The exact receiver class of a virtual/interface site, `0` otherwise.
    receiver_class: u32,
    invoke_kind: u8,
    leaf: bool,
    ret: u8,
    nparams: u8,
    params: [u8; INLINE_JIT_NATIVE_ARGS],
    callback: Option<cratonvm_native_api::NativeCallback>,
    native_id: Option<cratonvm_native_api::NativeMethodId>,
    capability_kind: Option<cratonvm_native_api::CapabilityKind>,
    /// The site cache's [`LeafNativeKind`]: `Callback` unless
    /// `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS` filed a leaf kind
    /// (FUNNEL2-19-2, [`lean_leaf_kind_fileable`]).
    kind: LeafNativeKind,
    /// The callback returns a fresh live base
    /// (`cratonvm_native_builtins::is_fresh_object_result_native`) and
    /// `CRATONVM_JIT_NATIVE_FRESH_RESULT` was on at the fill (FUNNEL2-19-3).
    fresh_result: bool,
}

impl LeanNativeSite {
    const EMPTY: LeanNativeSite = LeanNativeSite {
        key: 0,
        vm_identity: 0,
        jit_gen: 0,
        registry_gen: 0,
        receiver_class: 0,
        invoke_kind: 0,
        leaf: false,
        ret: 0,
        nparams: 0,
        params: [0; INLINE_JIT_NATIVE_ARGS],
        callback: None,
        native_id: None,
        capability_kind: None,
        kind: LeafNativeKind::Callback,
        fresh_result: false,
    };
}

thread_local! {
    static LEAN_NATIVE_SITES: std::cell::UnsafeCell<[LeanNativeSite; LEAN_NATIVE_SLOTS]> =
        const { std::cell::UnsafeCell::new([LeanNativeSite::EMPTY; LEAN_NATIVE_SLOTS]) };
}

#[inline]
fn lean_native_slot(info_addr: usize) -> usize {
    (info_addr >> 4) & (LEAN_NATIVE_SLOTS - 1)
}

/// FUNNEL2-19-1: the other slot of `slot`'s two-way set. An involution, and
/// both members share the low seven bits of [`lean_native_slot`], so a key's
/// set is the same whichever of the two it was filed in.
#[inline]
fn lean_native_alt_slot(slot: usize) -> usize {
    slot ^ (LEAN_NATIVE_SLOTS / 2)
}

/// Where a two-way fill writes (FUNNEL2-19-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeanPlace {
    /// The key's primary slot.
    Primary,
    /// The other way of the set.
    Alternate,
    /// Both ways hold other records: the primary's record moves to the
    /// alternate (evicting what was there) and the new record takes the
    /// primary. The most recently filed record of a set is in its primary way.
    Demote,
}

/// The two-way fill policy, as a pure function of the set's two identities
/// `(key, receiver class)` (`0` key = empty) and the record being filed. A
/// record already present is rewritten in place (a guard that moved refills
/// it); an empty way is taken before anything is evicted.
fn lean_native_two_way_place(
    primary: (usize, u32),
    alternate: (usize, u32),
    record: (usize, u32),
) -> LeanPlace {
    if primary == record {
        LeanPlace::Primary
    } else if alternate == record {
        LeanPlace::Alternate
    } else if primary.0 == 0 {
        LeanPlace::Primary
    } else if alternate.0 == 0 {
        LeanPlace::Alternate
    } else {
        LeanPlace::Demote
    }
}

/// FUNNEL2-19-2: the site-cache leaf kinds the lean table may file under
/// `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS`: every kind
/// [`try_jit_arraylist_leaf`] answers for a validated virtual/interface
/// receiver. `ThreadCurrentThread` (a static site the site-cache door answers
/// before any fill) and `Callback` (always filed) are not in the list.
fn lean_leaf_kind_fileable(kind: LeafNativeKind) -> bool {
    matches!(
        kind,
        LeafNativeKind::ArrayListGet
            | LeafNativeKind::ArrayListSize
            | LeafNativeKind::ChmGet
            | LeafNativeKind::IntWrapperHashCode
            | LeafNativeKind::IntWrapperEquals
            | LeafNativeKind::ObjectGetClass
            | LeafNativeKind::ReferenceRefersTo
            | LeafNativeKind::ClassIsInstance
    )
}

/// File `entry` (the memo's answer for the site at `info_addr`) in the lean
/// table when it is a plain callback the door can serve. Cold: runs on the
/// memo's way to a call, which is the slow route this table removes.
#[inline(never)]
fn lean_native_site_fill(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    info_addr: usize,
    entry: &NativeSiteCache,
    registry_gen: u32,
) {
    let natives = &vm.natives.native_methods;
    // (The table is filled only from `try_jit_site_cached_native_dispatch`,
    // which `CRATONVM_JIT=-native-site-cache` already switches off, so that
    // one declared kill switch removes the lean door too.)
    //
    // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-2): under
    // `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS` a leaf kind is filed too; the
    // door's hit runs the same no-wrapper probe the site-cache door runs and
    // falls back to the record's callback, as that door does.
    let kind_filed = entry.kind == LeafNativeKind::Callback
        || (natives.jit_native_site_door_leaf_kinds() && lean_leaf_kind_fileable(entry.kind));
    if !kind_filed
        || entry.poly
        || entry.sync.is_some()
        || entry.vh_value_site != 0
        || entry.refused_for_receiver
        || info.invoke_kind > 3
    {
        return;
    }
    // Interpreter round i7 wave 138, lane L2 (page
    // `interpreter-i138-L2-lean-native-door-skips-the-interface-special-receiver-guard-FIXED-20261021`):
    // the lean door answers at the top of `jit_invoke_dispatch`, BEFORE the
    // interface sender's receiver check (`iface_special_receiver_guard`), and
    // a kind-1 record carries no receiver guard. A record a passing receiver
    // filed would serve a later refused receiver class with no
    // `IllegalAccessError`, so such a site is never filed (it keeps the
    // site-cache door, which runs after the check).
    if iface_special_site_is_guarded(vm, info) {
        return;
    }
    let Some(params) = entry.desc.params() else {
        return;
    };
    // The diagnostic doors the extern helpers feed stay on the old route.
    if lean_native_site_diagnostics_on() {
        return;
    }
    // A custom loader's resource accessors take a null-name NPE before any
    // dispatch (both extern helpers).
    if matches!(
        (info.method_name, info.descriptor),
        ("getResource", "(Ljava/lang/String;)Ljava/net/URL;")
            | (
                "getResources",
                "(Ljava/lang/String;)Ljava/util/Enumeration;"
            )
            | (
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;"
            )
    ) {
        return;
    }
    let callback = entry.callback;
    let mut tags = [0u8; INLINE_JIT_NATIVE_ARGS];
    tags[..params.len()].copy_from_slice(params);
    let site = LeanNativeSite {
        key: info_addr,
        vm_identity: vm.vm_identity,
        jit_gen: cratonvm_jit::jit_cache_generation(),
        registry_gen,
        receiver_class: entry.receiver_class_id.unwrap_or(0),
        invoke_kind: info.invoke_kind,
        leaf: entry.leaf,
        ret: entry.desc.ret,
        nparams: params.len() as u8, // Cast: at most INLINE_JIT_NATIVE_ARGS
        params: tags,
        callback: Some(callback),
        native_id: entry.native_id,
        capability_kind: entry.capability_kind,
        kind: entry.kind,
        // FUNNEL2-19-3 (lane funnel3): the leaf wrapper has no fresh-result
        // variant (a leaf cannot allocate, and neither proven body is one).
        fresh_result: !entry.leaf
            && natives.jit_native_fresh_result()
            && cratonvm_native_builtins::is_fresh_object_result_native(callback),
    };
    // A virtual/interface entry without a receiver guard cannot be served.
    // (`ClassId(0)` is a real class here -- `java/lang/Object` -- so the guard
    // is "present", not "non-zero".)
    if matches!(info.invoke_kind, 0 | 2) && entry.receiver_class_id.is_none() {
        return;
    }
    lean_native_site_file(site, natives.jit_native_site_door_two_way());
}

/// The diagnostic doors the extern helpers feed (each keeps a call on the old
/// route, so no lean record is filed while one is on).
fn lean_native_site_diagnostics_on() -> bool {
    site_alias_detect_enabled()
        || crate::runtime::env_cache::jit_dispatch_dbg()
        || crate::runtime::env_cache::jit_putfield_diag()
        || crate::runtime::env_cache::disable_jit()
        || mic_prof::trace_enabled()
        || mic_prof::enabled()
        || cv_trace_enabled()
        || mic_trace_method_filter().is_some()
}

/// File `site` in this thread's lean table (its placement: the direct-mapped
/// slot, or under `two_way` the set's way [`lean_native_two_way_place`]
/// picks).
fn lean_native_site_file(site: LeanNativeSite, two_way: bool) {
    let info_addr = site.key;
    LEAN_NATIVE_SITES.with(|t| {
        // SAFETY: the table is thread-local and no reference into it is live
        // across this write (the door copies what it needs before any call).
        let table = unsafe { &mut *t.get() };
        let primary = lean_native_slot(info_addr);
        if !two_way {
            table[primary] = site;
            return;
        }
        // FUNNEL2-19-1 (lane funnel3): a record is identified by its site AND
        // its receiver class, so one site alternating between two receiver
        // classes keeps a record per class.
        let alternate = lean_native_alt_slot(primary);
        match lean_native_two_way_place(
            (table[primary].key, table[primary].receiver_class),
            (table[alternate].key, table[alternate].receiver_class),
            (site.key, site.receiver_class),
        ) {
            LeanPlace::Primary => table[primary] = site,
            LeanPlace::Alternate => table[alternate] = site,
            LeanPlace::Demote => {
                table[alternate] = table[primary];
                table[primary] = site;
            }
        }
    });
}

/// Interpreter round i3 wave 83, lane L3 (`CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE`,
/// `VmConfig::jit_reflect_door_lean_site`, default off; page
/// `interpreter-i81-L3-proposal-name-the-reflective-calls-gap-and-reentry-KEPT-OFF-20261019.md`,
/// "Progress (wave 83)"): file a compiled `Method.invoke` /
/// `Constructor.newInstance` site that the virtual-MIC helper's direct
/// reflective arm just served in the lean native table, as a plain callback
/// record for the receiver's exact class.
///
/// The host census put ~240 ns of a reflective call's way in at the helper
/// before the funnel (`gap_in_helper`): the body's prefix (the MH-constant
/// redirect, the boundary and SATB bookkeeping, the argument forwarding walk,
/// the dispatch-cache flush, the native site-cache probe that refuses the
/// `invoke` name, the `VarHandle` name test) and the forced native's by-name
/// memo, all re-run on every call to reach the same callback. The lean door
/// (`try_lean_native_door`) runs at the helper's first statement and serves
/// exactly this callback under its own guards: the VM, the JIT cache
/// generation (a recycled site address), the native registry generation (the
/// forced native is a registry answer, `native_override_for_cached_reflect_invoke`
/// memoizes on that generation), no class redefined, and the receiver's exact
/// class; it runs the call under the JIT-dispatch depth guard and the full
/// funnel (or the lean funnel under `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAN_FUNNEL`),
/// as for every other callback native. The record has no native id and no
/// capability gate, as the direct arm's `safe_native_call` has none.
#[inline(never)]
fn reflect_door_lean_site_fill(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    info_addr: usize,
    receiver: ObjectRef,
    callback: cratonvm_native_api::NativeCallback,
) {
    if !matches!(info.invoke_kind, 0 | 2) || lean_native_site_diagnostics_on() {
        return;
    }
    let JitDispatchClass::Class(receiver_class) = jit_dispatch_class(vm, receiver) else {
        return;
    };
    let desc = NativeSiteDescriptor::decode(info.descriptor);
    let Some(params) = desc.params() else {
        return;
    };
    let mut tags = [0u8; INLINE_JIT_NATIVE_ARGS];
    tags[..params.len()].copy_from_slice(params);
    let natives = &vm.natives.native_methods;
    let site = LeanNativeSite {
        key: info_addr,
        vm_identity: vm.vm_identity,
        jit_gen: cratonvm_jit::jit_cache_generation(),
        registry_gen: natives.generation(),
        receiver_class: receiver_class.as_u32(),
        invoke_kind: info.invoke_kind,
        leaf: false,
        ret: desc.ret,
        nparams: params.len() as u8, // Cast: at most INLINE_JIT_NATIVE_ARGS
        params: tags,
        callback: Some(callback),
        native_id: None,
        capability_kind: None,
        kind: LeafNativeKind::Callback,
        fresh_result: false,
    };
    lean_native_site_file(site, natives.jit_native_site_door_two_way());
}

/// FUNNEL2-19-2: the leaf kinds whose second argument word is a reference
/// (the `ChmGet` key, the `IntWrapperEquals` other, the `ReferenceRefersTo`
/// referent); the others take an `int` index or nothing.
fn lean_leaf_kind_takes_reference(kind: LeafNativeKind) -> bool {
    matches!(
        kind,
        LeafNativeKind::ChmGet
            | LeafNativeKind::IntWrapperEquals
            | LeafNativeKind::ReferenceRefersTo
            | LeafNativeKind::ClassIsInstance
    )
}

/// `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS`: count one lean-door event. Cold and
/// out of line; the door asks the switch first (one field load).
#[cold]
#[inline(never)]
fn lean_door_census(vm: &SharedVm, event: cratonvm_native_api::NativeSiteDoorEvent, add: u64) {
    vm.natives.native_methods.note_native_site_door(event, add);
}

/// The lean door's census on a decline: counted only while the switch is on.
#[inline]
unsafe fn lean_door_decline(vm_ptr: i64, event: cratonvm_native_api::NativeSiteDoorEvent) {
    if vm_ptr == 0 {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    if vm.natives.native_methods.dbg_native_site_door_census() {
        lean_door_census(vm, event, 1);
    }
}

/// The per-call guards of a lean record: the VM, the JIT cache generation
/// that retires an address, the native registry generation, and "no class was
/// ever redefined".
///
/// SAFETY: `slot` points into this thread's lean table.
#[inline(always)]
unsafe fn lean_native_guards_hold(slot: *const LeanNativeSite, vm: &SharedVm) -> bool {
    (*slot).vm_identity == vm.vm_identity
        && (*slot).jit_gen == cratonvm_jit::jit_cache_generation()
        && (*slot).registry_gen == vm.natives.native_methods.generation()
        && !crate::classloading::any_class_redefined()
}

/// The lean door: serve a compiled call to a plain callback native from the
/// thread-local table, or answer `None` and let the ordinary route run.
///
/// `from_mic` is the `invokevirtual`/`invokeinterface` helper's entry, which
/// runs a non-leaf native under the JIT-dispatch depth guard (a native that
/// re-enters Java must not outrun the native stack).
///
/// SAFETY: `vm_ptr`, `info_ptr`, `args_ptr` and `num_args` are the extern
/// helper's own arguments (a live `SharedVm`, a live `JitInvokeInfo`, the
/// caller's argument staging block).
#[inline]
unsafe fn try_lean_native_door(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    from_mic: bool,
) -> Option<i64> {
    use cratonvm_native_api::NativeSiteDoorEvent as Ev;
    let info_addr = info_ptr as usize;
    let base: *const LeanNativeSite = LEAN_NATIVE_SITES.with(|t| t.get() as *const LeanNativeSite);
    let mut index = lean_native_slot(info_addr);
    // FUNNEL2-19-1 (lane funnel3): the record was found in the set's other way.
    let mut second_way = false;
    if (*base.add(index)).key != info_addr {
        // FUNNEL2-19-1 (`CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY`): ask the
        // set's other way. One field load on this (slow-route) miss path
        // while the switch is off.
        if vm_ptr != 0 {
            let vm = &*(vm_ptr as *const SharedVm);
            let alternate = lean_native_alt_slot(index);
            if vm.natives.native_methods.jit_native_site_door_two_way()
                && (*base.add(alternate)).key == info_addr
            {
                index = alternate;
                second_way = true;
            }
        }
        if !second_way {
            // JIT round 19 wave 2 (lane funnel2): the census separates an
            // empty slot from one another site holds (direct-mapped eviction),
            // which is what decides whether a per-site record would serve more
            // crossings (FUNNEL17-5).
            if vm_ptr != 0 {
                let vm = &*(vm_ptr as *const SharedVm);
                if vm.natives.native_methods.dbg_native_site_door_census() {
                    lean_door_census(vm, Ev::Probe, 1);
                    let ev = if (*base.add(index)).key == 0 {
                        Ev::MissEmpty
                    } else {
                        Ev::MissOtherSite
                    };
                    lean_door_census(vm, ev, 1);
                }
            }
            return None;
        }
    }
    let mut slot: *const LeanNativeSite = base.add(index);
    let vm = &*(vm_ptr as *const SharedVm);
    // `CRATONVM_DBG_NATIVE_SITE_DOOR_CENSUS` (JIT round 19 wave 2, lane
    // funnel2): one field load when off.
    let census_t0 = vm
        .natives
        .native_methods
        .dbg_native_site_door_census()
        .then(|| {
            lean_door_census(vm, Ev::Probe, 1);
            mic_prof::now()
        });
    if !lean_native_guards_hold(slot, vm) {
        lean_door_decline(vm_ptr, Ev::DeclineGuard);
        return None;
    }
    // The site's shape. Both ways of a set that hold this key are this site's
    // records (FUNNEL2-19-1), so they agree on it.
    let invoke_kind = (*slot).invoke_kind;
    let nparams = (*slot).nparams as usize;

    let has_receiver = invoke_kind != 3;
    if num_args < 0 || (num_args > 0 && args_ptr == 0) {
        lean_door_decline(vm_ptr, Ev::DeclineShape);
        return None;
    }
    let args: &[i64] = if num_args == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize)
    };
    if args.len() != nparams + usize::from(has_receiver) {
        lean_door_decline(vm_ptr, Ev::DeclineShape);
        return None;
    }
    // GPU offload routes a static call through the kernel table first.
    #[cfg(feature = "gpu-offload")]
    if invoke_kind == 3 && cratonvm_jit::offload_hook::any_kernels() {
        return None;
    }

    // JIT round 19 wave 2 (lane funnel2; FUNNEL17-5 residual):
    // `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAN_FUNNEL=1` sends a non-leaf hit
    // through the lean funnel (`safe_native_call_jit_lean`), which does not
    // re-forward its arguments. The dispatch helpers treat the SATB flush as a
    // possible GC point ("can participate in a moving collection",
    // `jit_invoke_dispatch_body`), so on that route the flush runs HERE,
    // before any argument word is read. After it nothing up to the call is a
    // GC point (TLS reads, the capability gate, the depth guard), every
    // reference is validated below, and this call made no resolution: the
    // lean funnel's precondition, the argument `try_jit_site_cached_native_dispatch`
    // makes for its `warm` entries. The record is never a sync,
    // signature-polymorphic or `VarHandle`-armed entry
    // (`lean_native_site_fill`); a capability-gated one keeps the full funnel,
    // as in the site-cache door.
    //
    // Decided on the record found first. When the receiver selects the set's
    // other way (FUNNEL2-19-1) the lean funnel is used there only if this
    // flush already ran.
    let flushed_for_lean = !(*slot).leaf
        && (*slot).capability_kind.is_none()
        && vm.natives.native_methods.jit_native_site_door_lean_funnel();
    if flushed_for_lean {
        vm.mem.heap.flush_thread_satb();
    }

    let mut values = JitDecodedArgs::new();
    let mut receiver = None;
    if has_receiver {
        let raw = args[0] as u64;
        if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
            lean_door_decline(vm_ptr, Ev::DeclineReceiver);
            return None;
        }
        note_membership_walk(1);
        let Some(obj) = vm.mem.heap.is_object_address(raw as usize) else {
            lean_door_decline(vm_ptr, Ev::DeclineReceiver);
            return None;
        };
        if invoke_kind != 1 {
            let cid = match jit_dispatch_class_validated(vm, obj) {
                JitDispatchClass::Class(cid) => cid.as_u32(),
                _ => {
                    lean_door_decline(vm_ptr, Ev::DeclineReceiver);
                    return None;
                }
            };
            if (*slot).receiver_class != cid {
                // FUNNEL2-19-1 (lane funnel3): a site alternating between two
                // receiver classes keeps one record per class, in the two
                // ways of its set. One field load while the switch is off.
                let other = base.add(lean_native_alt_slot(index));
                if vm.natives.native_methods.jit_native_site_door_two_way()
                    && (*other).key == info_addr
                    && (*other).receiver_class == cid
                    && lean_native_guards_hold(other, vm)
                {
                    slot = other;
                    second_way = !second_way;
                } else {
                    lean_door_decline(vm_ptr, Ev::DeclineReceiver);
                    return None;
                }
            }
        }
        receiver = Some(obj);
        values.push(Value::Object(Some(obj)));
    }
    // Everything the call needs, read out before anything runs (a nested
    // dispatch may refill this slot).
    let callback = (*slot).callback?;
    let leaf = (*slot).leaf;
    let ret = (*slot).ret;
    let params = (*slot).params;
    let native_id = (*slot).native_id;
    let capability_kind = (*slot).capability_kind;
    let kind = (*slot).kind;
    let fresh_result = (*slot).fresh_result;
    let lean_funnel = flushed_for_lean && !leaf && capability_kind.is_none();
    if second_way && census_t0.is_some() {
        lean_door_census(vm, Ev::HitSecondWay, 1);
    }

    // From here on the call is served: nothing below may answer `None`.
    let (thread, _guard) = jit_thread_mut()?;

    // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-2): a filed leaf kind
    // runs the site-cache door's no-wrapper probe first. Its record exists
    // only under `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAF_KINDS`, is a
    // virtual/interface site (the receiver guard held above), never sync,
    // poly or capability-gated (`lean_native_site_fill`). The probe gets the
    // words Route S hands it: the receiver and the one reference argument
    // forwarded (the site-cache route's `forward_jit_reference_args`), here
    // with the forwarding read of a base this door validated.
    if kind != LeafNativeKind::Callback {
        if let Some(obj) = receiver {
            let recv = vm.mem.heap.load_and_forward_validated(obj);
            let mut staged = [recv.as_ptr() as i64, 0];
            // FUNNEL3-19-2 (JIT round 19 wave 6, lane mega6;
            // `CRATONVM_JIT_LEAF_PROBE_VALIDATED_ARGS`): the argument this door
            // just validated and forwarded, handed to the probe so it does not
            // walk the word again. `None` with the switch off.
            let mut validated_arg = None;
            if let Some(&raw) = args.get(1) {
                staged[1] = raw;
                let bits = raw as u64;
                if lean_leaf_kind_takes_reference(kind)
                    && bits != 0
                    && (bits & 0x7) == 0
                    && bits < (1u64 << 48)
                {
                    note_membership_walk(2);
                    if let Some(o) = vm.mem.heap.is_object_address(bits as usize) {
                        let forwarded = vm.mem.heap.load_and_forward_validated(o);
                        staged[1] = forwarded.as_ptr() as i64;
                        if vm.natives.native_methods.jit_leaf_probe_validated_args() {
                            validated_arg = Some(forwarded);
                        }
                    }
                }
            }
            let staged = &staged[..args.len().min(2)];
            cratonvm_jit::jit_ic_quiescent_point();
            thread.native_pending_return = None;
            if let Some(bits) =
                try_jit_arraylist_leaf(vm, thread, kind, recv, staged, validated_arg)
            {
                if leaf {
                    note_leaf_native_hit();
                } else {
                    note_site_cached_native_hit();
                }
                if matches!(
                    kind,
                    LeafNativeKind::ArrayListGet
                        | LeafNativeKind::ArrayListSize
                        | LeafNativeKind::ChmGet
                ) {
                    JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.collection_leaf_hits));
                }
                count_jit_native_dispatch(vm, native_id);
                if census_t0.is_some() {
                    lean_door_census(vm, Ev::HitLeafKind, 1);
                }
                return Some(bits);
            }
            if census_t0.is_some() {
                lean_door_census(vm, Ev::LeafKindFallback, 1);
            }
        }
    }

    let first = usize::from(has_receiver);
    for (i, &tag) in params[..nparams].iter().enumerate() {
        let raw = args[first + i];
        values.push(match tag {
            b'I' | b'B' | b'C' | b'S' | b'Z' => Value::Int(raw as i32),
            b'J' => Value::Long(raw),
            b'F' => Value::Float(f32::from_bits(raw as u32)),
            b'D' => Value::Double(f64::from_bits(raw as u64)),
            b'L' | b'[' => {
                let bits = raw as u64;
                if bits == 0 {
                    Value::Object(None)
                } else if (bits & 0x7) == 0 && bits < (1u64 << 48) {
                    note_membership_walk(2);
                    Value::Object(vm.mem.heap.is_object_address(bits as usize))
                } else {
                    Value::Object(None)
                }
            }
            _ => Value::Int(raw as i32),
        });
    }
    // JIT round 21, lane native (correctness, unconditional; review of the
    // opt-in `CRATONVM_JIT_NATIVE_SITE_DOOR_LEAN_FUNNEL`): the lean funnel
    // skips the funnel's argument re-forward on the premise that the door
    // FORWARDED every reference (Route S's `forward_jit_reference_args`). This
    // door only validated them (`is_object_address` on the raw word), and a
    // relocation in progress moves an object without moving any epoch (the FFM
    // door's note), so heal each validated reference with the forwarding read
    // here, as the full and leaf funnels do for this door's other routes.
    if lean_funnel {
        for value in values.iter_mut() {
            if let Value::Object(Some(object)) = value {
                *object = vm.mem.heap.load_and_forward_validated(*object);
            }
        }
    }

    if let Some(kind) = capability_kind {
        if crate::vm::check_native_dispatch_capability_for_slot(vm, kind, native_id).is_err() {
            lean_door_decline(vm_ptr, Ev::DeclineCapability);
            return None;
        }
    }
    // The extern helper's own entry/exit roots, done here: the previous
    // native's returned object stops being a root, and this call's result is
    // compiled code's the moment it returns.
    cratonvm_jit::jit_ic_quiescent_point();
    thread.native_pending_return = None;
    let _depth_guard = if from_mic && !leaf {
        match enter_jit_dispatch(vm, 0) {
            Ok(g) => Some(g),
            Err(sentinel) => return Some(sentinel),
        }
    } else {
        None
    };
    count_jit_native_dispatch(vm, native_id);
    let census_t1 = census_t0.map(|_| mic_prof::now());
    let called = if leaf {
        note_leaf_native_hit();
        crate::vm::safe_native_call_leaf_prevalidated(vm, thread, callback, &values)
    } else {
        note_site_cached_native_hit();
        if fresh_result {
            // FUNNEL2-19-3 (lane funnel3; `CRATONVM_JIT_NATIVE_FRESH_RESULT`):
            // the same funnel (lean or full, as below) with the result healed
            // by the forwarding read alone.
            if census_t0.is_some() {
                lean_door_census(vm, Ev::FreshResult, 1);
            }
            if !lean_funnel {
                vm.mem.heap.flush_thread_satb();
            }
            crate::vm::safe_native_call_jit_fresh_result(vm, thread, callback, &values, lean_funnel)
        } else if lean_funnel {
            // The SATB flush ran before the decode (above).
            crate::vm::safe_native_call_jit_lean(vm, thread, callback, &values)
        } else {
            vm.mem.heap.flush_thread_satb();
            crate::vm::safe_native_call_prevalidated_objects(vm, thread, callback, &values)
        }
    };
    let census_t2 = census_t0.map(|_| mic_prof::now());
    let value = match called {
        Ok(value) => crate::vm::coerce_native_return_typed(value, ret),
        Err(error) => {
            let info = &*(info_ptr as *const JitInvokeInfo);
            return Some(handle_jit_dispatch_error(vm, thread, error, info));
        }
    };
    thread.native_pending_return = None;
    let bits = match value {
        Some(Value::Int(v)) => v as i64,
        Some(Value::Long(v)) => v,
        Some(Value::Float(f)) => f.to_bits() as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    };
    // A throwing native's crossing is not timed (the split census's rule).
    if let (Some(t0), Some(t1), Some(t2)) = (census_t0, census_t1, census_t2) {
        let hit = if leaf {
            Ev::HitLeaf
        } else if lean_funnel {
            Ev::HitLeanFunnel
        } else {
            Ev::HitFunnel
        };
        lean_door_census(vm, hit, 1);
        lean_door_census(vm, Ev::CyclesFront, t1.wrapping_sub(t0));
        lean_door_census(vm, Ev::CyclesCall, t2.wrapping_sub(t1));
        lean_door_census(vm, Ev::CyclesTail, mic_prof::now().wrapping_sub(t2));
    }
    Some(bits)
}

#[cfg(test)]
mod r19_funnel3_lean_table_tests {
    use super::*;

    /// FUNNEL2-19-1: a set is `{i, i ^ 128}`; the primary slot is the
    /// direct-mapped one, so the switch off probes what it always probed.
    #[test]
    fn the_two_ways_of_a_set_share_the_primary_slots_low_bits() {
        for addr in [0usize, 0x10, 0x7ff0, 0x1234_5670, usize::MAX & !0xF] {
            let p = lean_native_slot(addr);
            let a = lean_native_alt_slot(p);
            assert!(p < LEAN_NATIVE_SLOTS && a < LEAN_NATIVE_SLOTS);
            assert_ne!(p, a);
            assert_eq!(lean_native_alt_slot(a), p);
            assert_eq!(
                p & (LEAN_NATIVE_SLOTS / 2 - 1),
                a & (LEAN_NATIVE_SLOTS / 2 - 1)
            );
        }
    }

    /// FUNNEL2-19-1: the fill rewrites a record in place, takes an empty way
    /// before evicting, and on a full set keeps the newest record in the
    /// primary way (the older one moves to the alternate).
    #[test]
    fn the_two_way_fill_policy() {
        use LeanPlace::*;
        let site = 0x5170_0010usize;
        let other = 0x5170_0810usize;
        let empty = (0usize, 0u32);
        // The same record, wherever it is.
        assert_eq!(
            lean_native_two_way_place((site, 7), (other, 1), (site, 7)),
            Primary
        );
        assert_eq!(
            lean_native_two_way_place((other, 1), (site, 7), (site, 7)),
            Alternate
        );
        // Empty ways first.
        assert_eq!(
            lean_native_two_way_place(empty, (other, 1), (site, 7)),
            Primary
        );
        assert_eq!(
            lean_native_two_way_place((other, 1), empty, (site, 7)),
            Alternate
        );
        // One site, two receiver classes: two records.
        assert_eq!(
            lean_native_two_way_place((site, 7), empty, (site, 9)),
            Alternate
        );
        assert_eq!(
            lean_native_two_way_place((site, 7), (site, 9), (site, 9)),
            Alternate
        );
        // A full set of other records: demote.
        assert_eq!(
            lean_native_two_way_place((other, 1), (site, 7), (site, 9)),
            Demote
        );
    }

    /// FUNNEL2-19-2: every kind `try_jit_arraylist_leaf` answers for a
    /// validated receiver is fileable; the plain callback is filed anyway, and
    /// `Thread.currentThread` (a static site) never.
    #[test]
    fn the_fileable_leaf_kinds() {
        for kind in [
            LeafNativeKind::ArrayListGet,
            LeafNativeKind::ArrayListSize,
            LeafNativeKind::ChmGet,
            LeafNativeKind::IntWrapperHashCode,
            LeafNativeKind::IntWrapperEquals,
            LeafNativeKind::ObjectGetClass,
            LeafNativeKind::ReferenceRefersTo,
            LeafNativeKind::ClassIsInstance,
        ] {
            assert!(lean_leaf_kind_fileable(kind));
        }
        assert!(!lean_leaf_kind_fileable(LeafNativeKind::Callback));
        assert!(!lean_leaf_kind_fileable(
            LeafNativeKind::ThreadCurrentThread
        ));
        assert!(lean_leaf_kind_takes_reference(LeafNativeKind::ChmGet));
        assert!(lean_leaf_kind_takes_reference(
            LeafNativeKind::IntWrapperEquals
        ));
        assert!(lean_leaf_kind_takes_reference(
            LeafNativeKind::ReferenceRefersTo
        ));
        assert!(lean_leaf_kind_takes_reference(
            LeafNativeKind::ClassIsInstance
        ));
        assert!(!lean_leaf_kind_takes_reference(
            LeafNativeKind::ArrayListGet
        ));
    }

    /// The door's order, pinned by text (it cannot be driven without compiled
    /// code): every guard and the receiver-class selection run before the
    /// record is read out, and a leaf kind's probe runs after the thread is
    /// taken and before the parameter decode.
    #[test]
    fn the_lean_door_reads_the_record_after_the_way_is_chosen() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let start = src
            .find("\nunsafe fn try_lean_native_door(")
            .expect("the door");
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("end of the door")];
        let guards = body
            .find("lean_native_guards_hold(slot, vm)")
            .expect("guards");
        let way = body.find("slot = other;").expect("the second way");
        let read_out = body
            .find("let callback = (*slot).callback?;")
            .expect("read-out");
        let thread = body.find("jit_thread_mut()?").expect("thread");
        let probe = body.find("try_jit_arraylist_leaf(").expect("leaf probe");
        let decode = body
            .find("for (i, &tag) in params[..nparams]")
            .expect("decode");
        assert!(guards < way && way < read_out && read_out < thread);
        assert!(thread < probe && probe < decode);
    }
}

/// Site-cached `ArrayList.get(int)` / `size()` hits answered by
/// `cratonvm_native_collections::jit_arraylist_get` / `jit_arraylist_size`
/// without the native-call wrapper ([`LeafNativeKind::ArrayListGet`]).
///
/// Zero beside a non-zero site-cached/leaf hit count on an ArrayList-heavy run
/// means the classification never matched (e.g. the registration moved to a
/// different body) or every receiver declined — which timings cannot tell
/// from "no faster". Counted per thread.
pub fn jit_collection_leaf_hit_count() -> u64 {
    jit_counter_total(|b| {
        b.collection_leaf_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// `VarHandle` instance-field CAS operations served inside the funnel, and the
/// ones that declined to the generic native.
///
/// The pair is the point, exactly as it is for the write direct helpers: a
/// zero HIT count beside a large DECLINE count says the fast path is reached
/// and refuses every handle, which looks identical to "no CAS happened" if
/// only one of the two is printed.
///
/// Counted per thread (r9-vmside) — see [`JitCounterBlock::vh_direct_served`].
///
/// `(served, declined)` counts for the in-funnel `VarHandle` CAS fast path.
pub fn varhandle_field_cas_counts() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        jit_counter_total(|b| b.vh_field_cas_served.load(Relaxed)),
        jit_counter_total(|b| b.vh_field_cas_declined.load(Relaxed)),
    )
}

/// `CRATONVM_JIT_VARHANDLE_CAS_FUNNEL_FAST=0` -- send every `VarHandle` CAS
/// back through the generic native, as before this fast path existed. Default
/// ON. Same role as the read and write bind switches: it is what makes an A/B
/// on ONE binary possible, which is the only kind that means anything when the
/// host's load drifts between runs.
///
/// JIT round 25 review: the default-ON opt-out rule of
/// [`cratonvm_types::flags::runtime_flag_default_on`] (`0` / `false` / `off` /
/// `no` kill); it was a hand-rolled `Ok("0")` match under which `=false` and
/// `=off` left the fast path on, as `varhandle_array_element_fast_from_flags`
/// did.
fn varhandle_cas_funnel_fast_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VARHANDLE_CAS_FUNNEL_FAST")
    })
}
/// `VarHandle` instance-field READS served as a direct field load, reported by
/// `CRATONVM_DBG=intrinsic-stats`. Zero here with a non-zero site-cached count
/// means every VarHandle site refused the plan — which is a different problem
/// from "no VarHandle site was reached", and only a counter separates them.
/// Counted per thread (r9-vmside).
pub fn varhandle_field_read_hit_count() -> u64 {
    jit_counter_total(|b| {
        b.vh_field_read_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// Serve a signature-polymorphic `VarHandle` READ mode as what it actually is
/// — one field load — instead of a native call.
///
/// The funnel route for `VH.get(receiver)` is: build a `NativeContextImpl`,
/// record a thread transition, run `varhandle_get`'s access-shape cascade,
/// **allocate a wrapper object** for the erased `Object` return, then unbox it
/// again against the call site's descriptor and throw the wrapper away. That
/// round trip is ~40% of a `VarHandle.get`, and netty pays it twice per
/// `ByteBuf.writeByte` through `ensureAccessible()` -> `RefCnt.isLiveNonVolatile`.
///
/// # What it refuses, and why each refusal matters
///
/// * **Anything but a read mode.** `set`/CAS/`getAndAdd` mutate, and the
///   write-side semantics (`safeConstructPutInt`, the ordered stores) live in
///   the native.
/// * **Any handle the side table does not describe as a resolved instance
///   field** — `varhandle_instance_field_plan_for_handle` answers `None` for static,
///   array-element, byte-array/ByteBuffer-view and `SegmentVarHandle`
///   handles, and for one whose slot is still unresolved (the native resolves
///   and memoises it, so the second call qualifies).
/// * **A receiver that is not an instance of the handle's class** — the native
///   raises `ClassCastException`; the slot index would name another layout.
/// * **A boxed primitive reaching a reference return, or the reverse.** That
///   is precisely the shape `unbox_poly_return_checked`'s W6-1 rule turns
///   into a `WrongMethodTypeException`; declining hands it back to the funnel
///   rather than reimplementing the rule here.
///
/// Every refusal is a fall-through to the existing dispatch, so the worst
/// case is the cost that was already being paid.
/// `VarHandle.compareAndSet` on an ordinary instance field, served from inside
/// the native funnel.
///
/// The twin of [`try_varhandle_instance_field_read`], and it exists because the
/// two access directions were not symmetric. A funnelled READ is caught here
/// and costs ~49 ns; a funnelled CAS ran the whole `varhandle_compare_and_set`
/// native — its segment-handle probe, its FFM-layout probe, its byte-view probe
/// and its array probe — before ever reaching the instance-field case, and cost
/// ~182 ns more than a bound call. Measured with the read and write kill
/// switches on `HibfixVarHandleProbe`.
///
/// Serving it here rather than only from a thin direct call is deliberate: this
/// route also covers the sites the JIT declines and every interpreter dispatch,
/// which a compile-time bind cannot reach.
///
/// Declines to the generic native for anything it cannot prove: a coordinate
/// count that is not `[VarHandle, receiver, expected, new]`, a handle the side
/// table does not describe as a resolved instance field, or a call site whose
/// declared operand types disagree with the variable's own kind. The native
/// then applies the `VarHandle` access-mode rules, as it does today.
///
/// SAFETY: the raw words come from a live compiled frame; every reference among
/// them is heap-validated before any dereference.
unsafe fn try_varhandle_instance_field_cas(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
) -> Option<i64> {
    if info.method_name != "compareAndSet" {
        return None;
    }
    // `[VarHandle, receiver, expected, new]` and nothing else. An array-element
    // or byte-view CAS carries an index and is not an instance-field CAS.
    if args_slice.len() != 4 {
        return None;
    }
    if info.return_type != b'Z' {
        return None;
    }
    let vh_raw = args_slice[0] as u64;
    let recv_raw = args_slice[1] as u64;
    if vh_raw == 0 || recv_raw == 0 {
        return None;
    }
    // The call site's declared operand types have to agree with the variable's
    // own kind, for the reason the read path checks its return type: the
    // registered descriptor is the erased `([Ljava/lang/Object;)Z`, so the
    // site's is the only statement of what the caller actually passed. A BOUND
    // site does not pay this parse — its slot is that statement, decided once
    // at bind time. On this route it is per call, and a native profile put it
    // at 10.3% of a reference CAS (`CharSearcher::next_match`) before the bind
    // existed to take the hot sites away from it. Still memoized per call
    // site below (`vh_cas_kinds_memoized`) for whatever declined/unbound
    // sites keep reaching this route -- a hot loop with no bind is exactly
    // the shape `HibfixComposeProbe2`'s own per-thread `VarHandle`s produce
    // whenever a chain outruns the bind's own warm-up.
    let (expected_desc, new_desc) = vh_cas_kinds_memoized(
        jit_site_key(vm.vm_identity, info as *const JitInvokeInfo as usize),
        info.descriptor,
    )?;
    if expected_desc != new_desc {
        return None;
    }
    // One implementation, shared with the thin direct helper, so what the two
    // consider a servable handle cannot drift.
    let swapped = varhandle_instance_field_cas_shared(
        vm,
        vh_raw,
        recv_raw,
        expected_desc,
        args_slice[2],
        args_slice[3],
    )?;
    Some(i64::from(swapped))
}

thread_local! {
    /// One-slot memo of [`varhandle_cas_operand_kinds`]: `(call site, the JIT
    /// cache generation it was filled at, answer)`.
    ///
    /// Keyed by the call site's [`JitSiteKey`] (its VM and its
    /// `JitInvokeInfo` address), with the generation beside it, and NOT by
    /// the descriptor's address alone, as it was until interpreter round i1
    /// wave 38 (lane L6). `JitInvokeInfo::descriptor` is only nominally
    /// `'static`: it points into a `Box<str>` the owning `CompiledMethod`
    /// keeps (`_jit_strings`) and is freed with it, so a later compilation
    /// could be handed the same address for another descriptor and read this
    /// site's operand kinds. The `JitInvokeInfo` address can be re-issued the
    /// same way, which is why every other site-keyed memo is flushed on a
    /// generation change ([`site_keyed_memos!`]); the drop of a
    /// `CompiledMethod` that owns `JitInvokeInfo`s moves
    /// `cratonvm_jit::jit_cache_generation`, so an entry stamped with an older
    /// generation is never believed. One slot and a stamp rather than a
    /// `site_keyed_memos!` map: a hit stays two compares and one load, the
    /// cost of the pointer compare it replaces plus the load.
    ///
    /// A miss (another site, the first call, or any JIT publication since)
    /// falls back to the uncached parse and replaces the slot; it is never
    /// wrong, only sometimes not yet warm. `vm_identity` 0 is never a real VM,
    /// so the initial value never matches.
    static VH_CAS_KINDS_MEMO: Cell<(JitSiteKey, u64, Option<(u8, u8)>)> =
        const { Cell::new(((0, 0), 0, None)) };
}

/// [`varhandle_cas_operand_kinds`] for the call site `site`, memoized per
/// thread. See [`VH_CAS_KINDS_MEMO`] for the key.
fn vh_cas_kinds_memoized(site: JitSiteKey, descriptor: &str) -> Option<(u8, u8)> {
    let generation = cratonvm_jit::jit_cache_generation();
    let (key, stamp, kinds) = VH_CAS_KINDS_MEMO.with(Cell::get);
    if key == site && stamp == generation {
        return kinds;
    }
    let kinds = varhandle_cas_operand_kinds(descriptor);
    VH_CAS_KINDS_MEMO.with(|c| c.set((site, generation, kinds)));
    kinds
}

/// The last two parameter kinds of a `compareAndSet` call site, collapsed the
/// way [`cratonvm_native_builtins::lang_invoke::varhandle_instance_field_plan_for_handle`]
/// collapses a field descriptor: any reference is `b'L'`.
///
/// `None` unless the descriptor is exactly `(receiver, expected, new)` — three
/// parameters, the first a reference.
fn varhandle_cas_operand_kinds(descriptor: &str) -> Option<(u8, u8)> {
    // The CLOSING `)`, walked, not the first one (a class name may hold `)`).
    let after_open = descriptor.strip_prefix('(')?;
    let close = cratonvm_jit_api::descriptor_param_list_end(descriptor)?;
    let params = after_open.get(..close.checked_sub(1)?)?;
    let mut kinds = [0u8; 3];
    let mut n = 0usize;
    let bytes = params.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if n == kinds.len() {
            return None;
        }
        match bytes[i] {
            b'L' => {
                let end = params[i..].find(';')? + i;
                kinds[n] = b'L';
                i = end + 1;
            }
            b'[' => {
                let mut j = i;
                while j < bytes.len() && bytes[j] == b'[' {
                    j += 1;
                }
                if j >= bytes.len() {
                    return None;
                }
                i = if bytes[j] == b'L' {
                    params[j..].find(';')? + j + 1
                } else {
                    j + 1
                };
                kinds[n] = b'L';
            }
            c @ (b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D') => {
                kinds[n] = c;
                i += 1;
            }
            _ => return None,
        }
        n += 1;
    }
    if n != 3 || kinds[0] != b'L' {
        return None;
    }
    Some((kinds[1], kinds[2]))
}

/// Decode one raw JIT-ABI operand word against the variable's kind.
///
/// The inverse of the read path's encode, and the same table: a reference is
/// its raw address (zero for `null`), a `float`/`double` travels as its bit
/// pattern, and the whole int category as the low word. A non-zero reference
/// that is not a heap address declines rather than being dereferenced.
fn varhandle_operand_value(vm: &SharedVm, raw: i64, value_desc: u8) -> Option<Value> {
    Some(match value_desc {
        b'L' => {
            if raw == 0 {
                Value::Object(None)
            } else {
                // `is_object_address` is a safe fn: it is the validation,
                // not something that assumes it.
                Value::Object(Some(vm.mem.heap.is_object_address(raw as usize)?))
            }
        }
        b'J' => Value::Long(raw),
        b'F' => Value::Float(f32::from_bits(raw as u32)), // Cast: JIT ABI -- float bits
        b'D' => Value::Double(f64::from_bits(raw as u64)), // Cast: JIT ABI -- double bits
        b'Z' | b'B' | b'C' | b'S' | b'I' => Value::Int(raw as i32), // Cast: JIT ABI -- low word
        _ => return None,
    })
}

/// May a `VarHandle` fast path apply `plan`'s slot to a receiver of class
/// `recv_class_id`?
///
/// The native (`vh_instance_refusal`) raises `ClassCastException` for a
/// receiver that is not an instance of the handle's class; applying
/// `field_index` to one reads or writes a slot of an unrelated layout. The
/// exact class is one integer compare; a subclass (CompletableFuture's
/// `Completion.NEXT` sees nothing else) goes through the positive-memoised
/// subtype walk. `false` only ever declines to the native, which decides.
fn varhandle_plan_admits_receiver(
    vm: &SharedVm,
    plan: &cratonvm_native_builtins::lang_invoke::VarHandleInstanceFieldPlan,
    recv_class_id: u32,
) -> bool {
    if plan.class_id == 0 {
        return false;
    }
    if recv_class_id == plan.class_id {
        return true;
    }
    // nio21 wave 14 L4: a subclass receiver the constant-handle prefix
    // declined for want of a display gets one (lock-free when present).
    vh_const_ensure_display(vm, recv_class_id);
    jit_is_subclass_of_cached(vm, ClassId::new(recv_class_id), ClassId::new(plan.class_id))
}

/// Would the native store `value` through `plan` without a `ClassCastException`?
///
/// `true` for a primitive (the call site's kind already agrees with the
/// variable's) and for `null`; for a reference field, `true` when the native
/// judges nothing (`ref_value_unchecked`, an `Object` field), when a
/// one-dimensional primitive array field receives exactly that array
/// (`ref_array_element`), when a one-dimensional reference array field
/// receives a reference array whose component is its own component or a
/// subtype of it (`ref_array_component_class_id`),
/// or when the value's class is the declared type or a
/// subtype of it — the first clauses of `reference_arg_admitted`, which then
/// answers "admitted" too. Anything else declines, so the native's own rule
/// (including its by-name fallback and its array-shape rule) decides.
fn varhandle_plan_admits_value(
    vm: &SharedVm,
    plan: &cratonvm_native_builtins::lang_invoke::VarHandleInstanceFieldPlan,
    value: &Value,
) -> bool {
    let Value::Object(Some(obj)) = value else {
        return true;
    };
    if plan.ref_value_unchecked {
        return true;
    }
    // A one-dimensional primitive array field admits exactly that array: the
    // native's `vh_array_field_value_admitted` rule, settled by the header's
    // element tag. Any other array-typed field has `ref_type_class_id == 0`
    // and declines below, so the native decides.
    if let Some(want) = plan.ref_array_element {
        return vm
            .mem
            .heap
            .is_object_address(obj.as_ptr() as usize)
            .is_some_and(|obj| vm.mem.heap.array_element_type(obj) == Some(want));
    }
    // A one-dimensional reference-array field (rt proposals W8-2, W9-2) admits
    // a reference array whose header names the field's component or a subtype
    // of it: `Sub[]` IS a `Base[]` (JLS 4.10.3), which `checkcast` (the
    // native's judge, `array_value_casts_to_desc`, loader-blind)
    // always admits. The header's class id of a reference array is its
    // component, so the element tag is what keeps a plain instance of the
    // component class (`String` into a `String[]` field) out. The subtype
    // question is the exact ClassId relation (`is_subclass_of`, interfaces
    // included) the plain reference-field clause below already trusts, so the
    // fast path stays a subset of what the native admits.
    if plan.ref_array_component_class_id != 0 {
        let Some(arr) = vm.mem.heap.is_object_address(obj.as_ptr() as usize) else {
            return false;
        };
        if vm.mem.heap.array_element_type(arr) != Some(ArrayElementType::Reference) {
            return false;
        }
        let component = vm.mem.heap.class_id_of(arr);
        return component.as_u32() == plan.ref_array_component_class_id
            || jit_is_subclass_of_cached(
                vm,
                component,
                ClassId::new(plan.ref_array_component_class_id),
            );
    }
    if plan.ref_type_class_id == 0 {
        return false;
    }
    // Round 11 wave 13 (lane `icwide`): an ARRAY value declines, so the
    // native decides. Its header carries its COMPONENT's id, so an `Integer[]`
    // passed the `Integer`-field test and was stored into an `Integer` field
    // (`jit_dispatch_class`); the array-typed fields are the two clauses above.
    let Some(value_cid) = jit_dispatch_class(vm, *obj).class_id() else {
        return false;
    };
    if value_cid.as_u32() == plan.ref_type_class_id {
        return true;
    }
    // nio21 wave 14 L4: as the receiver arm, for a value's class.
    vh_const_ensure_display(vm, value_cid.as_u32());
    jit_is_subclass_of_cached(vm, value_cid, ClassId::new(plan.ref_type_class_id))
}

/// One unaligned plain byte[] view load, without allocation or safepoints.
/// The weak metadata identity and both heap bases are validated before payload
/// access; fragmented arrays decline to the authoritative native implementation.
unsafe fn varhandle_byte_array_view_read_bits(
    vm: &SharedVm,
    vh_raw: u64,
    array_raw: u64,
    index: i64,
    site_ret: u8,
) -> Option<i64> {
    if vh_raw == 0 || array_raw == 0 || index < 0 {
        return None;
    }
    let (vh_class, key) = direct_receiver_facts(vm, vh_raw as usize)?;
    if vh_class == 0 {
        return None;
    }
    let plan = cratonvm_native_builtins::lang_invoke::varhandle_byte_array_view_plan_for_handle(
        vm.vm_identity,
        key,
        vh_raw as usize,
    )?;
    if plan.value_desc != site_ret {
        return None;
    }
    direct_receiver_facts(vm, array_raw as usize)?;
    let heap = &vm.mem.heap;
    let array = heap.load_and_forward(ObjectRef::from_raw(array_raw as usize as *mut u8));
    if heap.kind_of(array) != cratonvm_types::ObjectKind::Array
        || heap.element_type_of_validated(array) != ArrayElementType::Byte
    {
        return None;
    }
    let width = match site_ret {
        b'S' | b'C' => 2,
        b'I' | b'F' => 4,
        b'J' | b'D' => 8,
        _ => return None,
    };
    let length = heap.array_length(array);
    let index = usize::try_from(index).ok()?;
    if index.checked_add(width)? > length {
        return None;
    }
    let pointer = heap.array_data_ptr(array)?.add(index);
    // No poll, allocation, or Java call can move the validated array between
    // the pointer acquisition and this read. Plain get permits misalignment.
    let bits = match width {
        2 => {
            let word = std::ptr::read_unaligned(pointer.cast::<u16>());
            (if plan.little_endian {
                u16::from_le(word)
            } else {
                u16::from_be(word)
            }) as u64
        }
        4 => {
            let word = std::ptr::read_unaligned(pointer.cast::<u32>());
            (if plan.little_endian {
                u32::from_le(word)
            } else {
                u32::from_be(word)
            }) as u64
        }
        8 => {
            let word = std::ptr::read_unaligned(pointer.cast::<u64>());
            if plan.little_endian {
                u64::from_le(word)
            } else {
                u64::from_be(word)
            }
        }
        _ => unreachable!(),
    };
    Some(match site_ret {
        b'S' => bits as u16 as i16 as i64,
        b'C' => bits as u16 as i64,
        b'I' | b'F' => bits as u32 as i32 as i64,
        _ => bits as i64,
    })
}

// Only pure layout data is cached. Mutable limit, offset, address, backing
// array and segment fields are read again on every access.
#[derive(Clone, Copy)]
struct ByteBufferViewReadLayout {
    vm: usize,
    origin_epoch: u64,
    layout_epoch: u64,
    buffer_class: u32,
    hb: usize,
    offset: usize,
    limit: usize,
    address: usize,
    segment: usize,
    read_only: usize,
}
thread_local! {
    static BYTE_BUFFER_VIEW_READ_LAYOUT: std::cell::Cell<Option<ByteBufferViewReadLayout>> =
        const { std::cell::Cell::new(None) };
    static BYTE_BUFFER_VIEW_READ_REPORTED_VM: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static BYTE_BUFFER_VIEW_WRITE_REPORTED_VM: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}
fn byte_buffer_view_read_layout(vm: &SharedVm) -> Option<ByteBufferViewReadLayout> {
    let origin_epoch = cratonvm_classloading::class_origin_epoch();
    let layout_epoch = cratonvm_types::layout_generation();
    if let Some(plan) = BYTE_BUFFER_VIEW_READ_LAYOUT.with(std::cell::Cell::get) {
        if plan.vm == vm.vm_identity
            && plan.origin_epoch == origin_epoch
            && plan.layout_epoch == layout_epoch
        {
            return Some(plan);
        }
    }
    let cm = vm.classes.class_manager.read();
    let buffer = cm.find_bootstrap_class_by_name("java/nio/ByteBuffer")?;
    let base = cm.find_bootstrap_class_by_name("java/nio/Buffer")?;
    if !cm.get_class(buffer)?.origin.has_real_bytes()
        || !cm.get_class(base)?.origin.has_real_bytes()
    {
        return None;
    }
    // Resolve from the API's declaring classes, so a subclass's hidden field
    // cannot replace the field the genuine buffer implementation reads.
    let slot = |class, name, desc| {
        crate::vm::resolve_field_index_in_hierarchy_desc(class, name, Some(desc), &cm.class_store)
    };
    let plan = ByteBufferViewReadLayout {
        vm: vm.vm_identity,
        origin_epoch,
        layout_epoch,
        buffer_class: buffer.as_u32(),
        hb: slot(buffer, "hb", "[B")?,
        offset: slot(buffer, "offset", "I")?,
        limit: slot(base, "limit", "I")?,
        address: slot(base, "address", "J")?,
        segment: slot(base, "segment", "Ljava/lang/foreign/MemorySegment;")?,
        read_only: slot(buffer, "isReadOnly", "Z")?,
    };
    drop(cm);
    BYTE_BUFFER_VIEW_READ_LAYOUT.with(|cell| cell.set(Some(plan)));
    Some(plan)
}

/// Read a current field of an already checked/forwarded ByteBuffer. No Java
/// call or safepoint occurs in this helper. The canonical compact slot proof
/// validates the index, storage kind and current layout generation; reference
/// fields retain the same collector barrier as compiled getfield. Legacy or
/// mismatched layouts keep the ordinary typed heap accessor.
unsafe fn checked_byte_buffer_view_field(
    vm: &SharedVm,
    buffer: ObjectRef,
    index: usize,
    descriptor: u8,
) -> Value {
    if let Ok(index_word) = i64::try_from(index) {
        if let Some((offset, storage)) = jit_compact_field_slot(buffer.as_ptr() as i64, index_word)
        {
            if Some(storage) == cratonvm_types::FieldStorageKind::from_descriptor_byte(descriptor) {
                let ptr = buffer.as_ptr().add(offset);
                let value = if storage.is_reference() {
                    let bits = jit_load_ref_slot(
                        ptr,
                        Some(&vm.mem.heap),
                        ref_load_census::GETFIELD_COMPACT_REF,
                        "byte-buffer-view/compact-ref",
                    );
                    Value::Object(
                        (bits != 0).then(|| ObjectRef::from_raw(bits as usize as *mut u8)),
                    )
                } else {
                    cratonvm_types::read_compact_field(
                        ptr,
                        storage,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                };
                if crate::runtime::env_cache::dbg_jitc() {
                    BYTE_BUFFER_VIEW_COMPACT_FIELDS_REPORTED_VM.with(|reported| {
                        if reported.get() != vm.vm_identity {
                            reported.set(vm.vm_identity);
                            eprintln!(
                                "[cratonvm-jitc] byte-buffer-view-compact-fields served vm={}",
                                vm.vm_identity
                            );
                        }
                    });
                }
                return value;
            }
        }
    }
    vm.mem.heap.get_field_as(buffer, index, descriptor)
}
thread_local! {
    static BYTE_BUFFER_VIEW_COMPACT_FIELDS_REPORTED_VM: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

unsafe fn try_varhandle_byte_buffer_view_access(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
    thread: &mut JvmThread,
) -> Option<i64> {
    let write = info.method_name == "set";
    let prefix = "(Ljava/nio/ByteBuffer;I";
    let desc = info.descriptor.as_bytes();
    let value_desc = if write {
        if args.len() != 4
            || info.return_type != b'V'
            || !info.descriptor.starts_with(prefix)
            || desc.len() != prefix.len() + 3
            || &desc[prefix.len() + 1..] != b")V"
        {
            return None;
        }
        desc[prefix.len()]
    } else {
        if info.method_name != "get"
            || args.len() != 3
            || !info.descriptor.starts_with("(Ljava/nio/ByteBuffer;I)")
            || desc.len() != prefix.len() + 2
        {
            return None;
        }
        info.return_type
    };
    if args[0] == 0
        || args[1] == 0
        || !varhandle_thin_fast_arm_admitted(vm, args[0], info as *const _)
    {
        return None;
    }
    let width = match value_desc {
        b'S' | b'C' => 2,
        b'I' | b'F' => 4,
        b'J' | b'D' => 8,
        _ => return None,
    };
    let (_, key) = direct_receiver_facts(vm, args[0] as usize)?;
    let plan = cratonvm_native_builtins::lang_invoke::varhandle_byte_buffer_view_plan_for_handle(
        vm.vm_identity,
        key,
        args[0] as usize,
    )?;
    if plan.value_desc != value_desc {
        return None;
    }
    let layout = byte_buffer_view_read_layout(vm)?;
    let (buffer, live) = vm
        .mem
        .heap
        .load_and_forward_checked(ObjectRef::from_raw(args[1] as usize as *mut u8));
    if !live {
        return None;
    }
    let class = vm.mem.heap.class_id_of_validated(buffer).as_u32();
    if !jit_is_subclass_of_cached(vm, ClassId::new(class), ClassId::new(layout.buffer_class)) {
        return None;
    }
    let heap = &vm.mem.heap;
    if write
        && !matches!(
            checked_byte_buffer_view_field(vm, buffer, layout.read_only, b'Z'),
            Value::Int(0)
        )
    {
        return None;
    }
    // The window covers the proof lookup and the memory read. A proof is
    // published only by the checked native scope road, on this thread and at
    // the current GC/free/close epoch. Re-read the segment's session on reuse.
    let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
    if !window.admits_fast_path() {
        return None;
    }
    match checked_byte_buffer_view_field(vm, buffer, layout.segment, b'L') {
        Value::Object(None) => {}
        Value::Object(Some(segment)) => {
            let (segment, live) = heap.load_and_forward_checked(segment);
            if !live {
                return None;
            }
            let session = buffer_stored_session_with(vm, segment, || {
                let ctx = crate::vm::NativeContextImpl { shared: vm, thread };
                cratonvm_native_builtins::lang_invoke::byte_buffer_view_stored_session(
                    &ctx, segment,
                )
            })?;
            if !cratonvm_native_builtins::ffm_fast::is_validated(session.as_ptr() as u64, false) {
                return None;
            }
        }
        _ => return None,
    }
    let Value::Int(limit) = checked_byte_buffer_view_field(vm, buffer, layout.limit, b'I') else {
        return None;
    };
    let index = i32::try_from(args[2]).ok()?;
    if index < 0 || index.checked_add(width as i32)? > limit {
        return None;
    }
    let (base, offset) = match checked_byte_buffer_view_field(vm, buffer, layout.hb, b'L') {
        Value::Object(Some(array)) => {
            let (array, live) = heap.load_and_forward_checked(array);
            if !live
                || heap.kind_of(array) != cratonvm_types::ObjectKind::Array
                || heap.element_type_of_validated(array) != ArrayElementType::Byte
            {
                return None;
            }
            let Value::Int(offset) =
                checked_byte_buffer_view_field(vm, buffer, layout.offset, b'I')
            else {
                return None;
            };
            if offset < 0 {
                return None;
            }
            (
                array.as_ptr() as i64,
                16i64
                    .checked_add(i64::from(offset))?
                    .checked_add(i64::from(index))?,
            )
        }
        Value::Object(None) => {
            let gate = match (write, value_desc) {
                (true, b'S') => &UNSAFE_PUT_SHORT_GATE,
                (true, b'C') => &UNSAFE_PUT_CHAR_GATE,
                (true, b'I' | b'F') => &UNSAFE_PUT_INT_GATE,
                (true, _) => &UNSAFE_PUT_LONG_GATE,
                (false, b'S') => &UNSAFE_GET_SHORT_GATE,
                (false, b'C') => &UNSAFE_GET_CHAR_GATE,
                (false, b'I' | b'F') => &UNSAFE_GET_INT_GATE,
                (false, _) => &UNSAFE_GET_LONG_GATE,
            };
            if !unsafe_accessor_capability_admits(vm, gate) {
                return None;
            }
            let Value::Long(address) =
                checked_byte_buffer_view_field(vm, buffer, layout.address, b'J')
            else {
                return None;
            };
            if address <= 0 {
                return None;
            }
            // Native-owned blocks only. External/raw JNI addresses retain
            // the authoritative native callback and its provenance contract.
            if !cratonvm_native_builtins::unsafe_arena_names_block(address) {
                return None;
            }
            (0, address.checked_add(i64::from(index))?)
        }
        _ => return None,
    };
    let mut bytes = [0u8; 8];
    if write {
        let bits = args[3] as u64;
        for (i, byte) in bytes[..width].iter_mut().enumerate() {
            let shift = 8 * if plan.little_endian { i } else { width - i - 1 };
            *byte = (bits >> shift) as u8;
        }
        if !raw_accessor_write_bytes(vm, base, offset, &bytes[..width]) {
            return None;
        }
        if crate::runtime::env_cache::dbg_jitc() {
            BYTE_BUFFER_VIEW_WRITE_REPORTED_VM.with(|reported| {
                if reported.get() != vm.vm_identity {
                    reported.set(vm.vm_identity);
                    eprintln!(
                        "[cratonvm-jitc] byte-buffer-view-write served vm={}",
                        vm.vm_identity
                    );
                }
            });
        }
        return Some(0);
    }
    if !raw_accessor_read_bytes(vm, base, offset, &mut bytes[..width]) {
        return None;
    }
    let mut bits = 0u64;
    for (i, byte) in bytes[..width].iter().enumerate() {
        let shift = 8 * if plan.little_endian { i } else { width - i - 1 };
        bits |= u64::from(*byte) << shift;
    }
    if crate::runtime::env_cache::dbg_jitc() {
        BYTE_BUFFER_VIEW_READ_REPORTED_VM.with(|reported| {
            if reported.get() != vm.vm_identity {
                reported.set(vm.vm_identity);
                eprintln!(
                    "[cratonvm-jitc] byte-buffer-view-read served vm={}",
                    vm.vm_identity
                );
            }
        });
    }
    Some(match info.return_type {
        b'S' => bits as u16 as i16 as i64,
        b'C' => bits as u16 as i64,
        b'I' | b'F' => bits as u32 as i32 as i64,
        _ => bits as i64,
    })
}

unsafe fn try_varhandle_byte_array_view_read(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<i64> {
    cratonvm_jit::varhandle_byte_array_view_read_slot(info.method_name, info.descriptor)?;
    if !info.descriptor.starts_with("([BI)") || args.len() != 3 {
        return None;
    }
    varhandle_byte_array_view_read_bits(
        vm,
        args[0] as u64,
        args[1] as u64,
        args[2],
        info.return_type,
    )
}

static SYSTEM_ARRAYCOPY_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/System",
    method_name: "arraycopy",
    descriptor: "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    num_jit_args: 5,
    return_type: b'V',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// The primitive copy is allocation/poll free. Reference arrays, fragmented
/// carriers and every fault retain the authoritative native's exception order,
/// partial-copy semantics and collector barriers.
unsafe fn try_primitive_arraycopy(
    vm: &SharedVm,
    src: i64,
    src_pos: i64,
    dst: i64,
    dst_pos: i64,
    len: i64,
) -> Option<()> {
    if src == 0 || dst == 0 {
        return None;
    }
    let src_pos = usize::try_from(i32::try_from(src_pos).ok()?).ok()?;
    let dst_pos = usize::try_from(i32::try_from(dst_pos).ok()?).ok()?;
    let len = usize::try_from(i32::try_from(len).ok()?).ok()?;
    let heap = &vm.mem.heap;
    let (src, src_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(src as usize as *mut u8));
    let (dst, dst_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(dst as usize as *mut u8));
    if !src_live
        || !dst_live
        || heap.kind_of(src) != cratonvm_types::ObjectKind::Array
        || heap.kind_of(dst) != cratonvm_types::ObjectKind::Array
    {
        return None;
    }
    let kind = heap.element_type_of_validated(src);
    if kind == ArrayElementType::Reference || kind != heap.element_type_of_validated(dst) {
        return None;
    }
    let (src_len, dst_len) = (heap.array_length(src), heap.array_length(dst));
    try_primitive_arraycopy_pair(vm, kind, src, src_pos, src_len, dst, dst_pos, dst_len, len)
}

/// [`try_primitive_arraycopy`] past its validation: `src` / `dst` are live
/// arrays of the one primitive element type `kind` (validated by the caller
/// with no safepoint since), of `src_len` / `dst_len` elements.
#[allow(clippy::too_many_arguments)]
unsafe fn try_primitive_arraycopy_pair(
    vm: &SharedVm,
    kind: ArrayElementType,
    src: ObjectRef,
    src_pos: usize,
    src_len: usize,
    dst: ObjectRef,
    dst_pos: usize,
    dst_len: usize,
    len: usize,
) -> Option<()> {
    let heap = &vm.mem.heap;
    if src_pos.checked_add(len)? > src_len || dst_pos.checked_add(len)? > dst_len {
        return None;
    }
    let width = cratonvm_types::element_byte_size(kind);
    let bytes = len.checked_mul(width)?;
    let src_offset = src_pos.checked_mul(width)?;
    let dst_offset = dst_pos.checked_mul(width)?;
    let src_data = heap.array_data_ptr(src)?.add(src_offset);
    let dst_data = heap.array_data_ptr(dst)?.add(dst_offset);
    // No allocation, poll or Java call can relocate these validated carriers.
    //
    // JIT round 21 wave 3 (lane intrinsics; correctness, default ON,
    // `CRATONVM_JIT_ARRAYCOPY_DIRECT_ELEMENT_ATOMIC=0` restores the plain
    // `memmove`): every 2/4/8-byte element moves WHOLE, as HotSpot's
    // `conjoint_j*_atomic` stubs move it. This helper is the optimizing tier's
    // `System.arraycopy` (`ir_arraycopy_entry`, `CRATONVM_JIT_IR_ARRAYCOPY`
    // default ON); round 20 (fuzz14c / fuzz14d) made the native's
    // `bulk_array_copy` and the single-pass `REP MOVS*` arm element-atomic and
    // left this third copy a byte `memmove`, whose head/tail may move one
    // element in pieces, so a racing writer's element could be read TORN here
    // (`R20FuzzGTearCopy1` `tear.copyof`'s shape, reached from an IR body).
    // Both forms are memmove-correct for an overlapping same-array copy.
    if arraycopy_direct_element_atomic(vm) {
        crate::vm::vm_exec::bulk_copy_element_atomic(
            src_data as *const u8,
            dst_data,
            bytes,
            width,
        );
    } else {
        std::ptr::copy(src_data, dst_data, bytes);
    }
    Some(())
}

/// `CRATONVM_JIT_ARRAYCOPY_DIRECT_ELEMENT_ATOMIC` (JIT round 21 wave 3, lane
/// intrinsics; correctness, **default ON**, `0` restores the byte `memmove`):
/// whether [`try_primitive_arraycopy`] moves each element whole. Read once per
/// VM per thread (the helper runs on every IR-tier `System.arraycopy`), the
/// shape `lang_math`'s `number_parse_raise_jdk_body` caches its switch in.
fn arraycopy_direct_element_atomic(vm: &SharedVm) -> bool {
    arraycopy_direct_switches(vm).0
}

/// The two switches of `jit_system_arraycopy_direct`, read once per VM per
/// thread into ONE thread-local (JIT r23 wave 3b, lane L5: the second,
/// `CRATONVM_JIT_REF_ARRAYCOPY_DIRECT`, shares the first's slot rather than
/// adding a static -- `vm/tests/per_vm_state_statics_ratchet.rs`):
/// `(element_atomic, ref_direct, one_validation, new_array_mirror_memo)` --
/// the third is `CRATONVM_JIT_ARRAYCOPY_DIRECT_ONE_VALIDATION` (JIT r23 wave 5,
/// lane L5, [`try_arraycopy_validated_once`]); the fourth, sharing the slot for
/// the same ratchet reason, is `CRATONVM_JIT_REFLECT_NEW_ARRAY_MIRROR_MEMO`
/// ([`reflect_new_array_serve`]); the fifth (JIT r23 wave 8, lane L8) is
/// `CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY`, whose VM half is the
/// mirror-hint fill in [`reflect_new_array_serve`].
fn arraycopy_direct_switches(vm: &SharedVm) -> (bool, bool, bool, bool, bool) {
    thread_local! {
        static ON: std::cell::Cell<Option<(usize, [bool; 5])>> =
            const { std::cell::Cell::new(None) };
    }
    let id = vm.vm_identity;
    let [atomic, ref_direct, once, memo, inline_new_array] = ON.with(|c| match c.get() {
        Some((owner, on)) if owner == id => on,
        _ => {
            let on = [
                cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JIT_ARRAYCOPY_DIRECT_ELEMENT_ATOMIC",
                ),
                cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_REF_ARRAYCOPY_DIRECT"),
                cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JIT_ARRAYCOPY_DIRECT_ONE_VALIDATION",
                ),
                cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JIT_REFLECT_NEW_ARRAY_MIRROR_MEMO",
                ),
                cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY",
                ),
            ];
            c.set(Some((id, on)));
            on
        }
    });
    (atomic, ref_direct, once, memo, inline_new_array)
}

/// JIT r23 wave 5, lane L5 (`CRATONVM_JIT_ARRAYCOPY_DIRECT_ONE_VALIDATION`,
/// perf, default ON, `=0` restores the two self-validating attempts):
/// `jit_system_arraycopy_direct` validates the two arrays ONCE and branches on
/// their element types, instead of [`try_primitive_arraycopy`] validating
/// both, declining a reference pair, and [`try_reference_arraycopy`]
/// validating both again -- and each validation was four defensive accessors
/// (`kind_of`, `element_type_of_validated`, `array_length` per array), every
/// one of them re-running the ZGC object-start registry lookup
/// (`ZObjectStarts::contains`): the RIP profile of a compiled
/// `Object[10] -> Object[10]` copy loop on n23-w4 had `is_object_address` +
/// `ZObjectStarts::contains` at 45k of ~200k samples and the helper's own
/// frame at 28k, for 170-230 ns a copy against HotSpot's 2. Here each array is
/// validated once (`load_and_forward_checked`, the same live-base test the
/// accessors run) and its kind, element type and length are read straight off
/// the header (`native_array_footprint`'s argument: the defence buys nothing
/// for a base validated a moment ago with no safepoint between). The copies
/// themselves are the unchanged `*_pair` tails.
unsafe fn try_arraycopy_validated_once(
    vm: &SharedVm,
    src: i64,
    src_pos: i64,
    dst: i64,
    dst_pos: i64,
    len: i64,
    ref_direct: bool,
) -> Option<()> {
    if src == 0 || dst == 0 {
        return None;
    }
    let src_pos = usize::try_from(i32::try_from(src_pos).ok()?).ok()?;
    let dst_pos = usize::try_from(i32::try_from(dst_pos).ok()?).ok()?;
    let len = usize::try_from(i32::try_from(len).ok()?).ok()?;
    let heap = &vm.mem.heap;
    let (src, src_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(src as usize as *mut u8));
    let (dst, dst_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(dst as usize as *mut u8));
    if !src_live || !dst_live {
        return None;
    }
    let read = |obj: ObjectRef| {
        let header = heap.get_header(obj);
        (
            header.kind(),
            header.element_type(),
            header.array_length() as usize,
        )
    };
    let (src_kind, src_elem, src_len) = read(src);
    let (dst_kind, dst_elem, dst_len) = read(dst);
    if src_kind != cratonvm_types::ObjectKind::Array || dst_kind != cratonvm_types::ObjectKind::Array
    {
        return None;
    }
    match (src_elem, dst_elem) {
        (ArrayElementType::Reference, ArrayElementType::Reference) if ref_direct => {
            try_reference_arraycopy_pair(vm, src, src_pos, src_len, dst, dst_pos, dst_len, len)
        }
        (ArrayElementType::Reference, _) | (_, ArrayElementType::Reference) => None,
        (s, d) if s == d => {
            try_primitive_arraycopy_pair(vm, s, src, src_pos, src_len, dst, dst_pos, dst_len, len)
        }
        _ => None,
    }
}

/// `CRATONVM_JIT_REF_ARRAYCOPY_DIRECT` (JIT r23 wave 3b, lane L5; perf,
/// default ON, `0` restores the native dispatch): the optimizing tier's
/// `System.arraycopy` helper also answers a REFERENCE copy itself
/// ([`try_reference_arraycopy`]) instead of crossing the native funnel to
/// `native_system_arraycopy` (`r23-w3-reference-arraycopy-costs-microseconds-FIXED-20261021.md`:
/// ~0.6-1.3 us of fixed cost per call on ZGC -- the funnel, three validations
/// of each array and a `class_id_by_name("java/lang/Object")` lookup per
/// call). Read once per VM per thread ([`arraycopy_direct_switches`]).
fn ref_arraycopy_direct_enabled(vm: &SharedVm) -> bool {
    arraycopy_direct_switches(vm).1
}

/// `System.arraycopy` of two REFERENCE arrays, answered here when it is
/// certain to complete: both arrays live reference arrays, every position in
/// range, and every element storable -- the destination component equal to the
/// source's or `java/lang/Object` (no check needed), or every element proven
/// (null, or a plain object whose class is the destination component or a
/// subtype, `class_is_subtype`, memoised per call). `None` for anything else
/// (a null, a non-array, a primitive array, a bound the JDK throws on, an
/// element the proof cannot settle, an auto-box wrapper): the caller's native
/// dispatch then answers, throws (the exact `ArrayStoreException` /
/// `ArrayIndexOutOfBoundsException` texts) and commits the partial copy
/// exactly as before. Nothing is written before a `None`, except by a CHECKED
/// copy (two different arrays) that stops at its first unprovable element:
/// that prefix is exactly the one the native fallback re-copies first.
///
/// The stores carry the collector's obligations as
/// `NativeContextImpl::copy_reference_array_elements` does: on a quiet ZGC the
/// word copy plus one object-keyed card (`copy_reference_slots_raw`); on G1 /
/// Generational with no marking window and no humongous array the word copy
/// plus each stored reference's post barrier (the same function's
/// `CRATONVM_G1_GEN_RAW_REF_ARRAYCOPY` arm); otherwise the heap's own per-element accessor (G1 / ZGC publish the
/// overwritten value inside it; Generational's SATB pre-barrier is
/// `publish_overwritten_array_element`, and its card is the accessor's), in
/// memmove order. No allocation, poll or Java call runs here, so neither array
/// can move (the `try_primitive_arraycopy` argument).
unsafe fn try_reference_arraycopy(
    vm: &SharedVm,
    src: i64,
    src_pos: i64,
    dst: i64,
    dst_pos: i64,
    len: i64,
) -> Option<()> {
    if src == 0 || dst == 0 {
        return None;
    }
    let src_pos = usize::try_from(i32::try_from(src_pos).ok()?).ok()?;
    let dst_pos = usize::try_from(i32::try_from(dst_pos).ok()?).ok()?;
    let len = usize::try_from(i32::try_from(len).ok()?).ok()?;
    let heap = &vm.mem.heap;
    let (src, src_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(src as usize as *mut u8));
    let (dst, dst_live) =
        heap.load_and_forward_checked(ObjectRef::from_raw(dst as usize as *mut u8));
    if !src_live
        || !dst_live
        || heap.kind_of(src) != cratonvm_types::ObjectKind::Array
        || heap.kind_of(dst) != cratonvm_types::ObjectKind::Array
        || heap.element_type_of_validated(src) != ArrayElementType::Reference
        || heap.element_type_of_validated(dst) != ArrayElementType::Reference
    {
        return None;
    }
    let (src_len, dst_len) = (heap.array_length(src), heap.array_length(dst));
    try_reference_arraycopy_pair(vm, src, src_pos, src_len, dst, dst_pos, dst_len, len)
}

/// [`try_reference_arraycopy`] past its validation: `src` / `dst` are live
/// reference arrays of `src_len` / `dst_len` elements (validated by the caller
/// with no safepoint since), the positions and length non-negative `int`s.
#[allow(clippy::too_many_arguments)]
unsafe fn try_reference_arraycopy_pair(
    vm: &SharedVm,
    src: ObjectRef,
    src_pos: usize,
    src_len: usize,
    dst: ObjectRef,
    dst_pos: usize,
    dst_len: usize,
    len: usize,
) -> Option<()> {
    let heap = &vm.mem.heap;
    if src_pos.checked_add(len)? > src_len || dst_pos.checked_add(len)? > dst_len {
        return None;
    }
    if len == 0 {
        return Some(());
    }
    let src_component = heap.get_header(src).class_id;
    let dst_component = heap.get_header(dst).class_id;
    let unchecked = src_component == dst_component
        || crate::vm::vm_exec::object_class_id(vm) == Some(dst_component);
    let backward = src.as_ptr() == dst.as_ptr() && dst_pos > src_pos;
    // One proof per distinct element class, the last one memoised.
    let mut proven: Option<cratonvm_types::ClassId> = None;
    let mut storable = |elem: ObjectRef| -> bool {
        if unchecked {
            // Any reference is storable (an array element included); an
            // auto-box payload is refused by the reader below, not here.
            return true;
        }
        let header = heap.get_header(elem);
        if header.kind() != cratonvm_types::ObjectKind::Object {
            return false;
        }
        let cid = header.class_id;
        if cid == cratonvm_types::AUTOBOX_CLASS_ID {
            return false;
        }
        if proven == Some(cid) {
            return true;
        }
        if cid == dst_component
            || crate::runtime::interpreter::class_is_subtype(vm, cid, dst_component)
        {
            proven = Some(cid);
            return true;
        }
        false
    };
    // JIT r23 wave 3 review: a CHECKED copy proves the element it STORES
    // (one load, one proof, one store per slot), never a separate proof pass
    // followed by a copy -- a racing writer of `src` (an `Object[]` source
    // whose static type admits anything) could otherwise slip an unproven
    // reference into `dst` between the two and break heap type safety, which
    // HotSpot's checkcast arraycopy never allows. A checked copy is never a
    // same-array copy (one array has one component id), so when the proof
    // stops at slot `n` the prefix written is exactly what the native
    // fallback re-copies before it throws `ArrayStoreException` at that same
    // element (HotSpot's partial-copy semantics); re-copying a prefix of a
    // DIFFERENT array is idempotent.
    if !unchecked {
        let copied = match crate::vm::vm_exec::copy_reference_slots_raw_checked(
            heap,
            src,
            src_pos,
            dst,
            dst_pos,
            len,
            &mut storable,
        ) {
            Some(n) => n,
            None => {
                let mut n = 0usize;
                while n < len {
                    // The non-unboxing reader: a checked copy refuses an
                    // auto-box wrapper in `storable`, never re-boxes.
                    // One read: the value proven is the value stored.
                    let value = match heap.get_array_element(src, src_pos + n) {
                        Ok(v @ Value::Object(None)) => v,
                        Ok(v @ Value::Object(Some(elem))) if storable(elem) => v,
                        _ => break,
                    };
                    crate::vm::vm_exec::publish_overwritten_array_element(
                        heap,
                        || true,
                        dst,
                        dst_pos + n,
                    );
                    let _ = heap.set_array_element(dst, dst_pos + n, value);
                    n += 1;
                }
                n
            }
        };
        #[cfg(feature = "gpu-offload")]
        {
            if copied > 0 {
                crate::runtime::offload::input_cache::invalidate(dst);
            }
        }
        return (copied == len).then_some(());
    }
    // An UNCHECKED copy (any reference is storable): the raw word arm when the
    // collector admits it -- a quiet ZGC, or G1 / Generational with no marking
    // window and no humongous array (`CRATONVM_G1_GEN_RAW_REF_ARRAYCOPY`) --
    // words moved verbatim in memmove order.
    if crate::vm::vm_exec::copy_reference_slots_raw(heap, src, src_pos, dst, dst_pos, len, backward)
    {
        #[cfg(feature = "gpu-offload")]
        crate::runtime::offload::input_cache::invalidate(dst);
        return Some(());
    }
    // Every other collector state: the per-element accessors in memmove
    // order. An unchecked copy may overlap itself, so nothing may be written
    // unless the whole copy completes: refuse an auto-box payload (the only
    // element the unboxing reader cannot hand straight back to the store)
    // before the first store.
    for i in 0..len {
        match heap.get_array_element_unboxing(src, src_pos + i) {
            Ok(Value::Object(_)) => {}
            _ => return None,
        }
    }
    for step in 0..len {
        let i = if backward { len - 1 - step } else { step };
        crate::vm::vm_exec::publish_overwritten_array_element(heap, || true, dst, dst_pos + i);
        // The non-unboxing reader: a racing wrapper is copied verbatim (as
        // the raw arm does), never re-boxed -- no allocation runs here.
        let value = heap
            .get_array_element(src, src_pos + i)
            .unwrap_or(Value::Object(None));
        let _ = heap.set_array_element(dst, dst_pos + i, value);
    }
    #[cfg(feature = "gpu-offload")]
    crate::runtime::offload::input_cache::invalidate(dst);
    Some(())
}

pub unsafe extern "C" fn jit_system_arraycopy_direct(
    vm_ptr: i64,
    src: i64,
    src_pos: i64,
    dst: i64,
    dst_pos: i64,
    len: i64,
) -> i64 {
    let bits = contain(
        "jit_system_arraycopy_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            let vm = &*(vm_ptr as *const SharedVm);
            // JIT r23 wave 6, lane L5: ONE read of the switch slot per call
            // (the n23-w5 `Object[10]` copy profile had two -- 19k of ~250k
            // samples in the thread-local's accessor).
            let (_, ref_direct, once, _, _) = arraycopy_direct_switches(vm);
            let served = if once {
                try_arraycopy_validated_once(vm, src, src_pos, dst, dst_pos, len, ref_direct)
                    .is_some()
            } else {
                try_primitive_arraycopy(vm, src, src_pos, dst, dst_pos, len).is_some()
                    || (ref_arraycopy_direct_enabled(vm)
                        && try_reference_arraycopy(vm, src, src_pos, dst, dst_pos, len).is_some())
            };
            if served {
                if crate::runtime::env_cache::dbg_jitc() {
                    ARRAYCOPY_DIRECT_REPORTED_VM.with(|reported| {
                        if reported.get() != vm.vm_identity {
                            reported.set(vm.vm_identity);
                            eprintln!(
                                "[cratonvm-jitc] primitive-arraycopy direct served vm={}",
                                vm.vm_identity
                            );
                        }
                    });
                }
                return 0;
            }
            let args = [src, src_pos, dst, dst_pos, len];
            let bits = jit_invoke_dispatch(
                vm_ptr,
                &SYSTEM_ARRAYCOPY_INFO as *const _ as i64,
                args.as_ptr() as i64,
                5,
            );
            // handoff-w36b, only where a native ran: a served copy published
            // no return (wave 6: the unconditional call was the `JIT_THREAD`
            // thread-local, 9k samples, on every served copy).
            native_return_handed_to_compiled_code();
            bits
        },
    );
    bits
}
thread_local! {
    static ARRAYCOPY_DIRECT_REPORTED_VM: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Synthetic call-site info for [`jit_reflect_new_array_direct`]'s decline
/// edge: the registered `Array.newArray` native, exactly as the unbound site
/// dispatched it. A `const` (its `&` is promoted to one `'static` address), so
/// `per_vm_state_statics_ratchet` counts no new declaration.
const REFLECT_NEW_ARRAY_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/reflect/Array",
    method_name: "newArray",
    descriptor: "(Ljava/lang/Class;I)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// May [`jit_reflect_new_array_direct`] allocate an array whose component is
/// the class named `name` (the class record's own, `/`-separated) itself?
/// Only a reference component: a primitive (or `void`) mirror, and an array
/// class that already has 255 dimensions (HotSpot's `IllegalArgumentException`,
/// `Reflection::reflect_new_array`), go to the registered native, which raises
/// or allocates exactly as before.
pub(crate) fn reflect_new_array_direct_admits_name(name: &str) -> bool {
    if name.is_empty()
        || matches!(
            name,
            "void" | "V" | "int" | "long" | "float" | "double" | "boolean" | "byte" | "char"
                | "short"
        )
    {
        return false;
    }
    name.bytes().take_while(|&b| b == b'[').count() < 255
}

/// JIT r23 wave 8, lane L8: the mirror-hint kind of the class named `name`
/// for [`class_mirror_id_hint_fill`], from [`reflect_new_array_serve`] --
/// exactly the rule the `Class.isAssignableFrom` / `isInstance` natives give
/// it (`native-builtins` `note_reflect_mirror_id` over `NameFacts`), so the
/// two fillers never disagree about a class: `0` (no hint) for an array or a
/// primitive-named class, `1` (a hint whose misses are not answered) for a
/// `synthetic_implements` target, `2` otherwise.
pub(crate) fn reflect_new_array_hint_kind(name: &str) -> u8 {
    if name.starts_with('[')
        || matches!(
            name,
            "int" | "long" | "float" | "double" | "boolean" | "char" | "byte" | "short" | "void"
        )
    {
        0
    } else if cratonvm_types::synthetic_targets::synthetic_implements_may_admit(name) {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod r23_w8_reflect_new_array_hint_kind_tests {
    use super::reflect_new_array_hint_kind;

    /// No hint for an array or a primitive-named class (the inline prefix
    /// must never resolve them: IAE / primitive-array / multi-dimension arms),
    /// misses withheld for a synthetic target, a full hint otherwise.
    #[test]
    fn the_kind_is_the_assignable_filler_s_rule() {
        for name in [
            "[Ljava/lang/String;",
            "[I",
            "int",
            "void",
            "boolean",
            "double",
        ] {
            assert_eq!(reflect_new_array_hint_kind(name), 0, "{name}");
        }
        for t in cratonvm_types::synthetic_targets::SYNTHETIC_IMPLEMENTS_TARGETS {
            assert_eq!(reflect_new_array_hint_kind(t), 1, "{t}");
        }
        for name in ["java/lang/String", "java/util/HashMap", "R23RefArraycopy", "V", "I"] {
            assert_eq!(reflect_new_array_hint_kind(name), 2, "{name}");
        }
    }
}

/// JIT round 23 wave 4, lane L5 (`CRATONVM_JIT_REFLECT_NEW_ARRAY_DIRECT`, perf,
/// default ON, `=0` restores the plain dispatch; decided per site at compile
/// time): the thin helper both compile doors bind a
/// `java/lang/reflect/Array.newArray(Class, int)` site to -- the native the
/// JDK's `Array.newInstance` calls, and with it `Arrays.copyOf(T[], int)` and
/// `ArrayList.toArray(T[])`. The registered native (`native_array_new_array`)
/// costs ~0.5-2 us a call on the dispatch funnel and its per-call name
/// resolution (r23-w3 reference-arraycopy page, wave 4); this answers the
/// common case with the component mirror's reverse-map lookup, one class-record
/// read and the allocation a compiled `anewarray` makes
/// (`jit_anewarray_object_body`: its entry hooks, the TLAB bump, the collecting
/// slow path, its `OutOfMemoryError`). Everything else -- a null component
/// (NPE), a negative length (NASE), a primitive / `void` / 255-dimension
/// component, a mirror not in the reverse map, a JVMTI `VMObjectAlloc`
/// listener, an array past the TLAB cap -- is decided before anything can
/// collect and dispatches the registered native, which owns those exceptions
/// and the large-array ladder.
pub(in crate::jit) unsafe extern "C" fn jit_reflect_new_array_direct(
    vm_ptr: i64,
    component: i64,
    length: i64,
) -> i64 {
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        if let Some(array) = reflect_new_array_serve(vm_ptr, component, length) {
            return array;
        }
        let args = [component, length];
        let bits = jit_invoke_dispatch(
            vm_ptr,
            &REFLECT_NEW_ARRAY_INFO as *const JitInvokeInfo as i64,
            args.as_ptr() as i64,
            2,
        );
        // handoff-w36b: the native's returned array is in RAX now.
        native_return_handed_to_compiled_code();
        bits
    };
    contain(
        "jit_reflect_new_array_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// The served arm of [`jit_reflect_new_array_direct`]; `None` declines.
unsafe fn reflect_new_array_serve(vm_ptr: i64, component: i64, length: i64) -> Option<i64> {
    let length = usize::try_from(length as i32).ok()?;
    if vm_ptr == 0 || component == 0 {
        return None;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    #[cfg(feature = "experimental-debug")]
    if vm
        .debug
        .debugger_gates
        .native_event_armed(crate::debug::NativeEvent::VmObjectAlloc)
    {
        return None;
    }
    let (mirror, live) = vm
        .mem
        .heap
        .load_and_forward_checked(ObjectRef::from_raw(component as usize as *mut u8));
    if !live {
        return None;
    }
    // The reverse-map lookup and the name test, both stable for a mirror
    // address within one GC pause generation.
    let resolve = || {
        let component_id = crate::vm::class_id_from_mirror(vm, mirror)?;
        let classes = vm.classes.class_manager.read();
        let class = classes.get_class(component_id)?;
        reflect_new_array_direct_admits_name(&class.name).then_some(component_id)
    };
    // JIT r23 wave 5, lane L5 (`CRATONVM_JIT_REFLECT_NEW_ARRAY_MIRROR_MEMO`,
    // perf, default ON, `=0` asks the two locks per call): the n23-w4 profile
    // of an `Array.newInstance` spin had `class_id_from_mirror` (the reverse
    // map's read lock + hash) at 24k and the class manager's read guard at 8k
    // of ~180k samples. A hit of the per-thread `mirror_id_memo` (keyed by VM,
    // pause generation -- loaded BEFORE the lookup -- and mirror address; the
    // contract of that module) answers both: only ADMITTED components are
    // filled (the closure returns `None` otherwise, which the memo never
    // remembers), and an admitted entry is still a true mirror -> class id row
    // for the module's other reader. That other reader
    // (`NativeContext::class_id_from_mirror` under the opt-in
    // `CRATONVM_CLASS_ID_FROM_MIRROR_MEMO`) fills unfiltered rows -- a
    // 255-dimension array class among them -- so with it on this path asks
    // the locks, and a hit here always names an admitted component.
    let component_id = if arraycopy_direct_switches(vm).3
        && !cratonvm_native_builtins::mirror_id_memo::enabled()
    {
        let generation = vm
            .mem
            .gc_barrier
            .gc_generation
            .load(std::sync::atomic::Ordering::Acquire);
        cratonvm_native_builtins::mirror_id_memo::lookup_or_fill(
            vm.vm_identity,
            generation,
            mirror.as_ptr() as usize,
            resolve,
        )?
    } else {
        resolve()?
    };
    // JIT r23 wave 8, lane L8 (`CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY`):
    // publish this mirror's hint, which the optimizing tier's inline
    // `Array.newArray` prefix resolves the component through
    // (`cratonvm_jit::ir_lower::reflect_new_array_resolve_bytes`), so the
    // next compiled call of this component makes no call. Before the
    // allocation (which may collect and move the mirror). Never changes an
    // answer: compiled code proves a hint against the class word, and the
    // kind is the `Class.isAssignableFrom` filler's own rule. Steady state:
    // two loads (`class_mirror_id_hint_fill`'s early return).
    if arraycopy_direct_switches(vm).4 {
        class_mirror_id_hint_fill(vm, mirror, component_id, &|| {
            vm.classes
                .class_manager
                .read()
                .get_class(component_id)
                .map_or(0, |class| reflect_new_array_hint_kind(&class.name))
        });
    }
    // Past the TLAB cap the native's own ladder decides (old-gen / humongous
    // routing, the VM-limit refusal); decided here, before anything can
    // collect, so the decline still hands the native a valid mirror word.
    let data_size =
        cratonvm_types::array_data_size_checked(length, ArrayElementType::Reference)?;
    if cratonvm_types::ARRAY_DATA_OFFSET.checked_add(data_size)?
        > cratonvm_gc::tlab::tlab_max_alloc()
    {
        return None;
    }
    // r23 w4 review (`r23-w4-review-reflect-new-array-direct-skips-allocation-
    // entry-hooks`): the allocation is a compiled `anewarray`'s, entry hooks
    // included -- the SATB flush and the ZGC / G1 / Generational
    // concurrent-cycle starts, which let a compiled allocation loop start a
    // cycle (a TLAB-only serve never did), then the TLAB bump and, on a miss,
    // the collecting slow path. Those may run a whole cycle, so nothing past
    // this point reads the mirror (or its raw word): the allocation takes the
    // component's class id, as `anewarray` does, and there is no decline
    // after it. Its only failure is the `OutOfMemoryError` it publishes as
    // the pending exception with a 0 result; this helper's caller reads the
    // exception sentinel instead, so 0 becomes `i64::MIN`.
    let bits =
        jit_anewarray_object_body(vm_ptr, i64::from(component_id.as_u32()), length as i64);
    Some(if bits == 0 { i64::MIN } else { bits })
}

/// Decline-edge call-site infos for [`jit_string_matches_direct`] and
/// [`jit_string_replace_direct`]: the registered `String` natives, dispatched
/// as the unbound site did (`String` is final, so a special dispatch names the
/// one target). `const`s, as [`REFLECT_NEW_ARRAY_INFO`].
const STRING_MATCHES_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/String",
    method_name: "matches",
    descriptor: "(Ljava/lang/String;)Z",
    num_jit_args: 2,
    return_type: b'Z',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};
const STRING_REPLACE_ALL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/String",
    method_name: "replaceAll",
    descriptor: "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};
const STRING_REPLACE_FIRST_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/String",
    method_name: "replaceFirst",
    descriptor: "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// JIT round 23 wave 4, lane L5 (`CRATONVM_JIT_STRING_REGEX_DIRECT`, decided
/// per site at compile time): the served arm of the `String` regex thin
/// helpers. Runs the registered native's first step, its exact path
/// (`lang_string::string_regex_thin`), on the caller's thread without the
/// dispatch funnel; `None` (any decline, any exception) leaves the call to the
/// funnel, which runs the same native from the top and so answers -- or raises
/// -- exactly as the unbound site did.
unsafe fn string_regex_direct_serve(
    vm_ptr: i64,
    words: &[i64],
    op: cratonvm_native_builtins::lang_string::StringRegexThinOp,
) -> Option<i64> {
    if vm_ptr == 0 {
        return None;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let mut args = [Value::Object(None), Value::Object(None), Value::Object(None)];
    for (slot, &word) in args.iter_mut().zip(words) {
        if word == 0 {
            // A null regex / replacement is the native's NPE: dispatch it.
            return None;
        }
        let (obj, live) = vm
            .mem
            .heap
            .load_and_forward_checked(ObjectRef::from_raw(word as usize as *mut u8));
        if !live {
            return None;
        }
        *slot = Value::Object(Some(obj));
    }
    let (thread, _guard) = jit_thread_mut()?;
    // Review (wave 4): the result `String` is the exact path's one allocation,
    // and outside the funnel's `catch_unwind` the native allocators run their
    // INFALLIBLE arm, which aborts the process on a full heap where the
    // native raises a catchable `OutOfMemoryError`. The unwind scope turns a
    // refusal into a decline instead: the allocators never collect, so the
    // argument words below are still the objects' addresses, and the funnel
    // then re-runs the native (nothing was written) on its own OOM ladder.
    let answer = crate::runtime::native_oom::catch_alloc_oom_on(&mut *thread, |thread| {
        let mut ctx = crate::vm::NativeContextImpl {
            shared: vm,
            thread,
        };
        cratonvm_native_builtins::lang_string::string_regex_thin(
            &mut ctx,
            &args[..words.len()],
            op,
        )
    })
    .ok()??;
    match answer {
        Value::Int(b) => Some(i64::from(b)),
        Value::Object(Some(o)) => Some(o.as_ptr() as i64),
        _ => None,
    }
}

/// Thin direct-call target for compiled `String.matches(String)` sites.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_string_matches_direct(
    vm_ptr: i64,
    receiver: i64,
    regex: i64,
) -> i64 {
    use cratonvm_native_builtins::lang_string::StringRegexThinOp;
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        if receiver == 0 {
            set_jit_pending_npe_action(
                crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
            );
            return i64::MIN;
        }
        if let Some(bits) =
            string_regex_direct_serve(vm_ptr, &[receiver, regex], StringRegexThinOp::Matches)
        {
            return bits;
        }
        let args = [receiver, regex];
        jit_invoke_dispatch(
            vm_ptr,
            &STRING_MATCHES_INFO as *const JitInvokeInfo as i64,
            args.as_ptr() as i64,
            2,
        )
    };
    contain("jit_string_matches_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Thin direct-call target for compiled `String.replaceAll` (`FIRST` false) /
/// `replaceFirst` (`FIRST` true) sites.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_string_replace_direct<const FIRST: bool>(
    vm_ptr: i64,
    receiver: i64,
    regex: i64,
    replacement: i64,
) -> i64 {
    use cratonvm_native_builtins::lang_string::StringRegexThinOp;
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        if receiver == 0 {
            set_jit_pending_npe_action(
                crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
            );
            return i64::MIN;
        }
        let (op, info) = if FIRST {
            (StringRegexThinOp::ReplaceFirst, &STRING_REPLACE_FIRST_INFO)
        } else {
            (StringRegexThinOp::ReplaceAll, &STRING_REPLACE_ALL_INFO)
        };
        if let Some(bits) =
            string_regex_direct_serve(vm_ptr, &[receiver, regex, replacement], op)
        {
            return bits;
        }
        let args = [receiver, regex, replacement];
        let bits = jit_invoke_dispatch(
            vm_ptr,
            info as *const JitInvokeInfo as i64,
            args.as_ptr() as i64,
            3,
        );
        // handoff-w36b: the native's returned String is in RAX now.
        native_return_handed_to_compiled_code();
        bits
    };
    contain("jit_string_replace_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// JIT r23 wave 5, lane L5: the one-validation arraycopy door answers (and
/// declines) exactly as the two self-validating attempts it replaces.
#[cfg(test)]
mod r23_w5_arraycopy_one_validation_tests {
    use super::*;

    fn vm() -> std::sync::Arc<SharedVm> {
        let config = crate::config::VmConfig {
            use_synthetic_jdk: true,
            ..Default::default()
        };
        let vm = std::sync::Arc::new(crate::vm::vm_init::SharedVm::new(config));
        *vm.self_arc.write() = Some(std::sync::Arc::downgrade(&vm));
        vm
    }

    fn word(obj: ObjectRef) -> i64 {
        obj.as_ptr() as i64
    }

    fn ints(vm: &SharedVm, n: usize, base: i32) -> ObjectRef {
        let arr = vm.mem.heap.alloc_array(ClassId::new(0), ArrayElementType::Int, n);
        for i in 0..n {
            let _ = vm.mem.heap.set_array_element(arr, i, Value::Int(base + i as i32));
        }
        arr
    }

    fn refs(vm: &SharedVm, elems: &[ObjectRef], n: usize) -> ObjectRef {
        let arr = vm.mem.heap.alloc_array(ClassId::new(7), ArrayElementType::Reference, n);
        for (i, e) in elems.iter().enumerate() {
            let _ = vm.mem.heap.set_array_element(arr, i, Value::Object(Some(*e)));
        }
        arr
    }

    fn dump(vm: &SharedVm, arr: ObjectRef) -> Vec<String> {
        (0..vm.mem.heap.array_length(arr))
            .map(|i| format!("{:?}", vm.mem.heap.get_array_element(arr, i)))
            .collect()
    }

    /// `(answered, dst after)` for one copy through the new door (`once`) or the
    /// old pair of attempts, on fresh arrays built by `make`.
    fn run(
        vm: &SharedVm,
        once: bool,
        make: &dyn Fn(&SharedVm) -> (ObjectRef, ObjectRef),
        (sp, dp, len): (i64, i64, i64),
    ) -> (bool, Vec<String>) {
        let (src, dst) = make(vm);
        // SAFETY: both are live arrays of this heap; nothing allocates below.
        let (s, d) = (word(src), word(dst));
        let answered = unsafe {
            if once {
                try_arraycopy_validated_once(vm, s, sp, d, dp, len, true).is_some()
            } else {
                try_primitive_arraycopy(vm, s, sp, d, dp, len).is_some()
                    || try_reference_arraycopy(vm, s, sp, d, dp, len).is_some()
            }
        };
        (answered, dump(vm, dst))
    }

    #[test]
    fn one_validation_answers_what_the_two_attempts_answer() {
        let vm = vm();
        let objs: Vec<ObjectRef> = (0..6)
            .map(|_| vm.mem.heap.alloc_object(ClassId::new(7), 0))
            .collect();
        let shapes: Vec<Box<dyn Fn(&SharedVm) -> (ObjectRef, ObjectRef)>> = vec![
            Box::new(|vm| (ints(vm, 10, 1), ints(vm, 10, 100))),
            Box::new(|vm| {
                let a = ints(vm, 10, 1);
                (a, a)
            }),
            Box::new(|vm| {
                let l = vm.mem.heap.alloc_array(ClassId::new(0), ArrayElementType::Long, 10);
                (ints(vm, 10, 1), l)
            }),
            Box::new({
                let objs = objs.clone();
                move |vm| (refs(vm, &objs, 6), refs(vm, &[], 6))
            }),
            Box::new({
                let objs = objs.clone();
                move |vm| {
                    let a = refs(vm, &objs, 6);
                    (a, a)
                }
            }),
            Box::new({
                let objs = objs.clone();
                move |vm| (refs(vm, &objs, 6), ints(vm, 6, 0))
            }),
        ];
        let ranges = [
            (0, 0, 3),
            (2, 0, 5),
            (0, 2, 4),
            (1, 0, 9),
            (0, 0, 11),
            (0, 0, 0),
            (-1, 0, 1),
        ];
        for (s, make) in shapes.iter().enumerate() {
            for r in ranges {
                assert_eq!(
                    run(&vm, true, make.as_ref(), r),
                    run(&vm, false, make.as_ref(), r),
                    "shape {s} range {r:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod r23_w4_reflect_new_array_direct_tests {
    use super::reflect_new_array_direct_admits_name as admits;

    /// Reference components only; the native keeps primitives, `void` and the
    /// 255-dimension `IllegalArgumentException`.
    #[test]
    fn admits_reference_components_only() {
        assert!(admits("java/lang/String"));
        assert!(admits("Foo"));
        assert!(admits("[Ljava/lang/String;"));
        assert!(admits("[I"));
        assert!(admits(&format!("{}I", "[".repeat(254))));
        assert!(!admits(&format!("{}I", "[".repeat(255))));
        for p in [
            "", "void", "V", "int", "long", "float", "double", "boolean", "byte", "char", "short",
        ] {
            assert!(!admits(p), "{p}");
        }
    }
}

const VH_BYTE_ARRAY_VIEW_DESCRIPTORS: [&str; 6] =
    ["([BI)S", "([BI)C", "([BI)I", "([BI)J", "([BI)F", "([BI)D"];
const fn vh_byte_array_view_read_info(slot: usize) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/invoke/VarHandle",
        method_name: "get",
        descriptor: VH_BYTE_ARRAY_VIEW_DESCRIPTORS[slot],
        num_jit_args: 3,
        return_type: cratonvm_jit::VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[slot],
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}
static VH_BYTE_ARRAY_VIEW_READ_INFOS: [JitInvokeInfo; 6] = [
    vh_byte_array_view_read_info(0),
    vh_byte_array_view_read_info(1),
    vh_byte_array_view_read_info(2),
    vh_byte_array_view_read_info(3),
    vh_byte_array_view_read_info(4),
    vh_byte_array_view_read_info(5),
];

/// Four-word C ABI: VM/judged site, handle, array, byte index. Exact handles
/// and all refusals use the real call-site descriptor through the native road.
pub unsafe extern "C" fn jit_varhandle_byte_array_view_read_direct<const SLOT: usize>(
    vm_ptr: i64,
    vh: i64,
    array: i64,
    index: i64,
) -> i64 {
    let (vm_ptr, site) = varhandle_thin_call_site(vm_ptr);
    let f = || {
        crate::jit::conservative_roots::note_jit_boundary();
        let vm = &*(vm_ptr as *const SharedVm);
        let info = if site.is_null() {
            &VH_BYTE_ARRAY_VIEW_READ_INFOS[SLOT]
        } else {
            &*site
        };
        let bits = if info.descriptor.starts_with("(Ljava/nio/ByteBuffer;I)") {
            // The same lifetime, exactness and mutable-carrier checks as the
            // admitted native-site arm, without the MIC/native-site funnel.
            if let Some((thread, _guard)) = jit_thread_mut() {
                try_varhandle_byte_buffer_view_access(vm, info, &[vh, array, index], thread)
            } else {
                None
            }
        } else if varhandle_thin_fast_arm_admitted(vm, vh, site) {
            varhandle_byte_array_view_read_bits(
                vm,
                vh as u64,
                array as u64,
                index,
                cratonvm_jit::VARHANDLE_BYTE_ARRAY_VIEW_RETURNS[SLOT],
            )
        } else {
            None
        };
        if let Some(bits) = bits {
            note_vh_direct(VH_DIRECT_READ, true);
            note_funnel_bypass_hit();
            return bits;
        }
        note_vh_direct(VH_DIRECT_READ, false);
        let args = [vh, array, index];
        jit_invoke_dispatch(
            vm_ptr,
            varhandle_thin_cold_arm_info(site, &VH_BYTE_ARRAY_VIEW_READ_INFOS[SLOT]),
            args.as_ptr() as i64,
            3,
        )
    };
    let bits = contain(
        "jit_varhandle_byte_array_view_read_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    native_return_handed_to_compiled_code();
    bits
}

const VH_BYTE_BUFFER_VIEW_WRITE_DESCRIPTORS: [&str; 6] = [
    "(Ljava/nio/ByteBuffer;IS)V",
    "(Ljava/nio/ByteBuffer;IC)V",
    "(Ljava/nio/ByteBuffer;II)V",
    "(Ljava/nio/ByteBuffer;IJ)V",
    "(Ljava/nio/ByteBuffer;IF)V",
    "(Ljava/nio/ByteBuffer;ID)V",
];
const fn vh_byte_buffer_view_write_info(slot: usize) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: "java/lang/invoke/VarHandle",
        method_name: "set",
        descriptor: VH_BYTE_BUFFER_VIEW_WRITE_DESCRIPTORS[slot],
        num_jit_args: 4,
        return_type: b'V',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }
}
static VH_BYTE_BUFFER_VIEW_WRITE_INFOS: [JitInvokeInfo; 6] = [
    vh_byte_buffer_view_write_info(0),
    vh_byte_buffer_view_write_info(1),
    vh_byte_buffer_view_write_info(2),
    vh_byte_buffer_view_write_info(3),
    vh_byte_buffer_view_write_info(4),
    vh_byte_buffer_view_write_info(5),
];
/// Five integer-word C ABI; floating values carry their exact raw bits.
/// Only judged sites are planned. Refusals retain the real descriptor and
/// normal dispatch, including exactness and exception priority.
pub unsafe extern "C" fn jit_varhandle_byte_buffer_view_write_direct<const SLOT: usize>(
    vm_ptr: i64,
    vh: i64,
    buffer: i64,
    index: i64,
    value: i64,
) -> i64 {
    let (vm_ptr, site) = varhandle_thin_call_site(vm_ptr);
    let bits = contain(
        "jit_varhandle_byte_buffer_view_write_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            let vm = &*(vm_ptr as *const SharedVm);
            let args = [vh, buffer, index, value];
            if !site.is_null() {
                if let Some((thread, _guard)) = jit_thread_mut() {
                    if let Some(bits) =
                        try_varhandle_byte_buffer_view_access(vm, &*site, &args, thread)
                    {
                        note_vh_direct(VH_DIRECT_WRITE, true);
                        note_funnel_bypass_hit();
                        if crate::runtime::env_cache::dbg_jitc() {
                            BYTE_BUFFER_VIEW_DIRECT_WRITE_REPORTED_VM.with(|reported| {
                            let (identity, seen) = reported.get();
                            let seen = if identity == vm.vm_identity { seen } else { 0 };
                            let bit = 1u8 << SLOT;
                            if seen & bit == 0 {
                                reported.set((vm.vm_identity, seen | bit));
                                eprintln!("[cratonvm-jitc] byte-buffer-view-write-direct served vm={} slot={SLOT}", vm.vm_identity);
                            }
                        });
                        }
                        return bits;
                    }
                }
            }
            note_vh_direct(VH_DIRECT_WRITE, false);
            jit_invoke_dispatch(
                vm_ptr,
                varhandle_thin_cold_arm_info(site, &VH_BYTE_BUFFER_VIEW_WRITE_INFOS[SLOT]),
                args.as_ptr() as i64,
                4,
            )
        },
    );
    native_return_handed_to_compiled_code();
    bits
}
thread_local! {
    static BYTE_BUFFER_VIEW_DIRECT_WRITE_REPORTED_VM: std::cell::Cell<(usize, u8)> =
        const { std::cell::Cell::new((0, 0)) };
}
fn varhandle_byte_buffer_view_write_direct_fns() -> [usize; 6] {
    [
        jit_varhandle_byte_buffer_view_write_direct::<0> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<1> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<2> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<3> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<4> as *const () as usize,
        jit_varhandle_byte_buffer_view_write_direct::<5> as *const () as usize,
    ]
}
pub fn varhandle_byte_buffer_view_write_direct_fn(slot: usize) -> usize {
    varhandle_byte_buffer_view_write_direct_fns()[slot]
}

fn varhandle_byte_array_view_read_direct_fns() -> [usize; 6] {
    [
        jit_varhandle_byte_array_view_read_direct::<0> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<1> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<2> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<3> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<4> as *const () as usize,
        jit_varhandle_byte_array_view_read_direct::<5> as *const () as usize,
    ]
}
pub fn varhandle_byte_array_view_read_direct_fn(slot: usize) -> usize {
    varhandle_byte_array_view_read_direct_fns()[slot]
}

/// `CRATONVM_JIT_VARHANDLE_ARRAY_ELEMENT_FAST=0` -- send every array-element
/// `VarHandle` access back through the generic native, as before this fast
/// path existed. Default ON. Same role as the switches beside it: an A/B on
/// ONE binary. gci i27/kd: read once per VM
/// (`SharedVm::jit_varhandle_array_element_fast`,
/// [`varhandle_array_element_fast_from_flags`]); it was a process `OnceLock`
/// (the per-VM statics ratchet).
fn varhandle_array_element_fast_enabled(vm: &SharedVm) -> bool {
    vm.jit_varhandle_array_element_fast
}

/// The value rule of `CRATONVM_JIT_VARHANDLE_ARRAY_ELEMENT_FAST`, read from
/// the flag snapshot (once per VM, at construction): the default-ON opt-out
/// rule of [`cratonvm_types::flags::runtime_flag_default_on`] -- unset ON,
/// `0` / `false` / `off` / `no` OFF.
///
/// JIT round 25 review: it was a hand-rolled `Ok("0")` match, under which
/// `=false` / `=off` -- the spellings every other kill switch accepts -- left
/// the fast path on, silently (the `runtime_flag_default_on` doc's "kill
/// switch that cannot be turned off" defect), and the row's reader was
/// invisible to `types/tests/flag_default_matches_reader.rs`.
pub(crate) fn varhandle_array_element_fast_from_flags() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_VARHANDLE_ARRAY_ELEMENT_FAST")
}

#[cfg(test)]
mod r25_varhandle_array_element_fast_switch_tests {
    use super::varhandle_array_element_fast_from_flags;

    const KEY: &str = "CRATONVM_JIT_VARHANDLE_ARRAY_ELEMENT_FAST";

    /// Unset is ON; every off word of the default-ON rule kills it (before,
    /// only `0` did); any other value stays ON.
    #[test]
    fn r25_every_off_word_kills_the_array_element_fast_path() {
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            assert!(varhandle_array_element_fast_from_flags(), "default ON");
        });
        for off in ["0", "false", "off", "no", "OFF"] {
            cratonvm_types::flags::with_thread_overrides(&[(KEY, Some(off))], || {
                assert!(!varhandle_array_element_fast_from_flags(), "{off} kills");
            });
        }
        for on in ["1", "true", "yes"] {
            cratonvm_types::flags::with_thread_overrides(&[(KEY, Some(on))], || {
                assert!(varhandle_array_element_fast_from_flags(), "{on} keeps it on");
            });
        }
    }
}

/// In-funnel array-element `VarHandle` accesses served as one hardware access,
/// reported by `CRATONVM_DBG=intrinsic-stats`. Zero beside a non-zero
/// site-cached count means every array handle site declined.
pub fn varhandle_array_element_served_count() -> u64 {
    jit_counter_total(|b| {
        b.vh_array_element_served
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

thread_local! {
    /// `(vm identity, VarHandleInts$Array class id, VarHandleLongs$Array class
    /// id)` as this thread last resolved them; `0` is "not resolved yet".
    static VH_ARRAY_CLASS_MEMO: Cell<(usize, u32, u32)> = const { Cell::new((0, 0, 0)) };
}

/// Is `class_id` the JDK's `VarHandleInts$Array` (`wide == false`) or
/// `VarHandleLongs$Array` (`wide == true`) -- the handles
/// `MethodHandles.arrayElementVarHandle(int[].class / long[].class)` builds,
/// which `AtomicIntegerArray` / `AtomicLongArray` keep in a `static final`.
///
/// Both are `final` classes, so the exact class id decides. The id is memoised
/// per thread after the first name read; a class that is not one of them is
/// answered by the name read every time, which only a site whose descriptor
/// already looks like an `int[]` / `long[]` element access reaches.
fn vh_array_handle_class_is(vm: &SharedVm, class_id: u32, wide: bool) -> bool {
    let (memo_vm, ints, longs) = VH_ARRAY_CLASS_MEMO.with(Cell::get);
    let known = if memo_vm == vm.vm_identity {
        if wide {
            longs
        } else {
            ints
        }
    } else {
        0
    };
    if known != 0 {
        return known == class_id;
    }
    let want = if wide {
        "java/lang/invoke/VarHandleLongs$Array"
    } else {
        "java/lang/invoke/VarHandleInts$Array"
    };
    // `try_read`: this runs in a dispatch that may sit inside a class
    // initialiser while the thread still owns the class-manager write lock
    // (`dispatch_virtual`'s `receiver_is_java_util` documents the same
    // hazard); a busy table declines, and the generic native answers.
    let Some(matches) = vm.classes.class_manager.try_read().map(|cm| {
        cm.get_class(ClassId::new(class_id))
            .is_some_and(|class| &*class.name == want)
    }) else {
        return false;
    };
    if matches {
        VH_ARRAY_CLASS_MEMO.with(|memo| {
            let (id, mut ints, mut longs) = memo.get();
            if id != vm.vm_identity {
                ints = 0;
                longs = 0;
            }
            if wide {
                longs = class_id;
            } else {
                ints = class_id;
            }
            memo.set((vm.vm_identity, ints, longs));
        });
    }
    matches
}

/// What an array-element access does to the element, and with which ordering.
#[derive(Clone, Copy)]
enum VhArrayOp {
    Load(std::sync::atomic::Ordering),
    Store(std::sync::atomic::Ordering),
    CompareAndSet,
    GetAndAdd,
    GetAndSet,
}

/// An access to one element of a real `int[]` / `long[]` array `VarHandle`
/// (`MethodHandles.arrayElementVarHandle`), served as the one instruction it
/// is, in the dispatch funnel and before the generic `VarHandle` native.
///
/// `AtomicIntegerArray`, `AtomicLongArray` and netty's `MpscIntQueue` (the
/// free list of every `AdaptivePoolingAllocator` size class) reach the
/// handle's access modes. The generic native checks the handle's kind, arity,
/// coordinates, call-site return and boxing on every call; measured on a
/// single thread that is 1.2-1.6 us per `AtomicIntegerArray.get` against
/// 0.7 ns on HotSpot, and an alloc / grow / release cycle of the adaptive
/// allocator costs 17 us against 0.1 us.
///
/// Served only when every question the generic native would ask has a fixed
/// answer: the call site is exactly `([II)I`-shaped for the mode (a type the
/// handle does not take is `WrongMethodTypeException`'s, so it declines), the
/// receiver's class is exactly the JDK's final `VarHandleInts$Array` /
/// `VarHandleLongs$Array`, the array is a live, non-null, flat array of that
/// element type, and the index is in range. Anything else -- a null array, an
/// index out of bounds, a combinator handle, a `float[]` -- declines to the
/// generic native, which raises the exception HotSpot raises.
///
/// Every element of a primitive array is naturally aligned and flat on every
/// collector (`hw_cas_primitive_array_element`'s argument), so each mode is
/// one atomic access of the element's width. `get` / `getOpaque` / `set` /
/// `setOpaque` are relaxed; `getVolatile` / `getAcquire` acquire (a plain load
/// on x86); `setRelease` release; `setVolatile` sequentially consistent;
/// `compareAndSet` and its weak forms one strong `compare_exchange` (a weak
/// form may fail spuriously, never must); `getAndAdd` / `getAndSet` and their
/// acquire / release forms the sequentially consistent `fetch_add` / `swap`,
/// the strongest ordering, which is a legal implementation of each weaker one.
///
/// No safepoint, allocation or Java call lies between locating the element and
/// touching it, so a moving collector cannot invalidate the pointer. An `int` /
/// `long` array holds no references, so no barrier applies.
///
/// # Safety
/// `args` are the call's raw JIT argument words; `vm` is live.
unsafe fn try_varhandle_array_element(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Option<i64> {
    use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
    let d = info.descriptor.as_bytes();
    // `([I` or `([J`, then the index `I`: every served shape.
    if d.len() < 5 || d[0] != b'(' || d[1] != b'[' || d[3] != b'I' {
        return None;
    }
    let wide = match d[2] {
        b'I' => false,
        b'J' => true,
        _ => return None,
    };
    let (op, tail, argc) = match info.method_name {
        "get" | "getOpaque" => (
            VhArrayOp::Load(Ordering::Relaxed),
            if wide { ")J" } else { ")I" },
            3,
        ),
        "getVolatile" | "getAcquire" => (
            VhArrayOp::Load(Ordering::SeqCst),
            if wide { ")J" } else { ")I" },
            3,
        ),
        "set" | "setOpaque" => (
            VhArrayOp::Store(Ordering::Relaxed),
            if wide { "J)V" } else { "I)V" },
            4,
        ),
        "setRelease" => (
            VhArrayOp::Store(Ordering::Release),
            if wide { "J)V" } else { "I)V" },
            4,
        ),
        "setVolatile" => (
            VhArrayOp::Store(Ordering::SeqCst),
            if wide { "J)V" } else { "I)V" },
            4,
        ),
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => (
            VhArrayOp::CompareAndSet,
            if wide { "JJ)Z" } else { "II)Z" },
            5,
        ),
        "getAndAdd" | "getAndAddAcquire" | "getAndAddRelease" => {
            (VhArrayOp::GetAndAdd, if wide { "J)J" } else { "I)I" }, 4)
        }
        "getAndSet" | "getAndSetAcquire" | "getAndSetRelease" => {
            (VhArrayOp::GetAndSet, if wide { "J)J" } else { "I)I" }, 4)
        }
        _ => return None,
    };
    if &info.descriptor[4..] != tail || args.len() != argc {
        return None;
    }
    if !varhandle_array_element_fast_enabled(vm) {
        return None;
    }
    let vh_raw = args[0] as u64 as usize;
    let array_raw = args[1] as u64 as usize;
    // A word the compiled caller holds as an `int`: only its low half is the
    // index.
    let index = args[2] as i32;
    if vh_raw == 0 || array_raw == 0 || index < 0 {
        return None;
    }
    let (vh_class, _) = direct_receiver_facts(vm, vh_raw)?;
    if vh_class == 0 || !vh_array_handle_class_is(vm, vh_class, wide) {
        return None;
    }
    direct_receiver_facts(vm, array_raw)?;
    let heap = &vm.mem.heap;
    let array = heap.load_and_forward(ObjectRef::from_raw(array_raw as *mut u8));
    let element = if wide {
        ArrayElementType::Long
    } else {
        ArrayElementType::Int
    };
    if heap.kind_of(array) != cratonvm_types::ObjectKind::Array
        || heap.element_type_of_validated(array) != element
    {
        return None;
    }
    let index = index as usize;
    if index >= heap.array_length(array) {
        return None;
    }
    let base = heap.array_data_ptr(array)?;
    if wide {
        let pointer = base.add(index * 8);
        if (pointer as usize) & 7 != 0 {
            return None;
        }
        let cell = AtomicI64::from_ptr(pointer.cast::<i64>());
        Some(match op {
            VhArrayOp::Load(order) => cell.load(order),
            VhArrayOp::Store(order) => {
                cell.store(args[3], order);
                0
            }
            VhArrayOp::CompareAndSet => i64::from(
                cell.compare_exchange(args[3], args[4], Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok(),
            ),
            VhArrayOp::GetAndAdd => cell.fetch_add(args[3], Ordering::SeqCst),
            VhArrayOp::GetAndSet => cell.swap(args[3], Ordering::SeqCst),
        })
    } else {
        let pointer = base.add(index * 4);
        if (pointer as usize) & 3 != 0 {
            return None;
        }
        let cell = AtomicI32::from_ptr(pointer.cast::<i32>());
        Some(match op {
            VhArrayOp::Load(order) => i64::from(cell.load(order)),
            VhArrayOp::Store(order) => {
                cell.store(args[3] as i32, order);
                0
            }
            VhArrayOp::CompareAndSet => i64::from(
                cell.compare_exchange(
                    args[3] as i32,
                    args[4] as i32,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok(),
            ),
            VhArrayOp::GetAndAdd => i64::from(cell.fetch_add(args[3] as i32, Ordering::SeqCst)),
            VhArrayOp::GetAndSet => i64::from(cell.swap(args[3] as i32, Ordering::SeqCst)),
        })
    }
}

unsafe fn try_varhandle_instance_field_read(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    thread: &mut JvmThread,
) -> Option<i64> {
    if !cratonvm_jit::VARHANDLE_READ_MODES.contains(&info.method_name) {
        return None;
    }
    // [VarHandle, receiver] and nothing else: a coordinate-carrying access
    // (array index, byte offset) is not an instance-field read.
    if args_slice.len() != 2 {
        return None;
    }
    let vh_raw = args_slice[0] as u64;
    let recv_raw = args_slice[1] as u64;
    if vh_raw == 0 || recv_raw == 0 {
        return None;
    }
    let bits = varhandle_instance_field_read_bits(
        vm,
        vh_raw,
        recv_raw,
        info.return_type,
        Some(&mut *thread),
    )?;
    // Round 15 wave 2 hand-back (lane vh2; VH2-3): a site declaring a narrower
    // reference than `Object` casts what it reads. A value the cast refuses
    // declines to the native road, whose `unbox_poly_return_checked_vh` raises
    // the `ClassCastException`; the field is a reference variable here (the
    // read agreed with a reference site), which is what makes a box a CCE.
    if bits != 0
        && crate::vm::vm_exec::vh_site_returns_narrow_reference(info.method_name, info.descriptor)
    {
        if let Some(site_ret) = info.descriptor.rsplit(')').next() {
            let value = Value::Object(Some(ObjectRef::from_raw(bits as usize as *mut u8)));
            let mut ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            if cratonvm_native_builtins::lang_invoke::varhandle_reference_return_refusal(
                &mut ctx,
                value,
                site_ret,
                Some(true),
            )
            .is_some()
            {
                // The handoff root the read published is not handed off.
                thread.native_pending_return = None;
                return None;
            }
        }
    }
    Some(bits)
}

/// SBR3 lane ffmperf: `VarHandle.get(seg, long)` / `set(seg, long, v)` on a real
/// `SegmentVarHandle` over a real native segment, served from inside the funnel
/// as raw words -- no argument decode, no boxed `Object` return to unbox again,
/// no generic access-mode prologue. This is what
/// `MemorySegment.get(JAVA_INT, off)` compiles to once the JDK's own
/// `AbstractMemorySegmentImpl.get` runs.
///
/// Declines (to the funnel's native, which raises whatever it always raised) for
/// anything but `(segment, long)` / `(segment, long, carrier)` with the carrier
/// the handle's own, and for every access the lean path cannot prove
/// (`cratonvm_native_builtins::lang_invoke::segment_vh_lean_get_bits`).
///
/// SAFETY: the raw words come from a live compiled frame; both references are
/// heap-validated before any dereference, and nothing here allocates or reaches
/// a safepoint, so the forwarded references stay current.
unsafe fn try_varhandle_segment_access(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    thread: &mut JvmThread,
) -> Option<i64> {
    let write = match info.method_name {
        "get" => false,
        "set" => true,
        _ => return None,
    };
    if args_slice.len() != 3 + usize::from(write) {
        return None;
    }
    let vh_raw = args_slice[0] as u64;
    let seg_raw = args_slice[1] as u64;
    if vh_raw == 0 || seg_raw == 0 {
        return None;
    }
    // `(Lsegment;J)K` / `(Lsegment;JK)V`: the segment first, a `long` coordinate,
    // and for a store the carrier last.
    // Read from the tail, in constant time: this runs on every access.
    let d = info.descriptor.as_bytes();
    if d.len() < 7 || d[0] != b'(' || d[1] != b'L' {
        return None;
    }
    let n = d.len();
    let site_kind = if write {
        // `...;J<k>)V`
        match (d[n - 5], d[n - 4], d[n - 3], d[n - 2], d[n - 1]) {
            (
                b';',
                b'J',
                kind @ (b'I' | b'J' | b'F' | b'D' | b'Z' | b'B' | b'S' | b'C'),
                b')',
                b'V',
            ) => kind,
            _ => return None,
        }
    } else {
        // `...;J)<k>`
        if (d[n - 4], d[n - 3], d[n - 2]) != (b';', b'J', b')') {
            return None;
        }
        info.return_type
    };
    let (vh_class_id, vh_hash) = direct_receiver_facts(vm, vh_raw as usize)?;
    let (seg_class_id, _) = direct_receiver_facts(vm, seg_raw as usize)?;
    if vh_class_id == 0 || seg_class_id == 0 {
        return None;
    }
    let heap = &vm.mem.heap;
    let vh = heap.load_and_forward(ObjectRef::from_raw(vh_raw as usize as *mut u8));
    let seg = heap.load_and_forward(ObjectRef::from_raw(seg_raw as usize as *mut u8));
    let base = args_slice[2];
    let mut ctx = crate::vm::NativeContextImpl {
        shared: vm,
        thread: &mut *thread,
    };
    if write {
        cratonvm_native_builtins::lang_invoke::segment_vh_lean_set_bits(
            &mut ctx,
            vh,
            seg,
            base,
            site_kind,
            args_slice[3],
            vh_hash,
            seg_class_id,
        )
        .then_some(0)
    } else {
        cratonvm_native_builtins::lang_invoke::segment_vh_lean_get_bits(
            &mut ctx,
            vh,
            seg,
            base,
            site_kind,
            vh_hash,
            seg_class_id,
        )
    }
}

/// The read itself, shared by the funnel route above and the thin direct-call
/// helpers below so the two cannot drift apart in what they consider a
/// servable handle.
///
/// `site_ret` is the CALL SITE's declared return type — `info.return_type` on
/// the funnel route, the slot's baked return kind on the direct route. It is
/// what the funnel would have unboxed the erased `Object` result against, so
/// disagreeing with the variable's own kind is a refusal rather than a
/// conversion.
///
/// `thread` is required only to publish an OBJECT result as a handoff root;
/// a primitive-returning caller may pass `None`, and then a reference-valued
/// variable is declined instead of being returned unrooted.
///
/// SAFETY: `vh_raw`/`recv_raw` are raw references from a live compiled frame;
/// both are heap-validated here before any dereference.
unsafe fn varhandle_instance_field_read_bits(
    vm: &SharedVm,
    vh_raw: u64,
    recv_raw: u64,
    site_ret: u8,
    thread: Option<&mut JvmThread>,
) -> Option<i64> {
    // Both probes and the identity hash go through the per-epoch receiver memo.
    // This helper serves 269 768 215 calls on
    // `JdkZlibIntegrationTest#testHugeDecompress` — one per `ByteBuf.writeByte`,
    // from the `RefCnt` read inside `ensureAccessible` — and it asked
    // `is_object_address` TWICE and re-derived the handle's identity hash on
    // every one of them. `ZObjectStarts::contains` + `is_object_address` were
    // 10.3 % of that profile with every funnel already gone.
    //
    // The memo answers exactly what the two probes answered — "this address is
    // a live object base in this GC epoch" — and its `identity` is computed by
    // the same `java_identity_hash` arbitration this code used, so a thin-locked
    // or inflated header still yields the displaced value `vh_meta_get` filed
    // the handle under.
    let (vh_class_id, key) = direct_receiver_facts(vm, vh_raw as usize)?;
    if vh_class_id == 0 {
        return None;
    }
    let (recv_class_id, _) = direct_receiver_facts(vm, recv_raw as usize)?;
    if recv_class_id == 0 {
        return None;
    }
    let receiver = ObjectRef::from_raw(recv_raw as usize as *mut u8);
    let heap = &vm.mem.heap;
    // The plan of THIS handle of THIS VM (gc-common w36-a): its registry slot
    // at its current address, not the lone row filed under its hash.
    let plan = cratonvm_native_builtins::lang_invoke::varhandle_instance_field_plan_for_handle(
        vm.vm_identity,
        key,
        vh_raw as usize,
    )?;
    // A receiver of an unrelated class would read slot `field_index` of the
    // wrong layout; the native answers it with a ClassCastException.
    if !varhandle_plan_admits_receiver(vm, &plan, recv_class_id) {
        return None;
    }
    // Reference/primitive agreement between the variable and the call site.
    let site_is_ref = matches!(site_ret, b'L' | b'[');
    let plan_is_ref = plan.value_desc == b'L';
    if site_is_ref != plan_is_ref || (!site_is_ref && site_ret != plan.value_desc) {
        return None;
    }
    // Same read `NativeContextImpl::get_field` performs, minus the class-id
    // and descriptor-cache round trip: the declared descriptor is already on
    // the plan, straight from the handle's own metadata.
    let receiver = heap.load_and_forward(receiver);
    let value = if !plan_is_ref {
        jit_compact_field_slot(receiver.as_ptr() as i64, plan.field_index as i64)
            .filter(|(_, storage)| {
                Some(*storage)
                    == cratonvm_types::FieldStorageKind::from_descriptor_byte(plan.value_desc)
            })
            .map(|(offset, storage)| {
                // The validated receiver was forwarded above, the layout
                // bounds this exact slot, and its primitive storage agrees
                // with the handle. References and legacy cells retain the
                // heap's coercion, unboxing and read barriers below.
                cratonvm_types::read_compact_field(
                    receiver.as_ptr().add(offset),
                    storage,
                    std::sync::atomic::Ordering::Acquire,
                )
            })
            .unwrap_or_else(|| {
                heap.get_field_as(receiver, plan.field_index as usize, plan.value_desc)
            })
    } else {
        heap.get_field_as(receiver, plan.field_index as usize, plan.value_desc)
    };
    // Round 15 wave 2 hand-back (lane vh2;
    // `r15w2-vh2-varhandle-acquire-reads-served-by-plain-loads-FIXED-20260930.md`):
    // `getAcquire`/`getVolatile` order the reads after them. A plain load
    // already does on x86-64 (TSO), which is left byte-for-byte as it was;
    // elsewhere every mode pays the acquire barrier (stronger than `get`/
    // `getOpaque` need, never weaker).
    #[cfg(not(target_arch = "x86_64"))]
    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
    Some(match value {
        Value::Int(v) => v as i64,
        Value::Long(v) => v,
        Value::Float(f) => f.to_bits() as i64,
        Value::Double(d) => d.to_bits() as i64,
        Value::Object(Some(obj)) => {
            // Object-return handoff root, same contract as every other JIT
            // native fast path (see `jit_integer_value_of_direct`): the
            // reference is live only in a register until the caller stores
            // it, so it has to be reachable across that window. With no
            // thread borrow there is nowhere to publish it, so the read is
            // declined rather than handed back unrooted.
            thread?.native_pending_return = Some(obj);
            obj.as_ptr() as i64
        }
        Value::Object(None) => 0,
        // A slot whose storage tag disagrees with the declared descriptor is
        // exactly what `get_field_as` exists to normalise; anything still
        // unexpected here goes back to the funnel rather than being guessed at.
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// `VarHandle` read-mode thin direct-call helpers.
//
// The JIT half is `cratonvm_jit::VARHANDLE_READ_DIRECT_FNS` and the two
// recognitions that read it (`jit::try_compile`'s single-pass ladder and
// `jit_bridge::compile_osr_artifact`). This half is the callee: a

#[cfg(test)]
mod jr21_reflect2_lean_link_tests {
    use super::*;

    const DESCS: &[&str] = &[
        "()V",
        "()I",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        "(Ljava/lang/Object;II)I",
        "(ZBSCIJFD[ILjava/lang/String;[[Ljava/lang/Object;)Z",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        "(V)V",
        "(Ljava/lang/Object)V",
        "(I)Vx",
        "I)V",
        "(I",
        "",
        "(Q)V",
        "(J)D",
    ];

    #[test]
    fn the_stack_parse_answers_as_the_vec_parse() {
        for desc in DESCS {
            let mut buf = [0u8; JR21_REFLECT2_SIG_CAP];
            let lean = jr21_reflect2_basic_sig(desc, &mut buf)
                .expect("no test descriptor is wider than the buffer")
                .map(|(n, r)| (buf[..n].to_vec(), r));
            assert_eq!(lean, mh_link_basic_sig(desc), "{desc}");
        }
    }

    #[test]
    fn a_descriptor_wider_than_the_buffer_takes_the_vec_parse() {
        let wide = format!("({})V", "I".repeat(JR21_REFLECT2_SIG_CAP + 1));
        let mut buf = [0u8; JR21_REFLECT2_SIG_CAP];
        assert!(jr21_reflect2_basic_sig(&wide, &mut buf).is_err());
        // The agreement still answers, through the allocating parse.
        assert!(jr21_reflect2_basic_agree(&wide, &wide, 0, 0, 0, 0));
    }

    #[test]
    fn the_lean_agreement_answers_as_the_allocating_one() {
        let shapes: &[(usize, usize, usize, usize)] = &[
            (0, 0, 0, 0),
            (1, 0, 0, 1),
            (1, 0, 0, 0),
            (0, 1, 1, 0),
            (0, 0, 1, 0),
        ];
        for site in DESCS {
            for member in DESCS {
                for &(lead, front, back, trail) in shapes {
                    let reference = match (mh_link_basic_sig(site), mh_link_basic_sig(member)) {
                        (Some((sp, sr)), Some((mp, mr))) => jr21_reflect2_basic_agree_slices(
                            &sp, sr, &mp, mr, lead, front, back, trail,
                        ),
                        _ => false,
                    };
                    assert_eq!(
                        jr21_reflect2_basic_agree(site, member, lead, front, back, trail),
                        reference,
                        "{site} vs {member} ({lead},{front},{back},{trail})"
                    );
                }
            }
        }
        // The `Method.invoke` linker shape: a compiled
        // `invokeExact(Object,Object,Object)Object` site against the linker
        // `invokeExact_MT(handle, Object, Object, Object, appendix)Object`.
        assert!(jr21_reflect2_basic_agree(
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            1,
            0,
            0,
            1,
        ));
        // An `int` the member takes where the site passes a reference is the door's.
        assert!(!jr21_reflect2_basic_agree(
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            "(Ljava/lang/Object;IILjava/lang/Object;)Ljava/lang/Object;",
            1,
            0,
            0,
            1,
        ));
    }
}

/// JIT round 21 lane reflect4 (REFLECT3-21-2, `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`):
/// the hidden-owner static leaf of a compiled `invokeBasic` / `linkToStatic`
/// link is resolved by its class id, and no other site is.
#[cfg(test)]
mod jr21_reflect4_hidden_leaf_tests {
    use super::{
        jit_hidden_static_owner, jit_substituted_owner, mh_bind_hidden_leaf_info,
        mh_bind_leaf_info, JitInvokeInfo,
    };
    use cratonvm_native_builtins::lang_invoke::MhCompiledBind;

    const SPUN: &str = "cratonvm/test/jr21r4/LambdaForm$MH";
    const OTHER: &str = "cratonvm/test/jr21r4/Other";
    const DESC: &str = "(Ljava/lang/Object;I)I";

    fn site(invoke_kind: u8, owner: u32) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: SPUN,
            method_name: "invoke",
            descriptor: DESC,
            num_jit_args: 2,
            return_type: b'I',
            invoke_kind,
            declaring_class_id: owner,
            owner_class_id: owner,
        }
    }

    #[test]
    fn a_static_site_with_an_owner_id_is_resolved_by_that_id_only() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let (spun, other) = {
            let mut cm = shared.classes.class_manager.write();
            (
                cm.try_ensure_synthetic_class(SPUN, 0)
                    .expect("Compatible mode fabricates; this fixture never runs under --jdk-only"),
                cm.try_ensure_synthetic_class(OTHER, 0)
                    .expect("Compatible mode fabricates; this fixture never runs under --jdk-only"),
            )
        };
        assert_eq!(
            jit_hidden_static_owner(&shared, &site(3, spun.as_u32())),
            Some(spun)
        );
        assert_eq!(
            jit_substituted_owner(&shared, &site(3, spun.as_u32())),
            Some(spun),
            "every service that asks the substituted owner takes the hidden id"
        );
        // A compiled static site carries no owner id: by name, as before.
        assert_eq!(jit_hidden_static_owner(&shared, &site(3, 0)), None);
        assert_eq!(jit_substituted_owner(&shared, &site(3, 0)), None);
        // A kind-1 substitution is not the hidden leaf, and keeps its answer.
        assert_eq!(
            jit_hidden_static_owner(&shared, &site(1, spun.as_u32())),
            None
        );
        assert_eq!(
            jit_substituted_owner(&shared, &site(1, spun.as_u32())),
            Some(spun)
        );
        // Virtual and interface sites never carry one.
        assert_eq!(
            jit_substituted_owner(&shared, &site(0, spun.as_u32())),
            None
        );
        assert_eq!(
            jit_substituted_owner(&shared, &site(2, spun.as_u32())),
            None
        );
        // An id that names another class (a reused id) is ignored.
        assert_eq!(
            jit_hidden_static_owner(&shared, &site(3, other.as_u32())),
            None
        );
        assert_eq!(jit_hidden_static_owner(&shared, &site(3, u32::MAX)), None);
    }

    #[test]
    fn the_hidden_leaf_is_a_static_call_by_id_interned_apart_from_an_ordinary_leaf() {
        let bind = MhCompiledBind {
            invoke_kind: 3,
            class_id: 4242,
            class: SPUN.to_string(),
            name: "invoke".to_string(),
            desc: DESC.to_string(),
            bound: false,
        };
        let leaf = mh_bind_hidden_leaf_info(0x5eed, &bind, SPUN).expect("interned");
        assert_eq!(leaf.invoke_kind, 3);
        assert_eq!(leaf.owner_class_id, 4242);
        assert_eq!(leaf.declaring_class_id, 4242);
        assert_eq!(
            leaf.num_jit_args, 2,
            "static: one slot per value, no receiver"
        );
        assert_eq!(leaf.return_type, b'I');
        assert_eq!(leaf.class_name, SPUN);
        assert_eq!(leaf.method_name, "invoke");
        assert_eq!(leaf.descriptor, DESC);
        let again = mh_bind_hidden_leaf_info(0x5eed, &bind, SPUN).expect("interned");
        assert!(std::ptr::eq(leaf, again), "one leaf per member");
        // An ordinary leaf for the same bind is another row, without the id.
        let plain = mh_bind_leaf_info(0x5eed, &bind).expect("interned");
        assert!(!std::ptr::eq(leaf, plain));
        assert_eq!(plain.owner_class_id, 0);
    }
}

/// JIT round 21 lane reflect4 (REFLECT4-21-1 / REFLECT4-21-3): every census
/// row a site can count names a reason, and the served rows are followed by
/// their ladder twins.
#[cfg(test)]
mod jr21_reflect4_census_tests {
    use super::*;

    #[test]
    fn every_mh_link_census_row_names_its_reason() {
        let rows = [
            MHL_DISABLED,
            MHL_NO_THREAD,
            MHL_LINKER_WIDE,
            MHL_LINKER_HANDLE,
            MHL_LINKER_PAIR,
            MHL_LINKER_NULL_FIRST,
            MHL_LINKER_DECODE,
            MHL_LINKER_SHAPE,
            MHL_LINKER_LEAF,
            MHL_BASIC_WIDE,
            MHL_BASIC_HANDLE,
            MHL_BASIC_VMENTRY,
            MHL_BASIC_DECODE,
            MHL_BASIC_SHAPE,
            MHL_BASIC_LEAF_HIDDEN,
            MHL_BASIC_LEAF,
            MHL_LINKTO_WIDE,
            MHL_LINKTO_MEMBER_WORD,
            MHL_LINKTO_DECODE,
            MHL_LINKTO_KIND,
            MHL_LINKTO_SHAPE,
            MHL_LINKTO_LEAF,
        ];
        for row in rows {
            assert!(MH_LINK_CENSUS_REASONS[row].contains("(door"));
        }
        for served in [MHL_LINKER_SERVED, MHL_BASIC_SERVED, MHL_LINKTO_SERVED] {
            assert!(MH_LINK_CENSUS_REASONS[served].contains("served, the leaf's compiled entry"));
            assert!(MH_LINK_CENSUS_REASONS[served + 1].contains("dispatch ladder"));
        }
        assert!(MH_LINK_CENSUS_REASONS[MHL_BASIC_LEAF_HIDDEN].contains("hidden owner"));
        assert_eq!(MHL_LINKTO_SERVED + 2, MH_LINK_CENSUS_REASONS.len());
    }

    #[test]
    fn every_bytecode_only_census_row_names_its_reason() {
        let rows = [
            BCO_ENTERED_RETURNED,
            BCO_ENTERED_THREW,
            BCO_ENTERED_DECLINED,
            BCO_BODY_NOT_ENTERABLE,
            BCO_SWITCH_OFF,
            BCO_NO_JIT,
            BCO_NOT_INTRINSIC,
            BCO_SHAPE_REFUSED,
            BCO_COUNTING,
            BCO_COMPILE_PUBLISHED,
            BCO_COMPILE_REFUSED,
            BCO_ATTEMPTS_SPENT,
        ];
        assert_eq!(rows.len(), BYTECODE_ONLY_CENSUS_REASONS.len());
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(*row, i);
            assert!(!BYTECODE_ONLY_CENSUS_REASONS[*row].is_empty());
        }
    }
}

#[cfg(test)]
mod jr21_reflect3_agree_layout_tests {
    //! JIT round 21 lane reflect3 (`CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO`):
    //! the memo's layout key tells the three shapes' calls apart.
    use super::*;

    #[test]
    fn each_shape_layout_has_its_own_key() {
        let linker = jr21_reflect3_agree_layout(1, 0, 0, 1);
        let basic = jr21_reflect3_agree_layout(1, 0, 0, 0);
        let link_to_static = jr21_reflect3_agree_layout(0, 0, 1, 0);
        let link_to_virtual = jr21_reflect3_agree_layout(0, 1, 1, 0);
        let keys = [linker, basic, link_to_static, link_to_virtual];
        assert!(keys.iter().all(Option::is_some));
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // A count wider than two bits is not memoised.
        assert_eq!(jr21_reflect3_agree_layout(4, 0, 0, 0), None);
        assert_eq!(jr21_reflect3_agree_layout(3, 3, 3, 3), Some(0xFF));
    }
}

/// `CRATONVM_JIT_DISPATCH_COUNTER_VIA_MANAGER` (j27 wave 2, lane NATMISC;
/// **default ON**, `=0` is the kill switch; page
/// `j27-static-dispatch-counter-compiles-on-its-own-clock-FIXED-20261029.md`).
///
/// The static dispatch tail's `DISPATCH_COUNTER` counts per thread and per
/// CALL SITE, and at its threshold asked the callee door for a C2 body: by
/// default that door defers a C2 task to the worker
/// (`CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER`), otherwise it compiles on
/// the mutator; either way at this per-site clock, outside the tiered
/// manager's policy and the profile store. HotSpot keeps
/// one invocation counter per method and asks its compilation policy. Under
/// `CRATONVM_BG_COMPILE` (default on) the tail's own doors already count the
/// method and offer it to the manager (the template door through
/// `bytecode_callee_compiled_or_nominate`, the by-name tail through
/// `execute`'s background count), and the `jit_cache` probe above binds
/// `DISPATCH_CACHE` once the manager publishes a body. So the inline compile
/// is kept only for `CRATONVM_BG_COMPILE=0` (every nomination site's inline
/// mode) or with this switch killed. Read once per site per thread, at the
/// counter's crossing.
fn static_dispatch_counter_compiles_inline() -> bool {
    dispatch_counter_compile_is_inline(
        crate::runtime::env_cache::bg_compile(),
        cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_DISPATCH_COUNTER_VIA_MANAGER",
        ),
    )
}

/// The decision of [`static_dispatch_counter_compiles_inline`]: inline only
/// when compiles are not backgrounded, or when routing through the manager
/// is switched off.
fn dispatch_counter_compile_is_inline(bg_compile: bool, via_manager: bool) -> bool {
    !(bg_compile && via_manager)
}

/// `CRATONVM_JIT_VIRTUAL_DISPATCH_COUNTER_PROBE_ONLY` (j27 wave 3, lane
/// DISPATCH2; **default ON**, `=0` is the kill switch; page
/// `j27-static-dispatch-counter-compiles-on-its-own-clock-FIXED-20261029.md`): the
/// virtual twin of [`static_dispatch_counter_compiles_inline`].
///
/// The blind virtual arm's `VIRTUAL_DISPATCH_COUNTER` crossing (its threshold,
/// the doubling re-probes, the megamorphic table hint, a stale body dropped)
/// asks the receiver-keyed callee door. That ask cannot simply be skipped, as
/// the static one is: its probe is the only place the arm binds a published
/// body into `VIRTUAL_DISPATCH_CACHE`. So under the background compiler the
/// door is asked in probe-only mode ([`CalleeDoorMiss::ProbeOnly`]: every
/// filter on a published body, `None` where it would defer a C2 task at this
/// per-site, per-thread clock or compile), and the method's compile is left
/// to the tiered manager, which the call's tail counts and nominates. With
/// `CRATONVM_BG_COMPILE=0` or `=0` here the door defers or compiles as before.
/// Read at the crossing only.
fn virtual_dispatch_counter_door_miss() -> crate::runtime::interpreter::jit_bridge::CalleeDoorMiss {
    virtual_dispatch_counter_door_miss_for(
        crate::runtime::env_cache::bg_compile(),
        cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_VIRTUAL_DISPATCH_COUNTER_PROBE_ONLY",
        ),
    )
}

/// The decision of [`virtual_dispatch_counter_door_miss`], on the static
/// twin's rule ([`dispatch_counter_compile_is_inline`]).
fn virtual_dispatch_counter_door_miss_for(
    bg_compile: bool,
    probe_only: bool,
) -> crate::runtime::interpreter::jit_bridge::CalleeDoorMiss {
    use crate::runtime::interpreter::jit_bridge::CalleeDoorMiss;
    if dispatch_counter_compile_is_inline(bg_compile, probe_only) {
        CalleeDoorMiss::MayDefer
    } else {
        CalleeDoorMiss::ProbeOnly
    }
}

#[cfg(test)]
mod j27_virtual_dispatch_counter_probe_only_tests {
    use super::virtual_dispatch_counter_door_miss_for;
    use crate::runtime::interpreter::jit_bridge::CalleeDoorMiss;

    #[test]
    fn j27_virtual_dispatch_counter_only_probes_under_bg() {
        assert_eq!(
            virtual_dispatch_counter_door_miss_for(true, true),
            CalleeDoorMiss::ProbeOnly,
            "default: the manager compiles"
        );
        assert_eq!(
            virtual_dispatch_counter_door_miss_for(true, false),
            CalleeDoorMiss::MayDefer,
            "=0 kill: the door defers or compiles as before"
        );
        assert_eq!(
            virtual_dispatch_counter_door_miss_for(false, true),
            CalleeDoorMiss::MayDefer,
            "BG_COMPILE=0: the mutator is the compiler"
        );
    }

    #[test]
    fn j27_virtual_dispatch_counter_probe_only_switch_reads_default_on() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_VIRTUAL_DISPATCH_COUNTER_PROBE_ONLY", v)],
                || {
                    cratonvm_types::flags::runtime_flag_default_on(
                        "CRATONVM_JIT_VIRTUAL_DISPATCH_COUNTER_PROBE_ONLY",
                    )
                },
            )
        };
        assert!(read(None), "unset: on");
        assert!(!read(Some("0")), "=0 kills");
    }

    /// The crossing asks the door through the policy (source check).
    #[test]
    fn j27_virtual_dispatch_counter_crossing_asks_the_policy() {
        let src = include_str!("dispatch.rs").replace("\r\n", "\n");
        let at = src
            .find("let count = VIRTUAL_DISPATCH_COUNTER.with(|dc| {")
            .expect("the VIRTUAL_DISPATCH_COUNTER crossing");
        let door = at + src[at..]
            .find("try_jit_compile_callee_for_class_on_miss(")
            .expect("the crossing's door");
        let mode = door + src[door..]
            .find("virtual_dispatch_counter_door_miss(),")
            .expect("the door's mode");
        let close = door
            + src[door..]
                .find(".or_else(|| probe_selected_declaring_body(")
                .expect("the door's call ends before the declaring-body probe");
        assert!(mode < close, "the mode is the door's own argument");
    }
}

#[cfg(test)]
mod j27_dispatch_counter_via_manager_tests {
    use super::dispatch_counter_compile_is_inline;

    #[test]
    fn j27_dispatch_counter_leaves_the_compile_to_the_manager_under_bg() {
        assert!(!dispatch_counter_compile_is_inline(true, true), "default: manager");
        assert!(dispatch_counter_compile_is_inline(true, false), "=0 kill: inline");
        assert!(dispatch_counter_compile_is_inline(false, true), "BG_COMPILE=0: inline");
        assert!(dispatch_counter_compile_is_inline(false, false));
    }

    #[test]
    fn j27_dispatch_counter_via_manager_switch_reads_default_on() {
        let read = || {
            cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_JIT_DISPATCH_COUNTER_VIA_MANAGER",
            )
        };
        let unset = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_DISPATCH_COUNTER_VIA_MANAGER", None)],
            read,
        );
        assert!(unset, "unset: on");
        let killed = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_DISPATCH_COUNTER_VIA_MANAGER", Some("0"))],
            read,
        );
        assert!(!killed, "=0 kills");
    }

    /// The static tail's inline compile is behind the decision (source
    /// check: the crossing is the only place it is asked).
    #[test]
    fn j27_static_dispatch_counter_crossing_asks_the_policy() {
        let src = include_str!("dispatch.rs").replace("\r\n", "\n");
        let at = src
            .find("*count == crate::runtime::env_cache::jit_invocation_threshold()\n        })")
            .expect("the DISPATCH_COUNTER crossing");
        let tail = src.get(at..at + 400).expect("ASCII window after the crossing");
        assert!(tail.contains("&& static_dispatch_counter_compiles_inline();"));
    }
}

#[cfg(test)]
mod j27_mh_real_converting_tests {
    use super::*;
    use cratonvm_native_builtins::lang_invoke::{
        MhRealBox, MH_REAL_UNBOX_BYTE, MH_REAL_UNBOX_CHAR, MH_REAL_UNBOX_INT, MH_REAL_UNBOX_LONG,
        MH_REAL_UNBOX_SHORT,
    };

    fn conv(unbox0: u8, box_return: MhRealBox) -> MhRealConv {
        let mut unbox = [0u8; MH_BIND_MAX_SPLICED_ARGS];
        unbox[0] = unbox0;
        MhRealConv {
            unbox,
            wrappers: [(11, 0), (12, 0), (13, 0), (14, 0), (15, 0)],
            box_return,
        }
    }

    /// j27 wave 5 (lane MHASTYPE): a wrapper's value becomes the word an
    /// `int` / `long` widening of its primitive gives, whatever the slot
    /// carries above the primitive's width; a value of another variant is
    /// the door's.
    #[test]
    fn j27_real_unbox_widens_each_wrapper_as_its_primitive() {
        assert_eq!(mh_real_widen(MH_REAL_UNBOX_LONG, Value::Long(i64::MIN)), Some(i64::MIN));
        assert_eq!(mh_real_widen(MH_REAL_UNBOX_INT, Value::Int(-7)), Some(-7));
        assert_eq!(mh_real_widen(MH_REAL_UNBOX_SHORT, Value::Int(-2)), Some(-2));
        assert_eq!(mh_real_widen(MH_REAL_UNBOX_SHORT, Value::Int(0x1_8000)), Some(-32768));
        assert_eq!(mh_real_widen(MH_REAL_UNBOX_BYTE, Value::Int(0x80)), Some(-128));
        // `char` is unsigned: 'A' is 65, and U+FFFF is 65535, never -1.
        assert_eq!(mh_real_widen(MH_REAL_UNBOX_CHAR, Value::Int(65)), Some(65));
        assert_eq!(mh_real_widen(MH_REAL_UNBOX_CHAR, Value::Int(-1)), Some(65535));
        assert_eq!(mh_real_widen(MH_REAL_UNBOX_LONG, Value::Int(5)), None);
        assert_eq!(mh_real_widen(MH_REAL_UNBOX_INT, Value::Long(5)), None);
        assert_eq!(mh_real_widen(5, Value::Int(1)), None, "no sixth wrapper");
    }

    /// The converted words: marked words replaced by the unbox's answer, the
    /// others passed; a refused unbox (null, a wrapper the JDK's unboxer
    /// rejects) or a call wider than the buffer is the door's; the return box
    /// follows the admission's answer.
    #[test]
    fn j27_real_convert_args_unboxes_marked_words_only() {
        let boxed = 0x7_0000_0040i64;
        let mask = 1u8 << MH_REAL_UNBOX_INT;
        let unbox = |m: u8, w: i64| (m == mask && w == boxed).then_some(-42i64);
        let mut two = conv(0, MhRealBox::Long);
        two.unbox[1] = mask;
        let out = mh_real_convert_args(&two, &[5, boxed], unbox).expect("word 1 unboxes");
        assert_eq!(&out.words[..2], &[5, -42]);
        assert!(out.box_long_return && !out.box_int_return);
        assert!(mh_real_convert_args(&conv(mask, MhRealBox::Keep), &[0], unbox).is_none());
        let keep = mh_real_convert_args(&conv(0, MhRealBox::Int), &[1, 2], unbox).expect("plain");
        assert_eq!(&keep.words[..2], &[1, 2]);
        assert!(keep.box_int_return && !keep.box_long_return);
        let wide = [boxed; MH_BIND_MAX_SPLICED_ARGS + 1];
        assert!(mh_real_convert_args(&conv(mask, MhRealBox::Keep), &wide, unbox).is_none());
    }

    /// The carrier's converting words are unchanged by the shared loop, and
    /// never box a `long`.
    #[test]
    fn j27_carrier_convert_args_never_box_a_long() {
        let carrier = MhBindConv {
            type_slot: 0,
            type_word: 0,
            unbox_int: 0b1,
            box_int_return: true,
            integer_class_id: 1,
            value_slot: 0,
        };
        let out = mh_bind_convert_args(&carrier, &[9, 3], |w| Some(w * 2)).expect("converts");
        assert_eq!(&out.words[..2], &[18, 3]);
        assert!(out.box_int_return && !out.box_long_return);
    }
}

/// j27 wave 6 (lane T1LAM2; `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`):
/// the cold fill records a bound STATIC compile row (`invoke_kind 6`) only for
/// an unconverted real bound handle of a static member.
#[cfg(test)]
mod j27_t1lam2_bound_static_compile_row_tests {
    use super::mh_bound_static_compile_row_wanted as wanted;

    #[test]
    fn only_an_unconverted_bound_static_takes_the_row() {
        assert!(wanted(true, true, true, 3));
        assert!(!wanted(true, true, true, 0), "a bound virtual is the kind 4 row");
        assert!(!wanted(true, true, false, 3), "an unbound static is the plain kind 3 row");
        assert!(!wanted(true, false, true, 3), "a converting handle calls no member as it is");
        assert!(!wanted(false, true, true, 3), "no leaf, no memo or a carrier");
    }

    /// The fill asks the switch and records through the kind 6 writer, after
    /// the virtual bound block (source check).
    #[test]
    fn the_fill_records_the_bound_static_row() {
        let src = include_str!("dispatch.rs").replace("\r\n", "\n");
        let fill = src.find("unsafe fn mh_constant_bind_fill(").expect("the fill");
        let body = &src[fill..];
        let virt = body
            .find("note_mh_constant_target_bound(")
            .expect("the kind 4 row");
        let stat = body
            .find("note_mh_constant_target_bound_static(")
            .expect("the kind 6 row");
        let gate = body
            .find("mh_constant_compile_bound_static_enabled()")
            .expect("the switch");
        assert!(virt < gate && gate < stat);
    }
}
