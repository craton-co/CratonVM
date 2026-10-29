// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! How many classes this process has DEFINED, and which ones.
//!
//! # Why this exists
//!
//! `runtime::diagnostics::classes_loaded` was declared, initialised, reset,
//! formatted into the diagnostic report and unit-tested — and incremented by
//! nothing. It therefore reported `Classes loaded: 0` on every run, which reads
//! as an answer rather than as an absent instrument. That silence is load
//! bearing: `JitCache::invalidate_for_class` runs on class DEFINITION and
//! nowhere else, and it sat at ~1% of an H2 update profile taken minutes past
//! warm-up, with no way to ask what was still defining classes at that point.
//!
//! The counter here is unconditional and is one relaxed add on a path that is
//! not supposed to be hot — and if it turns out to be hot, that is precisely
//! the finding this instrument exists to make.
//!
//! `CRATONVM_DBG_DEFINE_CENSUS=1` additionally keeps a per-name tally and dumps
//! it hottest-first, so "what is defining classes" is answered by name rather
//! than by inference.

use std::sync::atomic::{AtomicU64, Ordering};

static DEFINES: AtomicU64 = AtomicU64::new(0);

/// Bound on distinct names retained by the census, so a workload that mints a
/// fresh name per definition (hidden classes, lambda proxies, generated
/// proxies) cannot grow it without limit. Counting continues past the cap; only
/// new *names* stop being admitted, and the dump says how many it dropped.
const CENSUS_NAME_CAP: usize = 8192;

#[allow(clippy::type_complexity)]
static CENSUS: std::sync::OnceLock<
    parking_lot::Mutex<(std::collections::HashMap<String, u64>, u64)>,
> = std::sync::OnceLock::new();

/// Record one class definition. Called from the single choke point every
/// `define_class*` entry point funnels through.
#[inline]
pub fn note(name: &str) {
    DEFINES.fetch_add(1, Ordering::Relaxed);
    if !crate::loader_flags().dbg_define_census {
        return;
    }
    note_slow(name);
}

#[cold]
fn note_slow(name: &str) {
    let cell = CENSUS.get_or_init(|| parking_lot::Mutex::new((Default::default(), 0)));
    let mut guard = cell.lock();
    let (map, dropped) = &mut *guard;
    if let Some(slot) = map.get_mut(name) {
        *slot += 1;
    } else if map.len() < CENSUS_NAME_CAP {
        map.insert(name.to_string(), 1);
    } else {
        *dropped += 1;
    }
}

/// Total class definitions since VM start.
///
/// This is DEFINITIONS, not distinct classes: a name defined by two loaders
/// counts twice, and so does a redefinition. That is the quantity the JIT-cache
/// invalidation and the class-manager write lock actually pay for.
pub fn total() -> u64 {
    DEFINES.load(Ordering::Relaxed)
}

/// The phases of a class definition timed under
/// `CRATONVM_DBG_DEFINE_CENSUS=1` (interpreter round i4 wave 88, lane L5): the
/// nanoseconds each spends, summed over every define, dumped with the name
/// tally. `ALL` is in the order a `ClassLoader.defineClass1` call runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefinePhase {
    /// `defineClass*` native, entry to the supertype pre-pass: argument reads,
    /// the byte copy, header checks, the loader's namespace id.
    NativeEntry = 0,
    /// The natives' supertype pre-pass (`link_supertypes_refusing_circularity`).
    SupertypePrepass,
    /// `define_class_full` before the class manager: initiating-loader
    /// refusal, transform hook, metaspace check.
    VmPreDefine,
    /// The class manager's `define_class_*` as a whole (under its write lock).
    ClassManagerDefine,
    /// Inside it: parse + attribute decode.
    Parse,
    /// Inside it: the class's code source (the class-path probe for a define
    /// without one).
    CodeSource,
    /// Inside it: verification (structural, Pass 3, or the type-map walk).
    Verify,
    /// Inside it: registration, vtable, hooks.
    Register,
    /// `define_class_full` after the class manager (JIT withdraw, initialize).
    VmPostDefine,
    /// The native after `define_class_full`: defining-loader record, mirror,
    /// `classLoader` / protection domain / class data.
    NativeMirror,
    /// Inside `Verify` (wave 89): the deferred user-loader route (structural
    /// scan + the type-map hand-off).
    VerifyDeferred,
    /// Inside `Verify` (wave 89): the full Pass 2 + Pass 3 route (built-in
    /// loaders' defines: `Lookup.defineClass`, hidden classes).
    VerifyFull,
    /// Inside `Register` (wave 89): index insert, class bytes, `ClassStore::add`.
    RegisterStore,
    /// Inside `Register` (wave 89): vtable build, install / override / load hooks.
    RegisterVtable,
    /// Inside `VmPostDefine` (wave 89): the JIT withdraw by name.
    PostWithdraw,
    /// Inside `VmPostDefine` (wave 89): `initialize` (`defineHiddenClass(b, true)`).
    PostInitialize,
    /// Inside `NativeMirror` (wave 89): `get_class_mirror`.
    MirrorCreate,
    // ---- Wave 90 (interpreter round i4, lane L5) ----
    /// A class-manager define NOT reached through a `defineClass*` native's
    /// `define_class_full` (a class-path / jimage load through the built-in
    /// delegation, or a define nested inside another define). Before wave 90
    /// these were charged to `parse` / `verify` / `register` with the native
    /// defines, so a mean there mixed the JDK's own classes with the tiny
    /// class a bench defines. The inner phases now count native defines only.
    OtherDefine,
    /// Inside `VerifyFull`: Pass 2 (`verify_class_structure`).
    VerifyStructure,
    /// Inside `VerifyFull`: Pass 3 (`verify_class_bytecode_with_strictness`,
    /// the type-state walk and the type-map publish).
    VerifyBytecode,
    /// `VerifyFull` again, for hidden defines only (a subset of its count).
    VerifyFullHidden,
    /// Inside `RegisterVtable`: the vtable descriptor build and its record.
    VtableBuild,
    /// Inside `RegisterVtable`: the VM's vtable install hook and the
    /// override signals.
    VtableInstallHook,
    /// Inside `PostInitialize`: `initialize_class_shared` as a whole (the rest
    /// of `PostInitialize` is the claim in `ensure_class_initialized_shared`).
    InitBody,
    /// Inside `InitBody`: `link_supertypes_on_thread`.
    InitLinkSupertypes,
    /// Inside `InitBody`: `link_claimed_class` for the class itself.
    InitLinkSelf,
    /// Inside `InitLinkSelf`: the class-path package step.
    LinkPackage,
    /// Inside `InitLinkSelf`: Pass 2 at link (and the per-class skip probes).
    LinkStructure,
    /// Inside `InitLinkSelf`: the pending type maps and the verifier loads.
    LinkVerifierLoads,
    /// Inside `InitLinkSelf`: the catch-type loads.
    LinkCatchTypes,
    /// Inside `InitLinkSelf`: the loader-constraint override check.
    LinkConstraints,
    /// Inside `InitLinkSelf`: preparation, `ClassPrepare`, the init-path mirror.
    LinkPrepare,
    /// Inside `InitBody`: superclass and superinterface initialization.
    InitSupertypes,
    /// Inside `InitBody`: from the `Initializing` state write to the end
    /// (`<clinit>` lookup and run, finalization, JFR event).
    InitClinit,
    // ---- Wave 91 (interpreter round i4, lane L5) ----
    /// Not a define phase: the work every class-manager WRITE guard does after
    /// its lock is released (`ClassManagerWriteGuard::drop`, the
    /// deferred-`new` re-sweep and the profile-replay seed), timed at every
    /// release in the process, charged or not. With any deferred-`new` retry
    /// held, each release re-sweeps every held method by name, whether or not
    /// its section defined or linked anything; this row says what that costs
    /// (its `count` is the number of releases, `total-us` their sum).
    WriteGuardSweep,
    // ---- Wave 93 (interpreter round i4, lane L5) ----
    /// Inside `NativeEntry` (`defineClass1`): the name, the byte copy, the
    /// header checks.
    EntryArgs,
    /// Inside `NativeEntry` (`defineClass1`): the boot-append probe, the
    /// loader's namespace, the protection domain's URL, the pins.
    EntryLoader,
    /// Inside `SupertypePrepass`: the class-file parse (kept for the define
    /// under `CRATONVM_DEFINE_ONE_PARSE`).
    PrepassParse,
    /// Inside `SupertypePrepass`: the isolated-URL-loader probe
    /// (`url_classloader_isolated_from_app`).
    PrepassIsolation,
    /// Inside `RegisterStore`: the name index, the constraint pin, the
    /// user-loader set.
    StoreIndex,
    /// Inside `RegisterStore`: the class-bytes cache and the per-class side
    /// tables.
    StoreBytes,
    /// Inside `RegisterStore`: `ClassStore::add` and the metaspace charge.
    StoreAdd,
    /// Inside `LinkPrepare`: the `Verified` / `Preparing` state writes.
    PrepareStates,
    /// Inside `LinkPrepare`: `prepare_class_shared` (the statics block).
    PrepareStatics,
    /// Inside `LinkPrepare`: the `Prepared` write and `ClassPrepare`.
    PrepareTail,
    /// Inside `LinkPrepare`: the init-path mirror (made here for a define that
    /// initializes, before the native's own mirror step).
    PrepareMirror,
    // ---- Wave 94 (interpreter round i4, lane L5): the class-manager define's
    // ~10 us outside its named rows, and the write lock around it ----
    /// Inside `ClassManagerDefine`: taking the class-manager write lock.
    CmWriteAcquire,
    /// Inside `ClassManagerDefine`: releasing it (`ClassManagerWriteGuard::drop`:
    /// the queued `ClassLoad` hooks, the deferred-`new` sweep, the profile seed).
    CmWriteRelease,
    /// Inside the class manager's define, after `Parse`: the access-flag,
    /// wrong-name, prohibited-package and member refusals, the duplicate and
    /// loader-constraint probes, the stub-upgrade probe.
    DefinePreChecks,
    /// After `DefinePreChecks`: superclass and interface resolution, the JFR
    /// event schema, the supertype shape checks.
    DefineSupertypes,
    /// After `DefineSupertypes`: field layout, the class attributes, module
    /// membership, the stored name, the code source (`CodeSource`, which this
    /// row CONTAINS), the origin, the runtime `Class` and its finalizer bit.
    DefineBuild,
    /// After `DefineBuild`: `supertype_link_refusal` (super access, sealed
    /// supertypes, final-method overrides).
    DefineLinkRefusal,
    // ---- Wave 95 (interpreter round i4, lane L1) ----
    /// Not a define phase: the queued class-lifecycle hooks every class-manager
    /// WRITE guard drains after its lock is released
    /// (`drain_pending_class_hooks`: `-Xlog:class+load` and the JVMTI
    /// `ClassLoad` post), timed at every release in the process, as
    /// `WriteGuardSweep` is. Its mean says what a define pays for `ClassLoad`
    /// with no agent attached.
    ClassLoadHooks,
    // ---- Wave 95 (interpreter round i4, lane L5): the register-vtable and
    // native-mirror rows split, and the native's whole body ----
    /// The `defineClass1` native from entry to return, every arm (the rows
    /// above are its parts; `define-lone` minus this is the Java side of
    /// `ClassLoader.defineClass` and the native call itself).
    NativeTotal,
    /// Inside `RegisterVtable`: the shadow-layout census and the positional
    /// claim check (`report_shadow_layout`, `check_safe_positional_claims`).
    VtableClaims,
    /// Inside `VtableInstallHook` (the VM's `vtable_install_adapter_scoped`):
    /// the VM's vtable-manager lookup by VM identity.
    InstallLookup,
    /// Inside `VtableInstallHook`: the manager's write lock and the
    /// descriptor-to-entry conversion (inherited snapshots shared, own ones
    /// built), or the inherited template under
    /// `CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE`.
    InstallConvert,
    /// Inside `VtableInstallHook`: `VtableManager::install_vtable` (the
    /// signature index and the table insert).
    InstallTable,
    /// Inside `NativeMirror`: `register_defining_loader` before the mirror.
    MirrorLoaderRecord,
    /// Inside `MirrorCreate` (`class_mirror_impl`, a fresh mirror only): the
    /// cache probes, the write lock, the layout, the allocation and slot 0.
    MirrorAlloc,
    /// Inside `MirrorCreate`: the name `String`, the modifiers and the other
    /// plain fields.
    MirrorFields,
    /// Inside `MirrorCreate`: `Class.classLoader` (the defining-loader
    /// lookup) and `Class.module`.
    MirrorLoaderModule,
    /// Inside `MirrorCreate`: the cache and reverse-map inserts and the
    /// loader pin.
    MirrorPublish,
    /// Inside `NativeMirror`, after `MirrorCreate`: the native's own
    /// `classLoader` and `protectionDomain` field stores.
    MirrorNativeFields,
    // ---- ZGC round zt, wave t6, lane nd4 ----
    /// Before `VmPreDefine`: the definer's allocations ahead of the define
    /// (the reserved mirror object and its parked name, zs s7-s9; the
    /// `ClassLoader.classes` presize, zt t6/nd4). Charged by the time the
    /// native spends there; `mirror-alloc` reads ~0 on an adopted mirror
    /// because its allocation is here.
    MirrorReserve,
    // ---- ZGC round zt, wave t30, lane dc: the late name binding
    // (`CRATONVM_ZGC_DEFINE_BIND_AFTER_ADD_CLASS`) and the rest of
    // `native-mirror` (t29's census: its unnamed remainder carried the bind
    // arm's extra ~17-31 us per define) ----
    /// Before `MirrorReserve`: the door's `wait_for_unbound_define` (one
    /// class-manager read when no define of the loader is unbound; the
    /// `Thread.yield()` rounds when one is). Bind arm only.
    BindWait,
    /// After `MirrorReserve`: the `retry_opts` `DefineClassFull` clone kept
    /// for a define after a withdrawn rival. Bind arm only.
    BindRetryClone,
    /// Inside `StoreAdd`: `ClassStore::withdraw_name` of an unbound define.
    BindWithdrawName,
    /// Inside `NativeMirror`, after `MirrorNativeFields`: the
    /// `ClassLoader.addClass` upcall (`add_defined_class_to_loader`, a Java
    /// call). Both arms.
    MirrorAddClass,
    /// Inside `NativeMirror`, after `MirrorAddClass`: `settle_unbound_define`
    /// as a whole (~0 off the bind arm, where it returns at once).
    MirrorBindSettle,
    /// Inside `MirrorBindSettle`: taking the class-manager write lock for
    /// `ClassManager::bind_unbound_define`.
    BindCmAcquire,
    /// Inside `MirrorBindSettle`: `ClassManager::bind_unbound_define` under
    /// that lock (`loaded_classes`, the name restore, the constraint pin, the
    /// queued `ClassLoad`).
    BindCmBody,
    /// Inside `BindCmBody`: `ClassStore::restore_name`.
    BindRestoreName,
    /// Inside `MirrorBindSettle`: that write guard's release
    /// (`ClassManagerWriteGuard::drop`: the `ClassLoad` drain and the two
    /// resweeps; the process-wide `class-load-hooks` / `write-guard-sweep`
    /// rows count it too).
    BindCmRelease,
    /// Inside `MirrorBindSettle`: the loader's by-name index insert
    /// (`index_defining_loader`).
    BindIndex,
    /// Inside `NativeMirror`, after `MirrorBindSettle`: the orphan note and
    /// the door trace.
    MirrorAddTail,
    /// Outside `NativeMirror`: `hold_unbound_define_across`'s own work (the
    /// owner-mark TLS push and pop around the door's mirror steps; the
    /// `catch_unwind` frame costs nothing unless a panic unwinds). Bind arm
    /// only.
    BindHold,
}

impl DefinePhase {
    /// Every phase, in order.
    pub const ALL: [DefinePhase; PHASES] = [
        DefinePhase::NativeTotal,
        DefinePhase::NativeEntry,
        DefinePhase::EntryArgs,
        DefinePhase::EntryLoader,
        DefinePhase::SupertypePrepass,
        DefinePhase::PrepassParse,
        DefinePhase::PrepassIsolation,
        DefinePhase::BindWait,
        DefinePhase::MirrorReserve,
        DefinePhase::BindRetryClone,
        DefinePhase::VmPreDefine,
        DefinePhase::ClassManagerDefine,
        DefinePhase::CmWriteAcquire,
        DefinePhase::Parse,
        DefinePhase::DefinePreChecks,
        DefinePhase::DefineSupertypes,
        DefinePhase::DefineBuild,
        DefinePhase::CodeSource,
        DefinePhase::DefineLinkRefusal,
        DefinePhase::Verify,
        DefinePhase::VerifyDeferred,
        DefinePhase::VerifyFull,
        DefinePhase::VerifyStructure,
        DefinePhase::VerifyBytecode,
        DefinePhase::VerifyFullHidden,
        DefinePhase::Register,
        DefinePhase::RegisterStore,
        DefinePhase::StoreIndex,
        DefinePhase::StoreBytes,
        DefinePhase::StoreAdd,
        DefinePhase::BindWithdrawName,
        DefinePhase::RegisterVtable,
        DefinePhase::VtableClaims,
        DefinePhase::VtableBuild,
        DefinePhase::VtableInstallHook,
        DefinePhase::InstallLookup,
        DefinePhase::InstallConvert,
        DefinePhase::InstallTable,
        DefinePhase::CmWriteRelease,
        DefinePhase::VmPostDefine,
        DefinePhase::PostWithdraw,
        DefinePhase::PostInitialize,
        DefinePhase::InitBody,
        DefinePhase::InitLinkSupertypes,
        DefinePhase::InitLinkSelf,
        DefinePhase::LinkPackage,
        DefinePhase::LinkStructure,
        DefinePhase::LinkVerifierLoads,
        DefinePhase::LinkCatchTypes,
        DefinePhase::LinkConstraints,
        DefinePhase::LinkPrepare,
        DefinePhase::PrepareStates,
        DefinePhase::PrepareStatics,
        DefinePhase::PrepareTail,
        DefinePhase::PrepareMirror,
        DefinePhase::InitSupertypes,
        DefinePhase::InitClinit,
        DefinePhase::NativeMirror,
        DefinePhase::MirrorLoaderRecord,
        DefinePhase::MirrorCreate,
        DefinePhase::MirrorAlloc,
        DefinePhase::MirrorFields,
        DefinePhase::MirrorLoaderModule,
        DefinePhase::MirrorPublish,
        DefinePhase::MirrorNativeFields,
        DefinePhase::MirrorAddClass,
        DefinePhase::MirrorBindSettle,
        DefinePhase::BindCmAcquire,
        DefinePhase::BindCmBody,
        DefinePhase::BindRestoreName,
        DefinePhase::BindCmRelease,
        DefinePhase::BindIndex,
        DefinePhase::MirrorAddTail,
        DefinePhase::BindHold,
        DefinePhase::OtherDefine,
        DefinePhase::WriteGuardSweep,
        DefinePhase::ClassLoadHooks,
    ];

    fn label(self) -> &'static str {
        match self {
            DefinePhase::NativeEntry => "native-entry",
            DefinePhase::SupertypePrepass => "supertype-prepass",
            DefinePhase::VmPreDefine => "vm-pre-define",
            DefinePhase::ClassManagerDefine => "class-manager-define",
            DefinePhase::Parse => "  parse",
            DefinePhase::CodeSource => "  code-source",
            DefinePhase::Verify => "  verify",
            DefinePhase::Register => "  register",
            DefinePhase::VmPostDefine => "vm-post-define",
            DefinePhase::NativeMirror => "native-mirror",
            DefinePhase::VerifyDeferred => "    verify-deferred",
            DefinePhase::VerifyFull => "    verify-full",
            DefinePhase::RegisterStore => "    register-store",
            DefinePhase::RegisterVtable => "    register-vtable",
            DefinePhase::PostWithdraw => "  post-withdraw",
            DefinePhase::PostInitialize => "  post-initialize",
            DefinePhase::MirrorCreate => "  mirror-create",
            DefinePhase::OtherDefine => "other-define",
            DefinePhase::VerifyStructure => "      verify-structure",
            DefinePhase::VerifyBytecode => "      verify-bytecode",
            DefinePhase::VerifyFullHidden => "    verify-full-hidden",
            DefinePhase::VtableBuild => "      vtable-build",
            DefinePhase::VtableInstallHook => "      vtable-install-hook",
            DefinePhase::InitBody => "    init-body",
            DefinePhase::InitLinkSupertypes => "      init-link-supers",
            DefinePhase::InitLinkSelf => "      init-link-self",
            DefinePhase::LinkPackage => "        link-package",
            DefinePhase::LinkStructure => "        link-structure",
            DefinePhase::LinkVerifierLoads => "        link-verifier-loads",
            DefinePhase::LinkCatchTypes => "        link-catch-types",
            DefinePhase::LinkConstraints => "        link-constraints",
            DefinePhase::LinkPrepare => "        link-prepare",
            DefinePhase::InitSupertypes => "      init-supertypes",
            DefinePhase::InitClinit => "      init-clinit",
            DefinePhase::WriteGuardSweep => "write-guard-sweep",
            DefinePhase::EntryArgs => "  entry-args",
            DefinePhase::EntryLoader => "  entry-loader",
            DefinePhase::PrepassParse => "  prepass-parse",
            DefinePhase::PrepassIsolation => "  prepass-isolation",
            DefinePhase::StoreIndex => "      store-index",
            DefinePhase::StoreBytes => "      store-bytes",
            DefinePhase::StoreAdd => "      store-add",
            DefinePhase::PrepareStates => "          prepare-states",
            DefinePhase::PrepareStatics => "          prepare-statics",
            DefinePhase::PrepareTail => "          prepare-tail",
            DefinePhase::PrepareMirror => "          prepare-mirror",
            DefinePhase::CmWriteAcquire => "  cm-write-acquire",
            DefinePhase::CmWriteRelease => "  cm-write-release",
            DefinePhase::DefinePreChecks => "  define-pre-checks",
            DefinePhase::DefineSupertypes => "  define-supertypes",
            DefinePhase::DefineBuild => "  define-build",
            DefinePhase::DefineLinkRefusal => "  define-link-refusal",
            DefinePhase::ClassLoadHooks => "class-load-hooks",
            DefinePhase::NativeTotal => "native-total",
            DefinePhase::VtableClaims => "      vtable-claims",
            DefinePhase::InstallLookup => "        install-lookup",
            DefinePhase::InstallConvert => "        install-convert",
            DefinePhase::InstallTable => "        install-table",
            DefinePhase::MirrorLoaderRecord => "  mirror-loader-record",
            DefinePhase::MirrorAlloc => "    mirror-alloc",
            DefinePhase::MirrorFields => "    mirror-fields",
            DefinePhase::MirrorLoaderModule => "    mirror-loader-module",
            DefinePhase::MirrorPublish => "    mirror-publish",
            DefinePhase::MirrorNativeFields => "  mirror-native-fields",
            DefinePhase::MirrorReserve => "mirror-reserve",
            DefinePhase::BindWait => "bind-wait",
            DefinePhase::BindRetryClone => "bind-retry-clone",
            DefinePhase::BindWithdrawName => "        bind-withdraw-name",
            DefinePhase::MirrorAddClass => "  mirror-add-class",
            DefinePhase::MirrorBindSettle => "  mirror-bind-settle",
            DefinePhase::BindCmAcquire => "    bind-cm-acquire",
            DefinePhase::BindCmBody => "    bind-cm-body",
            DefinePhase::BindRestoreName => "      bind-restore-name",
            DefinePhase::BindCmRelease => "    bind-cm-release",
            DefinePhase::BindIndex => "    bind-index",
            DefinePhase::MirrorAddTail => "  mirror-add-tail",
            DefinePhase::BindHold => "bind-hold",
        }
    }
}

/// Number of [`DefinePhase`]s.
// 53 -> 64: interpreter round i4 wave 95, lane L5 (eleven rows). If another
// lane's wave-95 rows land beside these, add its count here too.
// 64 -> 65: ZGC round zt t6/nd4 (`MirrorReserve`).
// 65 -> 77: ZGC round zt t30/dc (the late binding's twelve rows).
const PHASES: usize = 77;

// Wave 90 (interpreter round i4, lane L5): which defines and which
// initializations the inner phases charge. Each flag is ARMED by the outer
// caller just before the call it scopes and TAKEN (read and cleared) at the
// entry of the callee, so a define or an initialization nested inside it (a
// supertype loaded under the class manager, a superclass initialized first)
// finds it clear and is not charged to the outer one's rows.
thread_local! {
    /// Bit [`NATIVE_DEFINE_BIT`]: armed by the VM's `define_class_full` around
    /// its class-manager call. Bit [`CHARGED_SCOPE_BIT`] (wave 95, lane L5):
    /// set while a charged define calls OUT of the class manager or the
    /// native into code that times its own sub-rows (the VM's vtable install
    /// hook, `class_mirror_impl`), which has no `charged` flag of its own.
    /// One cell for both: a new thread-local is a new `static` for
    /// `vm/tests/per_vm_state_statics_ratchet.rs`.
    static NATIVE_DEFINE_ARMED: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
    /// Armed by the VM's `define_class_full` around its `initialize`.
    static DEFINE_INIT_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Armed by a charged `initialize_class_shared` around its own link step.
    static DEFINE_LINK_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[inline]
fn census_on() -> bool {
    crate::loader_flags().dbg_define_census
}

/// Arm the next class-manager define on this thread as a native define
/// (`define_class_full`). No-op unless `CRATONVM_DBG_DEFINE_CENSUS`.
#[inline]
pub fn arm_native_define() {
    if census_on() {
        NATIVE_DEFINE_ARMED.with(|c| c.set(c.get() | NATIVE_DEFINE_BIT));
    }
}

/// Take the native-define arm (the class manager's define entry). `false`
/// without `CRATONVM_DBG_DEFINE_CENSUS` (one flag read, no TLS access).
#[inline]
pub fn take_native_define() -> bool {
    census_on()
        && NATIVE_DEFINE_ARMED.with(|c| {
            let bits = c.get();
            c.set(bits & !NATIVE_DEFINE_BIT);
            bits & NATIVE_DEFINE_BIT != 0
        })
}

/// [`NATIVE_DEFINE_ARMED`]'s native-define arm.
const NATIVE_DEFINE_BIT: u8 = 1;
/// [`NATIVE_DEFINE_ARMED`]'s charged call-out mark.
const CHARGED_SCOPE_BIT: u8 = 2;

/// Mark this thread as inside a charged define's call-out (`charged`), or
/// not; returns the previous mark for [`restore_charged_scope`]. `false` and
/// no TLS access without `CRATONVM_DBG_DEFINE_CENSUS`. Wave 95 (interpreter
/// round i4, lane L5).
#[inline]
pub fn enter_charged_scope(charged: bool) -> bool {
    census_on()
        && NATIVE_DEFINE_ARMED.with(|c| {
            let bits = c.get();
            c.set(if charged {
                bits | CHARGED_SCOPE_BIT
            } else {
                bits & !CHARGED_SCOPE_BIT
            });
            bits & CHARGED_SCOPE_BIT != 0
        })
}

/// Restore the mark [`enter_charged_scope`] returned.
#[inline]
pub fn restore_charged_scope(previous: bool) {
    if census_on() {
        NATIVE_DEFINE_ARMED.with(|c| {
            let bits = c.get();
            c.set(if previous {
                bits | CHARGED_SCOPE_BIT
            } else {
                bits & !CHARGED_SCOPE_BIT
            });
        });
    }
}

/// Is this thread inside a charged define's call-out
/// ([`enter_charged_scope`])? `false` without `CRATONVM_DBG_DEFINE_CENSUS`.
#[inline]
pub fn charged_scope() -> bool {
    census_on() && NATIVE_DEFINE_ARMED.with(|c| c.get() & CHARGED_SCOPE_BIT != 0)
}

/// Arm the next `initialize_class_shared` on this thread as the define path's
/// (`define_class_full`'s `initialize`).
#[inline]
pub fn arm_define_init() {
    if census_on() {
        DEFINE_INIT_ARMED.with(|c| c.set(true));
    }
}

/// Take the define-init arm (`initialize_class_shared`'s entry).
#[inline]
pub fn take_define_init() -> bool {
    census_on() && DEFINE_INIT_ARMED.with(|c| c.replace(false))
}

/// Arm (or, with `false`, clear) the next `link_claimed_class` on this thread.
#[inline]
pub fn arm_define_link(charged: bool) {
    if census_on() {
        DEFINE_LINK_ARMED.with(|c| c.set(charged));
    }
}

/// Take the define-link arm (`link_claimed_class`'s entry).
#[inline]
pub fn take_define_link() -> bool {
    census_on() && DEFINE_LINK_ARMED.with(|c| c.replace(false))
}

/// [`phase_start`] for a scope that was charged ([`take_native_define`] and
/// friends answered `true`); `None` otherwise.
#[inline]
pub fn phase_start_if(charged: bool) -> Option<std::time::Instant> {
    charged.then(std::time::Instant::now)
}

/// Charges its phase when dropped, so a scope with early returns is timed on
/// every exit.
#[must_use]
pub struct PhaseGuard {
    phase: DefinePhase,
    start: Option<std::time::Instant>,
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        phase_end(self.phase, self.start.take());
    }
}

/// A guard charging `phase` from `start` (from [`phase_start_if`]) when it is
/// dropped.
#[inline]
pub fn phase_guard(phase: DefinePhase, start: Option<std::time::Instant>) -> PhaseGuard {
    PhaseGuard { phase, start }
}

/// The [`DefinePhase::OtherDefine`] clock of a class-manager define: running
/// only under `CRATONVM_DBG_DEFINE_CENSUS` for a define that was NOT armed
/// as a native one (`native_define`, from [`take_native_define`]).
#[inline]
pub fn other_define_guard(native_define: bool) -> PhaseGuard {
    PhaseGuard {
        phase: DefinePhase::OtherDefine,
        start: (!native_define && census_on()).then(std::time::Instant::now),
    }
}

static PHASE_NS: [AtomicU64; PHASES] = [const { AtomicU64::new(0) }; PHASES];
static PHASE_COUNT: [AtomicU64; PHASES] = [const { AtomicU64::new(0) }; PHASES];

/// A phase clock: `Some(now)` under `CRATONVM_DBG_DEFINE_CENSUS`, else `None`
/// (one flag read, no clock).
#[inline]
pub fn phase_start() -> Option<std::time::Instant> {
    crate::loader_flags()
        .dbg_define_census
        .then(std::time::Instant::now)
}

/// Charge the time since `start` (from [`phase_start`]) to `phase`.
#[inline]
pub fn phase_end(phase: DefinePhase, start: Option<std::time::Instant>) {
    if let Some(start) = start {
        let ns = start.elapsed().as_nanos() as u64;
        PHASE_NS[phase as usize].fetch_add(ns, Ordering::Relaxed);
        PHASE_COUNT[phase as usize].fetch_add(1, Ordering::Relaxed);
    }
}

/// Charge `ns` nanoseconds to `phase` as ONE occurrence, for a phase whose
/// time is the sum of disjoint pieces (zt t30/dc: `BindHold`'s TLS push and
/// pop around the door it wraps). Nothing unless `CRATONVM_DBG_DEFINE_CENSUS`.
#[inline]
pub fn phase_charge_ns(phase: DefinePhase, ns: u64) {
    if census_on() {
        PHASE_NS[phase as usize].fetch_add(ns, Ordering::Relaxed);
        PHASE_COUNT[phase as usize].fetch_add(1, Ordering::Relaxed);
    }
}

fn dump_phases() {
    eprintln!("[define-census] {:<30}{:>9}{:>13}{:>10}", "phase", "count", "total-us", "mean-ns");
    for phase in DefinePhase::ALL {
        let ns = PHASE_NS[phase as usize].load(Ordering::Relaxed);
        let n = PHASE_COUNT[phase as usize].load(Ordering::Relaxed);
        let mean = if n == 0 { 0 } else { ns / n };
        eprintln!(
            "[define-census] {:<30}{:>9}{:>13}{:>10}",
            phase.label(),
            n,
            ns / 1000,
            mean
        );
    }
}

/// Dump the per-name tally, hottest first, and the phase times. No-op unless
/// `CRATONVM_DBG_DEFINE_CENSUS` is set.
pub fn dump() {
    if !crate::loader_flags().dbg_define_census {
        return;
    }
    dump_phases();
    let Some(cell) = CENSUS.get() else {
        eprintln!("[define-census] {} definitions, no names recorded", total());
        return;
    };
    let (map, dropped) = &*cell.lock();
    let mut rows: Vec<(&String, &u64)> = map.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    eprintln!(
        "[define-census] {} definitions over {} distinct names ({} definitions of names dropped at the {} cap)",
        total(),
        map.len(),
        dropped,
        CENSUS_NAME_CAP
    );
    for (name, count) in rows.iter().take(30) {
        eprintln!("[define-census]   {count:>9} {name}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL` lists every phase exactly once, so each discriminant indexes its
    /// own accumulator and the dump shows every row.
    #[test]
    fn all_phases_listed_once() {
        let mut seen = [false; PHASES];
        for phase in DefinePhase::ALL {
            assert!(!seen[phase as usize], "{phase:?} listed twice in ALL");
            seen[phase as usize] = true;
        }
        assert!(seen.iter().all(|s| *s));
    }

    /// zt t6/nd4: the reservation's row exists, is dumped between the
    /// supertype pre-pass and the VM's pre-define (where the definers run
    /// it), and charges what it is given.
    #[test]
    fn zt6nd4_mirror_reserve_row_sits_before_the_define_and_charges() {
        let pos = |p: DefinePhase| DefinePhase::ALL.iter().position(|q| *q == p).unwrap();
        assert!(pos(DefinePhase::SupertypePrepass) < pos(DefinePhase::MirrorReserve));
        assert!(pos(DefinePhase::MirrorReserve) < pos(DefinePhase::VmPreDefine));
        assert_eq!(DefinePhase::MirrorReserve.label(), "mirror-reserve");
        let idx = DefinePhase::MirrorReserve as usize;
        let before = PHASE_COUNT[idx].load(Ordering::Relaxed);
        phase_end(DefinePhase::MirrorReserve, Some(std::time::Instant::now()));
        assert_eq!(PHASE_COUNT[idx].load(Ordering::Relaxed), before + 1);
        phase_end(DefinePhase::MirrorReserve, None);
        assert_eq!(
            PHASE_COUNT[idx].load(Ordering::Relaxed),
            before + 1,
            "no clock, no charge"
        );
    }

    /// zt t30/dc: the late binding's rows sit where their steps run (the wait
    /// and the clone around the reservation, the withdraw inside the store
    /// add, the upcall / settle / tail rows inside `native-mirror` after its
    /// older sub-rows, the bind's lock rows inside the settle), and
    /// `phase_charge_ns` charges one occurrence only with the census on.
    #[test]
    fn zt30dc_bind_rows_sit_where_their_steps_run() {
        let pos = |p: DefinePhase| DefinePhase::ALL.iter().position(|q| *q == p).unwrap();
        assert!(pos(DefinePhase::BindWait) < pos(DefinePhase::MirrorReserve));
        assert!(pos(DefinePhase::MirrorReserve) < pos(DefinePhase::BindRetryClone));
        assert!(pos(DefinePhase::BindRetryClone) < pos(DefinePhase::VmPreDefine));
        assert_eq!(
            pos(DefinePhase::StoreAdd) + 1,
            pos(DefinePhase::BindWithdrawName)
        );
        let mirror = [
            DefinePhase::NativeMirror,
            DefinePhase::MirrorLoaderRecord,
            DefinePhase::MirrorCreate,
            DefinePhase::MirrorAlloc,
            DefinePhase::MirrorFields,
            DefinePhase::MirrorLoaderModule,
            DefinePhase::MirrorPublish,
            DefinePhase::MirrorNativeFields,
            DefinePhase::MirrorAddClass,
            DefinePhase::MirrorBindSettle,
            DefinePhase::BindCmAcquire,
            DefinePhase::BindCmBody,
            DefinePhase::BindRestoreName,
            DefinePhase::BindCmRelease,
            DefinePhase::BindIndex,
            DefinePhase::MirrorAddTail,
            DefinePhase::BindHold,
        ];
        let first = pos(mirror[0]);
        for (k, phase) in mirror.iter().enumerate() {
            assert_eq!(
                pos(*phase),
                first + k,
                "{phase:?} out of the native-mirror block"
            );
        }
        // Nesting by indentation: a sub-row is indented deeper than its parent.
        let depth = |p: DefinePhase| p.label().len() - p.label().trim_start().len();
        assert!(depth(DefinePhase::BindCmBody) > depth(DefinePhase::MirrorBindSettle));
        assert!(depth(DefinePhase::BindRestoreName) > depth(DefinePhase::BindCmBody));
        assert!(depth(DefinePhase::MirrorAddClass) > depth(DefinePhase::NativeMirror));
        assert!(depth(DefinePhase::BindWithdrawName) > depth(DefinePhase::StoreAdd));
        assert_eq!(depth(DefinePhase::BindHold), 0);
        let idx = DefinePhase::BindHold as usize;
        let before = PHASE_COUNT[idx].load(Ordering::Relaxed);
        phase_charge_ns(DefinePhase::BindHold, 40);
        let expect = if census_on() { before + 1 } else { before };
        assert_eq!(PHASE_COUNT[idx].load(Ordering::Relaxed), expect);
    }

    /// The counter must move on a definition — the whole point is that its
    /// predecessor did not, and a replacement that also reads zero would be
    /// the same defect with a new name.
    #[test]
    fn note_increments_the_total() {
        let before = total();
        note("com/example/Probe");
        assert_eq!(
            total(),
            before + 1,
            "define_census::note did not increment; a counter that never moves \
             reports a confident zero, which is exactly what it replaced"
        );
    }
}
