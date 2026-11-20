#[allow(unused_imports)]
use super::*;


// ---------------------------------------------------------------------------
// Real-JDK mode — THE native-vs-bytecode decision point
// (`docs/book/src/user-guide/real-jdk-mode.md` §7)
// ---------------------------------------------------------------------------
//
// Everything in this section is re-exported through `crate::vm` by
// `pub use vm_exec::*` in `vm/src/vm.rs`, which is the path the interpreter,
// JNI and the `vm/tests/jdk_only_dispatch.rs` integration test use.
//
// Why one function: this repository's history is a catalogue of the same
// native-vs-bytecode question being answered independently at each dispatch
// site and the answers disagreeing (a native shadowing real bytecode on the
// cold path but not the warm one, so behaviour changed with JIT state). §7
// makes that decision a single pure function so the answer cannot depend on
// which door the call came through.

/// The contract (§7) names the method type `Method`; this repository's method
/// type is `cratonvm_reader::method::ClassFileMethod`. Aliased rather than
/// renamed so the signature reads as the contract specifies it without
/// touching ~400 existing `ClassFileMethod` mentions.
pub type Method = cratonvm_reader::method::ClassFileMethod;

/// What to run for one `(class, method)` pair under one `ExecutionPolicy`.
///
/// `Bytecode` borrows the method it was handed — never a re-resolved
/// same-named one. Re-resolving here would reintroduce the super-call
/// recursion bug this VM has hit before (a virtual dispatch that re-enters the
/// subclass override).
pub enum DispatchDecision<'a> {
    /// Run this method's own `Code`.
    Bytecode(&'a Method),
    /// Call the registered `NativeKind::Bridge` (or, under `Compatible`, any
    /// admissible native).
    NativeBridge(NativeCallback),
    /// Call the registered, reviewed `NativeKind::Intrinsic`. The one kind
    /// permitted to shadow concrete bytecode (§1.4).
    Intrinsic(NativeCallback),
    /// `JdkOnly` policy refusal. Surface as [`VmError::JdkOnly`]; never fall
    /// back to another implementation behind the operator's back.
    Reject(cratonvm_types::error::JdkOnlyViolation),
}

impl DispatchDecision<'_> {
    /// The callback for the two native-shaped decisions, `None` otherwise.
    ///
    /// Call sites that only need "is there a native to call here" use this and
    /// stay agnostic about bridge-vs-intrinsic, which is a census distinction,
    /// not a calling-convention one.
    #[inline]
    pub fn native_callback(&self) -> Option<NativeCallback> {
        match self {
            DispatchDecision::NativeBridge(cb) | DispatchDecision::Intrinsic(cb) => Some(*cb),
            DispatchDecision::Bytecode(_) | DispatchDecision::Reject(_) => None,
        }
    }
}

// Violation objects own their strings, so building one allocates three times
// and formats nothing. That is fine on a refusal (which terminates a call) and
// unacceptable on the dispatch fast path, so every constructor below is
// `#[cold] #[inline(never)]`: the allocation and the `String` machinery live
// off in their own functions and the hot path keeps only a not-taken branch.

#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn reject_synthetic_invocation(
    class: &str,
    method: &str,
    descriptor: &str,
) -> cratonvm_types::error::JdkOnlyViolation {
    cratonvm_types::error::JdkOnlyViolation::SyntheticNativeInvocation {
        class: class.to_string(),
        method: method.to_string(),
        descriptor: descriptor.to_string(),
        call_site: None,
    }
}

/// The JPMS module that owns `class`, when the class came out of the boot
/// runtime image.
///
/// §1.7 requires a strict-mode failure to name the class's origin, and the
/// `cratonvm_missing_native_total{module}` counter's label is this string. The
/// answer is already in the class: `ClassOrigin::BootImage` is the only origin
/// that carries a module attribution, and it is the origin every JDK class the
/// `MissingNative` report is about actually has. Application-classpath,
/// user-defined and VM-created classes have no module to name and correctly
/// yield `None` (they render as `unknown`), rather than being guessed at from
/// the package name — `java/…` is not a module and `jdk.internal.*` spans
/// several.
///
/// One `match` on a borrowed field: no lock, no allocation, no class-manager
/// read. Safe on the reject path and cheap enough to be safe anywhere.
#[inline]
pub(in crate::vm::vm_exec) fn declaring_module(class: &crate::classloading::Class) -> Option<&str> {
    match &class.origin {
        cratonvm_classloading::ClassOrigin::BootImage { module, .. } => module.as_deref(),
        _ => None,
    }
}

#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn reject_missing_native(
    class: &str,
    method: &str,
    descriptor: &str,
    module: Option<&str>,
) -> cratonvm_types::error::JdkOnlyViolation {
    cratonvm_types::error::JdkOnlyViolation::MissingNative {
        class: class.to_string(),
        method: method.to_string(),
        descriptor: descriptor.to_string(),
        module: module.map(str::to_string),
    }
}

#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn reject_missing_implementation(
    class: &str,
    method: &str,
    descriptor: &str,
) -> cratonvm_types::error::JdkOnlyViolation {
    cratonvm_types::error::JdkOnlyViolation::MissingImplementation {
        class: class.to_string(),
        method: method.to_string(),
        descriptor: descriptor.to_string(),
    }
}

// ---------------------------------------------------------------------------
// §1.4 observation sink: concrete bytecode beat a registered non-intrinsic
// native
// ---------------------------------------------------------------------------
//
// The interpreter's answer to §1.4 is an ACCEPT, not a reject: bytecode simply
// wins and execution continues. Nothing was refused, so nothing was recorded,
// and `cratonvm_real_bytecode_shadow_attempt_total` measured the JIT alone —
// the one producer of `NativeShadowsBytecode` in the tree
// (`jit/src/lib.rs`'s thin-direct-helper refusal). Yet "a native was registered
// for a triple that has real bytecode" is exactly the over-tagged-registration
// evidence wave 2 needs, and the interpreter sees far more of it than the JIT
// does.
//
// # Cost discipline
//
// This is the common case for a correctly-behaving VM, so the recording must
// not be on the `Compatible` path at all. Every call site below tests
// `policy.is_jdk_only()` — one `Copy` field read, already loaded for the
// policy decision itself — before anything else happens. In `Compatible` mode
// the whole mechanism is a single not-taken branch on a register: no atomic,
// no hash, no allocation, and the sink below stays empty and untouched for the
// life of the process. That is the "gate it on `is_jdk_only()` and say so"
// concession, taken deliberately: it means these counters are only populated
// under `--real-jdk`, which is the mode whose report consumes them.
//
// Under `JdkOnly` the steady-state cost is one relaxed `fetch_add` plus one
// relaxed load from the dedup filter. The `JdkOnlyViolation` — three `String`
// allocations — is built only the first time a given triple is seen, in a
// `#[cold]` function, and never after the bounded buffer saturates.
//
// # Shape
//
// Deliberately the same shape as `vm/src/jit/helpers.rs`'s
// `jdk_only_jit_helper_violations()` / `jdk_only_jit_fastpath_refusals()` pair:
// an exact `u64` counter plus a bounded, deduplicated `Vec<JdkOnlyViolation>`.
// The report writer folds all three sources through one pattern.

/// Times concrete bytecode was preferred over a registered non-intrinsic
/// native under `JdkOnly`. Exact; never saturates.
/// Reach counters for the §11 census, so "the site never runs" is
/// distinguishable from "it runs and never admits a native". Without this a
/// zero census reads as "the exception list is dead" when it may only mean the
/// probe never got there.
pub(in crate::vm::vm_exec) static CHECK_OVERRIDE_REACHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::vm::vm_exec) static CHECK_OVERRIDE_TRUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Times `invoke_or_native` dropped a registered native because an agent had
/// woven the class it is declared on. Non-zero on any run with an inline mock
/// maker, zero on every run without one.
pub(in crate::vm::vm_exec) static NATIVE_SHADOW_DROPPED_BY_REDEFINE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Read [`NATIVE_SHADOW_DROPPED_BY_REDEFINE`].
pub fn native_shadow_dropped_by_redefine() -> u64 {
    NATIVE_SHADOW_DROPPED_BY_REDEFINE.load(std::sync::atomic::Ordering::Relaxed)
}
/// Times a `check_override` NAME disjunct was dropped because an agent had
/// redefined the declaring class, so its woven bytecode is authoritative.
///
/// Counted rather than silent for the reason every other engagement counter in
/// this tree is: the fix is invisible in a passing run, and "the guard is
/// there" and "the guard fires" are different claims. A run of
/// `SimpleClientHttpResponseTests` reads non-zero here; a run with no agent
/// reads zero, which is what says the ordinary path is untouched.
pub(in crate::vm::vm_exec) static CHECK_OVERRIDE_REDEFINE_SUPPRESSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Read [`CHECK_OVERRIDE_REDEFINE_SUPPRESSED`].
pub fn check_override_redefine_suppressed() -> u64 {
    CHECK_OVERRIDE_REDEFINE_SUPPRESSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record a `--real-jdk` refusal of the §8 interface substitution.
///
/// The substitution runs a DIFFERENT class's native against a receiver that is
/// not an instance of it (`java/util/Iterator` -> `java/util/HashMap$KeyItr` is
/// the sharpest case). Under strict mode that is a compatibility substitution
/// §1 forbids, and it is silent, so it earns a violation row rather than a
/// counter: a reader of `--real-jdk-report` should see which interface was
/// asked for and that a substitution would have answered it.
///
/// A free-text `native_kind` tag, the same mechanism the JIT's
/// `"jit-thin-direct-helper"` rows use, so the row names the shape of the
/// decision instead of borrowing a `NativeKind` that does not describe it.
///
/// Deliberately does NOT bump `JDK_ONLY_NATIVE_SHADOW_ATTEMPTS`: that counter
/// surfaces as `interpreter_bytecode_preferred`, and nothing was preferred here
/// — the substitution was refused and the call falls through to ordinary
/// resolution failure. Counting it there would inflate a "bytecode won"
/// statistic with cases where nothing won.
pub fn record_interface_substitution_refusal(
    sink: &NativeShadowSink,
    interface: &str,
    canonical: &str,
    method_name: &str,
    descriptor: &str,
) {
    let _ = canonical;
    offer_native_shadow_observation(sink, interface, method_name, descriptor, "interface-substitution");
}

/// JDK-ONLY-WAVE2 §8 census: which of the five hard-coded interface
/// substitutions in `interpreter.rs` actually fires, and for what.
pub(in crate::vm::vm_exec) fn canonical_census(
) -> &'static parking_lot::Mutex<std::collections::BTreeMap<(String, String, String), u64>> {
    static C: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::BTreeMap<(String, String, String), u64>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| parking_lot::Mutex::new(std::collections::BTreeMap::new()))
}

/// Record one interface->canonical-class substitution.
pub fn record_canonical_substitution(interface: &str, canonical: &str, method_name: &str) {
    if !check_override_census_on() {
        return;
    }
    *canonical_census()
        .lock()
        .entry((
            interface.to_string(),
            canonical.to_string(),
            method_name.to_string(),
        ))
        .or_insert(0) += 1;
}

/// Dump the §8 census as TSV on stderr.
pub fn dump_canonical_census() {
    if !check_override_census_on() {
        return;
    }
    let c = canonical_census().lock();
    eprintln!("[CANONICAL_CENSUS] rows={}", c.len());
    for ((iface, canon, m), n) in c.iter() {
        eprintln!("[CANONICAL] {}	{}	{}	{}", iface, canon, m, n);
    }
}

/// Record a `--real-jdk` refusal of a `check_override` **name** disjunct.
///
/// The chain's name entries all say "prefer our native over the real JDK's
/// concrete bytecode", which §1.4 forbids under strict policy. Refusing them is
/// what the wave-2 record prescribes as the strict replacement for the whole
/// chain; recording the refusal is what keeps the two modes' difference
/// nameable rather than silent, and it is the same shape §8's
/// interface-substitution refusal uses.
///
/// The free-text `native_kind` tag distinguishes these rows from the
/// interpreter's `NativeKind`-tagged shadow observations and from the JIT's
/// `"jit-thin-direct-helper"` rows: all three are different facts about the
/// same method and all three belong in the report.
///
/// Like §8's, this deliberately does NOT bump `JDK_ONLY_NATIVE_SHADOW_ATTEMPTS`.
/// That counter surfaces as `interpreter_bytecode_preferred` and means "a
/// registered native yielded to bytecode at dispatch"; this is a resolution-time
/// refusal to mark the method native at all, one step earlier.
pub fn record_check_override_strict_refusal(
    sink: &NativeShadowSink,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    offer_native_shadow_observation(sink, class_name, method_name, descriptor, "check-override-name");
}

/// JDK-ONLY-WAVE2 §11 census: which `check_override` disjuncts actually admit
/// a native, keyed by the triple that reached the branch.
///
/// Off unless `CRATONVM_DBG_CHECK_OVERRIDE` is set, and then it is a `Mutex`
/// around a map on a cold-ish path — this is a measurement instrument for the
/// per-family deletion exercise, not something the fast path pays for.
#[allow(clippy::type_complexity)]
pub(in crate::vm::vm_exec) fn check_override_census(
) -> &'static parking_lot::Mutex<std::collections::BTreeMap<(String, String, String), (u64, bool)>>
{
    static C: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::BTreeMap<(String, String, String), (u64, bool)>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| parking_lot::Mutex::new(std::collections::BTreeMap::new()))
}

pub(in crate::vm::vm_exec) fn check_override_census_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

/// Record one `check_override`-admitted native. `abstract_only` marks the
/// contract-§7-legal case so it can be subtracted from the name-list evidence.
pub(crate) fn record_check_override_hit(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    is_abstract: bool,
) {
    if !check_override_census_on() {
        return;
    }
    let mut c = check_override_census().lock();
    let e = c
        .entry((
            class_name.to_string(),
            method_name.to_string(),
            descriptor.to_string(),
        ))
        .or_insert((0, true));
    e.0 += 1;
    // Sticky: if ANY hit for this triple was non-abstract, a name disjunct is
    // doing real work for it.
    e.1 &= is_abstract;
}

/// Dump the §11 census as TSV on stderr. Called from the VM shutdown path.
pub fn dump_check_override_census() {
    if !check_override_census_on() {
        return;
    }
    let c = check_override_census().lock();
    eprintln!(
        "[CHECK_OVERRIDE_CENSUS] rows={} reached={} chain_true={} redefine_suppressed={}",
        c.len(),
        CHECK_OVERRIDE_REACHED.load(std::sync::atomic::Ordering::Relaxed),
        CHECK_OVERRIDE_TRUE.load(std::sync::atomic::Ordering::Relaxed),
        CHECK_OVERRIDE_REDEFINE_SUPPRESSED.load(std::sync::atomic::Ordering::Relaxed),
    );
    for ((cls, m, d), (n, abstract_only)) in c.iter() {
        eprintln!(
            "[CHECK_OVERRIDE] {}	{}	{}	{}	{}",
            cls,
            m,
            d,
            n,
            if *abstract_only { "abstract" } else { "NAME" }
        );
    }
}

// ---------------------------------------------------------------------------
// The enforcement dial's per-door census  (`H17-3` N1)
// ---------------------------------------------------------------------------

/// Which dispatch door is asking [`resolve_native_dispatch_wave1`].
///
/// # Why a dispatch door has a name at all
///
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` -- the dial that makes contract §1.4
/// *enforced* rather than *counted* -- had exactly **one** live call site
/// tree-wide (`resolve_step1_native`), which `H17-2` §5 established by grep and
/// not by inference. Arming a class therefore armed only that class's cold,
/// step-1 dispatches; every dispatch served by a warm invoke-cache entry, by
/// the force-native interceptor or by a reflective `Method.invoke` ran the
/// native regardless. Four records priced retirements with that instrument, so
/// **an armed FAILURE was real and an armed ZERO was unreliable**.
///
/// Naming the doors is the precondition for fixing that: a door that cannot be
/// named cannot be counted, and `H17-3` §6's first instruction is to count
/// before editing. The variants below are the complete set of
/// `resolve_native_dispatch_wave1` call sites as of 2026-08-21 -- `rg -n
/// 'resolve_native_dispatch_wave1\($'` is the check that this list is still
/// complete, and a new door that reuses an existing variant is worse than a
/// new variant, because it silently merges two populations.
///
/// This tag is **measurement only**. No dispatch decision reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DispatchDoor {
    /// `resolve_step1_native` -- `try_stackless_invoke` step 1, the cold
    /// name-keyed lookup. The one door the dial has always reached.
    Step1,
    /// `invoke_or_native` -- the VM's hottest native path; the "always check
    /// the native registry first" probe.
    InvokeOrNative,
    /// `admit_forced_native{,_id}` -- the force-native interceptor, reached
    /// from seven call sites and from the per-call-site `NativeCallSite` memo.
    ForceIntercept,
    /// `revalidate_cached_native` -- a warmed invoke-cache entry being
    /// redeemed. Returning `None` here is the eviction signal.
    CacheRevalidate,
    /// `populate_invoke_cache` -- deciding what a cold call site caches.
    CachePopulate,
    /// `jdk_only_admit_jit_fast_native` -- JIT by-name native fast path,
    /// cache-fill time.
    JitFastNative,
    /// `invoke_on_class_shared_inner`, the `should_force_registered_native_
    /// over_bytecode` arm.
    ClassSharedForce,
    /// `invoke_on_class_shared_inner`, the `is_native` arm.
    ClassSharedNative,
    /// `try_stackless_invoke`'s own force-native arm (`invoke.rs`).
    StacklessForce,
    /// `resolve_native_for_dispatch` -- `execute`'s no-`Code` rescue chain.
    /// §7 step 3b: there is no bytecode here to shadow.
    NoCodeRescue,
    /// `elidable_ctor_native_would_run` -- the JIT's allocation-elision probe.
    ElidableCtor,
    /// `invoke_or_native`'s array-alias retry on the ORIGINAL class name.
    InvokeOrNativeAlias,
    /// `invoke_or_native`'s superclass walk, the arm where the parent has BOTH
    /// bytecode and a registered native.
    InvokeOrNativeParentShadow,
    /// `invoke_or_native`'s superclass walk, the arm where the parent has only
    /// a registered native.
    InvokeOrNativeParent,
}

impl DispatchDoor {
    /// Every door, in census-print order.
    pub const ALL: [DispatchDoor; 14] = [
        DispatchDoor::Step1,
        DispatchDoor::InvokeOrNative,
        DispatchDoor::InvokeOrNativeAlias,
        DispatchDoor::InvokeOrNativeParentShadow,
        DispatchDoor::InvokeOrNativeParent,
        DispatchDoor::ForceIntercept,
        DispatchDoor::CacheRevalidate,
        DispatchDoor::CachePopulate,
        DispatchDoor::JitFastNative,
        DispatchDoor::ClassSharedForce,
        DispatchDoor::ClassSharedNative,
        DispatchDoor::StacklessForce,
        DispatchDoor::NoCodeRescue,
        DispatchDoor::ElidableCtor,
    ];

    /// Stable census key. Also the name in `[DIAL_DOOR]` rows.
    pub fn name(self) -> &'static str {
        match self {
            DispatchDoor::Step1 => "step1",
            DispatchDoor::InvokeOrNative => "invoke_or_native",
            DispatchDoor::InvokeOrNativeAlias => "invoke_or_native.alias",
            DispatchDoor::InvokeOrNativeParentShadow => "invoke_or_native.parent_shadow",
            DispatchDoor::InvokeOrNativeParent => "invoke_or_native.parent",
            DispatchDoor::ForceIntercept => "force_intercept",
            DispatchDoor::CacheRevalidate => "cache_revalidate",
            DispatchDoor::CachePopulate => "cache_populate",
            DispatchDoor::JitFastNative => "jit_fast_native",
            DispatchDoor::ClassSharedForce => "class_shared_force",
            DispatchDoor::ClassSharedNative => "class_shared_native",
            DispatchDoor::StacklessForce => "stackless_force",
            DispatchDoor::NoCodeRescue => "no_code_rescue",
            DispatchDoor::ElidableCtor => "elidable_ctor",
        }
    }

    #[inline]
    pub(in crate::vm::vm_exec) fn index(self) -> usize {
        match self {
            DispatchDoor::Step1 => 0,
            DispatchDoor::InvokeOrNative => 1,
            DispatchDoor::InvokeOrNativeAlias => 2,
            DispatchDoor::InvokeOrNativeParentShadow => 3,
            DispatchDoor::InvokeOrNativeParent => 4,
            DispatchDoor::ForceIntercept => 5,
            DispatchDoor::CacheRevalidate => 6,
            DispatchDoor::CachePopulate => 7,
            DispatchDoor::JitFastNative => 8,
            DispatchDoor::ClassSharedForce => 9,
            DispatchDoor::ClassSharedNative => 10,
            DispatchDoor::StacklessForce => 11,
            DispatchDoor::NoCodeRescue => 12,
            DispatchDoor::ElidableCtor => 13,
        }
    }
}

/// Armed-class `Bridge` dispatches that ARRIVED at each door.
pub(in crate::vm::vm_exec) static DIAL_DOOR_REACHED: [std::sync::atomic::AtomicU64; 14] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Of those, the ones the door sent to bytecode (`bytecode_available` true).
pub(in crate::vm::vm_exec) static DIAL_DOOR_YIELDED: [std::sync::atomic::AtomicU64; 14] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Per-triple detail, `CRATONVM_DBG_DIAL_DOORS=1` only: which (door, triple)
/// pairs the dial armed and what each one decided. A `Mutex<BTreeMap>` is far
/// too expensive for the hot path, which is why the totals above are separate
/// and always available on an armed run.
#[allow(clippy::type_complexity)]
pub(in crate::vm::vm_exec) fn dial_door_detail() -> &'static parking_lot::Mutex<
    std::collections::BTreeMap<(&'static str, String, String, String), (u64, u64)>,
> {
    static C: std::sync::OnceLock<
        parking_lot::Mutex<
            std::collections::BTreeMap<(&'static str, String, String, String), (u64, u64)>,
        >,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| parking_lot::Mutex::new(std::collections::BTreeMap::new()))
}

/// `CRATONVM_DBG_DIAL_DOORS` -- add the per-triple rows to the door census.
pub fn dial_door_detail_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// Note one armed-class `Bridge` arrival at `door`, and whether the door sent
/// it to bytecode.
///
/// Ordered cheapest-test-first: a `--fast-boot` run stops on the `Copy` policy
/// read, an unarmed `--real-jdk` run stops on the enum compare or the memoised
/// dial bool, and only a run that has actually armed this class pays for the
/// counter. That is the same budget `resolve_step1_native` has always paid to
/// ask the dial at all.
///
/// Called from [`resolve_native_dispatch_wave1`] for every routed door, and
/// directly from the three `invoke_or_native` doors that are not routed through
/// it at all (an unrouted `find` is invisible to a resolver-side census, which
/// is exactly the kind of hole this census exists to make countable).
#[inline]
pub(crate) fn note_dial_door(
    door: DispatchDoor,
    policy: cratonvm_types::compat::ExecutionPolicy,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: cratonvm_native_api::NativeKind,
    yielded: bool,
) {
    if policy.is_jdk_only()
        && crate::runtime::env_cache::dial_applies_to_kind(kind)
        && crate::runtime::env_cache::jdk_only_enforce_shadow_for(class_name)
    {
        record_dial_door_armed(door, class_name, method_name, descriptor, yielded);
    }
}

/// Record one armed-class `Bridge` arrival at `door`.
///
/// Called from [`resolve_native_dispatch_wave1`] under `is_jdk_only()` only, and
/// it returns on the first line for every class the dial does not cover -- so
/// an unarmed `--real-jdk` run pays one memoised bool read and one enum
/// compare, and a `--fast-boot` run never reaches it at all. That is the same
/// budget `resolve_step1_native` has always paid to ask the dial.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn record_dial_door_armed(
    door: DispatchDoor,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    yielded: bool,
) {
    DIAL_DOOR_REACHED[door.index()].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if yielded {
        DIAL_DOOR_YIELDED[door.index()].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    if dial_door_detail_on() {
        let mut d = dial_door_detail().lock();
        let e = d
            .entry((
                door.name(),
                class_name.to_string(),
                method_name.to_string(),
                descriptor.to_string(),
            ))
            .or_insert((0, 0));
        e.0 += 1;
        if yielded {
            e.1 += 1;
        }
    }
}

/// The per-door census as `(door, reached, yielded)`, for the report writer.
///
/// Doors with nothing to say are still returned: a door missing from the list
/// would be ambiguous between "no armed dispatch arrived here" and "this binary
/// does not know about that door", and telling those apart is the whole point
/// of a census whose predecessor was a deduplicated presence set with no
/// counts.
pub fn dial_door_counts() -> Vec<(&'static str, u64, u64)> {
    DispatchDoor::ALL
        .iter()
        .map(|d| {
            (
                d.name(),
                DIAL_DOOR_REACHED[d.index()].load(std::sync::atomic::Ordering::Relaxed),
                DIAL_DOOR_YIELDED[d.index()].load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .collect()
}

/// Dump the per-door census. No-op unless the dial is armed or
/// `CRATONVM_DBG_DIAL_DOORS` is set, so a normal run prints nothing.
///
/// Read the two columns as a leak report: `reached` is armed-class `Bridge`
/// dispatches that arrived at the door, `yielded` is the ones it handed to
/// bytecode. **`reached - yielded` is the price the dial is not charging.**
pub fn dump_dial_door_census() {
    // The `CRATONVM_PURE_BYTECODE` probe lever arms the dial implicitly; that
    // is not a dial measurement, so it must not print a census onto stderr
    // (it would be the last line of every vector and defeat output
    // classification).
    let armed = !crate::runtime::env_cache::enforce_shadow_scope().is_off()
        && !cratonvm_native_api::unretire::pure_bytecode();
    if !armed && !dial_door_detail_on() {
        return;
    }
    let mut total_r = 0u64;
    let mut total_y = 0u64;
    for d in DispatchDoor::ALL {
        total_r += DIAL_DOOR_REACHED[d.index()].load(std::sync::atomic::Ordering::Relaxed);
        total_y += DIAL_DOOR_YIELDED[d.index()].load(std::sync::atomic::Ordering::Relaxed);
    }
    eprintln!(
        "[DIAL_DOOR_CENSUS] armed={armed} reached={total_r} yielded={total_y} leaked={}",
        total_r.saturating_sub(total_y)
    );
    for d in DispatchDoor::ALL {
        let r = DIAL_DOOR_REACHED[d.index()].load(std::sync::atomic::Ordering::Relaxed);
        let y = DIAL_DOOR_YIELDED[d.index()].load(std::sync::atomic::Ordering::Relaxed);
        if r == 0 && y == 0 {
            continue;
        }
        eprintln!(
            "[DIAL_DOOR] {}\treached={}\tyielded={}\tleaked={}",
            d.name(),
            r,
            y,
            r.saturating_sub(y)
        );
    }
    if dial_door_detail_on() {
        let d = dial_door_detail().lock();
        eprintln!("[DIAL_DOOR_DETAIL] rows={}", d.len());
        for ((door, cls, m, desc), (r, y)) in d.iter() {
            eprintln!("[DIAL_TRIPLE] {door}\t{cls}\t{m}\t{desc}\treached={r}\tyielded={y}");
        }
    }
}

/// The interpreter's `--real-jdk-report` observation sink: the dispatch
/// resolver's `NativeShadowsBytecode` rows and counters, and the intrinsic
/// census exemption beside them.
///
/// t1 wave 4 (lane Y2): these were eleven process statics, so two VMs in one
/// process (an embedder, `cratonvm-embed`'s sequential VMs, a unit test that
/// builds two) wrote ONE report, and a `--compatible` VM's report could name a
/// `--real-jdk` sibling's shadows. AGENTS.md: no process globals for
/// compatibility state. Each VM now owns one on
/// `NativeRealm::native_shadow_sink`, and the resolver's production doors pass
/// it ([`resolve_dispatch_in`], [`resolve_native_dispatch_wave1_in`]). The
/// sink-less spellings ([`resolve_dispatch`], [`resolve_native_dispatch_wave1`])
/// record into [`process_native_shadow_sink`]; they exist for callers with no
/// VM in hand (the policy-matrix tests), and nothing in a VM reads that sink.
///
/// Every field keeps the semantics its static had; the long explanations stay
/// on the accessor functions below, each of which now takes the sink.
pub struct NativeShadowSink {
    /// Exact number of times bytecode won over a registered non-intrinsic
    /// native ([`jdk_only_native_shadow_attempts`]).
    pub(in crate::vm::vm_exec) attempts: std::sync::atomic::AtomicU64,
    // t1 wave 6, lane Y3: the `unenforced` counter ("a `Bridge` stood in front
    // of concrete bytecode under `JdkOnly` and ran anyway") and its recorder
    // `record_native_shadow_ran_over_bytecode` were removed. The recorder's
    // only caller, `resolve_step1_native`'s observe-but-not-enforce arm, was
    // deleted by bytecode-first dispatch (`3d31008561`): step 1 now lets only a
    // reviewed `Intrinsic` win before method resolution, and
    // `resolve_dispatch_in` step 3 always runs concrete bytecode over a
    // `Bridge`. No `JdkOnly` door is left where that event can happen, so the
    // counter read 0 on every run.
    /// Non-zero 64-bit digests of triples already offered to the buffer.
    ///
    /// A pure optimisation: a hit skips the mutex, a miss (including a hash
    /// collision evicting another triple) costs one redundant lock, and the
    /// `Vec` dedups regardless. Correctness never depends on it.
    pub(in crate::vm::vm_exec) filter: [std::sync::atomic::AtomicU64; JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS],
    /// Set once the buffer is full, so later filter misses stop taking the
    /// lock.
    pub(in crate::vm::vm_exec) full: std::sync::atomic::AtomicBool,
    /// Distinct observations the sink had no room for — the number that
    /// converts `violations[]` from a floor into a total.
    ///
    /// **Monotonic, and it keeps counting past the cap.** That is the whole
    /// point: a boolean says the list is short, this says *by how much*, and
    /// `recorded + dropped` is the population the run actually saw. Before
    /// 2026-08-20 the only signal was `full`, and every count in
    /// `docs/known-issues/jdk-only/` was a floor for that reason (`G90-1` §8
    /// N1).
    ///
    /// **What it counts, precisely, and what it does not.** It is incremented
    /// by [`offer_native_shadow_observation`] on an offer that reaches a full
    /// buffer **and was not absorbed by the filter** — so repeats of an
    /// already-offered triple do not inflate it, and it approximates the number
    /// of distinct dropped triples. It is not exact: the filter is
    /// [`JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS`] wide, so once the dropped
    /// population exceeds it, collisions can count one triple twice. It is
    /// therefore an upper bound on distinct-dropped and a lower bound on
    /// nothing — quote it as "approximately, and never fewer than", and prefer
    /// raising `CRATONVM_NATIVE_SHADOW_SINK_CAP` until it reads zero.
    pub(in crate::vm::vm_exec) dropped: std::sync::atomic::AtomicU64,
    /// The retained, deduplicated, capped observations.
    pub(in crate::vm::vm_exec) rows: parking_lot::Mutex<Vec<cratonvm_types::error::JdkOnlyViolation>>,
    /// THE CENSUS EXEMPTION, MADE COUNTABLE: event count of
    /// [`record_intrinsic_shadow_exempt`].
    ///
    /// `NativeKind::Intrinsic` is §1.4's reviewed exception: it MAY shadow real
    /// bytecode, so neither shadow recorder fires for it, at either of the two
    /// sites that could (`resolve_dispatch` step 2 returns the intrinsic before
    /// step 3's arm is reached; `resolve_native_dispatch_wave1` tests
    /// `kind != NativeKind::Intrinsic` outright). That is a defensible design —
    /// every JVM does it for `Math.sqrt` — but it means every `native-shadows-
    /// bytecode` figure the report has ever carried is a FLOOR, by construction
    /// and not by capacity, and the size of the gap was never published
    /// anywhere.
    ///
    /// `WORKER-3-NOTE-5` (2026-08-22) measured it statically at **398**
    /// registry rows standing where JDK 25 declares the method with code — +29%
    /// on the then-published 1387 — and asked for the exemption to be published
    /// beside any quoted figure. This is the *publication* half: it counts
    /// intrinsics that actually DISPATCHED in front of bytecode this run.
    ///
    /// Deliberately NOT a `violations[]` row. `difftest/src/census.rs` and
    /// every record in `docs/known-issues/jdk-only/` tally that array by
    /// `kind()`, so adding rows there would move a published number by
    /// redefining it. It is a sibling block instead.
    pub(in crate::vm::vm_exec) exempt_events: std::sync::atomic::AtomicU64,
    /// Distinct triples behind `exempt_events`, capped at
    /// [`jdk_only_native_shadow_cap`] and deduplicated through its own filter.
    ///
    /// Its own filter, not the shadow sink's: sharing one would let an exempt
    /// intrinsic absorb the offer of a same-named `Bridge` row (or the reverse)
    /// and silently delete a row from whichever sink lost the race.
    pub(in crate::vm::vm_exec) exempt_filter: [std::sync::atomic::AtomicU64; JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS],
    pub(in crate::vm::vm_exec) exempt_dropped: std::sync::atomic::AtomicU64,
    pub(in crate::vm::vm_exec) exempt_full: std::sync::atomic::AtomicBool,
    pub(in crate::vm::vm_exec) exempt_triples: parking_lot::Mutex<Vec<(String, String, String)>>,
}

impl NativeShadowSink {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        NativeShadowSink {
            attempts: AtomicU64::new(0),
            filter: [const { AtomicU64::new(0) }; JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS],
            full: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            rows: parking_lot::Mutex::new(Vec::new()),
            exempt_events: AtomicU64::new(0),
            exempt_filter: [const { AtomicU64::new(0) }; JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS],
            exempt_dropped: AtomicU64::new(0),
            exempt_full: AtomicBool::new(false),
            exempt_triples: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

impl Default for NativeShadowSink {
    fn default() -> Self {
        Self::new()
    }
}

/// The sink of resolver calls made with no VM in hand ([`resolve_dispatch`],
/// [`resolve_native_dispatch_wave1`]): the policy-matrix tests. No VM's report
/// reads it.
pub fn process_native_shadow_sink() -> &'static NativeShadowSink {
    static PROCESS: std::sync::OnceLock<NativeShadowSink> = std::sync::OnceLock::new();
    PROCESS.get_or_init(NativeShadowSink::new)
}

/// `native_kind` tag for a "the `Bridge` DISPATCHED in front of real bytes"
/// observation. Not a [`cratonvm_native_api::NativeKind`] spelling. Same
/// convention as the JIT's `"jit-thin-direct-helper"`.
///
/// **No interpreter recorder writes it any more** (t1 wave 6, lane Y3): its
/// recorder lost its only caller with bytecode-first dispatch (`3d31008561`)
/// and was removed. The alias stays because `types` still classifies rows by
/// the tag, and an older report may carry it.
///
/// **Now an alias**, not a definition. The tag is the discriminator between the
/// two opposite outcomes a `NativeShadowsBytecode` row can carry, and `types` is
/// the crate that writes those rows' `summary`, `reason` and JSON — so `types`
/// owns the spelling and both sides cannot drift. See
/// [`cratonvm_types::error::NATIVE_SHADOW_RAN_TAG`], which records what the split
/// spelling cost the first time somebody read the report without it.
pub const JDK_ONLY_SHADOW_UNENFORCED_TAG: &str = cratonvm_types::error::NATIVE_SHADOW_RAN_TAG;

/// Maximum number of distinct structured observations retained, by default.
///
/// Read through [`jdk_only_native_shadow_cap`], never directly: an operator can
/// raise it, and a site that reads this constant would report the default while
/// the sink obeyed something else.
///
/// # Why 4096 and not 256
///
/// It was 256 until 2026-08-20. At that value **3 of the 36 `--real-jdk`
/// regression vectors saturated it**, which made every
/// `native-shadows-bytecode` figure quoted anywhere in
/// `docs/known-issues/jdk-only/` a floor — including the headline "980 -> 943",
/// which is a difference of two censored measurements (`G90-1` §1, §8 N1).
///
/// The argument that used to sit below this constant — that raising the default
/// "costs every strict run more memory" — is wrong, and the correction is the
/// reason this number could move at all. The sink is a `Vec` that is **grown by
/// `push`**: its memory is proportional to the number of distinct observations
/// actually recorded, not to the ceiling. A run with 80 shadows allocates 80
/// rows whether the cap is 256 or 4096. The cap only bounds the WORST case, and
/// the worst case at 4096 is ~4096 x (three `String`s + a `&'static str`), call
/// it under a megabyte on a VM that has just mapped a JDK image. That is not a
/// change to the shipping configuration's cost; it is a change to the ceiling
/// nobody reaches.
///
/// 4096 is chosen against the measured populations rather than picked round:
/// the union of `native-won` triples over ALL 36 vectors is 943 (`G90-1` §1),
/// the largest single-vector population measured is the embedded-Tomcat census
/// below, and both recorders share this one sink. A ceiling four times the
/// whole-corpus union is one no single vector is expected to reach.
///
/// **PREDICTED, not measured:** that no `--real-jdk` regression vector saturates
/// at 4096. What falsifies it is a report with `observation_sink.truncated:
/// true` — which now says so in the file, which is the actual fix here. A bigger
/// cap is still a cap; the drop counter and the flag are the load-bearing half.
pub const JDK_ONLY_NATIVE_SHADOW_CAP: usize = 4096;

/// Ceiling on the operator override below. 65,536 distinct triples is more than
/// twice the whole registry, so it cannot be reached by a real workload — it is
/// there so a mistyped value cannot turn a diagnostic sink into a memory leak.
pub(in crate::vm::vm_exec) const JDK_ONLY_NATIVE_SHADOW_CAP_MAX: usize = 65_536;

/// The cap this process is actually using.
///
/// # Why this is an override and not a bigger constant
///
/// 256 was right for the workload the sink was designed against — a probe or a
/// regression vector, where the population fits and the list is the answer. It
/// was **not enough for an application**, and that is measured rather than
/// argued: embedded Tomcat booting, serving one GET and one 404, and shutting
/// down under `--real-jdk` saturated it, with 188 distinct `native-won` triples
/// recorded before the sink stopped learning — against 58 for the reflection
/// vector G60-1 §1 counted. So the record's §5 N3 ("run this report against an
/// application, not a vector") could not be *completed* at 256: the answer
/// arrived truncated, and narrowing the workload until it fits is the opposite
/// of what N3 asks for.
///
/// **CORRECTION, 2026-08-20.** This doc used to argue that raising the default
/// "was the alternative and is worse", because "a default nobody asked for that
/// costs every strict run more memory is a change to the shipping
/// configuration". That reasoning does not survive reading `Vec::push`: the
/// sink's memory is proportional to the observations it actually holds, not to
/// its ceiling, so raising the ceiling costs a run that never reaches it exactly
/// nothing. The default is now 4096 — see [`JDK_ONLY_NATIVE_SHADOW_CAP`] — and
/// this override remains, for the census run that wants a bigger sink still.
/// The contract §2 point about process globals is untouched by either: the
/// global existed before this constant did and is unaffected by its value.
///
/// Read once, at the first observation. `CRATONVM_NATIVE_SHADOW_SINK_CAP=0`, an
/// empty or a non-numeric value leaves the default in place — a diagnostic must
/// never be the thing that fails, and a cap of zero would silently report an
/// empty population as a complete one, which is the exact failure this whole
/// area exists to remove.
///
/// **CORRECTION, 2026-08-21 (WORKER-5).** A value ABOVE
/// [`JDK_ONLY_NATIVE_SHADOW_CAP_MAX`] used to fall back to the default too, so
/// `…=200000` produced 4096 — smaller than the ceiling it was trying to exceed
/// — and printed nothing. That is self-inflicted: `vm-cli`'s own saturation
/// advice prints `(recorded + dropped) * 2`, which exceeds 65,536 for any
/// workload with more than ~32,768 shadows. Such a value is now CLAMPED to the
/// ceiling, and every rejected or adjusted value prints one `[cratonvm]` line.
/// See [`cratonvm_types::flags::resolve_capped_usize`].
///
/// **The filter is deliberately NOT resized with it.**
/// [`JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS`] stays 512, so above that the filter
/// stops absorbing most repeats and each new triple costs one extra mutex
/// acquisition on a cold path. That is a throughput cost on a census run, paid
/// only by a run that asked for a bigger sink, and `Vec::contains` keeps the
/// buffer correct regardless — the filter's own doc says correctness never
/// depends on it.
pub fn jdk_only_native_shadow_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        // `runtime_var`, not `std::env::var`: the flag is DECLARED
        // (`flag_groups::INVENTORY`, `CRATONVM_DBG=native-shadow-sink-cap`), and a
        // declared name read through bare `getenv` comes from a different source
        // than the latched snapshot every other knob is served from ---
        // `types/tests/flag_declaration_guard.rs` documents that split, and it is
        // the reason an undeclared read site is a defect rather than untidiness.
        cratonvm_types::flags::resolve_capped_usize(
            "CRATONVM_NATIVE_SHADOW_SINK_CAP",
            "interpreter observation sink",
            JDK_ONLY_NATIVE_SHADOW_CAP,
            JDK_ONLY_NATIVE_SHADOW_CAP_MAX,
        )
    })
}

/// Slots in the lock-free "already recorded" filter. A power of two so the
/// index is a mask, and larger than the cap so a saturated buffer still
/// answers most repeats from the filter.
pub(in crate::vm::vm_exec) const JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS: usize = 512;

/// Snapshot of the structured `NativeShadowsBytecode` observations the
/// interpreter's dispatch resolver recorded this run. Read when building
/// `--real-jdk-report`; never hot.
///
/// Disjoint from `cratonvm_jit::jdk_only_jit_violations()` (compile-time
/// refusals) and from `crate::jit::jdk_only_jit_helper_violations()` (runtime
/// fast-path refusals). All three belong in the report.
pub fn jdk_only_native_shadow_observations(
    sink: &NativeShadowSink,
) -> Vec<cratonvm_types::error::JdkOnlyViolation> {
    let recorded = sink.rows.lock();
    recorded.as_slice().to_vec()
}

/// Exact number of times bytecode won over a registered non-intrinsic native
/// on a dispatch path resolved here. Feeds
/// `cratonvm_real_bytecode_shadow_attempt_total` alongside the JIT's count.
///
/// Always `>=` the length of [`jdk_only_native_shadow_observations`], which is
/// deduplicated and capped.
pub fn jdk_only_native_shadow_attempts(sink: &NativeShadowSink) -> u64 {
    sink.attempts.load(std::sync::atomic::Ordering::Relaxed)
}

/// Did the bounded observation sink SATURATE -- i.e. was at least one distinct
/// observation dropped for want of room?
///
/// The one question `--real-jdk-report`'s violation list could not answer about
/// itself. `violations[]` is one row per distinct triple, capped at
/// [`JDK_ONLY_NATIVE_SHADOW_CAP`], and a truncated list is identical in SHAPE to
/// a complete one -- so a reader who took the list as the population got a floor
/// and could not tell. G60-1 §4 states the hazard exactly: *a saturated buffer
/// looks exactly like a complete one from the JSON*.
///
/// **Not `recorded.len() == CAP`.** That test is wrong in the one place it
/// matters: a run whose last distinct observation is the 256th fills the buffer
/// exactly and drops nothing, and would be reported truncated. The flag below is
/// set by [`offer_native_shadow_observation`] only when an offer arrives and
/// finds no room, so it means "something WAS dropped", which is the fact a
/// reader needs. Reading it costs one relaxed load and is never hot.
///
/// Once it is true the sink's LIST teaches nothing further -- but, since
/// 2026-08-20, the counting does not stop with it. See
/// `NativeShadowSink::dropped` and
/// [`jdk_only_shadow_already_observed`]: the producer keeps discovering
/// distinct triples past saturation, so `recorded + dropped` is the population
/// rather than `recorded` alone.
///
/// **One bit is not enough, which is why this is no longer the only signal.**
/// A boolean says the list is short; it cannot say by how much, and a reader
/// who saw it had no way to turn a floor into a number. Read it together with
/// [`jdk_only_native_shadow_sink_dropped`], which is what the report's
/// `observation_sink.dropped` carries.
pub fn jdk_only_native_shadow_sink_saturated(sink: &NativeShadowSink) -> bool {
    sink.full.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many distinct observations were offered to the sink and found no room.
///
/// Zero on any run that did not saturate — and a run that reports non-zero here
/// is telling you `violations[]` is short by about this much. See
/// `NativeShadowSink::dropped` for what "about" means and why it is not
/// exact.
///
/// The fix for a non-zero value is `CRATONVM_NATIVE_SHADOW_SINK_CAP=<bigger>`,
/// not narrowing the workload — narrowing the workload is what made every
/// figure in `docs/known-issues/jdk-only/` a floor in the first place.
pub fn jdk_only_native_shadow_sink_dropped(sink: &NativeShadowSink) -> u64 {
    sink.dropped.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many distinct observations the sink is holding right now.
///
/// The denominator for [`jdk_only_native_shadow_sink_saturated`], and cheaper
/// than [`jdk_only_native_shadow_observations`] for a caller that only wants the
/// count: it clones nothing. Both recorders share this one sink -- the
/// bytecode-won rows and the `bridge-ran-over-bytecode` rows -- so this is the
/// number to compare against [`JDK_ONLY_NATIVE_SHADOW_CAP`], not the count of
/// either kind on its own.
pub fn jdk_only_native_shadow_sink_len(sink: &NativeShadowSink) -> usize {
    sink.rows.lock().len()
}

/// Has this triple already been offered to the observation buffer?
///
/// Exposed so a caller that must do real work (a class-manager read lock and a
/// hierarchy walk) to *discover* a shadow can skip that work once the shadow is
/// recorded. Advisory in both directions — a filter collision answers `false`
/// for a triple that was recorded, which costs one redundant walk and one
/// redundant offer that the buffer dedups. Never a correctness input.
///
/// # The saturation short-circuit that used to live here, and why it is gone
///
/// Until 2026-08-20 this function opened with
/// `if JDK_ONLY_NATIVE_SHADOW_FULL { return true; }` — once the sink filled,
/// every triple answered "already seen", the caller stopped walking, and the
/// unenforced-shadow counter froze (that counter is gone since t1 wave 6, lane
/// Y3: bytecode-first dispatch left it no recorder). That single line is why every
/// shadow figure in `docs/known-issues/jdk-only/` is a floor: the instrument
/// stopped measuring at the moment it ran out of room to *name* what it
/// measured, and conflated the two.
///
/// It is now answered from the filter in both states.
/// [`offer_native_shadow_observation`] publishes the digest of a DROPPED triple
/// as well as a retained one, so a saturated run still answers `true` for
/// everything it has already seen and pays no repeat walks — it only pays one
/// walk for each genuinely new triple, which is the same cost profile an
/// unsaturated run pays for its first [`jdk_only_native_shadow_cap`] triples.
///
/// **The residual cost, stated rather than hidden.** The filter is only
/// [`JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS`] wide and is deliberately not resized
/// with the cap, so a run whose distinct-shadow population greatly exceeds that
/// will evict entries and re-walk them. The remedy is to raise
/// `CRATONVM_NATIVE_SHADOW_SINK_CAP` so the sink never saturates at all; a
/// wider filter is nominated, not done.
pub fn jdk_only_shadow_already_observed(
    sink: &NativeShadowSink,
    class: &str,
    method: &str,
    descriptor: &str,
    kind_tag: &str,
) -> bool {
    use std::sync::atomic::Ordering;
    let digest = jdk_only_shadow_digest(class, method, descriptor, kind_tag);
    sink.filter[(digest as usize) & (JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS - 1)]
        .load(Ordering::Relaxed)
        == digest
}

/// FNV-1a over the triple plus the `native_kind` tag. Only ever computed under
/// `JdkOnly`, inside the `#[cold]` recorder.
///
/// Keyed on the tag STRING rather than on `NativeKind`, so the two recorders
/// below cannot alias: the same triple legitimately produces a
/// `"bridge"` row (bytecode won) and a `"bridge-ran-over-bytecode"` row (the
/// native won) in one run, and a digest that could not tell them apart would
/// silently drop the second.
pub(in crate::vm::vm_exec) fn jdk_only_shadow_digest(class: &str, method: &str, descriptor: &str, kind_tag: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    class.hash(&mut hasher);
    0xffu8.hash(&mut hasher);
    method.hash(&mut hasher);
    0xffu8.hash(&mut hasher);
    descriptor.hash(&mut hasher);
    0xffu8.hash(&mut hasher);
    kind_tag.hash(&mut hasher);
    let h = hasher.finish();
    // 0 is the filter's "empty" marker.
    if h == 0 {
        1
    } else {
        h
    }
}

/// Offer one structured observation to the bounded, deduplicated buffer.
///
/// The half of [`record_native_shadows_bytecode`] that is not the counter. (It
/// was split out so a second recorder, "the native won instead", could share
/// the identical filter, cap and dedup; that recorder was removed in t1 wave 6,
/// lane Y3, once bytecode-first dispatch left it no caller.)
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn offer_native_shadow_observation(
    sink: &NativeShadowSink,
    class: &str,
    method: &str,
    descriptor: &str,
    kind_tag: &'static str,
) {
    use std::sync::atomic::Ordering;

    // The filter is consulted FIRST, in both the full and the not-full state.
    // It is what keeps `NativeShadowSink::dropped` a count of distinct
    // triples rather than of dispatch events: a repeat of a triple already
    // offered — retained or dropped — returns here and increments nothing.
    let digest = jdk_only_shadow_digest(class, method, descriptor, kind_tag);
    let slot = &sink.filter[(digest as usize) & (JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS - 1)];
    if slot.load(Ordering::Relaxed) == digest {
        return;
    }

    // Saturated: count it and publish the digest anyway. Publishing is what
    // makes `jdk_only_shadow_already_observed` able to stop the caller
    // re-walking this triple now that it no longer short-circuits on the full
    // flag, and it is why this counter does not run away.
    if sink.full.load(Ordering::Relaxed) {
        sink.dropped.fetch_add(1, Ordering::Relaxed);
        slot.store(digest, Ordering::Relaxed);
        return;
    }

    let violation = cratonvm_types::error::JdkOnlyViolation::NativeShadowsBytecode {
        class: class.to_string(),
        method: method.to_string(),
        descriptor: descriptor.to_string(),
        native_kind: kind_tag,
        // Deliberately not resolved here. This runs on the strict dispatch
        // path; a class-store read lock and a hierarchy walk per observation is
        // exactly the cost the `#[cold]` split and the digest filter exist to
        // keep off it. `--real-jdk-report` and `--trace-jdk-only` both hold the
        // store and both fill it in with `with_class_origin`.
        class_origin: None,
    };
    let mut recorded = sink.rows.lock();
    if recorded.len() >= jdk_only_native_shadow_cap() {
        // The transition into saturation. `FULL` is the flag every other reader
        // consults; this offer is itself the first drop, so it counts too —
        // otherwise a report could say `truncated: true, dropped: 0`, which is
        // a contradiction a reader would have to resolve by guessing.
        sink.full.store(true, Ordering::Relaxed);
        sink.dropped.fetch_add(1, Ordering::Relaxed);
        slot.store(digest, Ordering::Relaxed);
        return;
    }
    if !recorded.contains(&violation) {
        recorded.push(violation);
    }
    // Published last: a reader that sees the digest is guaranteed the triple
    // has already been OFFERED to the buffer — which since 2026-08-20 means
    // "retained or counted as dropped", not "retained".
    slot.store(digest, Ordering::Relaxed);
}

/// Record one intrinsic dispatch that stood in front of concrete bytecode and
/// was exempted from the §1.4 census.
///
/// **Only call this under `JdkOnly`, and only when the caller has ESTABLISHED
/// that concrete bytecode exists.** A site that has not established it passes
/// nothing here, exactly as it records nothing in the shadow sink — an
/// unestablished premise would turn this number into a count of intrinsic
/// dispatches, which is a different and far larger population.
#[cold]
#[inline(never)]
pub(crate) fn record_intrinsic_shadow_exempt(
    sink: &NativeShadowSink,
    class: &str,
    method: &str,
    descriptor: &str,
) {
    use std::sync::atomic::Ordering;
    sink.exempt_events.fetch_add(1, Ordering::Relaxed);

    if sink.exempt_full.load(Ordering::Relaxed) {
        sink.exempt_dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let digest = jdk_only_shadow_digest(class, method, descriptor, "intrinsic-census-exempt");
    let slot = &sink.exempt_filter[(digest as usize) & (JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS - 1)];
    if slot.load(Ordering::Relaxed) == digest {
        return;
    }
    let triple = (
        class.to_string(),
        method.to_string(),
        descriptor.to_string(),
    );
    let mut recorded = sink.exempt_triples.lock();
    if recorded.len() >= jdk_only_native_shadow_cap() {
        sink.exempt_full.store(true, Ordering::Relaxed);
        sink.exempt_dropped.fetch_add(1, Ordering::Relaxed);
        slot.store(digest, Ordering::Relaxed);
        return;
    }
    if !recorded.contains(&triple) {
        recorded.push(triple);
    }
    slot.store(digest, Ordering::Relaxed);
}

/// Event count of [`record_intrinsic_shadow_exempt`] — one per dispatch, not
/// per triple, the same polarity as
/// [`jdk_only_native_shadow_attempts`].
pub fn jdk_only_intrinsic_shadow_exempt_events(sink: &NativeShadowSink) -> u64 {
    sink.exempt_events.load(std::sync::atomic::Ordering::Relaxed)
}

/// The distinct triples the exemption covered this run, capped.
pub fn jdk_only_intrinsic_shadow_exempt_observations(
    sink: &NativeShadowSink,
) -> Vec<(String, String, String)> {
    sink.exempt_triples.lock().as_slice().to_vec()
}

/// Distinct triples the exempt sink had no room for. Same upper-bound caveat as
/// [`jdk_only_native_shadow_sink_dropped`].
pub fn jdk_only_intrinsic_shadow_exempt_dropped(sink: &NativeShadowSink) -> u64 {
    sink.exempt_dropped.load(std::sync::atomic::Ordering::Relaxed)
}

/// Did the exempt sink saturate?
pub fn jdk_only_intrinsic_shadow_exempt_saturated(sink: &NativeShadowSink) -> bool {
    sink.exempt_full.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record one "concrete bytecode won over a registered non-intrinsic native"
/// observation.
///
/// `#[cold] #[inline(never)]`, mirroring the reject helpers above: the counter
/// increment and the filter probe are cheap, but they live off the dispatch
/// path with the `String` machinery rather than inline in it.
///
/// **Only call this under `JdkOnly`.** The caller owns that test so
/// `Compatible` never even reaches the call instruction.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn record_native_shadows_bytecode(
    sink: &NativeShadowSink,
    class: &str,
    method: &str,
    descriptor: &str,
    kind: cratonvm_native_api::NativeKind,
) {
    sink.attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    offer_native_shadow_observation(sink, class, method, descriptor, kind.as_str());
}

/// THE single native-vs-bytecode decision point (§7). Interpreter, JIT,
/// reflection, JNI and method handles route through this (or, where the site
/// has no resolved `&Method`, through [`resolve_native_dispatch_wave1`], which
/// takes the identical decision from the names).
///
/// Normative order (§7), plus one deliberate deviation documented below:
///
/// 1. `method.is_native()` → the registered bridge or intrinsic. Under
///    `JdkOnly` a `SyntheticStub` is `Reject(SyntheticNativeInvocation)`;
///    nothing registered is `Reject(MissingNative)`. Under `Compatible` every
///    kind is accepted, exactly as today.
/// 2. a registered `Intrinsic` wins next — reviewed and semantics-preserving,
///    so it may shadow bytecode (§1.4's single exception).
/// 3. `method.code().is_some()` → `Bytecode`. **Concrete bytecode beats a
///    registered `Bridge` or `SyntheticStub`.** This is the whole point of the
///    mode: a fake must not stand in front of correct real bytes.
/// 4. otherwise → `Reject(MissingImplementation)`.
///
/// # Deliberate deviation from the literal §7 order
///
/// Between steps 3 and 4 this function inserts: *a method with no `Code` but a
/// registered native dispatches to that native.* The literal order sends every
/// `Code`-less method straight to `Reject(MissingImplementation)`, which would
/// reject every interface-level bridge the VM boots on — `Path.getFileSystem`,
/// `Iterator.hasNext` on a synthetic wrapper, `Enumeration.hasMoreElements`,
/// the whole `FileSystemProvider` surface. Nothing would start in *either*
/// mode, so the deviation is not a strictness concession, it is the difference
/// between a resolver that can be turned on and one that cannot.
///
/// It does not weaken §1.4. §1.4 forbids a native **shadowing** bytecode, and
/// this branch is only reachable when `method.code()` is `None` — there is no
/// bytecode to shadow. `SyntheticStub` is still refused here under `JdkOnly`,
/// so §1.3 is enforced on this branch too.
///
/// **Do not "fix" this to match §7's literal wording.** It is a reviewed,
/// justified, self-documented deviation, and reverting it stops the VM booting
/// in both modes — which makes it a very fast way to discover that the wording
/// was the thing that was wrong. The wave-2 record that adjudicated it (§11a)
/// is retired; this banner is the surviving statement.
///
/// The right long-term shape is for the abstract declaration to resolve to the
/// *implementing* class's method before reaching here, at which point step 3
/// covers it and this branch can go. That is a **resolution-order** change with
/// no defect behind it — nothing observable is wrong today — so it is an
/// architectural cleanup and not a bug to be scheduled.
///
/// This spelling records its §1.4 observations into
/// [`process_native_shadow_sink`], which no VM's report reads: it is for
/// callers with no VM in hand (the policy-matrix tests). A door inside a VM
/// calls [`resolve_dispatch_in`] with its VM's
/// `NativeRealm::native_shadow_sink` (t1 wave 4, lane Y2).
pub fn resolve_dispatch<'a>(
    policy: cratonvm_types::compat::ExecutionPolicy,
    class: &crate::classloading::Class,
    method: &'a Method,
    native: Option<(NativeCallback, cratonvm_native_api::NativeKind)>,
) -> DispatchDecision<'a> {
    resolve_dispatch_in(process_native_shadow_sink(), policy, class, method, native)
}

/// [`resolve_dispatch`], recording into `sink` (the dispatching VM's
/// `NativeRealm::native_shadow_sink`, which its `--real-jdk-report` reads).
pub fn resolve_dispatch_in<'a>(
    sink: &NativeShadowSink,
    policy: cratonvm_types::compat::ExecutionPolicy,
    class: &crate::classloading::Class,
    method: &'a Method,
    native: Option<(NativeCallback, cratonvm_native_api::NativeKind)>,
) -> DispatchDecision<'a> {
    use cratonvm_native_api::NativeKind;

    let strict = policy.is_jdk_only();

    // Step 1 — ACC_NATIVE. There is no bytecode by definition, so the only
    // question is which registration answers, and whether policy admits it.
    if method.is_native() {
        return match native {
            Some((cb, NativeKind::Intrinsic)) => DispatchDecision::Intrinsic(cb),
            Some((_, NativeKind::SyntheticStub)) if strict => DispatchDecision::Reject(
                reject_synthetic_invocation(&class.name, &method.name, &method.descriptor),
            ),
            Some((cb, _)) => DispatchDecision::NativeBridge(cb),
            None => DispatchDecision::Reject(reject_missing_native(
                &class.name,
                &method.name,
                &method.descriptor,
                // §1.7 / `cratonvm_missing_native_total{module}`: the owning
                // JDK module, straight off the declaring class's origin.
                declaring_module(class),
            )),
        };
    }

    // Step 2 — a reviewed intrinsic may shadow concrete bytecode in both
    // runtime policies (unless the opt-in `CRATONVM_PURE_BYTECODE` probe
    // mode yields it).
    // Application-level intrinsics must never shadow concrete bytecode of
    // classes defined by a user-defined class loader (such as historical library
    // jars loaded in isolated ClassLoaders).
    let user_loader_bytecode = method.code().is_some()
        && matches!(class.loader_id, cratonvm_types::ClassLoaderId::UserDefined(_))
        && !crate::runtime::interpreter::constants::is_global_resolution_namespace(&class.name);
    let pure = (strict
        && method.code().is_some()
        && cratonvm_native_api::unretire::pure_bytecode_for(&class.name, &method.name))
        || user_loader_bytecode;
    if let (false, Some((cb, NativeKind::Intrinsic))) = (pure, native) {
        // …and when it does, SAY SO. This return is one of the two reasons
        // every §1.4 figure is a floor by construction: step 3 below is the
        // only recorder, and step 2 consumes every `Intrinsic` before it. The
        // exemption stays — the intrinsic still wins — but it is now counted,
        // in its own sibling block. `method.code().is_some()` is the same test
        // step 3 makes, and it is what keeps this a count of SHADOWS rather
        // than a count of intrinsic dispatches.
        if method.code().is_some() {
            if strict {
                record_intrinsic_shadow_exempt(sink, &class.name, &method.name, &method.descriptor);
            }
        }
        return DispatchDecision::Intrinsic(cb);
    }

    // Step 3 — real class bytes are authoritative.
    if method.code().is_some() {
        // …and if something was nevertheless registered for this triple, that
        // registration is a native standing where real code exists: §1.4's
        // `NativeShadowsBytecode`. It loses, so this is an ACCEPT, not a
        // reject — but it is the over-tagged-registration evidence wave 2
        // needs, and until now only the JIT ever recorded it.
        //
        // Step 2 above has already consumed every `Intrinsic`, so anything
        // still here is a `Bridge` or a `SyntheticStub` — precisely the two
        // kinds §1.4 forbids shadowing bytecode.
        //
        // `strict` is the field read taken at the top of this function; in
        // `Compatible` mode this is one not-taken branch and nothing else.
        if strict {
            if let Some((_, kind)) = native {
                record_native_shadows_bytecode(
                    sink,
                    &class.name,
                    &method.name,
                    &method.descriptor,
                    kind,
                );
            }
        }
        return DispatchDecision::Bytecode(method);
    }

    // Documented deviation (see the banner above): no `Code`, but something is
    // registered. Nothing is being shadowed, so run it — minus stubs under
    // `JdkOnly`, which stay refused.
    if let Some((cb, kind)) = native {
        if strict && kind == NativeKind::SyntheticStub {
            return DispatchDecision::Reject(reject_synthetic_invocation(
                &class.name,
                &method.name,
                &method.descriptor,
            ));
        }
        return DispatchDecision::NativeBridge(cb);
    }

    // Step 4 — nothing to run.
    DispatchDecision::Reject(reject_missing_implementation(
        &class.name,
        &method.name,
        &method.descriptor,
    ))
}

/// Apply the bytecode-first resolver at by-name native doors once the class
/// is loaded. The registration's callback may be specialized, but its kind
/// cannot bypass a concrete method merely because this door has only a name.
pub(crate) fn registered_native_yields_to_loaded_bytecode(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: NativeCallback,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    registered_native_yields_to_bytecode_on_owner(
        shared,
        None,
        class_name,
        method_name,
        descriptor,
        callback,
        kind,
    )
}

/// [`registered_native_yields_to_loaded_bytecode`] for a door that already
/// holds the declaring class's id (a `CachedBytecodeMethod`'s
/// `declaring_class_id`): selection starts at that class, not at whatever
/// class of the name the class manager finds first, so a same-named class of
/// another loader can never answer. Interpreter round i7 wave 139, lane L2
/// (`i138-L2-virtual-door-tier-up-reads-registry-presence-not-the-resolver`).
pub(crate) fn registered_native_yields_to_bytecode_of(
    shared: &SharedVm,
    owner: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: NativeCallback,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    registered_native_yields_to_bytecode_on_owner(
        shared,
        Some(owner),
        class_name,
        method_name,
        descriptor,
        callback,
        kind,
    )
}

/// [`registered_native_yields_to_loaded_bytecode`] decided on `resolved_owner`
/// (the class a door's call site actually resolved, by id) when it has one,
/// so two loaders' classes of one name cannot be confused; `None` falls back
/// to the loader-blind name table. The JIT native site cache's fill passes
/// the site's loader-faithful owner (zt t29/jd,
/// `CRATONVM_JIT_NATIVE_SITE_OWNER_BY_ID`).
pub(crate) fn registered_native_yields_to_bytecode_on_owner(
    shared: &SharedVm,
    resolved_owner: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: NativeCallback,
    kind: cratonvm_native_api::NativeKind,
) -> bool {
    // Some cache-admission callers already hold this guard. A recursive read
    // cannot deadlock against a queued class-loading writer in that case.
    let cm = shared.classes.class_manager.read_recursive();
    selected_body_dispatches_bytecode(
        shared,
        &cm,
        resolved_owner,
        class_name,
        method_name,
        descriptor,
        Some((callback, kind)),
    )
}

/// The one walk behind [`registered_native_yields_to_bytecode_on_owner`] and
/// [`registered_bridge_yields_to_loaded_bytecode_census_free`]: select the
/// method the registration's class (or `resolved_owner`) answers for the
/// triple and ask [`resolve_dispatch`] whether that body is what runs.
pub(in crate::vm::vm_exec) fn selected_body_dispatches_bytecode(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    resolved_owner: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    native: Option<(NativeCallback, cratonvm_native_api::NativeKind)>,
) -> bool {
    let Some(cid) = resolved_owner.or_else(|| cm.get_loaded_class_id(class_name)) else {
        return false;
    };
    let Some((method, declaring_id)) =
        crate::classloading::find_method_recursive(cid, method_name, descriptor, &cm.class_store)
    else {
        return false;
    };
    let Some(class) = cm.get_class(declaring_id) else {
        return false;
    };
    matches!(
        resolve_dispatch_in(
            &shared.natives.native_shadow_sink,
            dispatch_policy(shared),
            class,
            method,
            native
        ),
        DispatchDecision::Bytecode(_)
    )
}

/// JIT round 21 (lane refs221, REFS221-3; handoff
/// `r21-HANDOFF-next-jit-round-20261015.md` section 6 item 7): the JIT gates'
/// question "does a `Bridge` registered on `class_name` lose to the selected
/// method's bytecode at dispatch?", answered by the SAME walk and the SAME
/// [`resolve_dispatch`] the by-name doors use
/// ([`registered_native_yields_to_loaded_bytecode`]), without the dispatch
/// census.
///
/// The census is the only difference. `resolve_dispatch` records
/// `record_native_shadows_bytecode` at step 3 when a registration is passed
/// (it counts DISPATCHES; a compile gate asking would inflate it), so this
/// passes none. For a `Bridge` that changes no verdict: step 1 (ACC_NATIVE)
/// answers a native side either way, step 2 consumes only an `Intrinsic`,
/// step 3 answers `Bytecode` whenever the method has `Code`, and the
/// documented no-`Code` deviation answers a native side or a reject, never
/// `Bytecode`. So the answer here equals the by-name doors' answer for every
/// `Bridge`; the caller owns the `Bridge` / mode / switch terms
/// (`native_override::bridge_yields_to_loaded_bytecode_for_gates`).
///
/// `cm`: the caller's class-manager guard when it holds one; otherwise a
/// recursive read is taken (callers may hold a guard they do not pass).
pub(crate) fn registered_bridge_yields_to_loaded_bytecode_census_free(
    shared: &SharedVm,
    cm: Option<&crate::classloading::ClassManager>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    match cm {
        Some(cm) => selected_body_dispatches_bytecode(
            shared,
            cm,
            None,
            class_name,
            method_name,
            descriptor,
            None,
        ),
        None => {
            let cm = shared.classes.class_manager.read_recursive();
            selected_body_dispatches_bytecode(
                shared,
                &cm,
                None,
                class_name,
                method_name,
                descriptor,
                None,
            )
        }
    }
}
/// The table is a dispatch optimization, not a second registration policy.
/// Resolve its callback with the live slot's kind and the selected Java body.
/// Builder callbacks without a live slot are legacy bridges; the table's other
/// built-in entries have their own reviewed intrinsic implementations.
pub(crate) fn table_intrinsic_admitted(
    shared: &SharedVm,
    class: &crate::classloading::Class,
    method: &crate::vm::Method,
    intrinsic: cratonvm_native_api::InterpIntrinsic,
) -> bool {
    use cratonvm_native_api::NativeKind;
    let registry = &shared.natives.native_methods;
    let kind = registry
        .resolve_id(&class.name, &method.name, &method.descriptor)
        .and_then(|id| registry.kind_of_id(id))
        .unwrap_or_else(|| {
            if cratonvm_native_builtins::intrinsics::is_string_builder_entry(intrinsic) {
                NativeKind::Bridge
            } else {
                NativeKind::Intrinsic
            }
        });
    crate::vm::resolve_dispatch_in(
        &shared.natives.native_shadow_sink,
        crate::vm::dispatch_policy(shared),
        class,
        method,
        Some((
            cratonvm_native_builtins::intrinsics::callback_for(intrinsic),
            kind,
        )),
    )
    .native_callback()
    .is_some()
}

/// Registration-only adapter for dispatch sites that do not carry the resolved
/// `&Class` and `&Method` into this function. When a caller has already
/// established concrete bytecode, Bridge and SyntheticStub registrations yield
/// in both runtime policies; only a reviewed Intrinsic can shadow that code.
/// Without that fact, the caller's `compat_native_wins` verdict still controls
/// whether a name-only registration is considered.
///
/// `bytecode_available` is the resolved-method fact when a door has it. Treating
/// it consistently here keeps the shared adapter aligned with `resolve_dispatch`
/// and prevents a warm or compatibility-only path from bypassing the contract.
///
/// Returns `None` for "no opinion / bytecode wins" and therefore preserves the
/// existing caller fallback. `'static` is only a return-type convenience: this
/// adapter never returns a borrowed `Bytecode` decision.
///
/// Like [`resolve_dispatch`], this spelling records into
/// [`process_native_shadow_sink`]; a door inside a VM calls
/// [`resolve_native_dispatch_wave1_in`] with its VM's sink.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn resolve_native_dispatch_wave1(
    door: DispatchDoor,
    policy: cratonvm_types::compat::ExecutionPolicy,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    native: Option<(NativeCallback, cratonvm_native_api::NativeKind)>,
    compat_native_wins: bool,
    bytecode_available: bool,
) -> Option<DispatchDecision<'static>> {
    resolve_native_dispatch_wave1_in(
        process_native_shadow_sink(),
        door,
        policy,
        class_name,
        method_name,
        descriptor,
        native,
        compat_native_wins,
        bytecode_available,
    )
}

/// [`resolve_native_dispatch_wave1`], recording into `sink` (the dispatching
/// VM's `NativeRealm::native_shadow_sink`; t1 wave 4, lane Y2).
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn resolve_native_dispatch_wave1_in(
    sink: &NativeShadowSink,
    // Measurement only: which dispatch door is asking. Nothing in this
    // function's decision reads it -- see [`DispatchDoor`] for why every
    // door nevertheless has to be able to say its own name.
    door: DispatchDoor,
    policy: cratonvm_types::compat::ExecutionPolicy,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    native: Option<(NativeCallback, cratonvm_native_api::NativeKind)>,
    compat_native_wins: bool,
    bytecode_available: bool,
) -> Option<DispatchDecision<'static>> {
    use cratonvm_native_api::NativeKind;

    // No registration: nothing to decide. Deliberately NOT `MissingNative` —
    // these sites have not established that the method is `ACC_NATIVE`, so an
    // absent registration here just means "no override", the same thing it
    // meant before §7 existed.
    //
    // This is also why the `module` half of §1.7 is absent from this adapter
    // and only `resolve_dispatch` fills it in: the owning JPMS module lives on
    // the declaring class's `ClassOrigin::BootImage`, and this function has a
    // name triple, not a `&Class`. Looking one up would mean a class-manager
    // read lock and a hierarchy walk — the exact cost this adapter exists to
    // avoid — on a path that does not emit a `MissingNative` anyway. If a
    // future `MissingNative` is ever raised from here, its `module` must stay
    // `None` unless the CALLER (which usually does hold a class) passes it in.
    let (callback, kind) = native?;

    // `H17-3` N1's per-door counter.
    note_dial_door(
        door,
        policy,
        class_name,
        method_name,
        descriptor,
        kind,
        bytecode_available,
    );

    // Resolved bytecode is authoritative in both policies. A reviewed
    // Intrinsic is the only registration permitted to shadow its Code.
    if bytecode_available && kind != NativeKind::Intrinsic {
        if policy.is_jdk_only() {
            record_native_shadows_bytecode(sink, class_name, method_name, descriptor, kind);
        }
        return None;
    }

    if !compat_native_wins {
        // The site preferred bytecode on its own. A registration exists for a
        // triple the site has told us has real bytecode, so this is §1.4's
        // `NativeShadowsBytecode` — recorded, not refused, because the right
        // implementation already won. `bytecode_available` is what makes the
        // claim true rather than assumed: a site that has not established the
        // bytecode fact passes `false` and records nothing.
        //
        // `is_jdk_only()` is tested FIRST so `Compatible` short-circuits on a
        // `Copy` field read and the documented "a default `--fast-boot` run
        // cannot observe that the resolver exists" property still holds: the
        // added cost on this branch is one register comparison, no atomic, no
        // hash, no allocation, and the observation sink stays untouched.
        if policy.is_jdk_only() && bytecode_available {
            if kind == NativeKind::Intrinsic {
                // The second half of the same exemption as `resolve_dispatch`
                // step 2. It is reached on the door that PREFERRED bytecode, so
                // the intrinsic did not even run here — but it is still a
                // registered intrinsic standing over real bytes, which is the
                // population `WORKER-3-NOTE-5` asked to have published.
                record_intrinsic_shadow_exempt(sink, class_name, method_name, descriptor);
            } else {
                record_native_shadows_bytecode(sink, class_name, method_name, descriptor, kind);
            }
        }
        return None;
    }

    // Opt-in `CRATONVM_PURE_BYTECODE` probe: real bytes beat every kind here.
    if bytecode_available
        && policy.is_jdk_only()
        && cratonvm_native_api::unretire::pure_bytecode_for(class_name, method_name)
    {
        record_native_shadows_bytecode(sink, class_name, method_name, descriptor, kind);
        return None;
    }

    if !policy.is_jdk_only() {
        // Compatible: the kind is irrelevant, exactly as it was when these
        // sites called bare `find` and threw the kind away.
        return Some(match kind {
            NativeKind::Intrinsic => DispatchDecision::Intrinsic(callback),
            _ => DispatchDecision::NativeBridge(callback),
        });
    }

    match kind {
        // §1.3 — a fake may not be invoked under `--real-jdk`. This one IS a
        // refusal, and the caller turns it into `VmError::JdkOnly`, so it is
        // counted as a `SyntheticNativeInvocation` there rather than here.
        NativeKind::SyntheticStub => Some(DispatchDecision::Reject(reject_synthetic_invocation(
            class_name,
            method_name,
            descriptor,
        ))),
        // §1.4 — the reviewed exception; may shadow bytecode. Counted when it
        // actually does, for the reason given on
        // [`record_intrinsic_shadow_exempt`]: the exemption is correct and its
        // SIZE is what nothing published.
        NativeKind::Intrinsic => {
            if bytecode_available {
                record_intrinsic_shadow_exempt(sink, class_name, method_name, descriptor);
            }
            Some(DispatchDecision::Intrinsic(callback))
        }
        // §7 step 3 — concrete bytecode beats a bridge. The bridge lost, so
        // this is the other half of the §1.4 observation: the site would have
        // run a registered native in front of real bytes and strict mode sent
        // it to the bytecode instead.
        NativeKind::Bridge if bytecode_available => {
            record_native_shadows_bytecode(sink, class_name, method_name, descriptor, kind);
            None
        }
        NativeKind::Bridge => Some(DispatchDecision::NativeBridge(callback)),
    }
}

/// What a native method that has no implementation under its own name links
/// to under JVMTI `SetNativeMethodPrefix` ([`prefixed_native_link`]).
pub(in crate::vm::vm_exec) enum PrefixedNativeLink {
    /// The VM's registered implementation of the unprefixed name.
    Registry(NativeCallback),
    /// A JNI function bound to the unprefixed name (`RegisterNatives`, or a
    /// library symbol).
    Jni(usize),
}

/// JVMTI `SetNativeMethodPrefix`'s retry (interpreter round i1 wave 44, lane
/// L3): the native method `class_name.method_name descriptor` found no
/// implementation under its own name, so remove the prefixes this VM's agents
/// set (`instrument::native_method_prefixes`), the last-applied first, each
/// only where the name still starts with it, and, when the resulting name
/// names a method of the class with the same descriptor that is NOT native
/// (the agent's wrapper), link to the implementation of that name: the VM's
/// registered one (under the same policy as any registered native), else,
/// when `jni_allowed`, a JNI function bound to it. `None` when no prefix
/// applies or nothing is bound. Read only where a native call has already
/// failed to link: one read of the per-VM transformer chain, nothing on any
/// linked call. `CRATONVM_DBG_RETRANSFORM` prints one
/// `[instrument] native prefix link:` line per link made.
pub(in crate::vm::vm_exec) fn prefixed_native_link(
    shared: &SharedVm,
    declaring_class_id: ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    jni_allowed: bool,
) -> Result<Option<PrefixedNativeLink>, MethodCallFailed> {
    #[cfg_attr(not(feature = "experimental-debug"), allow(unused_mut))]
    let mut prefixes = crate::runtime::instrument::native_method_prefixes(shared.vm_identity);
    // Interpreter round i2 wave 63, lane L1: the C JVMTI envs' prefixes
    // (`SetNativeMethodPrefix(es)`), every env's in env order, ahead of the
    // `java.lang.instrument` agents' (HotSpot lists all its envs').
    #[cfg(feature = "experimental-debug")]
    {
        let mut all = crate::jvmti::native_env::all_native_method_prefixes(shared);
        if !all.is_empty() {
            all.append(&mut prefixes);
            prefixes = all;
        }
    }
    if prefixes.is_empty() {
        return Ok(None);
    }
    let mut wrapper_name = method_name;
    for prefix in prefixes.iter().rev() {
        if let Some(rest) = wrapper_name.strip_prefix(prefix.as_str()) {
            wrapper_name = rest;
        }
    }
    if wrapper_name.len() == method_name.len() || wrapper_name.is_empty() {
        return Ok(None);
    }
    let wrapper_is_java = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(declaring_class_id)
            .and_then(|class| class.find_method(wrapper_name, descriptor))
            .is_some_and(|wrapper| !wrapper.is_native())
    };
    if !wrapper_is_java {
        return Ok(None);
    }
    let dbg = false;
    if let Some((callback, kind)) =
        shared
            .natives
            .native_methods
            .find_with_kind(class_name, wrapper_name, descriptor)
    {
        match resolve_native_dispatch_wave1_in(
            &shared.natives.native_shadow_sink,
            DispatchDoor::ClassSharedNative,
            dispatch_policy(shared),
            class_name,
            wrapper_name,
            descriptor,
            Some((callback, kind)),
            true,
            false,
        ) {
            Some(DispatchDecision::Reject(violation)) => {
                return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
            }
            Some(decision) => {
                if let Some(callback) = decision.native_callback() {
                    check_native_dispatch_capability(shared, class_name, wrapper_name, descriptor)?;
                    if dbg {
                        eprintln!(
                            "[instrument] native prefix link: {class_name}.{method_name}{descriptor} \
                             -> {wrapper_name} (registry)"
                        );
                    }
                    return Ok(Some(PrefixedNativeLink::Registry(callback)));
                }
            }
            None => {}
        }
    }
    if !jni_allowed {
        return Ok(None);
    }
    let bound =
        crate::native::jni::find_jni_native(&shared.natives, class_name, wrapper_name, descriptor)
            .or_else(|| {
                let loader_id = jni_loader_id_for_class(shared, declaring_class_id);
                crate::native::jni::resolve_jni_native_in_libraries(
                    &shared.natives,
                    loader_id,
                    class_name,
                    wrapper_name,
                    descriptor,
                )
            });
    if dbg && bound.is_some() {
        eprintln!(
            "[instrument] native prefix link: {class_name}.{method_name}{descriptor} \
             -> {wrapper_name} (jni)"
        );
    }
    Ok(bound.map(PrefixedNativeLink::Jni))
}

/// Loader namespace id used for loader-scoped lazy JNI symbol resolution.
#[inline]
pub(in crate::vm::vm_exec) fn jni_loader_id_for_class(shared: &SharedVm, class_id: ClassId) -> u32 {
    shared
        .classes
        .class_manager
        .read()
        .get_loader_id(class_id)
        .unwrap_or(cratonvm_types::ClassLoaderId::Application)
        .to_native_id()
}

/// The policy this VM is running under. One `Copy` field read; no lock, no
/// allocation, safe to call per dispatch.
#[inline]
pub fn dispatch_policy(shared: &SharedVm) -> cratonvm_types::compat::ExecutionPolicy {
    shared.config.execution_policy()
}

/// Dispatch-side capability gate: the coarse safety net under the per-call-site
/// gates in `native-builtins` / `native-io`.
///
/// # Why it exists
///
/// The per-call-site gates cover the natives someone has edited. This one
/// covers every native whose `(class, method)` is in
/// [`classify_native`](cratonvm_native_api::capability::classify_native) —
/// `ProcessBuilder.start`, `Runtime.exec`, `System.load*`, `Unsafe.*`, the FFM
/// `Linker`, the `java.io` file streams, the `java.net` sockets — *including the
/// ones whose implementation still has no gate of its own*. It can only ever
/// report [`Scope::Any`](cratonvm_native_api::Scope::Any): the arguments have
/// not been decoded here, so there is no path or host to name. An `Enforce`
/// deployment therefore has to hold the unscoped grant (`process-spawn:*`) for
/// a class of native to dispatch at all, and the per-call-site gate then makes
/// the scoped decision.
///
/// # Cost, on the path every native dispatch in the VM takes
///
/// Three early-outs, in this order:
///
/// 1. `capabilities()` — one `Option` discriminant test on a registry field.
///    This is the *only* cost when no policy is installed (an embedder that
///    builds its registry by hand, and every `NativeMethodRegistry::new`).
/// 2. `classify_native(class, method)` — a `match` on `class_name`, which
///    rustc lowers to a length switch plus a handful of `memcmp`s and answers
///    `None` for the ~3,100 natives that are not capability-relevant. This is
///    the *whole* cost of the installed-but-`Permissive` default for all but a
///    few dozen triples.
/// 3. For those few dozen, under `Permissive` only, a thread-local memo (see
///    below). `Audit` and `Enforce` skip it and take the full check every time.
///
/// No allocation, no lock, no atomic and no hashing on any of the three. In
/// particular there is no `NativeMethodId` resolution on the common path, which
/// is deliberate: `NativeMethodRegistry::check_dispatch_capability` wants an id,
/// these three dispatch sites reach their callback through `find_with_kind`,
/// which does not return one, and re-deriving it with `resolve_id` would pay a
/// *second* full 128-bit `(class, method, descriptor)` hash per dispatch — the
/// exact duplicate-hash cost `find_with_kind` was introduced to remove (see its
/// doc comment and the H2 `TestFileSystem.testConcurrent` profile). The id is
/// resolved only after step 2 has said this native is capability-relevant, in
/// the `#[cold]` half.
///
/// A `find_with_kind`-shaped registry lookup that also returned the
/// `NativeMethodId` would let step 2 collapse into the precomputed
/// `sensitive_slots` lookup; that is a `native-api` edit and is reported
/// separately.
///
/// # Why `Permissive` is memoized and what that trades away
///
/// `classify_native` maps **all** of `jdk/internal/misc/Unsafe` to
/// `RawMemory` — every `compareAndSetInt`, `getReferenceVolatile`, `park`,
/// every AQS and every j.u.c collection operation that lands in a native.
/// `CapabilitySet::check` takes a `parking_lot::Mutex` on the audit map, and
/// that map is **shared by every thread of the VM**. Recording per call would
/// put a single VM-wide mutex on the hottest native path in the interpreter —
/// not a per-call instruction cost but a serialization point, which is how a
/// throughput regression becomes a hang.
///
/// So under `Permissive` (and only under `Permissive`, which cannot refuse
/// anything) the first dispatch of each `CapabilityKind` for each policy on
/// each thread takes the full check, and later ones are a thread-local load, two
/// `usize` compares and a bit test. The trade is the same one
/// `native-builtins`' `gate_raw_memory` already documents and accepts
/// (`docs/security/capability-wiring.md` §4.4): the report still names the
/// capability, its scope and its first call site, and **under-reports `count`**.
/// `count` is the one number a least-privilege grant set does not depend on,
/// and `Audit` — the mode that exists to price an `Enforce` flip — counts every
/// call exactly.
///
/// The memo is keyed on `(vm_identity, policy address)`, so swapping a VM's
/// policy or creating a second VM invalidates it rather than inheriting a
/// neighbour's answer.
#[inline]
pub(in crate::vm::vm_exec) fn check_native_dispatch_capability(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Result<(), MethodCallFailed> {
    let registry = &shared.natives.native_methods;
    let Some(caps) = registry.capabilities() else {
        return Ok(());
    };
    let Some(kind) = cratonvm_native_api::capability::classify_native(class_name, method_name)
    else {
        return Ok(());
    };
    if caps.mode() == cratonvm_native_api::CapabilityMode::Permissive
        && permissive_dispatch_already_recorded(shared.vm_identity, caps, kind)
    {
        return Ok(());
    }
    check_native_dispatch_capability_cold(registry, kind, class_name, method_name, descriptor)
}

std::thread_local! {
    /// `(vm_identity, policy address, kinds already recorded)` for this thread.
    ///
    /// A `Cell` of a `Copy` payload: no `RefCell` borrow flag, no `Arc` clone,
    /// no allocation. `vm_identity` is never 0 (`NEXT_VM_IDENTITY` starts at 1),
    /// so the all-zero initial value cannot collide with a real VM.
    pub static PERMISSIVE_DISPATCH_MEMO: std::cell::Cell<(usize, usize, u16)> =
        const { std::cell::Cell::new((0, 0, 0)) };
}

/// Whether this thread has already recorded `kind` for this VM's current
/// policy — and mark it recorded if not.
///
/// Only ever consulted under [`CapabilityMode::Permissive`], where the answer
/// can only suppress a *counter*, never an authorization decision.
#[inline]
pub(in crate::vm::vm_exec) fn permissive_dispatch_already_recorded(
    vm_identity: usize,
    caps: &std::sync::Arc<cratonvm_native_api::CapabilitySet>,
    kind: cratonvm_native_api::CapabilityKind,
) -> bool {
    // `CapabilityKind` is `#[repr(u8)]` with 9 fieldless variants, so the
    // discriminant is a shift amount in 0..9 and the mask fits a `u16`.
    let bit = 1u16 << (kind as u8);
    let policy = std::sync::Arc::as_ptr(caps) as usize;
    PERMISSIVE_DISPATCH_MEMO.with(|memo| {
        let (memo_vm, memo_policy, mask) = memo.get();
        if memo_vm == vm_identity && memo_policy == policy {
            if mask & bit != 0 {
                return true;
            }
            memo.set((vm_identity, policy, mask | bit));
        } else {
            // Different VM, or this VM's policy was replaced: start over rather
            // than inherit the other one's answer.
            memo.set((vm_identity, policy, bit));
        }
        false
    })
}

/// [`check_native_dispatch_capability`] for a caller that already knows the
/// classification and the slot, and has no strings to spend on re-deriving
/// either.
///
/// The JIT's per-call-site native cache
/// (`jit::helpers::try_jit_site_cached_native_dispatch`) is that caller. It
/// resolved the site once, against the RECEIVER's runtime class — which is not
/// necessarily `JitInvokeInfo::class_name`, the constant-pool owner — and it
/// holds the `NativeMethodId`. Re-entering through the string form would
/// re-classify from the CP owner, and `java/io/InputStream` (unclassified) with
/// a `java/io/FileInputStream` receiver (`FileRead`) is exactly the shape where
/// that silently drops the gate. So the kind travels on the cache entry and the
/// decision is made from it, never re-derived.
///
/// Otherwise identical to the string form, arm for arm: the permissive-mode
/// thread memo first, then the slot's own precomputed classification, then the
/// `classify_native` answer as the fallback for a slot that carries none.
pub(crate) fn check_native_dispatch_capability_for_slot(
    shared: &SharedVm,
    kind: cratonvm_native_api::CapabilityKind,
    native_id: Option<cratonvm_native_api::NativeMethodId>,
) -> Result<(), MethodCallFailed> {
    let registry = &shared.natives.native_methods;
    let Some(caps) = registry.capabilities() else {
        return Ok(());
    };
    if caps.mode() == cratonvm_native_api::CapabilityMode::Permissive
        && permissive_dispatch_already_recorded(shared.vm_identity, caps, kind)
    {
        return Ok(());
    }
    if let Some(id) = native_id {
        if registry.capability_of_id(id).is_some() {
            return registry.check_dispatch_capability(id).map_err(Into::into);
        }
    }
    caps.check(cratonvm_native_api::Capability::of(
        kind,
        cratonvm_native_api::Scope::Any,
    ))
    .map_err(Into::into)
}

/// The ~35-native half of [`check_native_dispatch_capability`]. Out of line so
/// the hot path is two predicted-not-taken branches and nothing else.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn check_native_dispatch_capability_cold(
    registry: &crate::native::registry::NativeMethodRegistry,
    kind: cratonvm_native_api::CapabilityKind,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Result<(), MethodCallFailed> {
    // Preferred route: the registry's own gate, reading the classification it
    // precomputed for this slot at registration time. Costs one `resolve_id`
    // (a triple hash) — irrelevant next to spawning a process or opening a file.
    if let Some(id) = registry.resolve_id(class_name, method_name, descriptor) {
        if registry.capability_of_id(id).is_some() {
            return registry.check_dispatch_capability(id).map_err(Into::into);
        }
    }
    // Fallback: the slot carries no classification. That happens when the
    // registration predates the policy (an embedder that calls
    // `set_capabilities` after its own `register_*` pass), or when the
    // descriptor-quirk path resolved to a slot registered under a different
    // descriptor. `classify_native` already answered for this `(class, method)`,
    // so check that answer directly rather than let the gate vanish.
    match registry.capabilities() {
        Some(caps) => caps
            .check(cratonvm_native_api::Capability::of(
                kind,
                cratonvm_native_api::Scope::Any,
            ))
            .map_err(Into::into),
        None => Ok(()),
    }
}

/// §4 census hook: count one native dispatch for the `(class, method,
/// descriptor)` triple.
///
/// For dispatch sites that reach a native through a **cached** target and so
/// never hold a `NativeMethodId`. Sites that resolve an id anyway must call
/// `NativeMethodRegistry::record_invocation(id)` directly instead — this
/// wrapper pays one triple hash that they do not need to.
///
/// The acceptance criterion for the mode is *zero* `SyntheticStub` invocations
/// through **any** path, and an uncounted path is an unverifiable one.
#[inline]
pub fn record_native_dispatch(
    shared: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    let registry = &shared.natives.native_methods;
    if let Some(id) = registry.resolve_id(class_name, method_name, descriptor) {
        registry.record_invocation(id);
    }
}

// ───────────────────── the general resolver's census arm (G47-1) ───────────
//
// `invoke_or_native` is the VM's *general* native resolver: the arm every
// dispatch falls to that an inline cache did not serve. `G42-1` §3 measured
// what that means for the census and named it the fourth bypass family, the
// largest one: a compiled `invokevirtual` emits `jit_invoke_virtual_mic`, the
// leaf/native site cache **refuses** any site whose method name is on
// `site_name_is_special_cased` (`"invoke"` is), no MIC/PIC entry can be
// published for a registered native, and the site falls to bare
// `invoke_or_native` — which resolves with `find_with_kind`, holds no
// `NativeMethodId`, and calls `safe_native_call`. Nothing counts.
//
// It is **not** a JIT-only family and not a reflection one. `invoke_or_native`
// is also called from `dispatch_static.rs`, `lambda.rs`, `agent_loader.rs` and
// `debug/mod.rs`, so the `--nojit` arm loses these calls too — it simply loses
// far fewer of them, because the interpreter's cached-native target carries an
// id and counts. Any native can land here; the set is not a fixed table whose
// contents can be inspected the way `G33-1` §4 inspected the intrinsic table
// and the direct-helper list.
//
// # Why this is not "mark at bind time", and why that decision was not open
//
// `G37-1` and `G42-1` both chose bind-time marking over a per-call counter,
// on `G33-1` §5's +9.2 ns/call measurement. Both had a **bind point**: a call
// site being wired to a pre-resolved callback (a JIT direct helper, an
// intrinsic inline-cache fill), where one cold store buys silence forever.
//
// This site has none. `invoke_or_native` re-resolves the triple on every call
// and wires nothing, so "mark once at bind" does not exist here; the cheapest
// honest thing available is "mark on dispatch". And a mark needs a
// `NativeMethodId`, which means `resolve_id` — the *same* lookup a count needs.
// So the two options cost the same lookup and differ only by a relaxed
// `fetch_add` versus a relaxed `store`, and the mark tells you strictly less.
// Marking is not the free option here; it is the same-price, less-informative
// one. That reframes the choice, and it is the one thing about this site that
// could be settled from source without a build.
//
// # What is actually done, and why it is affordable unmeasured
//
// Default: **declare, at most once per (thread, VM, registry generation,
// callback)**. A one-entry-deep pair of memo slots turns the steady state — a
// loop calling the same native, which is exactly the shape `G42-1` §2 measured
// — into a thread-local load and two compares, with `resolve_id` paid only on
// the first dispatch of a callback. The census then says the row is a floor,
// which is what schema 5 exists to print.
//
// On request (`CRATONVM_CENSUS_EXACT_INVOCATIONS`): **count exactly**, one
// `resolve_id` plus one relaxed `fetch_add` per dispatch. That is the arm
// `G42-1` §6 N2 wanted and could not cost; it is off by default precisely
// because nobody has run the interleaved A/B the brief requires — and the one
// run that *needs* it, a census run, is the one run where a few ns on the
// native boundary buys the entire point of the file.
//
// This is the shape the capability gate ~16,300 lines above already argues for
// on this same path and for this same reason ("re-deriving it with `resolve_id`
// would pay a *second* full 128-bit hash per dispatch … the id is resolved only
// after step 2 has said this native is capability-relevant, in the `#[cold]`
// half"). It also inherits that gate's stated trade: under the default arm the
// dump **under-reports `invocations` and says so**, rather than reporting a
// number it cannot stand behind.
//
// # What would make this exact for free
//
// The in-source comment at the `find_with_kind` hit already prescribes it: "a
// `find_with_kind`-shaped lookup that also returns the slot id". That is a
// `native-api` edit and it is nominated, not done here.
//
// NOTE for whoever writes it: `G42-1` §6 N2 glosses the fix as `resolve_id` +
// `callback_of` + **`kind_of_id`**, and that gloss is wrong. `find_with_kind`'s
// own doc records that its cold descriptor-quirk arm looks the kind up with the
// ORIGINAL descriptor, misses, and **deliberately** falls back to `Bridge`;
// `kind_of_id` would return the slot's true kind instead. On a quirky
// descriptor that flips `synthetic_stub_native`, which is the input to the
// real-JDK `SyntheticStub` drop and to `resolve_native_dispatch_wave1` — a
// dispatch-semantics change, in `--real-jdk`, which is the mode all 99
// regression vectors run in. `find_with_kind` is left untouched here for
// exactly that reason.

/// Whether this run was asked for an **exact** `invocations` column on the
/// general-resolver arm (`CRATONVM_CENSUS_EXACT_INVOCATIONS`), at the price of
/// one `resolve_id` per native dispatch that reaches `invoke_or_native`.
///
/// Off by default. `OnceLock` and one relaxed load after the first call, the
/// same shape `registry::lookup_census::enabled` uses on this very path — the
/// disabled case is a load and a not-taken branch.
#[inline]
pub(in crate::vm::vm_exec) fn census_exact_general_dispatch_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if let Some(v) = ENABLED.get() {
        return *v;
    }
    *ENABLED.get_or_init(|| {
        None::<std::ffi::OsString>.is_some()
    })
}

std::thread_local! {
    /// `(vm_identity, registry generation, two callback addresses already
    /// declared)` for this thread.
    ///
    /// A `Cell` of a `Copy` payload, mirroring `PERMISSIVE_DISPATCH_MEMO`
    /// directly above: no `RefCell` borrow flag, no `Arc` clone, no allocation.
    ///
    /// Two slots rather than one because the measured shape is a loop over a
    /// small set of natives, and rather than four because every extra slot is a
    /// compare on the VM's general native path. A miss costs one `resolve_id`
    /// and a relaxed store, never a wrong answer:
    /// `mark_invocations_incomplete` is idempotent and sticky by contract, so
    /// thrash re-declares an already-declared slot and changes nothing.
    ///
    /// Keyed on the **registry generation** as well as the VM, so a
    /// `RegisterNatives` that introduces new slots invalidates the memo instead
    /// of letting a stale callback address vouch for a slot it no longer names.
    /// A callback address is never 0, so the all-zero initial value cannot
    /// collide with a real entry — the same reasoning `PERMISSIVE_DISPATCH_MEMO`
    /// records for `vm_identity`.
    pub static GENERAL_DISPATCH_CENSUS_MEMO: std::cell::Cell<(usize, u32, usize, usize)> =
        const { std::cell::Cell::new((0, 0, 0, 0)) };
}

/// Whether this thread has already declared the slot behind `callback`
/// incomplete for this VM and registry generation — and record it if not.
///
/// Insert-at-front, evict-the-older: a two-entry direct-mapped cache with no
/// hashing.
#[inline]
pub(in crate::vm::vm_exec) fn general_dispatch_census_already_declared(
    vm_identity: usize,
    generation: u32,
    callback: usize,
) -> bool {
    GENERAL_DISPATCH_CENSUS_MEMO.with(|memo| {
        let (memo_vm, memo_gen, first, second) = memo.get();
        if memo_vm == vm_identity && memo_gen == generation {
            if first == callback || second == callback {
                return true;
            }
            memo.set((memo_vm, memo_gen, callback, first));
        } else {
            // Different VM, or the registry grew: start over rather than
            // inherit an answer that was about other slots.
            memo.set((vm_identity, generation, callback, 0));
        }
        false
    })
}

/// Resolve the slot this dispatch is about to run and either **count** it or
/// **declare it uncounted**, per [`census_exact_general_dispatch_enabled`].
///
/// `#[cold]` and out of line: on the default arm it runs once per callback per
/// thread, and on the exact arm the caller is about to pay the ~141 ns
/// `safe_native_call` funnel anyway.
///
/// # The verification, and why it is not paranoia
///
/// `resolve_id` and `find_with_kind` agree on the fast exact-hash path by
/// construction, and both fall back to a descriptor-quirk rewrite — but through
/// *different* functions (`resolve_id_with_descriptor_quirks` versus
/// `find_with_descriptor_quirks`). Rather than assume the two rewrites always
/// land on the same slot, this checks that the resolved slot's callback **is
/// the one about to run**, by address, and does nothing otherwise. Marking or
/// counting a neighbouring slot would put a wrong number on a row that looks
/// authoritative, which is worse than the silence it replaced.
///
/// Compared as `usize` rather than as function pointers: `==` on `fn` pointers
/// draws `unpredictable_function_pointer_comparisons`, and the address is the
/// identity this memo already keys on.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn census_general_dispatch_cold(
    registry: &crate::native::registry::NativeMethodRegistry,
    callback: cratonvm_native_api::NativeCallback,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    exact: bool,
) {
    let Some(id) = registry.resolve_id(class_name, method_name, descriptor) else {
        return;
    };
    if registry.callback_of(id).map(|cb| cb as usize) != Some(callback as usize) {
        return;
    }
    if exact {
        registry.record_invocation(id);
    } else {
        registry.mark_invocations_incomplete(id);
    }
}

/// The census hook every native-dispatching arm of [`invoke_or_native`] calls
/// immediately before `safe_native_call`.
///
/// Purely additive: it reads the registry, writes only census state, returns
/// nothing and cannot change which callback runs. Deliberately placed at the
/// dispatch — not at the `find_with_kind` hit — for the reason the capability
/// gate states one line away: the arms between the lookup and here can still
/// route the call to real bytecode, and a slot that did not run must not be
/// declared to have bypassed anything.
#[inline]
pub(in crate::vm::vm_exec) fn census_general_native_dispatch(
    shared: &SharedVm,
    callback: cratonvm_native_api::NativeCallback,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    let registry = &shared.natives.native_methods;
    if census_exact_general_dispatch_enabled() {
        census_general_dispatch_cold(
            registry,
            callback,
            class_name,
            method_name,
            descriptor,
            true,
        );
        return;
    }
    if general_dispatch_census_already_declared(
        shared.vm_identity,
        registry.generation(),
        callback as usize,
    ) {
        return;
    }
    census_general_dispatch_cold(
        registry,
        callback,
        class_name,
        method_name,
        descriptor,
        false,
    );
}

#[cfg(target_os = "windows")]
extern "system" {
    fn GetModuleHandleA(lpModuleName: *const i8) -> *mut std::ffi::c_void;
    fn GetProcAddress(
        hModule: *mut std::ffi::c_void,
        lpProcName: *const i8,
    ) -> *mut std::ffi::c_void;
}

#[cfg(not(target_os = "windows"))]
pub(in crate::vm::vm_exec) fn lookup_known_system_library_symbol(name: &str, c_name: &std::ffi::CStr) -> Option<usize> {
    if !name.starts_with("ZSTD_") {
        return None;
    }

    static LIBZSTD_HANDLE: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let handle = *LIBZSTD_HANDLE.get_or_init(|| {
        for lib in [b"libzstd.so.1\0".as_slice(), b"libzstd.so\0".as_slice()] {
            let handle =
                unsafe { libc::dlopen(lib.as_ptr() as *const libc::c_char, libc::RTLD_LAZY) };
            if !handle.is_null() {
                return Some(handle as usize);
            }
        }
        None
    });

    let handle = handle? as *mut libc::c_void;
    let addr = unsafe { libc::dlsym(handle, c_name.as_ptr()) };
    if addr.is_null() {
        None
    } else {
        Some(addr as usize)
    }
}

#[cfg(target_os = "windows")]
pub(in crate::vm::vm_exec) fn lookup_known_system_library_symbol(_name: &str, _c_name: &std::ffi::CStr) -> Option<usize> {
    None
}

pub(in crate::vm::vm_exec) fn lookup_process_native_symbol(name: &str) -> Option<usize> {
    let c_name = std::ffi::CString::new(name).ok()?;
    #[cfg(target_os = "windows")]
    {
        for lib in ["ucrtbase.dll\0", "msvcrt.dll\0", "vcruntime140.dll\0"] {
            // SAFETY: each literal is NUL-terminated and Windows owns the
            // returned module handle for the process lifetime.
            let handle = unsafe { GetModuleHandleA(lib.as_ptr() as *const i8) };
            if handle.is_null() {
                continue;
            }
            // SAFETY: `handle` is non-null and `c_name` is a live CString.
            let addr = unsafe { GetProcAddress(handle, c_name.as_ptr()) };
            if !addr.is_null() {
                return Some(addr as usize);
            }
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        // SAFETY: `c_name` is NUL-terminated and `RTLD_DEFAULT` requests a
        // process-wide symbol lookup without transferring ownership.
        let addr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c_name.as_ptr()) };
        if !addr.is_null() {
            return Some(addr as usize);
        }
        lookup_known_system_library_symbol(name, c_name.as_c_str())
    }
}

/// DBG (CRATONVM_DBG_WATCHREF, extended): RandomizedContext WeakHashMap
/// residual investigation — logs a `[watchref]` line ONLY when a given
/// thread's OWN `Thread.currentThread()` mirror address or identity hash
/// code CHANGES from what was last observed for that `ThreadId`. A change
/// here would mean a `WeakHashMap<Thread,...>` keyed on an earlier snapshot
/// of `Thread.currentThread()` could no longer find its own entry — either
/// because the mirror's ADDRESS moved without the map's cached hash bucket
/// being consulted correctly, or because the STAMPED identity hash
/// (assigned once at allocation via `next_hash()`, expected to be preserved
/// verbatim across every GC relocation path) was not actually preserved.
/// Silent under normal operation (dedup keyed by thread_id, so a stable
/// thread logs at most once).
pub(in crate::vm::vm_exec) fn debug_log_thread_mirror_identity(shared: &SharedVm, thread_id: u64, obj: ObjectRef) {
    use std::sync::OnceLock;
    static SEEN: OnceLock<parking_lot::Mutex<std::collections::HashMap<u64, (usize, i32)>>> =
        OnceLock::new();
    let hash = shared.mem.heap.identity_hash_code(obj);
    let addr = obj.as_ptr() as usize;
    let map = SEEN.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let mut m = map.lock();
    match m.get(&thread_id) {
        Some(&(prev_addr, prev_hash)) if prev_addr == addr && prev_hash == hash => {}
        Some(&(prev_addr, prev_hash)) => {
            eprintln!(
                "[watchref] THREAD MIRROR IDENTITY CHANGED tid={thread_id} addr=0x{prev_addr:x}->0x{addr:x} hash={prev_hash}->{hash}"
            );
            m.insert(thread_id, (addr, hash));
        }
        None => {
            eprintln!(
                "[watchref] thread mirror first-seen tid={thread_id} addr=0x{addr:x} hash={hash}"
            );
            m.insert(thread_id, (addr, hash));
        }
    }
}

// gci i47w lane pr47 -- holder page item 2 (`gci-i15orch-g1-parked-virtual-
// thread-holder-graph-...`): the young regions G1 keeps out of every collection
// set as "JIT-pinned" on the 64m virtual-thread probes are pinned by the
// HELPER-WINDOW pass over idle ForkJoinPool carriers parked in compiled
// `awaitWork` (i47z `_st` rows: `hw_windows=20-29 hw_roots=2400-3360` on every
// pause up to the OOME, `jit_pinned_out` 35-62). The pass copies a window's
// whole `[rsp, stack top)` band and pins every word the G1 probe answers --
// any aligned word inside a live region -- so stale Rust-frame words (TLAB
// bookkeeping, dead object addresses) pin a region each, on every pause, for
// as long as the carrier stays parked. HotSpot pins nothing for a frame: its
// oop maps are exact.
//
// gci i20/vh's demotion (default ON since i24/jp) already treats such a band
// as a pure-native one -- its compiled chain walked, marked and pinned per
// pause through the deposit's deferred record, the rest repaired at the wake
// through the stack-slot captures -- but only for a peer whose blocking
// deposit DEFERRED its chain, and only the blocking-region deposit
// (`deposit_root_snapshot_for_blocking_region`) ever defers. `Unsafe.park`
// reaches `park_with_state_until`, whose deposit was the eager
// `deposit_root_snapshot`: no record, no demotion, so the carrier population
// the demotion was written for stayed a pinning window.
//
// The park now takes the blocking region's deposit on G1. It is the same
// deposit (same cache clears, same snapshot; the lazy half applies only under
// `CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT`, default ON, and refuses a
// terminating thread or foreign code), and the park's window has the shape the
// lazy deposit's five obligations (`conservative_roots`, "the lazy blocked-
// thread JIT deposit") ask for, step for step with
// `begin_blocking_region_with_state`: the deposit raises the flag, then
// `GcBarrier::enter_blocked` publishes the code-reclamation band
// (`note_blocking_transition_enter`), the wait runs no compiled code, the
// guard's drop waits out an active pause, and the wake is
// `check_post_block_gc_quiet_refs` -> `check_post_block_gc_inner`, which takes
// the record right after the flag goes down and refreshes without the chain
// only under the lazy wake's own terms. A nested park (flag already up) keeps
// the eager walk (`deposit_root_snapshot_inner_with`). The park's own Rust
// frames hold no raw reference across the wait (the blocker is in
// `native_pin_roots`), the contract i24/jp's point 2 already states for
// `park_with_state_until`.
//
// Soundness of the pinned set: no word a compiled frame names loses its pin --
// every frame of the deferred chain is marked and pinned by
// `resolve_deferred_blocked_jit_chains` in every pause, and a chain the dry
// walk cannot prove frozen is not deferred (the deposit scans it eagerly and
// the band stays a pinning window). What is dropped are the pins of the
// non-compiled words of the band, the over-retention itself. G1 only: the
// demotion acts only there, and Generational / ZGC keep the eager park
// deposit byte for byte. `=0` on the demotion switch restores both halves.

/// gci i47w lane pr47: does a blocking park deposit through the lazy
/// blocking-region deposit? Enabled whenever the deferred-chain demotion
/// is on (`ThreadRegistry::park_defers_jit_chain`). Pure.
pub(crate) fn park_deposit_defers_jit_chain(_is_g1: bool, demotion_on: bool) -> bool {
    demotion_on
}

pub(in crate::vm::vm_exec) fn thread_start_handoff_grace() -> std::time::Duration {
    static GRACE: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *GRACE.get_or_init(|| {
        let millis = cratonvm_types::flags::runtime_var("CRATONVM_THREAD_START_GRACE_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        std::time::Duration::from_millis(millis)
    })
}

// ---------------------------------------------------------------------------
// CP-resolved-interface plumbing for the default-method rescue
// ---------------------------------------------------------------------------
//
// invokeinterface on a receiver whose runtime class is bare `java/lang/Object`
// (e.g. a synthetic ServiceLoader provider stub) loses track of the CP-resolved
// interface as control flows through `execute_invoke` → `invoke_shared` →
// `invoke_on_class_shared_inner`. The receiver-based rescues in those layers
// cannot recover a default method declared on the interface itself because the
// receiver class doesn't list the interface in its `interfaces` table.
//
// We stash the CP-resolved interface class id on a per-thread slot just before
// the dispatch site, and consume it at the NSME-emit site to give the lookup
// one more chance against the interface's own hierarchy. The slot is
// per-thread and is cleared after each invoke to avoid leaking between calls.
std::thread_local! {
    static INVOKE_CP_IFACE_CID: std::cell::Cell<Option<ClassId>> =
        const { std::cell::Cell::new(None) };
}

/// RAII guard that swaps the per-thread "pending CP iface" slot on construction
/// and restores the prior value on drop. Use at each invoke boundary so nested
/// invokes don't leak each other's CP iface ids.
pub struct PendingCpIfaceGuard {
    pub(in crate::vm::vm_exec) prev: Option<ClassId>,
}

impl PendingCpIfaceGuard {
    pub fn new(new_cid: Option<ClassId>) -> Self {
        let prev = INVOKE_CP_IFACE_CID.with(|c| {
            let p = c.get();
            c.set(new_cid);
            p
        });
        Self { prev }
    }
}

impl Drop for PendingCpIfaceGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        INVOKE_CP_IFACE_CID.with(|c| c.set(prev));
    }
}

/// Read (without clearing) the pending CP-resolved interface id, if any.
pub(in crate::vm::vm_exec) fn pending_cp_iface() -> Option<ClassId> {
    INVOKE_CP_IFACE_CID.with(|c| c.get())
}

// ---------------------------------------------------------------------------
// Method-return value coercion
// ---------------------------------------------------------------------------

/// The `byte` / `char` / `short` narrowing of a returned `int` (JVMS 2.11.4
/// i2b / i2c / i2s); the identity for every other tag. `boolean` is left to the
/// caller (see [`coerce_native_return_typed`]).
#[inline]
pub(in crate::vm::vm_exec) fn narrow_sub_int_return(v: i32, ret_type: u8) -> i32 {
    match ret_type {
        b'B' => i32::from(v as i8),  // Cast: JVMS i2b
        b'C' => i32::from(v as u16), // Cast: JVMS i2c
        b'S' => i32::from(v as i16), // Cast: JVMS i2s
        _ => v,
    }
}

/// Coerce a return value to match a method descriptor's return type.
///
/// Native methods (and the JIT return-stub) sometimes hand back a `Value`
/// whose variant doesn't match the static return type вЂ” e.g. a char array
/// element retrieved as `Value::Object(Some(p))` where the descriptor says
/// `C`. The interpreter's typed pop helpers reject the mismatched variant
/// (`expected int on stack, got ref(...)`).
///
/// This is the write-side analogue of the getfield read-side coercion in
/// the interpreter: when a primitive value has been smuggled through the
/// heap as an Object pointer, reinterpret its raw bits as the primitive
/// the descriptor promises. For `L`/`[` returns, an integer 0 is treated
/// as `null` (the inverse: zero-init heap slot for a reference field).
#[inline]
pub fn coerce_value_for_return(value: Value, ret_type: u8) -> Value {
    match ret_type {
        b'I' | b'B' | b'C' | b'S' | b'Z' => match value {
            Value::Object(None) => Value::Int(0),
            // B4: an `Object(Some(_))` reaching an int return slot is a
            // type-confusion case that "shouldn't happen". Truncating the
            // 64-bit heap pointer to i32 (the old `p.as_ptr() as usize as i32`)
            // would silently emit a corrupted int. Fail loudly in debug builds
            // and yield the zero default in release rather than a half-pointer.
            Value::Object(Some(_)) => {
                debug_assert!(
                    false,
                    "coerce_value_for_return: Object reached I/B/C/S/Z return slot"
                );
                Value::Int(0)
            }
            Value::Long(v) => Value::Int(narrow_sub_int_return(v as i32, ret_type)),
            // JIT tier2 wave 3, lane uns21 (UNSAFE21-1): a `byte` / `char` /
            // `short` return reaches the operand stack in range whatever a
            // native answered (HotSpot's native wrapper sign / zero-extends
            // the C result, and `ireturn` does i2b / i2c / i2s). `boolean` is
            // NOT narrowed here: this function also sees an `ireturn`'s raw
            // value, which JVMS 6.5 masks with `& 1`, while a native's result
            // is HotSpot's `c2bool` -- see `coerce_native_return_typed`.
            Value::Int(v) if matches!(ret_type, b'B' | b'C' | b'S') => {
                Value::Int(narrow_sub_int_return(v, ret_type))
            }
            other => other,
        },
        b'J' => match value {
            Value::Object(None) => Value::Long(0),
            // B4: same type-confusion guard as the int arm. A `long` slot is
            // wide enough to hold a pointer without truncation, but surfacing a
            // raw heap address as a Java `long` is still a bug, so default to 0.
            Value::Object(Some(_)) => {
                debug_assert!(
                    false,
                    "coerce_value_for_return: Object reached J return slot"
                );
                Value::Long(0)
            }
            Value::Int(v) => Value::Long(v as i64),
            other => other,
        },
        // Interpreter round i7 wave 143, lane L2 (item 1 of
        // `i136-L2-door-boundary-suspects-of-wave-135`): the B4 guard of the
        // `I`/`J` arms, for `float` / `double`. These two arms passed an
        // `Object` through untouched, so a native returning a reference for an
        // `F`/`D` method left that reference in a primitive operand-stack slot,
        // which the interpreter-to-compiled doors then encode as its address
        // (`jit_bridge::jit_abi_arg_bits`). A null is the zero default, as in
        // the `I`/`J` arms; a non-null reference fails a debug build and is
        // the zero default in release.
        b'F' => match value {
            Value::Int(v) => Value::Float(f32::from_bits(v as u32)),
            Value::Object(None) => Value::Float(0.0),
            Value::Object(Some(_)) => {
                debug_assert!(
                    false,
                    "coerce_value_for_return: Object reached F return slot"
                );
                Value::Float(0.0)
            }
            other => other,
        },
        b'D' => match value {
            Value::Long(v) => Value::Double(f64::from_bits(v as u64)),
            Value::Object(None) => Value::Double(0.0),
            Value::Object(Some(_)) => {
                debug_assert!(
                    false,
                    "coerce_value_for_return: Object reached D return slot"
                );
                Value::Double(0.0)
            }
            other => other,
        },
        b'L' | b'[' => match value {
            Value::Int(0) | Value::Long(0) => Value::Object(None),
            // vm-vmexec-coerce: a `Value::Long` reaching an `L`/`[` return slot
            // used to be reinterpreted as a heap pointer whenever
            // `jlong_bits_as_aligned_object_ptr` returned `Some` — but that
            // helper only checks `p != 0 && p % 8 == 0`, with NO heap-membership
            // test. A `long` carrying an 8-aligned hash / size / length thus
            // became a *fabricated* `ObjectRef`; once stored as `VTAG_OBJECT`,
            // the next GC marks/moves that bogus pointer → Win32 AV / 0xC0000005.
            //
            // We MUST NOT fabricate an `ObjectRef` here without proving the bits
            // point into the heap, and this context-free coercer has no
            // `SharedVm`/heap to consult. So for the aligned-Long case we yield
            // `Value::Object(None)` and force callers that legitimately surface
            // jobject-as-jlong handles (JNI / internal bridges) onto the
            // heap-validating sibling `coerce_value_for_return_validated`, which
            // round-trips the bits through `shared.mem.heap.is_object_address`.
            //
            // Genuine object returns arrive as `Value::Object(Some(_))` and flow
            // unchanged through the `other` arm below — their behavior is
            // untouched.
            Value::Long(_) => Value::Object(None),
            other => other,
        },
        _ => value,
    }
}

/// Coerce an `Option<Value>` (the shape returned by native callbacks) using
/// the method descriptor's return type.
#[inline]
pub fn coerce_native_return(value: Option<Value>, descriptor: &str) -> Option<Value> {
    coerce_native_return_typed(value, crate::jit::return_type(descriptor))
}

/// [`coerce_native_return`] for a caller that already holds the descriptor's
/// return tag.
///
/// `crate::jit::return_type` is a byte-at-a-time scan for `)`, and the compiled
/// native dispatch path runs it on every call for a descriptor it has already
/// decoded once (see `jit::helpers::JitArgShape`). Same answer, no scan.
#[inline]
pub fn coerce_native_return_typed(value: Option<Value>, ret: u8) -> Option<Value> {
    // W5C: a value already of the declared kind (the common native answer) needs
    // neither the per-kind coercion nor the boolean narrowing (`Z`/`B`/`C`/`S`
    // are excluded on purpose: they narrow).
    if ret == b'V'
        || matches!(
            (&value, ret),
            (Some(Value::Object(_)), b'L' | b'[')
                | (Some(Value::Int(_)), b'I')
                | (Some(Value::Long(_)), b'J')
                | (Some(Value::Float(_)), b'F')
                | (Some(Value::Double(_)), b'D')
        )
    {
        return value;
    }
    value.map(|v| coerce_native_return_value(v, ret))
}

/// [`coerce_value_for_return`] for a value a NATIVE answered. A `boolean`
/// native result is HotSpot's `c2bool` (low byte nonzero -> 1), which is not
/// an `ireturn`'s `& 1`; only a native door knows the value came from a
/// native, so the rule lives here and not in `coerce_value_for_return`. Every
/// native-result push (the funnel and the interpreter's) calls this one
/// function. `0` / `1` (every in-tree native) pass.
#[inline]
pub fn coerce_native_return_value(value: Value, ret: u8) -> Value {
    match coerce_value_for_return(value, ret) {
        Value::Int(x) if ret == b'Z' && !matches!(x, 0 | 1) => {
            Value::Int(i32::from(x & 0xFF != 0))
        }
        other => other,
    }
}

/// Validated variant of [`coerce_value_for_return`] for the `b'L' | b'['`
/// path: instead of trusting any aligned `Value::Long` bits as a valid
/// jobject pointer (the legacy bug that lets a `long` carrying e.g. a file
/// size or hash code be reinterpreted as an `ObjectRef`, then later marked
/// by GC -> Win32 SEGV / 0xC0000005), round-trip the bits through
/// `shared.mem.heap.is_object_address`. Real `Value::Object(_)` and primitive
/// return types are forwarded to the legacy coercer unchanged.
///
/// This mirrors `value_as_validated_object_ref` and is the read-side fix
/// for the same "Value::Long misidentified as ObjectRef" family of crashes
/// that the ξ patch fixed on the native-pin path.
#[inline]
pub fn coerce_value_for_return_validated(shared: &SharedVm, value: Value, ret_type: u8) -> Value {
    if matches!(ret_type, b'L' | b'[') {
        return match value {
            Value::Int(0) | Value::Long(0) => Value::Object(None),
            Value::Object(_) => value,
            Value::Long(v) => match jlong_bits_as_aligned_object_ptr(v as u64) {
                Some(p) => match shared.mem.heap.is_object_address(p) {
                    Some(obj) => Value::Object(Some(obj)),
                    None => Value::Object(None),
                },
                None => Value::Object(None),
            },
            other => other,
        };
    }
    coerce_value_for_return(value, ret_type)
}

/// Unbox the return value of a signature-polymorphic native (invoke/
/// invokeExact/VarHandle.get/etc.) against the **call-site** descriptor.
///
/// The native is registered with a generic `(вЂ¦Ljava/lang/Object;)Ljava/lang/Object;`
/// shape and returns a boxed wrapper (via `auto_box_return` inside
/// `native-builtins/src/lang_invoke.rs`), but the bytecode at the call site
/// was compiled against the real descriptor (e.g. `()J`) and will pop a
/// primitive. Without this unwrap the boxed Long lands on the caller's
/// operand stack where an `lreturn` expects a raw long вЂ” triggering
/// `expected long on stack, got ref(0x...)` in `ValueStack::pop_long`.
///
/// C7: Inverse of `coerce_return`'s primitiveв†’reference boxing for SAM
/// lambdas вЂ” here we go referenceв†’primitive per the call-site descriptor.
pub fn coerce_value_against_ret_char(value: Value, ret_char: u8, shared: &SharedVm) -> Value {
    coerce_value_against_ret_char_impl(value, ret_char, shared, false)
}

/// [`coerce_value_against_ret_char`] for an `invokeBasic` call site, whose
/// descriptor is in BASIC types (`LambdaForm`'s `I` stands for `boolean`,
/// `byte`, `short`, `char` and `int`): a `Boolean` box at an `I` site is its
/// `0` / `1`, where the plain coercion zeroes it (a box of another wrapper).
/// The JDK's own VarHandle invoker forms call this model's invoker of a
/// `compareAndSet` that way and read `false` for a CAS that succeeded
/// (interpreter round i2 wave 57, lane L4; `L4W56SpunLambdaForms` `cas-true`
/// under `CRATONVM_VH_SYNTHETIC_MH_TABLE`). `byte`/`short`/`char` boxes
/// already widen; nothing else differs, and the arm runs only where the
/// plain coercion fabricated a zero.
pub fn coerce_value_against_basic_ret_char(value: Value, ret_char: u8, shared: &SharedVm) -> Value {
    coerce_value_against_ret_char_impl(value, ret_char, shared, true)
}

#[inline]
pub(in crate::vm::vm_exec) fn coerce_value_against_ret_char_impl(
    value: Value,
    ret_char: u8,
    shared: &SharedVm,
    basic: bool,
) -> Value {
    // `V` is void вЂ” nothing to coerce; caller shouldn't reach here with void
    // anyway, but preserve as-is defensively.
    if ret_char == b'V' {
        return value;
    }
    let is_primitive_ret = matches!(
        ret_char,
        b'J' | b'I' | b'B' | b'S' | b'C' | b'Z' | b'F' | b'D'
    );
    if !is_primitive_ret {
        return value;
    }
    // Already a primitive-typed Value вЂ” keep as-is.
    if !matches!(value, Value::Object(Some(_))) {
        return value;
    }
    let obj = match value {
        Value::Object(Some(o)) => o,
        _ => return value,
    };
    // Identify the wrapper class and unbox from field 0 (the `value` slot in
    // our synthetic wrapper layout, which matches how `box_primitive` lays
    // them out and how `Long.valueOf`/etc. store their payload).
    let cid = shared.mem.heap.class_id_of(obj);
    let cm = shared.classes.class_manager.read();
    let cls_name = cm
        .get_class(cid)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    drop(cm);
    // Expected wrapper class for each primitive ret_char.
    let expected_wrapper: &str = match ret_char {
        b'J' => "java/lang/Long",
        b'I' => "java/lang/Integer",
        b'B' => "java/lang/Byte",
        b'S' => "java/lang/Short",
        b'C' => "java/lang/Character",
        b'Z' => "java/lang/Boolean",
        b'F' => "java/lang/Float",
        b'D' => "java/lang/Double",
        _ => "",
    };
    if cls_name != expected_wrapper {
        // A wrapper whose primitive WIDENS to the call site's (JLS 5.1.2):
        // `asType` unboxes and widens, so `(long) intVarHandle.get(h)` is the
        // int's value, not zero (interpreter round i1 wave 37, lane L4;
        // `L4W37VarHandleExact` row `plain-get-long`).
        if let Some(widened) =
            widen_boxed_primitive(&cls_name, shared.mem.heap.get_field(obj, 0), ret_char)
        {
            return widened;
        }
        if basic && ret_char == b'I' && cls_name == "java/lang/Boolean" {
            if let Value::Int(v) = shared.mem.heap.get_field(obj, 0) {
                return Value::Int(i32::from(v != 0));
            }
        }
        return match ret_char {
            b'J' => Value::Long(0),
            b'F' => Value::Float(0.0),
            b'D' => Value::Double(0.0),
            _ => Value::Int(0),
        };
    }
    let inner = shared.mem.heap.get_field(obj, 0);
    // C15: When the caller's bytecode expects a primitive (signature-polymorphic
    // invoke call-site), NEVER leave a reference on the stack. If the wrapper
    // class matches, unbox field 0 вЂ” coercing any variant (including malformed
    // Object(None) from an incomplete MethodHandle) to the target primitive's
    // default so the subsequent load opcode can read it correctly.
    match (ret_char, inner) {
        (b'J', Value::Long(v)) => Value::Long(v),
        (b'J', Value::Int(v)) => Value::Long(v as i64),
        (b'I' | b'B' | b'S' | b'C' | b'Z', Value::Int(v)) => Value::Int(v),
        (b'F', Value::Float(v)) => Value::Float(v),
        (b'F', Value::Int(v)) => Value::Float(f32::from_bits(v as u32)),
        (b'D', Value::Double(v)) => Value::Double(v),
        (b'D', Value::Long(v)) => Value::Double(f64::from_bits(v as u64)),
        // Wrapper present but field 0 is null or otherwise malformed
        // (e.g. from a MethodHandle that returned Object(None)) вЂ” yield
        // the primitive zero so the caller's bytecode doesn't see a ref.
        (b'J', _) => Value::Long(0),
        (b'F', _) => Value::Float(0.0),
        (b'D', _) => Value::Double(0.0),
        (_, _) => Value::Int(0),
    }
}

/// The boxed value `inner` (field 0 of a `class_name` wrapper) unboxed and
/// widened to the primitive `ret_char` names, when `class_name`'s primitive
/// widens to it (JLS 5.1.2, `ValueConversions.unboxWiden`); `None` for any
/// other pair, the identity included (the caller's exact-wrapper arm has it)
/// and `Boolean`, which widens to nothing.
pub(in crate::vm::vm_exec) fn widen_boxed_primitive(class_name: &str, inner: Value, ret_char: u8) -> Option<Value> {
    // The source as an integer, or as a float for `Float`.
    enum Src {
        Int(i64),
        Float(f32),
    }
    let (src_char, src) = match (class_name, inner) {
        ("java/lang/Byte", Value::Int(v)) => (b'B', Src::Int(i64::from(v))),
        ("java/lang/Short", Value::Int(v)) => (b'S', Src::Int(i64::from(v))),
        ("java/lang/Character", Value::Int(v)) => (b'C', Src::Int(i64::from(v))),
        ("java/lang/Integer", Value::Int(v)) => (b'I', Src::Int(i64::from(v))),
        ("java/lang/Long", Value::Long(v)) => (b'J', Src::Int(v)),
        ("java/lang/Float", Value::Float(f)) => (b'F', Src::Float(f)),
        ("java/lang/Float", Value::Int(bits)) => (b'F', Src::Float(f32::from_bits(bits as u32))),
        _ => return None,
    };
    let widens = match src_char {
        b'B' => matches!(ret_char, b'S' | b'I' | b'J' | b'F' | b'D'),
        b'S' | b'C' => matches!(ret_char, b'I' | b'J' | b'F' | b'D'),
        b'I' => matches!(ret_char, b'J' | b'F' | b'D'),
        b'J' => matches!(ret_char, b'F' | b'D'),
        b'F' => ret_char == b'D',
        _ => false,
    };
    if !widens {
        return None;
    }
    Some(match (src, ret_char) {
        (Src::Int(v), b'S' | b'I') => Value::Int(v as i32),
        (Src::Int(v), b'J') => Value::Long(v),
        (Src::Int(v), b'F') => Value::Float(v as f32),
        (Src::Int(v), b'D') => Value::Double(v as f64),
        (Src::Float(f), b'D') => Value::Double(f64::from(f)),
        _ => return None,
    })
}

/// Unbox a polymorphic-invoke native return using the call-site descriptor.
///
/// # Why `b'L' | b'['` is a PASSTHROUGH and must stay one (W8-13)
///
/// The symmetric-looking fix — "the call site wants a reference and the native
/// handed back a primitive, so box it here" — is not available at this layer,
/// and the JDK 25 oracle says why. `Value::Int` carries `boolean`, `byte`,
/// `char`, `short` AND `int` (see `cratonvm_types::Value`), so a raw
/// `Value::Int(1)` arriving here could correctly box to `Boolean.TRUE`,
/// `Byte(1)`, `Character('')`, `Short(1)` or `Integer(1)`, and an erased
/// call site's `)Ljava/lang/Object;` names none of them. HotSpot boxes by the
/// VARIABLE's declared type: measured, `booleanVarHandle.getAndSet(arr, 1,
/// false)` at an `Object` call site yields `java.lang.Boolean` printing `true`,
/// where a tag-based guess here would yield `java.lang.Integer` printing `1`.
///
/// Only the native knows the element/field descriptor, so the boxing belongs
/// there — and every `VarHandle` access mode that returns a value now does it,
/// through the single `vh_box_access_result` funnel in
/// `native-builtins/src/lang_invoke.rs`. Before W8-13 only `varhandle_get`
/// boxed, and `getAndSet` / `getAndAdd` / `getAndBitwise*` /
/// `compareAndExchange` returned bare primitives that this passthrough then
/// delivered into a reference return slot: `System.out.println(
/// va.getAndSet(arr, 1, 20))` printed `null` where HotSpot prints `2`.
///
/// The MethodHandle side is the same story for the same reason: `invoke` /
/// `invokeExact` natives box before returning, and this arm hands the box on.
pub fn unbox_poly_return(
    shared: &SharedVm,
    value: Option<Value>,
    descriptor: &str,
) -> Option<Value> {
    let ret = crate::jit::return_type(descriptor);
    match ret {
        b'V' => None,
        b'L' | b'[' => value,
        _ => value.map(|v| coerce_value_against_ret_char(v, ret, shared)),
    }
}

/// The eight classes a primitive boxes to, in the order
/// [`primitive_wrapper_for_ret_char`] names them.
///
/// The membership test — "is the thing the handle produced a boxed primitive at
/// all?" — is the whole safety margin of the strict check below, so it is a
/// closed list rather than a `starts_with("java/lang/")` guess: a synthetic
/// stand-in, an un-nameable fabricated class, a `MethodHandle` that failed to
/// dispatch and a Groovy `Closure` all fall outside it and keep today's
/// behaviour exactly.
pub(in crate::vm::vm_exec) const PRIMITIVE_WRAPPER_CLASSES: [&str; 8] = [
    "java/lang/Long",
    "java/lang/Integer",
    "java/lang/Byte",
    "java/lang/Short",
    "java/lang/Character",
    "java/lang/Boolean",
    "java/lang/Float",
    "java/lang/Double",
];

/// The wrapper class `ret_char`'s primitive boxes to; `None` for `V`, for a
/// reference/array return, and for any byte that is not a primitive field
/// descriptor.
///
/// This is the same eight-way table [`coerce_value_against_ret_char`] applies
/// inline. Named here so the strict `invokeExact` check and the coercion it
/// guards cannot drift apart — they must agree on "which wrapper did the call
/// site want", or the check would fire on a value the coercion would have
/// unboxed correctly.
pub(in crate::vm::vm_exec) fn primitive_wrapper_for_ret_char(ret_char: u8) -> Option<&'static str> {
    Some(match ret_char {
        b'J' => "java/lang/Long",
        b'I' => "java/lang/Integer",
        b'B' => "java/lang/Byte",
        b'S' => "java/lang/Short",
        b'C' => "java/lang/Character",
        b'Z' => "java/lang/Boolean",
        b'F' => "java/lang/Float",
        b'D' => "java/lang/Double",
        _ => return None,
    })
}

/// The index in [`PRIMITIVE_WRAPPER_CLASSES`] of the wrapper
/// [`primitive_wrapper_for_ret_char`] names for `ret_char` (interpreter round
/// i2 wave 55, lane L7; `NativeRealm::exact_return_wrapper_ids`).
pub(in crate::vm::vm_exec) fn primitive_wrapper_slot_for_ret_char(ret_char: u8) -> Option<usize> {
    Some(match ret_char {
        b'J' => 0,
        b'I' => 1,
        b'B' => 2,
        b'S' => 3,
        b'C' => 4,
        b'Z' => 5,
        b'F' => 6,
        b'D' => 7,
        _ => return None,
    })
}

/// `CRATONVM_MH_STRICT_INVOKEEXACT` — opt-in strict **return-type** check for
/// `MethodHandle.invokeExact`. Declared as `CRATONVM_COMPAT=mh-strict-invokeexact`
/// in `types/src/flag_groups.rs`.
///
/// **Default off.** Unset, every path below is byte-identical to what it was.
///
/// Read once through the declared-flag boundary
/// (`cratonvm_types::flags::runtime_var_os`), which serves declared
/// `CRATONVM_*` names from the one immutable `VmFlags` snapshot. So it
/// **latches twice over** — once when the snapshot is built at VM start, and
/// again in the `OnceLock` here. Exporting the variable before launching the
/// process is the only way to set it; an in-process `set_var` after any flag
/// has been read is invisible.
#[inline]
pub(in crate::vm::vm_exec) fn mh_strict_invokeexact() -> bool {
    static STRICT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STRICT.get_or_init(|| {
        // DEFAULT FLIPPED 2026-08-07 (W5-4). Set the exact string `0` to
        // restore the pre-W3-1 fabricated zero.
        //
        // The check's fire set is a strict SUBSET of the "fabricate a zero"
        // set: the two wrapper tables are byte-identical, and the checked
        // wrapper additionally declines an unresolvable class and a non-wrapper
        // object, both of which the coercion still zeroes. So the only delta
        // anywhere is: on a subset of today's fabricated zeros, an exception.
        //
        // One honest caveat, and it is why this needed an A/B rather than an
        // argument: a fabricated zero is the CORRECT observable answer whenever
        // the true answer is 0/false. "Nothing correct can become an exception"
        // holds for the computation (that branch invents a value) but not for
        // the outcome. The one mechanism where HotSpot would disagree is
        // `asType`, which here is a passthrough leaving MH_DESC carrying the
        // LEAF return type — already broken for every non-zero value, so this
        // converts "silently wrong except at zero" into "loudly wrong always".
        //
        // Measured before flipping, interleaved, on the wave-4 binary:
        //   Netty echo (`invokeExact(Thread)Z` — the one named risk that can
        //     structurally reach the fire set): NETTY_OK in BOTH arms;
        //   Groovy 4.0.21 (`IndyInterface`, which a strict check on this path
        //     killed outright once before): identical PASS markers in both.
        //   Groovy/Jackson/Spring/JRuby return REFERENCES, so they cannot reach
        //     the fire set at any input.
        //
        // Permanent blind spot, recorded rather than papered over: Panama
        // primitive downcalls can reach the fire set and have NO Java fixture
        // in this tree — coverage is Rust-side only, and those calls do not
        // route through `unbox_poly_return_checked`.
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0")
        )
    })
}

/// [`unbox_poly_return`], plus the one JDK rule the coercion cannot express:
/// **`invokeExact` does not convert.**
///
/// # The rule
///
/// JLS §15.12.3 / `java.lang.invoke.MethodHandle`: `invokeExact` requires the
/// call site's symbolic descriptor to be *identical* to the handle's
/// `type()` — no widening, no boxing, no `asType`. A mismatch is a
/// `WrongMethodTypeException`. `invoke` is the opposite: it is specified to
/// apply `asType` conversions, and `invokeBasic` / the `VarHandle` accessors
/// are likewise permissive here. **Only `invokeExact` is gated below.** Making
/// `invoke` strict is the mistake that would break Groovy's `IndyInterface`,
/// whose call sites depend on the conversion.
///
/// # Why this is a *return-type-only* check, and why it is narrow
///
/// `register_t4_method_handle_invoke`'s `invokeExact` registration carries an
/// in-tree warning that a strict **arity** check there once aborted the VM and
/// killed every Groovy `IndyInterface` call site: CratonVM's `MH_KIND_*`
/// adapters (insertArguments / asCollector / asSpreader / dropArguments /
/// guardWithTest) keep their inner target's descriptor, so apparent arity
/// routinely disagrees with the real call site. That objection does not
/// transfer to the *return* type, which those adapters preserve.
///
/// This fires on one shape and one only: the native produced a **boxed
/// primitive of a different wrapper class** than the call-site return char
/// names. That is precisely the branch where
/// [`coerce_value_against_ret_char`] gives up and fabricates a zero
/// (`cls_name != expected_wrapper` → `Long(0)`/`Int(0)`/…), i.e. every value
/// reaching it is *already* a silent wrong answer today. There is no case
/// where this check replaces a correct result with an exception.
///
/// Deliberately NOT covered, each keeping today's behaviour:
///
/// * reference/array/void returns — [`unbox_poly_return`] returns those
///   untouched, and a strict check there would need the handle's full
///   `MethodType`, not a value shape;
/// * a `null` result, or any non-wrapper object (a failed dispatch, a
///   synthetic stand-in, an un-nameable fabricated class) — these are the
///   modelling gaps a caller may be tolerating, and turning one into a throw
///   is the broad gate this is written to avoid;
/// * argument count and argument types.
///
/// So it is strictly less strict than HotSpot. That is intentional: the defect
/// being fixed is the *silent wrong answer*, not full spec conformance.
pub fn unbox_poly_return_checked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    value: Option<Value>,
    descriptor: &str,
    method_name: &str,
) -> Result<Option<Value>, MethodCallFailed> {
    // W6-1 — VARHANDLE WRONG-TYPE ACCESS. The `VarHandle` accessors are
    // registered with an erased `Object` return and BOX their primitive
    // result, and `unbox_poly_return` passes `b'L' | b'['` through untouched.
    // So `String s = (String) intVarHandle.get(h)` yields a live `Integer`
    // typed as `String` — and javac emits NO `checkcast` for a
    // signature-polymorphic call (the cast lives in the call site's own
    // symbolic descriptor), so nothing downstream catches it. HotSpot raises
    // WrongMethodTypeException from the access-mode type check. Fire set: a
    // boxed primitive (the same closed eight-class table the invokeExact rule
    // uses) reaching a NON-`Object` reference return — i.e. exactly the values
    // that are already a silent wrong answer today.
    // Interpreter round i2 wave 57, lane L4 (`CRATONVM_VH_PRIMITIVE_RETURN_ASTYPE`):
    // a VarHandle access whose REFERENCE variable reaches a primitive site is
    // `asType`'s run-time unboxing (`ValueConversions.unbox*`), which refuses
    // what the zero arm below fabricated `0` for. A primitive variable that
    // does not convert never gets here with the switch on: the access native
    // refused it before the access.
    if let Some(Value::Object(_)) = value {
        let ret = crate::jit::return_type(descriptor);
        if ret != b'V'
            && ret != b'L'
            && ret != b'['
            && cratonvm_types::flags().natives.vh_primitive_return_astype
            && vh_mode_returns_variable(method_name)
        {
            return vh_unbox_reference_return(shared, thread, value, ret).map(Some);
        }
    }
    // Wave 58 (lane L4): retired behind `VmConfig::vh_w6_1_retired`, counted
    // either way (`site_stats::VH_W6_1_REFUSAL`, see `vh_w6_1_counted`).
    vh_w6_1_counted(shared, value, descriptor, method_name);
    if method_name != "invokeExact" || !mh_strict_invokeexact() {
        // A basic-type site (wave 57, lane L4): see
        // `coerce_value_against_basic_ret_char`.
        if method_name == "invokeBasic" {
            let ret = crate::jit::return_type(descriptor);
            if !matches!(ret, b'V' | b'L' | b'[') {
                return Ok(value.map(|v| coerce_value_against_basic_ret_char(v, ret, shared)));
            }
        }
        return Ok(unbox_poly_return(shared, value, descriptor));
    }
    let ret = crate::jit::return_type(descriptor);
    let (Some(expected), Some(Value::Object(Some(obj)))) =
        (primitive_wrapper_for_ret_char(ret), value)
    else {
        return Ok(unbox_poly_return(shared, value, descriptor));
    };
    let Some(actual) = exact_return_wrapper_mismatch(shared, obj, ret, expected) else {
        return Ok(unbox_poly_return(shared, value, descriptor));
    };
    // Not HotSpot's wording (its message renders both `MethodType`s, which we
    // do not have here — only the produced value's class). It names the two
    // facts that identify the site.
    let message = format!(
        "invokeExact call site {descriptor} requires {}, but the handle produced {}",
        crate::runtime::exceptions::hotspot_external_name(&expected),
        crate::runtime::exceptions::hotspot_external_name(&actual)
    );
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/invoke/WrongMethodTypeException",
        Some(&message),
    ) {
        Ok(exc) => Err(MethodCallFailed::ExceptionThrown(exc)),
        // The throwable would not load — a `synthetic-jdk` build need not model
        // it. Degrade to today's fabricated zero rather than replace a wrong
        // answer with an uncatchable internal abort.
        Err(_) => Ok(unbox_poly_return(shared, value, descriptor)),
    }
}

/// With W6-1 retired (`VmConfig::vh_w6_1_retired`, interpreter round i2 wave
/// 58, lane L4): the rule is still evaluated when the site census is on
/// (`CRATONVM_DBG_FIELD_SITE`), and a would-be refusal counted in
/// `site_stats::VH_W6_1_REFUSAL`, never raised. The measurement that decides
/// whether the rule can be deleted (proposal
/// `interpreter-i57-L4-proposal-retire-the-w6-1-varhandle-return-text-KEPT-20261224`).
#[inline]
pub(in crate::vm::vm_exec) fn vh_w6_1_counted(shared: &SharedVm, value: Option<Value>, descriptor: &str, method_name: &str) {
    if crate::runtime::interpreter::site_cache::site_stats::on() {
        vh_w6_1_counted_cold(shared, value, descriptor, method_name);
    }
}

#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn vh_w6_1_counted_cold(
    shared: &SharedVm,
    value: Option<Value>,
    descriptor: &str,
    method_name: &str,
) {
    if let Some(actual) =
        varhandle_reference_return_mismatch(shared, value, descriptor, method_name)
    {
        crate::runtime::interpreter::site_cache::site_stats::bump(
            crate::runtime::interpreter::site_cache::site_stats::VH_W6_1_REFUSAL,
        );
        eprintln!("[VH_W6_1] would refuse {method_name}{descriptor}: produced {actual}");
    }
}

/// The strict `invokeExact` return check's verdict on the box `obj` at a
/// site expecting the wrapper `expected`: the name of the OTHER wrapper class
/// it is (a refusal), or `None` (the coercion's business).
///
/// With `VmConfig::mh_exact_return_by_id` (interpreter round i2 wave 55, lane
/// L7; proposal
/// `interpreter-L7-proposal-the-invokeexact-return-check-by-class-id-FIXED-20261019`), a
/// box whose class id is the one this VM recorded for `expected`
/// (`NativeRealm::exact_return_wrapper_ids`) is answered here: a class-id
/// read, one relaxed load and a compare, where the name check takes the
/// class-manager read lock (two atomic read-modify-writes on the VM's most
/// contended lock) per call. Everything else -- the first box of each
/// wrapper, another wrapper, a non-wrapper, a second class of the wrapper's
/// name -- takes the name check, which records the id it admits. So the
/// refusals are exactly the name check's.
#[inline]
pub(in crate::vm::vm_exec) fn exact_return_wrapper_mismatch(
    shared: &SharedVm,
    obj: ObjectRef,
    ret: u8,
    expected: &'static str,
) -> Option<String> {
    let cid = shared.mem.heap.class_id_of(obj);
    let slot = primitive_wrapper_slot_for_ret_char(ret);
    {
        if let Some(slot) = slot {
            if shared.natives.exact_return_wrapper_ids[slot]
                .load(std::sync::atomic::Ordering::Relaxed)
                == cid.as_u32()
            {
                crate::runtime::interpreter::site_cache::site_stats::bump(
                    crate::runtime::interpreter::site_cache::site_stats::MH_EXACT_RETURN_BY_ID,
                );
                return None;
            }
        }
    }
    exact_return_wrapper_mismatch_by_name(shared, cid, expected, slot)
}

/// The name check of [`exact_return_wrapper_mismatch`]: under the
/// class-manager read lock, the box's class name against the eight wrappers.
/// Scoped so the guard is released before the caller's
/// `create_exception_object` takes the write lock to load the throwable; the
/// name is copied only for the refusal's message (interpreter round i2 wave
/// 54, lane L7). With `VmConfig::mh_exact_return_by_id`, a box of exactly
/// `expected` records its class id in `slot` (the first id recorded stays).
#[inline(never)]
pub(in crate::vm::vm_exec) fn exact_return_wrapper_mismatch_by_name(
    shared: &SharedVm,
    cid: ClassId,
    expected: &'static str,
    slot: Option<usize>,
) -> Option<String> {
    let cm = shared.classes.class_manager.read();
    let c = cm.get_class(cid)?;
    if *c.name == *expected {
        if let (true, Some(slot)) = (true, slot) {
            let _ = shared.natives.exact_return_wrapper_ids[slot].compare_exchange(
                u32::MAX,
                cid.as_u32(),
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        return None;
    }
    PRIMITIVE_WRAPPER_CLASSES
        .contains(&&*c.name)
        .then(|| c.name.to_string())
}

/// The value a VarHandle access returned at a PRIMITIVE call site `ret`,
/// unboxed as `asType`'s run-time conversion does on HotSpot
/// (`ValueConversions.unbox*` -> `primitiveConversion(wrap, x, false)`),
/// measured on 25.0.3 (`L4W57VarHandleReturnConversions`):
///
/// * the site's own wrapper, or a wrapper whose primitive widens to `ret`
///   (`Short`/`Character` into `int`): its value, widened;
/// * `null`: a `NullPointerException` (`primitiveConversion` answers `null`
///   and `intValue()` is called on it);
/// * any other wrapper, or a `Number` that is not one (`BigInteger`):
///   `Class.cast`'s `ClassCastException`, `Cannot cast java.lang.Long to
///   java.lang.Integer`;
/// * anything else (a `String`, an array): the `(Number)` checkcast's
///   `ClassCastException`, in HotSpot's checkcast wording.
///
/// [`coerce_value_against_ret_char`] fabricated `0` for the last three. Only
/// a reference variable reaches here with a refusal (a primitive one that does
/// not convert is refused by its access native, `CRATONVM_VH_PRIMITIVE_RETURN_ASTYPE`).
/// Costs what the coercion costs (one class-name read). Interpreter round i2
/// wave 57, lane L4.
#[inline(never)]
pub(in crate::vm::vm_exec) fn vh_unbox_reference_return(
    shared: &SharedVm,
    thread: &mut JvmThread,
    value: Option<Value>,
    ret: u8,
) -> Result<Value, MethodCallFailed> {
    let raise = |shared: &SharedVm, thread: &mut JvmThread, class: &str, message: Option<&str>| {
        match crate::runtime::exceptions::create_exception_object(shared, thread, class, message) {
            Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
            Err(e) => e,
        }
    };
    let obj = match value {
        Some(Value::Object(Some(o))) => o,
        Some(Value::Object(None)) => {
            // HotSpot's helpful text for `primitiveConversion`'s `null`
            // (interpreter round i2 wave 58, lane L4; `L4W58VarHandleReturnNullText`).
            // A variable typed as the site's own wrapper (`Integer` at `int`)
            // unboxes through `unboxInteger(Integer x)` there, whose text names
            // `"x"`; this rule does not know the variable's type, so the access
            // native raises that one itself (`lang_invoke::vh_own_wrapper_null`,
            // wave 59, lane L4; page
            // `interpreter-L4-a-varhandle-null-wrapper-variable-names-the-wrong-unboxing-FIXED-20261023`).
            let getter = match ret {
                b'B' => "byteValue",
                b'S' => "shortValue",
                b'J' => "longValue",
                b'F' => "floatValue",
                b'D' => "doubleValue",
                _ => "intValue",
            };
            let message = format!(
                "Cannot invoke \"java.lang.Number.{getter}()\" because the return value of \
                 \"sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, \
                 Object, boolean)\" is null"
            );
            return Err(raise(
                shared,
                thread,
                "java/lang/NullPointerException",
                Some(&message),
            ));
        }
        Some(other) => return Ok(coerce_value_against_ret_char(other, ret, shared)),
        None => {
            return Ok(coerce_value_against_ret_char(
                Value::Object(None),
                ret,
                shared,
            ))
        }
    };
    let want = primitive_wrapper_for_ret_char(ret).unwrap_or("java/lang/Object");
    let cid = shared.mem.heap.class_id_of(obj);
    // The common case -- the site's own wrapper -- by class id, as the
    // `invokeExact` return check does (`NativeRealm::exact_return_wrapper_ids`):
    // then the coercion is the only class-name read, as before this rule.
    // An array's header carries its COMPONENT's id (`Integer[]` reads as
    // `Integer`): the kind is asked first.
    let is_plain_object = shared.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Object;
    let slot = primitive_wrapper_slot_for_ret_char(ret);
    if let (true, Some(slot)) = (is_plain_object, slot) {
        if shared.natives.exact_return_wrapper_ids[slot].load(std::sync::atomic::Ordering::Relaxed)
            == cid.as_u32()
        {
            return Ok(coerce_value_against_ret_char(
                Value::Object(Some(obj)),
                ret,
                shared,
            ));
        }
    }
    let name = if is_plain_object {
        let cm = shared.classes.class_manager.read();
        cm.get_class(cid)
            .map(|c| c.name.to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    if is_plain_object && name == want {
        if let (true, Some(slot)) = (true, slot) {
            let _ = shared.natives.exact_return_wrapper_ids[slot].compare_exchange(
                u32::MAX,
                cid.as_u32(),
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        return Ok(coerce_value_against_ret_char(
            Value::Object(Some(obj)),
            ret,
            shared,
        ));
    }
    let is_wrapper = is_plain_object && PRIMITIVE_WRAPPER_CLASSES.contains(&name.as_str());
    if is_wrapper {
        if let Some(widened) = widen_boxed_primitive(&name, shared.mem.heap.get_field(obj, 0), ret)
        {
            return Ok(widened);
        }
    }
    // The refusal path only.
    let is_number = is_plain_object
        && !is_wrapper
        && shared
            .classes
            .class_manager
            .read()
            .is_assignable_to_name(cid, "java/lang/Number");
    let message = if is_wrapper || is_number {
        format!(
            "Cannot cast {} to {}",
            crate::runtime::exceptions::hotspot_external_name(&name),
            crate::runtime::exceptions::hotspot_external_name(&want)
        )
    } else {
        let from = if is_plain_object {
            crate::runtime::exceptions::hotspot_external_name(&name)
        } else {
            vh_array_display_name(shared, obj)
        };
        crate::runtime::exceptions::hotspot_class_cast_message(
            shared,
            thread,
            &from,
            "java.lang.Number",
            None,
        )
        .unwrap_or_else(|| format!("class {from} cannot be cast to class java.lang.Number"))
    };
    Err(raise(
        shared,
        thread,
        "java/lang/ClassCastException",
        Some(&message),
    ))
}

/// `Class.getName()` of the array `arr` (`[I`, `[Ljava.lang.String;`), for
/// [`vh_unbox_reference_return`]'s checkcast text. A reference array's header
/// carries its COMPONENT's class id; a nested array reads as one level (the
/// heap does not record the depth here), which only shortens the text.
pub(in crate::vm::vm_exec) fn vh_array_display_name(shared: &SharedVm, arr: ObjectRef) -> String {
    use crate::memory::heap::ArrayElementType as E;
    let prim = match shared.mem.heap.array_element_type(arr) {
        Some(E::Boolean) => "Z",
        Some(E::Byte) => "B",
        Some(E::Char) => "C",
        Some(E::Short) => "S",
        Some(E::Int) => "I",
        Some(E::Long) => "J",
        Some(E::Float) => "F",
        Some(E::Double) => "D",
        _ => "",
    };
    if !prim.is_empty() {
        return format!("[{prim}");
    }
    let cid = shared.mem.heap.class_id_of(arr);
    let cm = shared.classes.class_manager.read();
    // JIT round 21 wave 24, lane names (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`):
    // a component that is itself an array is named by its BOTTOM class's
    // hidden flag (`bottom_class_hidden_in`), not by its name's shape.
    let component = cm
        .get_class(cid)
        .map(|c| {
            crate::runtime::exceptions::external_name_by_bottom_class_or(
                &c.name,
                crate::runtime::exceptions::bottom_class_hidden_in(&cm, cid, &c.name),
                |name| crate::runtime::exceptions::hotspot_external_name_of_class(name, c.hidden),
            )
        })
        .unwrap_or_default();
    format!("[L{component};")
}

// Round 15 wave 2 hand-back (lane vh2; VH2-3,
// `r15w2-vh2-varhandle-reference-return-not-cast-to-site-type-FIXED-20260930.md`):
// a `VarHandle` access that returns its variable into a call site declaring a
// narrower reference type is `asType`'s reference CAST on HotSpot, so a value
// that is not one is a `ClassCastException`. javac emits no `checkcast` after
// a signature-polymorphic call, so without this an arbitrary object reached a
// local of the site's type. Switch: `CRATONVM_VH_REFERENCE_RETURN_CHECKCAST`.

/// The `VarHandle` access modes that return the accessed variable (the names
/// [`varhandle_reference_return_mismatch`] lists).
pub(in crate::vm::vm_exec) fn vh_mode_returns_variable(method_name: &str) -> bool {
    matches!(
        method_name,
        "get" | "getVolatile" | "getOpaque" | "getAcquire" | "getPlain"
    ) || method_name.starts_with("getAndSet")
        || method_name.starts_with("getAndAdd")
        || method_name.starts_with("getAndBitwise")
        || method_name.starts_with("compareAndExchange")
}

/// The error path of a door that armed the `VarHandle` parameter-site word:
/// a value or coordinate the call site's static type cannot convert is
/// HotSpot's invoker `asType` refusal, whose message names the access mode
/// type and this call site's `descriptor`
/// (`cannot convert MethodHandle(VarHandle,int[],int,int)int to
/// (VarHandle,int[],int,double)Object`). The refusing native left the
/// handle's shape (`poly_call_site::note_vh_astype_refusal`); this rewrites
/// the message (`lang_invoke::varhandle_astype_refusal_with_site`). One
/// predictable branch on success. Interpreter round i2 wave 53, lane L4.
#[inline]
pub(crate) fn vh_astype_refusal_site_text(
    shared: &SharedVm,
    thread: &mut JvmThread,
    result: MethodCallResult,
    method_name: &str,
    descriptor: &str,
) -> MethodCallResult {
    match result {
        Err(failed @ MethodCallFailed::ExceptionThrown(_)) => Err(
            vh_astype_refusal_site_text_cold(shared, thread, failed, method_name, descriptor),
        ),
        other => other,
    }
}

/// The cold half of [`vh_astype_refusal_site_text`], for a door that has the
/// failure in hand (the JIT's, which may first recover a stand-in site's
/// descriptor).
#[cold]
#[inline(never)]
pub(crate) fn vh_astype_refusal_site_text_cold(
    shared: &SharedVm,
    thread: &mut JvmThread,
    failed: MethodCallFailed,
    method_name: &str,
    descriptor: &str,
) -> MethodCallFailed {
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    cratonvm_native_builtins::lang_invoke::varhandle_astype_refusal_with_site(
        &mut ctx,
        failed,
        method_name,
        descriptor,
    )
}

/// Does `descriptor`, the call site of the `VarHandle` access `method_name`,
/// declare a reference return narrower than `Object` (a class or an array)?
pub(crate) fn vh_site_returns_narrow_reference(method_name: &str, descriptor: &str) -> bool {
    vh_mode_returns_variable(method_name)
        && descriptor.rsplit(')').next().is_some_and(|ret| {
            (ret.starts_with('L') || ret.starts_with('[')) && ret != "Ljava/lang/Object;"
        })
}

/// Whether the variable of the handle `receiver` has a reference type, for a
/// site [`vh_site_returns_narrow_reference`] names (else `None`). Read BEFORE
/// the native runs: it may move the handle, and a box's CCE-or-WMTE answer
/// depends on it ([`unbox_poly_return_checked_vh`]).
pub(crate) fn vh_reference_return_variable(
    shared: &SharedVm,
    thread: &mut JvmThread,
    method_name: &str,
    descriptor: &str,
    receiver: Option<ObjectRef>,
) -> Option<bool> {
    if !vh_site_returns_narrow_reference(method_name, descriptor)
        || !cratonvm_native_builtins::lang_invoke::vh_reference_return_checkcast_enabled()
    {
        return None;
    }
    let vh = receiver?;
    let mut ctx = NativeContextImpl { shared, thread };
    cratonvm_native_builtins::lang_invoke::varhandle_variable_is_reference(&mut ctx, vh)
}

/// [`unbox_poly_return_checked`] after `asType`'s checkcast of a `VarHandle`
/// reference return (`lang_invoke::varhandle_reference_return_refusal`).
/// `variable_is_reference` is [`vh_reference_return_variable`]'s answer,
/// taken before the native ran. A box refused for a primitive or undescribed
/// variable falls through to W6-1's `WrongMethodTypeException`, as before.
pub fn unbox_poly_return_checked_vh(
    shared: &SharedVm,
    thread: &mut JvmThread,
    value: Option<Value>,
    descriptor: &str,
    method_name: &str,
    variable_is_reference: Option<bool>,
) -> Result<Option<Value>, MethodCallFailed> {
    if let Some(Value::Object(Some(obj))) = value {
        if vh_site_returns_narrow_reference(method_name, descriptor) {
            if let Some(site_ret) = descriptor.rsplit(')').next() {
                let mut ctx = NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                if let Some(error) =
                    cratonvm_native_builtins::lang_invoke::varhandle_reference_return_refusal(
                        &mut ctx,
                        Value::Object(Some(obj)),
                        site_ret,
                        variable_is_reference,
                    )
                {
                    return Err(error);
                }
            }
        }
    }
    unbox_poly_return_checked(shared, thread, value, descriptor, method_name)
}

/// Kill switch for the `VarHandle` reference-return rule below. Set the exact
/// string `0` to restore the pre-W6-1 silent wrong answer.
///
/// Deliberately SEPARATE from `CRATONVM_MH_STRICT_INVOKEEXACT`: the two rules
/// fire on disjoint method names (`invokeExact` versus the `VarHandle` access
/// modes) and share only the funnel they sit in, so one going wrong in the
/// field must not force the other off.
pub(crate) fn vh_strict_reference_return() -> bool {
    static STRICT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STRICT.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0")
        )
    })
}

/// Every transitive supertype of each of the eight wrapper classes, measured on
/// the JDK 25 oracle (`Class.getSuperclass`/`getInterfaces`, transitively).
///
/// This is the FALLBACK half of the assignability test in
/// [`varhandle_reference_return_mismatch`]. The primary half is a real walk of
/// the loaded hierarchy; this table exists because that walk reports `false`
/// when the wrapper's interfaces are not resolved in the class store, and
/// `false` there means "fire", i.e. it fails in the dangerous direction. The
/// two halves cover each other: a JDK that adds a supertype this table does not
/// know is caught by the walk, and an unresolved hierarchy is caught by the
/// table.
///
/// `java/lang/Object` is included in every row — an erased `Object` call site
/// legitimately receives a box, and that used to be this function's only
/// exclusion.
pub(in crate::vm::vm_exec) fn boxed_primitive_supertypes(wrapper: &str) -> &'static [&'static str] {
    const OBJ: &str = "java/lang/Object";
    const CMP: &str = "java/lang/Comparable";
    const SER: &str = "java/io/Serializable";
    const CONSTABLE: &str = "java/lang/constant/Constable";
    const CONSTANT_DESC: &str = "java/lang/constant/ConstantDesc";
    const NUM: &str = "java/lang/Number";
    match wrapper {
        // Integer/Long/Float/Double are `ConstantDesc`; Byte/Short are not.
        "java/lang/Integer" | "java/lang/Long" | "java/lang/Float" | "java/lang/Double" => {
            &[OBJ, NUM, CMP, SER, CONSTABLE, CONSTANT_DESC]
        }
        "java/lang/Byte" | "java/lang/Short" => &[OBJ, NUM, CMP, SER, CONSTABLE],
        // Character/Boolean are not `Number`.
        "java/lang/Character" | "java/lang/Boolean" => &[OBJ, CMP, SER, CONSTABLE],
        _ => &[],
    }
}

/// Is `name` one of the eight primitive wrapper classes?
///
/// The predicate half of [`PRIMITIVE_WRAPPER_CLASSES`], exposed because the
/// `VarHandle` reference-read thin direct bind reproduces W6-1 on its own cold
/// arm (`jit::helpers::varhandle_strict_reference_return_check`) and the two
/// must fire on exactly the same eight classes.
pub(crate) fn is_primitive_wrapper_class_name(name: &str) -> bool {
    PRIMITIVE_WRAPPER_CLASSES.contains(&name)
}

/// The `VarHandle` access modes that RETURN the accessed variable, paired with
/// a call site whose descriptor names a reference type that the produced box
/// cannot be. `None` means "no complaint"; `Some(cls)` names what was produced.
///
/// # The predicate, and why it is assignability rather than equality
///
/// Measured on the JDK 25 oracle (`java 25.0.3`, a `VarHandle` over an `int`
/// field read at a range of reference call sites):
///
/// | call site return | HotSpot |
/// | --- | --- |
/// | `Object` / `Number` / `Comparable` / `Serializable` | no throw |
/// | `Constable` / `ConstantDesc` / `Integer` | no throw |
/// | `String` / `CharSequence` / `Long` / `Double` | `WrongMethodTypeException` |
///
/// and, for the non-`Number` wrappers, `char`/`boolean` at a `Number` call site
/// throws while at a `Comparable` call site it does not. So HotSpot's rule for
/// a boxed result at a reference call site is exactly **"is the wrapper class
/// assignable to the call site's declared return type?"** — the class-hierarchy
/// relation, nothing more.
///
/// The original form of this check tested `actual == want` with a single
/// hand-rolled exclusion for `java/lang/Object`. That is a *fragment* of the
/// relation, so it fired on `Number`, `Comparable`, `Serializable`,
/// `Constable` and `ConstantDesc` call sites, every one of which HotSpot
/// accepts — a false positive on shapes as ordinary as
/// `Number n = (Number) vh.get(o)` over an `Integer` field. Fixed here by
/// testing real assignability.
///
/// # Why the remaining fire set has no false positives
///
/// After the fix the rule is: *the access produced a boxed primitive that is
/// not assignable to the call site's declared reference return type*. On
/// HotSpot that is always an exception — `WrongMethodTypeException` when the
/// handle's `varType` is primitive (`asType` refuses the conversion) and
/// `ClassCastException` when it is a reference (the cast fails at runtime).
/// CratonVM cannot tell those apart from the produced value alone and raises
/// `WrongMethodTypeException` for both; that is a wrong exception *class* in
/// the reference-`varType` case, never a wrong outcome.
///
/// Still deliberately outside the fire set, each keeping today's behaviour: a
/// `null` result, an array return, and any non-wrapper object (a synthetic
/// stand-in, an un-nameable fabricated class, a genuine reference value).
///
/// # Interaction with the native-side boxing funnel (W8-13)
///
/// This check can only see values `lang_invoke.rs`'s `vh_box_access_result`
/// produces, and the two cannot double-fire or disagree:
///
/// * they act on disjoint outcomes — the funnel BOXES, this check THROWS, and
///   `java/lang/Object` plus every wrapper supertype is in
///   `boxed_primitive_supertypes`, so the erased `Object` call site the funnel
///   exists to serve is declined here by construction;
/// * before the funnel, the RMW modes returned a bare `Value::Int`/`Long`/… ,
///   which is not `Value::Object(Some(_))`, so this check *structurally could
///   not fire* on `getAndSet` / `getAndAdd` / `getAndBitwise*` /
///   `compareAndExchange` — its method-name list covered them but its value
///   test never matched. The funnel is what makes those names reachable, i.e.
///   it closes a hole in this check rather than colliding with it;
/// * `compareAndSet` / `weakCompareAndSet*` stay out of the list and stay
///   correct. javac fixes their call-site descriptor at `)Z` — the return type
///   of a signature-polymorphic call comes from the cast context only for the
///   modes DECLARED to return `Object`, and `compareAndSet` is declared
///   `boolean`. Verified on JDK 25: `Object o = vh.compareAndSet(a,0,1,11)`
///   compiles to `compareAndSet:([IIII)Z` followed by `Boolean.valueOf`, and
///   `(String) vh.compareAndSet(...)` does not compile at all. So no boolean
///   mode can reach a reference call site, and none needs boxing.
pub(in crate::vm::vm_exec) fn varhandle_reference_return_mismatch(
    shared: &SharedVm,
    value: Option<Value>,
    descriptor: &str,
    method_name: &str,
) -> Option<String> {
    if !vh_strict_reference_return() {
        return None;
    }
    // The access modes that hand back the accessed variable. `set*` return
    // void and `compareAndSet`/`weakCompareAndSet*` return boolean, so neither
    // can reach a reference call site.
    if !matches!(
        method_name,
        "get"
            | "getVolatile"
            | "getOpaque"
            | "getAcquire"
            | "getPlain"
            | "getAndSet"
            | "getAndSetAcquire"
            | "getAndSetRelease"
            | "getAndAdd"
            | "getAndAddAcquire"
            | "getAndAddRelease"
            | "getAndBitwiseOr"
            | "getAndBitwiseOrAcquire"
            | "getAndBitwiseOrRelease"
            | "getAndBitwiseAnd"
            | "getAndBitwiseAndAcquire"
            | "getAndBitwiseAndRelease"
            | "getAndBitwiseXor"
            | "getAndBitwiseXorAcquire"
            | "getAndBitwiseXorRelease"
            | "compareAndExchange"
            | "compareAndExchangeAcquire"
            | "compareAndExchangeRelease"
    ) {
        return None;
    }
    if crate::jit::return_type(descriptor) != b'L' {
        return None;
    }
    let want = descriptor.rsplit(')').next()?;
    let want = want.strip_prefix('L')?.strip_suffix(';')?;
    let Some(Value::Object(Some(obj))) = value else {
        return None;
    };
    // An array's header carries its COMPONENT's class id, so an `Integer[]`
    // would be named `java/lang/Integer` below. An array is never a boxed
    // primitive (page `r11w13-icwide-vm-exec-varhandle-return-check-array-alias`;
    // the JIT twin screens it out the same way).
    if shared.mem.heap.kind_of(obj) != cratonvm_types::ObjectKind::Object {
        return None;
    }
    let cid = shared.mem.heap.class_id_of(obj);
    // The guard is confined to this block; `create_exception_object` in the
    // caller takes the class-manager WRITE lock to load the throwable and must
    // not find this read guard still held.
    let cm = shared.classes.class_manager.read();
    let actual = cm.get_class(cid).map(|c| c.name.to_string())?;
    // Only a boxed primitive complains — a synthetic stand-in, an un-nameable
    // fabricated class or a genuine reference value keeps today's behaviour.
    if actual == want || !PRIMITIVE_WRAPPER_CLASSES.contains(&actual.as_str()) {
        return None;
    }
    // Assignable by either route (see `boxed_primitive_supertypes`) is a legal
    // widening, not a mismatch.
    if boxed_primitive_supertypes(&actual).contains(&want) || cm.is_assignable_to_name(cid, want) {
        return None;
    }
    drop(cm);
    Some(actual)
}

/// The descriptors a signature-polymorphic `MethodHandle` / `VarHandle`
/// native is actually registered under.
///
/// A polymorphic call site names its OWN descriptor (`(LFoo;)I`), which is
/// never the registration descriptor, so a registry lookup by the call-site
/// triple always misses. The dispatch tail below re-probes with these three,
/// in this order, and so does the JIT's per-call-site native cache
/// (`jit::helpers::resolve_native_owner_for_receiver`) — the constant is
/// shared so the two cannot drift.
pub(crate) const SIGNATURE_POLYMORPHIC_NATIVE_DESCRIPTORS: [&str; 3] = [
    "([Ljava/lang/Object;)Ljava/lang/Object;",
    "([Ljava/lang/Object;)V",
    "([Ljava/lang/Object;)Z",
];

/// Is this one of the method names JVMS §5.4.3.4's signature-polymorphic rule
/// covers? Extracted from the dispatch tail below so the JIT site cache asks
/// exactly the same question.
pub(crate) fn is_signature_polymorphic_method_name(method_name: &str) -> bool {
    matches!(
        method_name,
        "invoke"
            | "invokeExact"
            | "invokeWithArguments"
            | "invokeBasic"
            | "get"
            | "set"
            | "getVolatile"
            | "setVolatile"
            | "getOpaque"
            | "setOpaque"
            | "getAcquire"
            | "setRelease"
            | "compareAndSet"
            | "compareAndExchange"
            | "compareAndExchangeAcquire"
            | "compareAndExchangeRelease"
            | "weakCompareAndSet"
            | "weakCompareAndSetPlain"
            | "weakCompareAndSetAcquire"
            | "weakCompareAndSetRelease"
            | "getAndSet"
            | "getAndSetAcquire"
            | "getAndSetRelease"
            | "getAndAdd"
            | "getAndAddAcquire"
            | "getAndAddRelease"
            | "getAndBitwiseOr"
            | "getAndBitwiseOrAcquire"
            | "getAndBitwiseOrRelease"
            | "getAndBitwiseAnd"
            | "getAndBitwiseAndAcquire"
            | "getAndBitwiseAndRelease"
            | "getAndBitwiseXor"
            | "getAndBitwiseXorAcquire"
            | "getAndBitwiseXorRelease"
    )
}

/// `MethodHandle.linkToStatic` / `linkToVirtual` / `linkToInterface` /
/// `linkToSpecial` / `linkToNative`: the linker intrinsics every JDK
/// `LambdaForm` linker ends in (`DirectMethodHandle$Holder.invokeStatic` ...),
/// `static native @PolymorphicSignature` (JVMS 2.9.3), so a call from bytecode
/// carries its own concrete descriptor (`(ILjava/lang/invoke/MemberName;)I`)
/// and NO receiver. They are registered under the erased descriptor next to
/// `invokeBasic` (`native_method_handle_link_to`, which validates the spread
/// layout and refuses `linkToNative`'s `NativeEntryPoint`). Kept out of
/// [`is_signature_polymorphic_method_name`] on purpose: that list's names are
/// instance calls on a handle receiver, and its other askers (the route cache,
/// the `VarHandle` gates) read `args[0]` as that receiver. Interpreter round
/// i2 wave 55, lane L4 (`VmConfig::mh_linkto_from_bytecode`).
#[inline]
pub(crate) fn is_method_handle_linker_intrinsic(class_name: &str, method_name: &str) -> bool {
    method_name.starts_with("linkTo")
        && class_name == "java/lang/invoke/MethodHandle"
        && matches!(
            method_name,
            "linkToStatic" | "linkToVirtual" | "linkToInterface" | "linkToSpecial" | "linkToNative"
        )
}

/// The positive control of [`is_method_handle_linker_intrinsic`]'s doors:
/// `CRATONVM_DBG_MH_DISPATCH=1` prints `[MH_LINKTO] <name><site descriptor>`
/// for a `linkTo*` call from bytecode reaching the erased native.
#[cold]
#[inline(never)]
pub(crate) fn note_linker_from_bytecode(method_name: &str, descriptor: &str) {
    if cratonvm_types::flags().natives.dbg_mh_dispatch {
        eprintln!("[MH_LINKTO] {method_name}{descriptor} from bytecode");
    }
}

pub(crate) fn is_method_handle_signature_polymorphic_receiver(class_name: &str) -> bool {
    class_name == "java/lang/invoke/MethodHandle"
        || class_name.starts_with("java/lang/invoke/MethodHandle")
        || (class_name.starts_with("java/lang/invoke/") && class_name.contains("MethodHandle"))
        || class_name == "java/lang/foreign/DowncallHandle"
}

pub(crate) fn is_var_handle_signature_polymorphic_receiver(class_name: &str) -> bool {
    class_name == "java/lang/invoke/VarHandle"
        || class_name.starts_with("java/lang/invoke/VarHandle")
        || (class_name.starts_with("java/lang/invoke/") && class_name.contains("VarHandle"))
}

pub(in crate::vm::vm_exec) fn prefers_exact_signature_polymorphic_receiver(class_name: &str) -> bool {
    class_name == "java/lang/foreign/DowncallHandle"
}

// Whether this dispatch should publish its CALL-SITE descriptor on
// [`cratonvm_native_api::poly_call_site`] for the native it is about to run.
//
// Two names need it, for two different reasons:
//
// * `invoke` -- its collect-or-passthrough answer for a trailing `null`
//   depends on the type the caller WROTE, and a `null` carries no runtime
//   type. This is the original consumer the channel was built for.
// * `invokeExact` -- the rule that the call site must match the handle's
//   `type()` EXACTLY is a statement about the call site, so the check cannot
//   be made anywhere that cannot see it. See
//   `lang_invoke::exact_call_site_refusal`.
//
// `java/lang/foreign/DowncallHandle` is excluded from the `invokeExact` arm on
// purpose. Its `invokeExact` resolves to Panama's OWN native, not
// `MethodHandle`'s, and that native does not TAKE the channel -- an armed
// descriptor it left behind would be read by a later dispatch as its own, and
// the module doc is explicit that a WRONG call-site type is worse than none.
// VarHandle read natives also take this channel to judge byte-view return
// adaptation before null/type/index checks. Every read callback consumes it.
pub(crate) fn arms_poly_call_site(class_name: &str, method_name: &str) -> bool {
    match method_name {
        "invoke" => is_method_handle_signature_polymorphic_receiver(class_name),
        "invokeExact" => {
            is_method_handle_signature_polymorphic_receiver(class_name)
                && !prefers_exact_signature_polymorphic_receiver(class_name)
        }
        "get" | "getVolatile" | "getOpaque" | "getAcquire" => {
            is_var_handle_signature_polymorphic_receiver(class_name)
        }
        _ => false,
    }
}

/// Whether this dispatch should publish the STATIC TYPES of its parameters on
/// [`cratonvm_native_api::poly_call_site::arm_vh_param_site`]: every
/// `VarHandle` access mode (every signature-polymorphic name except the
/// `invoke*` family). Round 15 wave 3 (lane vh4; part 2 of
/// `r15w3-vh3-vm-exec-exact-mint-sweep-and-param-site-patch-FIXED-20260930.md`): the
/// four plain reads too, and the whole parameter-site word (coordinates
/// included), which an `IndirectVarHandle`'s coordinates and the index of an
/// array or byte-view handle are judged by.
///
/// Every native those names reach in `lang_invoke.rs` (`varhandle_get`,
/// `varhandle_set`, `_compare_and_set`, `_compare_and_exchange`,
/// `_get_and_set`, `_get_and_add`, `_get_and_bitwise`) takes the cell as its
/// first statement.
/// It is armed ONLY at the dispatch point in `invoke_on_class_shared_inner`,
/// immediately before the native runs, so no Java code (a `<clinit>`, a
/// class load) can run between the arm and the take and consume it for a
/// nested access. It does not touch `arms_poly_call_site`, so the
/// native-callee memo is unaffected. See
/// `r11w15-rt-varhandle-call-site-descriptor-patch-FIXED-20260925.md`.
pub(crate) fn arms_vh_value_site(class_name: &str, method_name: &str) -> bool {
    is_var_handle_signature_polymorphic_receiver(class_name)
        && is_signature_polymorphic_method_name(method_name)
        && !matches!(
            method_name,
            "invoke" | "invokeExact" | "invokeWithArguments" | "invokeBasic"
        )
}

// ---------------------------------------------------------------------------
// Safe native callback invocation
// ---------------------------------------------------------------------------

// NOTE (B5): the historical *unvalidated* `value_as_object_ref` was removed.
// It reinterpreted any aligned `Value::Long` bits as an `ObjectRef` via
// `ObjectRef::from_raw` without checking the address belongs to the heap —
// the exact pattern that lets a `long` carrying a file size / hash code be
// marked/moved by the GC and SEGV (0xC0000005). It had no callers in
// `vm/src` (all sites use the validated variant below). Keeping it deleted
// prevents a future caller from reintroducing the GC-mark hazard. Use
// [`value_as_validated_object_ref`] instead.

/// Verifies that a `Value::Long` whose bits look like an aligned pointer
/// actually points at a heap object before returning it. Without this check,
/// a real `Value::Long` carrying e.g. a file size that happens to be 8-byte
/// aligned would be reinterpreted as a jobject; the GC would later try to
/// mark/move that bogus pointer and SEGV. Real `Value::Object(Some(_))` is
/// always returned as-is.
#[inline]
pub fn value_as_validated_object_ref(shared: &SharedVm, v: Value) -> Option<ObjectRef> {
    match v {
        Value::Object(Some(o)) => Some(o),
        Value::Long(bits) => {
            let p = jlong_bits_as_aligned_object_ptr(bits as u64)?;
            shared.mem.heap.is_object_address(p)
        }
        _ => None,
    }
}

/// Validate that a raw native byte range `[base, base + len)` does not wrap
/// past the end of the (64-bit) address space.
///
/// Used by `copy_to_native_memory` / `copy_from_native_memory` to reject a
/// forged length on a high `base` before the unchecked `copy_nonoverlapping`.
/// A wrapping range would let the copy run off the end of the addressable
/// space (out-of-bounds read/write = UB); we refuse it instead. `len == 0` is
/// in bounds (a no-op copy). The Java-side array bounds are enforced separately
/// by the caller, which passes an already-sliced `&[u8]` / `&mut [u8]`.
#[inline]
pub(in crate::vm::vm_exec) fn native_range_is_in_bounds(base: u64, len: usize) -> bool {
    // `checked_add` is `None` exactly when `base + len` overflows `u64`.
    base.checked_add(len as u64).is_some()
}

/// Atomically claim a raw `Box<JoinHandle<()>>` pointer for exactly-once
/// consumption, returning `true` only on the FIRST claim of a given pointer.
///
/// `register_native_thread` / `attach_join_handle_to_native_thread` receive a
/// `usize` that the caller produced via `Box::into_raw` and reconstruct it with
/// `Box::from_raw`. If the SAME pointer value were ever passed twice (a
/// duplicated handle, or a register-then-attach on the same raw pointer), the
/// second `Box::from_raw` would reconstruct an already-freed allocation and
/// double-free it on drop. Gating each reconstruction on this claim makes the
/// `Box::from_raw` happen at most once per pointer: a second call sees the
/// pointer already recorded, returns `false`, and the caller skips the unsafe
/// reconstruction (a leak of that handle is acceptable; a double-free is not).
///
/// Pointers reused by the allocator after their owning thread has fully torn
/// down are not a concern here — the dedup set only needs to prevent a
/// double-consume of a handle that is still considered live by a caller, and
/// in practice each spawned thread's handle is claimed exactly once. The set is
/// bounded by the number of live native-thread handles, which is small.
pub(in crate::vm::vm_exec) fn claim_raw_join_handle(ptr: usize) -> bool {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static CLAIMED: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    let set = CLAIMED.get_or_init(|| Mutex::new(HashSet::new()));
    // A poisoned lock here only means a prior holder panicked; the contained
    // set is still structurally valid, so recover the guard and proceed.
    let mut guard = set.lock().unwrap_or_else(|e| e.into_inner());
    // `HashSet::insert` returns `true` iff the value was NOT already present,
    // i.e. iff this is the first claim — exactly the exactly-once predicate.
    guard.insert(ptr)
}

/// Pin a value that may encode a jobject as `Value::Long` for the duration
/// of a native call (see `safe_native_call`).
#[inline]
pub(in crate::vm::vm_exec) fn pin_value_for_native_call(shared: &SharedVm, roots: &mut Vec<ObjectRef>, v: &Value) {
    if let Some(o) = value_as_validated_object_ref(shared, *v) {
        roots.push(o);
    }
}

/// `java.lang.Object`'s `ClassId`, resolved once per (host thread, VM) and
/// cached lock-free thereafter. `java.lang.Object` is always loaded before any
/// bytecode runs (it roots the class hierarchy the VM needs to bootstrap), so
/// in practice the lookup below succeeds on the very first call — but if it
/// were ever to return `None`, nothing is cached and the next call retries the
/// real lookup rather than permanently disabling the stale-receiver check.
///
/// PER-VM STATE (P0, `docs/architecture/per-vm-state.md`). This was a
/// process-global `OnceLock<ClassId>`. `ClassId`s are allocated per-VM
/// (`ClassStore::next_id` returns `self.classes.len()`), so with two VMs live
/// the first VM to boot pinned ITS `java/lang/Object` id for the whole
/// process; the second VM then compared its own receivers' ids against a
/// foreign VM's id. The only caller is
/// `recover_stale_lambda_receiver_from_native_pins`'s `stale_object_receiver`
/// test, so the failure mode is silent: a genuinely-stale `Object`-typed
/// receiver stops being recovered (spurious `NoSuchMethodError`), or an
/// ordinary receiver is misclassified as stale and the native-pin scan runs on
/// every virtual call. Keyed by `shared.vm_identity` it cannot alias, and the
/// cache is a thread-local `Cell` exactly like the two wrapper-class caches
/// below (same shape, same `(vm_identity, ClassId)` key convention).
#[inline]
pub(crate) fn object_class_id(shared: &SharedVm) -> Option<ClassId> {
    thread_local! {
        static OBJECT_CLASS_ID: std::cell::Cell<Option<(usize, ClassId)>> =
            const { std::cell::Cell::new(None) };
    }
    let vm_key = shared.vm_identity;
    if let Some((cached_vm, id)) = OBJECT_CLASS_ID.with(|c| c.get()) {
        if cached_vm == vm_key {
            return Some(id);
        }
    }
    let resolved = shared
        .classes
        .class_manager
        .read()
        .find_bootstrap_class_by_name("java/lang/Object")?;
    OBJECT_CLASS_ID.with(|c| c.set(Some((vm_key, resolved))));
    Some(resolved)
}

thread_local! {
    /// Last primitive-wrapper class observed by this host thread, scoped to a
    /// specific SharedVm so sequential in-process VMs cannot alias ClassIds.
    pub(in crate::vm::vm_exec) static PRIMITIVE_WRAPPER_CLASS_CACHE: std::cell::Cell<Option<(usize, ClassId)>> =
        const { std::cell::Cell::new(None) };

    /// Last class this host thread confirmed is **not** a primitive wrapper,
    /// scoped to a `SharedVm` exactly like `PRIMITIVE_WRAPPER_CLASS_CACHE`.
    ///
    /// Only the positive answer used to be memoized, so every
    /// `fast_unbox_primitive_wrapper` call on a non-wrapper key re-took the
    /// `class_manager` read lock and cloned the class's `Arc<str>` name just to
    /// conclude "no". `map_hash_key` and `map_keys_equal` both start with an
    /// unbox attempt, so a single String-keyed `ConcurrentHashMap.get` paid
    /// that seven times over (measured: 7.2 locked lookups per `get`), and the
    /// contended lock plus the `Arc` refcount traffic on the shared
    /// `java/lang/String` metadata is what made the lookup collapse under
    /// concurrency. "Is not a wrapper" is exactly as durable a fact as "is" —
    /// a `ClassId`'s name is fixed for the life of the VM — so both answers are
    /// cached now.
    pub(in crate::vm::vm_exec) static NON_PRIMITIVE_WRAPPER_CLASS_CACHE: std::cell::Cell<Option<(usize, ClassId)>> =
        const { std::cell::Cell::new(None) };
}

/// Shared tail of `fast_unbox_primitive_wrapper`: read the boxed primitive out
/// of an object whose class has already been confirmed to be a JDK wrapper.
pub(in crate::vm::vm_exec) fn read_wrapper_primitive_field(shared: &SharedVm, obj: ObjectRef) -> Option<Value> {
    match shared.mem.heap.get_field(obj, 0) {
        value @ (Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)) => {
            Some(value)
        }
        _ => None,
    }
}

pub(in crate::vm::vm_exec) fn recover_stale_lambda_receiver_from_native_pins(
    shared: &SharedVm,
    thread: &JvmThread,
    receiver_class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<ObjectRef> {
    if is_object_member(method_name, descriptor) {
        return None;
    }

    // Perf: check the cheap sentinel/cached-ClassId comparisons before ever
    // taking the `class_manager` RwLock — this runs on EVERY non-Object-method
    // invoke_virtual (e.g. every `HashMap.put`/`.get` call), and the vast
    // majority of receivers are ordinary, non-stale objects. See
    // reference_hashmap_native_call_dispatch_overhead_20260711.
    let stale_object_receiver = receiver_class_id == ClassId::new(0)
        || receiver_class_id == ClassId::new(u32::MAX)
        || object_class_id(shared) == Some(receiver_class_id);
    if !stale_object_receiver {
        return None;
    }

    let sam_arg_count = crate::runtime::interpreter::split_method_descriptor(descriptor)
        .0
        .len();
    if sam_arg_count != args.len() {
        return None;
    }

    // No `lambda_proxies` guard is held across the loop: the compatibility
    // screen asks `is_lambda_proxy_class`, and a nested `parking_lot` read
    // blocks behind a queued writer (a concurrent lambda bootstrap). The
    // screen never loads, so `args` and `pinned` stay valid raw addresses.
    for pinned in thread.native_pin_roots.iter().rev().copied() {
        let pinned = shared.mem.heap.load_and_forward(pinned);
        let pinned_class_id = shared.mem.heap.class_id_of(pinned);
        let Some(call_site) = shared.classes.lambda_call_site_for(pinned_class_id) else {
            continue;
        };

        if crate::runtime::interpreter::lambda_accepts_descriptor(
            shared,
            pinned_class_id,
            &call_site,
            method_name,
            descriptor,
        ) && crate::runtime::interpreter::lambda_args_sam_compatible(shared, descriptor, args)
        {
            if crate::runtime::env_cache::nsme_dbg() {
                eprintln!(
                    "[NSME_DBG] recovered stale lambda receiver for {}{} from native pins",
                    method_name, descriptor
                );
            }
            return Some(pinned);
        }
    }

    None
}

/// Call a native callback, catching panics and converting them to
/// `MethodCallFailed` so that a bug in a native method doesn't crash the VM.
/// Cached `CRATONVM_DBG_YOUNGSCAN` gate (bc math-ec `0x4` mutator-write hunt).
#[inline]
pub(in crate::vm::vm_exec) fn youngscan_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Cached `CRATONVM_DBG_BLOCKGC` gate — blocked-GC/pin canaries on the
/// native-call funnel and the pin/unpin paths. PERF (perf/halfgap-20260717):
/// the uncached `cratonvm_types::flags::runtime_var_os` probes of this flag ran on EVERY native
/// call (the PIN-UNDERFLOW guard) and on pin-table operations; `getenv`
/// linear-scans `environ`, and the probes measured ~7% of a HashMapOnly-4M
/// run. Same read-once semantics every other debug flag in this codebase
/// uses (see `dbg_jit_alloc_filter`'s identical history: a per-call getenv
/// was once ~13% of binarytrees-18).
#[inline]
pub(in crate::vm::vm_exec) fn blockgc_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| false)
}

/// gci i48 lane jl48: may the full wake of a JNI native call in native keep
/// the snapshot its window's pauses folded instead of refreshing it
/// (`NativeContextImpl::check_post_block_gc_in_native`)? Pure. Each refusing
/// term is a way the frames, or what the deposit read beside them, can differ
/// from what the last full deposit scanned:
///
/// * `frame_writes_waiting`: a deferred debugger frame write is pending;
/// * `had_deferred_chain`: the deposit left its compiled chain to the
///   collector (never for a JNI in-native deposit; refused all the same);
/// * `blockgc`: `CRATONVM_DBG_BLOCKGC`, whose checks run in the rebuild;
/// * `!redefinitions_current`: a redefinition the frames are not converted
///   for (the wake's tail converts them, after this decision);
/// * `jit_depth != 0`: compiled frames below the native. Their conservative
///   words, pins and coverage proof are the deposit's (or, with the band at
///   the pause, the pause's, and the band is withdrawn at this wake), so only
///   a thread whose frames are all interpreted keeps.
pub(in crate::vm::vm_exec) fn in_native_wake_keeps_snapshot(
    frame_writes_waiting: bool,
    had_deferred_chain: bool,
    blockgc: bool,
    redefinitions_current: bool,
    jit_depth: usize,
) -> bool {
    !frame_writes_waiting
        && !had_deferred_chain
        && !blockgc
        && redefinitions_current
        && jit_depth == 0
}

/// gci i48 lane jl48: `slot_origins` for the incremental re-entry after a
/// kept in-native wake: every slot `origins` tracked, now tracked from the
/// address the wake's write-back left in it (`orig == cur == so.cur`), with
/// its frame, index, kind and liveness. Replaces `tracked`.
pub(in crate::vm::vm_exec) fn rebase_slot_origins(
    origins: &[crate::threading::jvm_thread::SlotOrigin],
    tracked: &mut Vec<crate::threading::jvm_thread::SlotOrigin>,
) {
    use crate::threading::jvm_thread::SlotOrigin;
    tracked.clear();
    tracked.extend(origins.iter().map(|so| SlotOrigin {
        orig: so.cur,
        ..*so
    }));
}

/// gci i16/dp (i15/jk R3): keep, of `snapshot[from..]`, exactly the entries
/// whose address `keep` accepts, in their order; `snapshot[..from]` is
/// untouched. The deposit's operand-stack screen
/// (`deposit_root_snapshot_inner_with`) did this with `split_off(from)` and a
/// re-push, one heap `Vec` per frame with stack references; this compacts in
/// place. Same contents, same order (`gci_i16dp_deposit_screen_tests`).
pub(in crate::vm::vm_exec) fn deposit_keep_heap_addrs_from(
    snapshot: &mut Vec<ObjectRef>,
    from: usize,
    mut keep: impl FnMut(usize) -> bool,
) {
    let mut kept = from;
    let mut next = from;
    while next < snapshot.len() {
        let o = snapshot[next];
        if keep(o.as_ptr() as usize) {
            snapshot[kept] = o;
            kept += 1;
        }
        next += 1;
    }
    snapshot.truncate(kept);
}


/// Cached `CRATONVM_DBG_HEAPCOPY` gate — the managed-heap-destination tripwire
/// on `copy_to_native_memory`. PERF: that tripwire sat on the single-byte
/// `DirectByteBuffer.put` path, so an uncached `runtime_var_os` probe ran once
/// per byte written through a direct buffer. Same read-once treatment as
/// [`blockgc_dbg`] and for the same reason.
#[inline]
pub(in crate::vm::vm_exec) fn heapcopy_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// Cached `CRATONVM_DBG_STRAYSTACK` gate — native-side stray-receiver dump.
#[inline]
thread_local! {
    /// DBG (CRATONVM_DBG_STRAYSTACK): stack of (callback-address, name) of
    /// natives currently executing on this thread, so the stray-receiver dump
    /// can name + RVA-locate the one that wrote through a relocated/zeroed
    /// receiver.
    pub(in crate::vm::vm_exec) static CURRENT_NATIVE_STACK: std::cell::RefCell<Vec<(usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(in crate::vm::vm_exec) fn youngscan_straystack_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// `CRATONVM_DBG_PAD_ACCESS` (JIT round 16 wave 13, lane fieldsite2, OP16-2):
/// report a native read or write of a PADDED slot, that is a slot inside the
/// receiver class's `num_total_fields` that no field describes. Those slots
/// exist only because the synthetic floor
/// (`classloading` `apply_synthetic_floor`) padded a real class; a class whose
/// padded slots are never touched over a broad run is an exemption candidate
/// with empirical support, and one whose slots ARE touched names its native.
/// Print-only: nothing about the access changes. Off, it is one cached load on
/// the no-descriptor arm of `get_field` / `set_field` and one on
/// `set_field_raw`.
pub(in crate::vm::vm_exec) fn pad_access_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

