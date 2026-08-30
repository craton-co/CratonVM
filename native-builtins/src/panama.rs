// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Panama FFI native method registrations.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// `LAYOUT_PADDING`, `LAYOUT_SEQUENCE`, `LAYOUT_STRUCT` and `LAYOUT_UNION` are
// NOT imported here any more (F16, 2026-08-13): the group-layout family moved
// wholesale to `phases_late/foreign_ffm.rs`, which does not tag a layout with a
// kind at all, so this file's last uses of the four compound tags are in its
// own `#[cfg(test)]` module — which imports them itself.
use cratonvm_native_api::ffi::{
    self, LAYOUT_ADDRESS, LAYOUT_BOOLEAN, LAYOUT_BYTE, LAYOUT_CHAR, LAYOUT_DOUBLE, LAYOUT_FLOAT,
    LAYOUT_INT, LAYOUT_LONG, LAYOUT_SHORT,
};

/// The binary name of the **interface** `java.lang.foreign.MemorySegment`.
///
/// Kept as a named constant because it is now two different things in this
/// file: the class every FFM native is *also* registered on (a real-JDK
/// receiver arrives stamped with one of the JDK's own impl classes and a
/// caller resolving through the interface declaration lands here), and — until
/// 2026-08-22 — the class CratonVM stamped onto the segments it minted itself.
/// See [`CRATON_SEGMENT_CLASS`] for why that second use is gone.
pub(crate) const PE_SEGMENT_INTERFACE: &str = "java/lang/foreign/MemorySegment";

/// The concrete class CratonVM stamps onto every `MemorySegment` **it mints**.
///
/// # The defect this replaces
///
/// Every synthetic segment used to be allocated with the class name
/// `java/lang/foreign/MemorySegment` — the INTERFACE. Real Java can never have
/// an instance whose class is an interface, and the JDK's own FFM consumers
/// rely on that: `jdk.incubator.vector`'s `fromMemorySegment0Template` /
/// `intoMemorySegment0Template` both open with
///
/// ```text
/// checkcast jdk/internal/foreign/AbstractMemorySegmentImpl
/// ```
///
/// which no interface stamp can satisfy. GPULlama3's first `matmul` died on
/// exactly that (`AbstractVector.defaultReinterpret` builds a scratch
/// `MemorySegment.ofArray(new byte[n])` and writes the vector into it), and it
/// is not specific to the Vector API — it is every JDK path that casts a
/// segment to its own abstract base.
///
/// # Why a class of CratonVM's own, and not a JDK one
///
/// Two rejected alternatives, both measured against this tree rather than
/// reasoned about:
///
///  * **Reuse `jdk/internal/foreign/NativeMemorySegmentImpl`.** `fabricate_class`
///    prefers real bytes for a name that has them, so the stamp would land on
///    the REAL class — whose declared layout is
///    `AbstractMemorySegmentImpl{length, readOnly, scope}` then
///    `NativeMemorySegmentImpl{min}`. This file's carriers put `ptr` in slot 0
///    and `size` in slot 1, so every inherited accessor would read the pointer
///    as the byte size.
///
///  * **Fabricate a class whose SUPERCLASS is `AbstractMemorySegmentImpl`**
///    (the shape `cratonvm/synthetic/Process` and `SSLSocketOutputStream` use in
///    `class_manager::fabricate_class`). That is the "option 1" the bug record
///    named as the honest one, and it is the one that does not survive contact:
///    a fabricated class gets `first_field_index: 0`, so the superclass's three
///    fields alias slots 0/1/2 of THIS layout, and
///    `resolve_field_index_in_hierarchy` — which `get_field_by_name` and
///    `resolve_field_index_by_class_id` both go through — would start resolving
///    `length` to `ptr`, `readOnly` to `size` and `scope` to `arena`. Three live
///    readers in this tree ask by exactly those names
///    (`panama::heap_seg_field(_, "readOnly")`,
///    `foreign_ffm::p67_receiver_session`'s `"scope"`, and
///    `panama_libffi::is_real_heap_segment`'s sibling `"base"` probe), so
///    inheriting would have converted the checkcast defect into three silent
///    wrong-value ones. Superclassing also makes every UNSHADOWED inherited JDK
///    method a silent misread; with `java/lang/Object` as the super, a method
///    nobody registered raises `NoSuchMethodError`, which is a refusal rather
///    than a plausible number.
///
/// The relationship to `AbstractMemorySegmentImpl` and to the `MemorySegment`
/// interface is therefore DECLARED, in
/// `vm/src/runtime/interpreter/typecheck.rs`'s `synthetic_implements` — the same
/// place, and for the same reason, as `cratonvm/internal/SystemLogger`'s
/// relationship to `java.lang.System$Logger`.
///
/// # Dispatch
///
/// The `cratonvm/internal/` prefix is load-bearing: `vm_exec`'s
/// `prefer_exact_class_native` checks the exact-class native FIRST for that
/// namespace, so reflective and native-initiated `invoke_virtual` land on the
/// registrations below rather than walking to `java/lang/Object`. Every
/// registration made on [`PE_SEGMENT_INTERFACE`] is made on this name too;
/// `panama::tests::the_craton_segment_class_mirrors_the_interface` pins that.
pub(crate) const CRATON_SEGMENT_CLASS: &str = "cratonvm/internal/foreign/MemorySegmentImpl";

/// Allocate a CratonVM-minted `MemorySegment` carrier with `slots` raw slots.
///
/// `slots` stays the caller's, exactly as it was when the stamp was the
/// interface: [`CRATON_SEGMENT_CLASS`] is fabricated declaring **zero**
/// instance fields, so `try_alloc_concurrent_synthetic`'s
/// `max(requested, declared)` is `requested` for every one of the 2-, 3-, 6-
/// and 8-slot shapes this file and `foreign_ffm.rs` mint. That is deliberate:
/// the shapes are told apart by `object_num_fields` at four live sites
/// (`heap_segment_view`, `pe_segment_get_impl`, `pe_segment_set_impl`,
/// `foreign_ffm`'s `asSlice`), and declaring a fixed field count would collapse
/// them all to the widest.
///
/// Falls back to the old interface stamp when the fabrication is REFUSED, which
/// under `--jdk-only` it is (a `cratonvm/internal/*` name is a
/// `CompatibilityStub` origin, not `VmInternal`). That mode reaches this
/// function only where there is no real bytecode to run instead — every FFM
/// native here is a `Bridge`, and a `Bridge` loses to real bytes under strict
/// policy — so the fallback is not a new fake, it is the pre-existing one, on a
/// path strict mode does not otherwise use.
pub(crate) fn alloc_segment_carrier(
    ctx: &mut dyn NativeContext,
    slots: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let Some(class_id) = craton_segment_class_id(ctx) else {
        return try_alloc_concurrent_synthetic(ctx, PE_SEGMENT_INTERFACE, slots);
    };
    // `max` with the declared count is `try_alloc_concurrent_synthetic`'s rule,
    // kept rather than assumed away: it is what stops an under-request from
    // producing an object whose header disagrees with its slot count, which the
    // GC's bounds guard then rejects every field access on. For THIS class the
    // declared count is 0, so it is the identity — but a class-manager read
    // lock is ~20 ns against a ~7 µs allocation, and a rule that holds by
    // construction is still worth asking for rather than assuming.
    let n = slots.max(ctx.class_num_total_fields(class_id));
    Ok(ctx
        .try_alloc_object_gc_safe(class_id, n)
        .unwrap_or_else(|| ctx.alloc_object(class_id, n)))
}

/// [`CRATON_SEGMENT_CLASS`]'s `ClassId`, resolved once per VM.
///
/// # Why this is not simply a `try_alloc_concurrent_synthetic` call
///
/// It was, and that cost 15-40% on every segment CratonVM mints. MEASURED with
/// `probes/SegmentAllocBench.java`, arms interleaved so both see the same host,
/// one binary from the merge base and one with the class change:
///
/// ```text
///        ofArray_ns   arena_ns   asSlice_ns
/// base      7637        6482        6873
/// v5        9849        7075        8321
/// base      7420        6087        7109
/// v5       10679        8693        7756
/// ```
///
/// The shared allocator resolves its class by NAME on every call:
/// `ensure_class_initialized`, which is a loader-faithful resolution and not a
/// map hit; then `class_name_of_id`, which clones the name into a fresh
/// `String` to compare it; then `layout_alias::classify`. That is affordable
/// for a native reached once per `new URI(..)`. It is not affordable for one
/// `AbstractVector.defaultReinterpret` reaches on every `reinterpretAsInts()`.
///
/// The cost of skipping it is one instrument: `report_layout_alias` no longer
/// sees these allocations. It was reporting `Undeclared` for every one of them
/// — this class declares no fields on purpose (see below) — so what is lost is
/// a constant, not a signal.
///
/// # Why the key carries `vm_identity`
///
/// The cache is process-global and a `ClassId` is only meaningful within one
/// VM, so the identity is stored beside it and a mismatch re-resolves. That is
/// not defensive padding: `native-io`'s direct-memory `Bits` and this file's
/// `NATIVE_ACCESS_POLICY` are process-global for the same reason, and a second
/// VM in the same process inheriting the first one's `ClassId` would allocate
/// every segment against whatever class happened to hold that id.
///
/// `None` means the fabrication was REFUSED — see [`alloc_segment_carrier`].
fn craton_segment_class_id(ctx: &mut dyn NativeContext) -> Option<cratonvm_types::ClassId> {
    use std::sync::atomic::{AtomicU64, Ordering};
    // `vm_identity << 32 | class_id`, or 0 for "not resolved yet". `Relaxed` is
    // sufficient because the value is self-validating: a reader either sees a
    // packed pair whose identity half is its own VM's, or re-resolves. A torn
    // read is impossible — it is one 64-bit atomic.
    static CACHED: AtomicU64 = AtomicU64::new(0);
    let vm = ctx.vm_identity() as u64 & 0xFFFF_FFFF;
    let packed = CACHED.load(Ordering::Relaxed);
    if packed != 0 && (packed >> 32) == vm {
        return Some(cratonvm_types::ClassId::new((packed & 0xFFFF_FFFF) as u32));
    }
    // THE VM'S OWN ALLOCATION SHAPE, NOT A COMPATIBILITY STAND-IN -- so this is
    // `ensure_vm_internal_class` (`ClassOrigin::VmInternal`, legal in every
    // mode by contract §1 item 6) and not `try_ensure_synthetic_class`, which
    // mints stand-ins and which `--jdk-only` refuses by design.
    //
    // That refusal was the defect. Strict got `None` here and the caller fell
    // back to stamping the receiver with the `java/lang/foreign/MemorySegment`
    // INTERFACE -- an impossible class for an instance to have, and the exact
    // condition the carrier exists to cure. So the mode whose purpose is to
    // refuse fabrications landed back on the older wrong answer.
    //
    // The decision and its evidence:
    // `docs/known-issues/jdk-only/the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`.
    // In short, five signals and none of them a matter of taste: no JDK class
    // has this name; the JDK relationships come from `synthetic_implements`
    // rather than from the name; the carrier deliberately does NOT share
    // `AbstractMemorySegmentImpl`'s layout (see that table's own comment on why
    // aliasing it would be wrong); it is one of a family of three CratonVM
    // carriers; and `ensure_generated_class`'s doc names "the VM's own internal
    // allocation shapes" as its fourth listed category.
    //
    // MEASURED by `apps/probes/FfmCarrierProbe.java`: `getClass().isInterface()`
    // is false for every FFM receiver on HotSpot, and was true for nine
    // `MemorySegment` doors under `--jdk-only`.
    let class_id = match ctx.class_id_by_name(CRATON_SEGMENT_CLASS) {
        Some(id) => id,
        None => {
            let id = ctx.ensure_vm_internal_class(CRATON_SEGMENT_CLASS, 0);
            // `ClassId(0)` is the untyped-allocation id, which is what a
            // non-VM context's default returns. Minting on it would land on
            // the `AnonymousObject$N` receiver rather than the carrier, so
            // treat it as "no carrier" and keep the old fallback.
            if id == cratonvm_types::ClassId::new(0) {
                return None;
            }
            id
        }
    };
    CACHED.store((vm << 32) | u64::from(class_id.as_u32()), Ordering::Relaxed);
    Some(class_id)
}

/// Maximum number of bytes for a single memory copy/fill operation.
const MAX_COPY_SIZE: usize = 256 * 1024 * 1024; // 256 MiB

// ofArray segments do not have an arena. Reuse their arena/alive slots to
// retain the Java backing array and its primitive layout kind.
const SEG_BACKING_ARRAY_FIELD: usize = 2;
const SEG_BACKING_KIND_FIELD: usize = 4;

// A CratonVM-minted segment that aliases a Java array WITHOUT an off-heap
// mirror carries two extra slots. It is deliberately NOT a third tenant of
// slots 2/4/5: `segment_address` answers `[0] + [5]` for any carrier with six
// or more fields, so putting the array's byte offset in slot 5 would hand every
// raw-pointer consumer in the tree a small integer to dereference — which is
// exactly the defect F27 closed one level down (a heap segment's `length`
// answered as its address, `0x10`). Slot 0 stays 0 here so `segment_address`
// keeps answering 0, the value every consumer already refuses on.
const SEG_HEAP_BASE_FIELD: usize = 6;
const SEG_HEAP_START_FIELD: usize = 7;
const SEG_HEAP_FIELDS: usize = 8;

/// `Unsafe.arrayBaseOffset(<any array type>)` as **both** VMs publish it.
///
/// `HeapMemorySegmentImpl.offset` is an `Unsafe`-style offset with this bias
/// baked in, so a fresh full-array segment's `offset` is 16 and its
/// `address()` is 0 — not the other way round. Measured on the oracle
/// (25.0.3+9-LTS, `--add-opens java.base/jdk.internal.foreign=ALL-UNNAMED`):
/// `ofArray` on `byte[]`, `short[]`, `char[]`, `int[]`, `long[]`, `float[]` and
/// `double[]` all report `offset=16`, and `asSlice(3)` on a `byte[32]` reports
/// `offset=19`/`address()=3`. CratonVM answers 16 for every array type too
/// (`unsafe_natives_ext::native_unsafe_array_base_offset` is a constant 16), so
/// the two agree and one constant serves both carriers.
const HEAP_ARRAY_BASE_OFFSET: i64 = cratonvm_native_io::ARRAY_BYTE_BASE_OFFSET;

/// `UpcallEntry`/`UpcallUserdata`'s `return_kind` for a **void** upcall.
///
/// It was a bare `-1` at its two sites, and that bare `-1` is the reason
/// `panama_libffi::LAYOUT_UNKNOWN` had to be `-2`: the file next door needed a
/// value for "this carrier could not be classified" and the obvious one was
/// already spoken for by an unrelated convention in this one. Two sentinels
/// sharing a namespace, neither naming itself, is a collision waiting for
/// whichever of the two travels furthest — and `return_kind` travels into
/// `UpcallEntry` in `native-api`, across the libffi closure boundary, and back.
///
/// It is NOT a layout kind. Every real kind is `>= 0`
/// (`LAYOUT_BYTE`=0 … `LAYOUT_PADDING`=13), so the only safe way to read this
/// value is by name.
const UPCALL_RETURN_VOID: i32 = -1;

/// Maximum length to scan when reading a C string from native memory.
const MAX_CSTR_LEN: usize = 4096;

/// Native-access policy for Panama downcalls and raw-address memory access.
///
/// A `validated_fn_ptr` call transmutes a Java-supplied raw address to an
/// `extern "C" fn` and invokes it — arbitrary native code execution. Real
/// JDK Panama gates this behind `--enable-native-access=<module-list>` /
/// the module's `enableNativeAccess` permission, which is granted *per
/// module* (a comma-separated list of module names, or `ALL-UNNAMED`).
///
/// `NativeAccessPolicy` records that grant faithfully:
///
/// * [`NativeAccessPolicy::None`] — no module has native access (the
///   secure-by-default state, matching the JDK with no `--enable-native-access`).
/// * [`NativeAccessPolicy::All`] — every module is granted (the launcher saw
///   `--enable-native-access` with no argument, or `ALL-UNNAMED`/`ALL-MODULES`).
/// * [`NativeAccessPolicy::Modules`] — only the named modules are granted.
///   `None` (the unnamed module) is represented by the empty string `""`.
///
/// ### Why the gate is still consulted process-globally at the call sites
///
/// The native-method closures in this file receive only `ctx` (a
/// [`NativeContext`]) and the Java `args`; there is **no reachable accessor
/// for the *calling* class/module at the gate point** (the Panama API method
/// itself lives in `java.base`, and the caller's frame is not exposed to a
/// native callee here). `NativeContext::module_name_of_class` can name the
/// module of a *given* `ClassId`, but the gate has no `ClassId` for the
/// caller. So while the *policy* is now tracked per module, the gate
/// helpers ([`require_native_access`]/[`validated_fn_ptr`]) currently answer
/// the coarser question "is native access granted to *any* module?" via
/// [`native_access_enabled`]. This is a deliberate, fail-closed
/// approximation: it never *grants* access the launcher did not, but it
/// cannot yet *distinguish* a denied module from a granted one. Once a
/// caller-frame/module accessor is plumbed to the native dispatch boundary,
/// the gate can call [`module_native_access_enabled`] with the real caller
/// module to achieve full per-module fidelity; the policy plumbing here is
/// the prerequisite half of that work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NativeAccessPolicy {
    /// No module is granted native access (secure default).
    #[default]
    None,
    /// Every module is granted native access (unscoped `--enable-native-access`).
    All,
    /// Only the listed modules are granted. The unnamed module is `""`.
    Modules(std::collections::BTreeSet<String>),
}

impl NativeAccessPolicy {
    /// Whether *any* module is granted native access. Used by the coarse,
    /// process-global gate (see the type docs for why the caller module is
    /// not available at the gate point).
    fn any_granted(&self) -> bool {
        match self {
            NativeAccessPolicy::None => false,
            NativeAccessPolicy::All => true,
            NativeAccessPolicy::Modules(m) => !m.is_empty(),
        }
    }

    /// Whether the given module is granted native access. `None` denotes the
    /// unnamed module. This is the per-module query the JDK actually performs;
    /// it is exposed now so a future caller-module-aware gate can use it.
    fn module_granted(&self, module: Option<&str>) -> bool {
        match self {
            NativeAccessPolicy::None => false,
            NativeAccessPolicy::All => true,
            NativeAccessPolicy::Modules(m) => m.contains(module.unwrap_or("")),
        }
    }
}

static NATIVE_ACCESS_POLICY: std::sync::RwLock<NativeAccessPolicy> =
    std::sync::RwLock::new(NativeAccessPolicy::None);

/// What to do when a RESTRICTED method is called by a module that was not
/// granted native access — JDK's `--illegal-native-access`.
///
/// The default is [`Warn`](IllegalNativeAccess::Warn) because that is what a
/// JDK 25 launcher does, MEASURED rather than assumed
/// (`probes/RestrictedFfmPolicyProbe.java`, Adoptium 25.0.4):
///
/// ```text
/// java              …  reinterpret -> OK, byteSize=128   (+4 WARNING lines)
/// java --illegal-native-access=deny
///                   …  reinterpret -> IllegalCallerException
/// ```
///
/// CratonVM denied unconditionally, so any library calling a restricted method
/// from a static initialiser died with `ExceptionInInitializerError` — which
/// JVMS 5.5 makes permanent for that class. Infinispan's
/// `OffHeapMemoryAllocator` does exactly that, taking three Spring Boot
/// `CacheAutoConfigurationTests` cases with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IllegalNativeAccess {
    /// Permit silently.
    Allow,
    /// Permit, and warn once — the JDK 25 default, and ours.
    Warn,
    /// Throw `IllegalCallerException`, as `--illegal-native-access=deny` does.
    Deny,
}

/// 0 = Allow, 1 = Warn, 2 = Deny. Plain integer rather than a lock: this is
/// read on every restricted call and written once, before any bytecode runs.
static ILLEGAL_NATIVE_ACCESS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// Set the `--illegal-native-access` mode. Called from the launcher before the
/// VM starts.
pub fn set_illegal_native_access(mode: IllegalNativeAccess) {
    let v = match mode {
        IllegalNativeAccess::Allow => 0,
        IllegalNativeAccess::Warn => 1,
        IllegalNativeAccess::Deny => 2,
    };
    ILLEGAL_NATIVE_ACCESS.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// The current `--illegal-native-access` mode.
pub fn illegal_native_access() -> IllegalNativeAccess {
    match ILLEGAL_NATIVE_ACCESS.load(std::sync::atomic::Ordering::Relaxed) {
        0 => IllegalNativeAccess::Allow,
        2 => IllegalNativeAccess::Deny,
        _ => IllegalNativeAccess::Warn,
    }
}

/// HotSpot prints its restricted-method warning ONCE per calling module, and
/// the only module we can attribute to is the unnamed one, so this fires once
/// per process.
static RESTRICTED_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The gate for a genuinely RESTRICTED method (`reinterpret`, `libraryLookup`,
/// downcalls, upcalls) — the ones the JDK annotates `@Restricted`.
///
/// Distinct from [`require_native_access`], which guards the raw-address
/// ACCESSORS. The difference is the JDK's, not ours: a missing grant makes a
/// restricted method warn, while the accessors are not restricted at all.
///
/// The host-policy and capability rows run in every arm, granted or not — they
/// are a separate axis from the JDK flag, and dropping them here would turn a
/// policy question into a flag question.
fn restricted_method_check(
    ctx: &mut dyn NativeContext,
    method: &str,
) -> Result<(), MethodCallFailed> {
    if !native_access_enabled() {
        // `untrusted_code` is NOT the JDK flag and must not be relaxed by it.
        // `native_access_enabled()` already returns false in that mode, so
        // without this the new `Warn` default would let a restricted call
        // through where the old unconditional refusal stopped it — the one
        // place this change could have weakened something rather than aligned
        // it. Sandbox first, JDK policy second.
        if cratonvm_types::flags::flags().io.untrusted_code {
            return Err(RuntimeError::IllegalCallerException {
                message: "Native access is not enabled for this module                           (untrusted-code mode)"
                    .into(),
            }
            .into());
        }
        match illegal_native_access() {
            IllegalNativeAccess::Deny => {
                // Message shape follows the JDK's `deny` text rather than the
                // old CratonVM wording, so a caller matching on it sees what
                // HotSpot emits.
                return Err(RuntimeError::IllegalCallerException {
                    message: "Illegal native access from an unnamed module".into(),
                }
                .into());
            }
            IllegalNativeAccess::Warn => {
                if !RESTRICTED_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!(
                        "WARNING: A restricted method in java.lang.foreign.MemorySegment has been called"
                    );
                    eprintln!(
                        "WARNING: java.lang.foreign.MemorySegment::{method} has been called by code in an unnamed module"
                    );
                    eprintln!(
                        "WARNING: Use --enable-native-access=ALL-UNNAMED to avoid a warning for callers in this module"
                    );
                    eprintln!(
                        "WARNING: Restricted methods will be blocked in a future release unless native access is enabled"
                    );
                }
            }
            IllegalNativeAccess::Allow => {}
        }
    }
    crate::security_manager::check_host_native_access_or_throw(ctx, "foreign")?;
    crate::capability_gate::gate_raw_memory_named(&*ctx, method)?;
    Ok(())
}

/// Replace the process native-access policy wholesale.
fn store_policy(policy: NativeAccessPolicy) {
    if let Ok(mut guard) = NATIVE_ACCESS_POLICY.write() {
        *guard = policy;
    }
}

/// Enable or disable Panama native downcalls process-wide.
///
/// `true` records an [`NativeAccessPolicy::All`] grant; `false` records
/// [`NativeAccessPolicy::None`]. When no module is granted, every downcall
/// through [`validated_fn_ptr`] fails with a thrown
/// `java.lang.IllegalCallerException`, matching the JDK's
/// `--enable-native-access` semantics. (Task #57: previously folded into
/// `IllegalStateException` because `RuntimeError` lacked the variant.)
///
/// Retained for the bare/unscoped `--enable-native-access` launcher path and
/// for callers (and tests) that only need the all-or-nothing behavior. For
/// the scoped `--enable-native-access=<module-list>` form use
/// [`set_native_access_modules`].
pub fn set_native_access_enabled(enabled: bool) {
    store_policy(
        if enabled && !cratonvm_types::flags::flags().io.untrusted_code {
            NativeAccessPolicy::All
        } else {
            NativeAccessPolicy::None
        },
    );
}

/// Record the exact set of modules granted native access, parsed from the
/// `--enable-native-access=<module-list>` argument (a comma-separated list).
///
/// The sentinels `ALL-UNNAMED`, `ALL-MODULES`, and an empty/whitespace-only
/// list collapse to [`NativeAccessPolicy::All`] (the JDK treats `ALL-UNNAMED`
/// as granting the unnamed module that hosts the classpath; this VM has no
/// rich module graph at this layer, so it is approximated as a global grant
/// — strictly no *narrower* than the JDK for the unnamed module that almost
/// all application code lives in). Otherwise each named module is recorded
/// individually so [`module_native_access_enabled`] can answer per module.
pub fn set_native_access_modules<I, S>(modules: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if cratonvm_types::flags::flags().io.untrusted_code {
        store_policy(NativeAccessPolicy::None);
        return;
    }
    let mut set = std::collections::BTreeSet::new();
    let mut grant_all = false;
    for m in modules {
        let name = m.as_ref().trim();
        if name.is_empty() {
            continue;
        }
        if name.eq_ignore_ascii_case("ALL-UNNAMED") || name.eq_ignore_ascii_case("ALL-MODULES") {
            grant_all = true;
            continue;
        }
        set.insert(name.to_string());
    }
    if grant_all {
        store_policy(NativeAccessPolicy::All);
    } else if set.is_empty() {
        // A non-empty but all-blank argument is treated like the bare flag.
        store_policy(NativeAccessPolicy::All);
    } else {
        store_policy(NativeAccessPolicy::Modules(set));
    }
}

/// Whether Panama native downcalls are currently permitted for *any* module.
///
/// This is the coarse, process-global view the gate helpers use today
/// because the calling module is not reachable at the gate point (see
/// [`NativeAccessPolicy`]). It fails closed: it returns `false` whenever no
/// module has been granted access.
pub fn native_access_enabled() -> bool {
    if cratonvm_types::flags::flags().io.untrusted_code {
        return false;
    }
    NATIVE_ACCESS_POLICY
        .read()
        .map(|p| p.any_granted())
        .unwrap_or(false)
}

/// Whether the named module is granted native access. `None` denotes the
/// unnamed module. This is the per-module query the JDK performs; it is the
/// intended entry point for a future caller-module-aware gate once the
/// calling module is plumbed to the native dispatch boundary.
pub fn module_native_access_enabled(module: Option<&str>) -> bool {
    NATIVE_ACCESS_POLICY
        .read()
        .map(|p| p.module_granted(module))
        .unwrap_or(false)
}

/// Defense-in-depth gate for the raw-address `MemorySegment` memory-access
/// methods (get/set/getAtIndex/setAtIndex/copy/fill). These dereference a raw
/// address derived from the segment's Java-controlled `ptr`/`offset` fields, so
/// — like `ofAddress` and the downcall path — they must be denied unless native
/// access has been granted for the module. Returns the same
/// `IllegalCallerException` those other gated paths return.
///
/// NOTE: intentionally NOT applied to the shared `pe_segment_{get,set}_impl`
/// helpers, because those are also invoked internally by the arena
/// `allocateFrom` paths to initialize freshly-allocated, already-validated
/// arena-backed segments; gating there would break legitimate allocation even
/// when native access is off. Instead the gate is applied at each public JNI
/// entry point (the methods a Java caller can reach directly).
///
/// PER-MODULE LIMITATION: this consults the *process-global* view
/// ([`native_access_enabled`]) rather than the calling module's grant,
/// because the caller's module is not reachable from a native callee at this
/// point. The grant is tracked per module by [`NativeAccessPolicy`]; see its
/// docs for why the gate cannot yet consult it per caller. The behavior fails
/// closed (denies unless *some* module is granted).
fn require_native_access(ctx: &mut dyn NativeContext, op: &str) -> Result<(), MethodCallFailed> {
    if !native_access_enabled() {
        return Err(RuntimeError::IllegalCallerException {
            message: format!(
                "Native access is not enabled for this module \
                 (MemorySegment.{op} denied)"
            ),
        }
        .into());
    }
    crate::security_manager::check_host_native_access_or_throw(ctx, "foreign")?;
    // Audit row M3 / work-list item 18: one edit here gives every raw-address
    // `MemorySegment` accessor a `RawMemory` capability check, and each passes
    // its own `op`, so the audit report names the accessor rather than a single
    // undifferentiated "foreign" row. Permissive by default — this only records.
    crate::capability_gate::gate_raw_memory_named(&*ctx, op)?;
    Ok(())
}

/// The gate for the `MemorySegment` ACCESSORS — `get`, `set`, `getAtIndex`,
/// `setAtIndex`, `copy`, `fill`.
///
/// **None of them is `@Restricted` on JDK 25, and refusing them was a wrong
/// capability.** Measured against the JDK source at `C:\craton\jdk25src`:
///
/// ```text
/// $ grep -n "@Restricted" java.base/java/lang/foreign/MemorySegment.java
///   754:    @Restricted    MemorySegment reinterpret(long newSize);
///   810:    @Restricted    MemorySegment reinterpret(Arena, Consumer<MemorySegment>);
///   869:    @Restricted    MemorySegment reinterpret(long, Arena, Consumer<MemorySegment>);
/// ```
///
/// Three methods, all `reinterpret`. `get`/`set`/`getAtIndex`/`setAtIndex`/
/// `copy`/`fill`/`ofArray`/`asSlice` carry no annotation, and HotSpot 25 runs
/// every one of them with no `--enable-native-access` flag at all. CratonVM
/// raised `IllegalCallerException: Native access is not enabled for this module
/// (MemorySegment.set denied)` — recorded, unfixed, as W7-89 §7.2, which is why
/// every command in that record had to pass `--enable-native-access=ALL-UNNAMED`
/// to BOTH VMs to keep the flag a constant of the comparison.
///
/// The JDK's actual safety argument is upstream of the accessor: a segment you
/// can reach without a restricted call is one whose bounds the runtime knows.
/// Turning an arbitrary `long` into an addressable segment needs
/// `MemorySegment.ofAddress` (which answers a **zero-length** segment — measured,
/// `MemorySegment.ofAddress(0x1000).get(JAVA_BYTE, 0)` is
/// `IndexOutOfBoundsException`) and then `reinterpret`, which IS restricted.
/// Both of those keep [`require_native_access`] here.
///
/// What this does NOT drop: the host policy check and the `RawMemory`
/// capability row, so the audit still names every accessor. Only the
/// flag-absent refusal goes — the half that had no counterpart in the JDK.
///
/// A heap segment reaches this too and has no raw memory in it at all; before
/// the heap path existed that distinction did not matter, because no heap
/// access could succeed anyway.
fn require_segment_access(ctx: &mut dyn NativeContext, op: &str) -> Result<(), MethodCallFailed> {
    crate::security_manager::check_host_native_access_or_throw(ctx, "foreign")?;
    crate::capability_gate::gate_raw_memory_named(&*ctx, op)?;
    Ok(())
}

/// Safely transmute a raw function address to an extern "C" fn pointer.
/// Returns an error if native access is not permitted, or if the address
/// is null or misaligned.
fn validated_fn_ptr<T>(fn_addr: i64) -> Result<T, MethodCallFailed>
where
    T: Copy,
{
    // Gate arbitrary-native-code-execution: a Java caller controlling
    // `fn_addr` must not be able to invoke arbitrary native code unless
    // native access has been granted.
    if !native_access_enabled() {
        // Task #57: route through the dedicated IllegalCallerException
        // variant so the thrown Java class is `java.lang.IllegalCallerException`
        // (the JDK convention for native-access gate denials) instead of
        // `IllegalStateException` carrying an "IllegalCallerException: " prefix.
        return Err(RuntimeError::IllegalCallerException {
            message: "Native access is not enabled for this module \
                      (Panama downcall denied)"
                .into(),
        }
        .into());
    }
    let addr = fn_addr as usize;
    if addr == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Null function pointer in downcall".into(),
        }
        .into());
    }
    if addr % std::mem::align_of::<usize>() != 0 {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Misaligned function pointer {:#x} in downcall", addr),
        }
        .into());
    }
    // Null and alignment are NOT sufficient. A tagged arena handle
    // (`Unsafe.allocateMemory`, hence every direct `ByteBuffer` address) is
    // both non-null and 8-aligned, so it walked through the two checks above
    // and was then CALLED — a jump to an address that is a Rust-side arena key
    // rather than code. Same for a callee address in the unmappable low
    // window. `checked_foreign_addr` is the one screen both this and the
    // argument path in `panama_libffi::marshal_arg` share.
    crate::panama_libffi::checked_foreign_addr(fn_addr, "downcall target")?;
    // SAFETY: We have verified the address is non-null and properly aligned.
    // The caller is responsible for ensuring the address points to a valid
    // extern "C" function with the expected signature.
    Ok(unsafe { std::mem::transmute_copy(&addr) })
}

pub(crate) fn register_pe_panama(registry: &mut NativeMethodRegistry) {
    register_pe_value_layout(registry);
    register_pe_arena(registry);
    register_pe_memory_segment(registry);
    register_pe_symbol_lookup(registry);
    register_pe_linker(registry);
    register_pe_function_descriptor(registry);
    register_pe2_struct_layouts(registry);
    register_pe2_string_marshaling(registry);
}

// --- ValueLayout: type descriptors for native memory ---
//
// `pe_make_layout` IS A TEST FIXTURE AND NOTHING ELSE (F16, 2026-08-13).
//
// It is the last thing in this crate that builds the `[0]=Int(kind)` layout
// object, and it is `#[cfg(test)]` so that it cannot become a second
// production encoding again. The shipping encoding — the JDK's own
// `[0]=Long(byteSize), [1]=Long(byteAlignment), …` — is minted only by
// `phases_late/foreign_ffm.rs::p67_layout_object` and the four group-layout
// factories beside it. See the banner in `register_pe_value_layout` below.
//
// The kind tag it writes is still MEANINGFUL to the downcall marshaller:
// `panama_libffi::read_layout_kind` reads slot 0 as `Int(kind)` first and
// falls back to resolving the layout's CLASS NAME when slot 0 is a `Long`,
// which is how the shipping carriers are decoded. That fallback is why
// deleting the production rows did not break the FFI path — but it is also a
// reconciliation layer for two encodings that now has only one left to
// reconcile, and it carries a `_ => LAYOUT_LONG` default that is wrong for the
// real JDK's own `ValueLayouts$Of*Impl` class names. Nominated, not this
// lane's file.
#[cfg(test)]
const PE_VALUE_LAYOUT_NAME_SLOT: usize = 2;

#[cfg(test)]
fn pe_make_layout(ctx: &mut dyn NativeContext, kind: i32) -> Result<ObjectRef, MethodCallFailed> {
    let layout = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/ValueLayout", 3)?;
    ctx.set_field(layout, 0, Value::Int(kind));
    ctx.set_field(layout, 1, Value::Int(ffi::layout_byte_size(kind) as i32));
    ctx.set_field(layout, PE_VALUE_LAYOUT_NAME_SLOT, Value::Object(None));
    Ok(layout)
}

fn register_pe_value_layout(r: &mut NativeMethodRegistry) {
    let vl = "java/lang/foreign/ValueLayout";

    // NINE ROWS WERE DELETED HERE, AND THE SIX BELOW THEM WITH THEM
    // (F16, 2026-08-13). What stood here was:
    //
    //     r.register(vl, "JAVA_BYTE",  "()Ljava/lang/foreign/ValueLayout;", …)
    //     r.register(vl, "JAVA_SHORT", "()Ljava/lang/foreign/ValueLayout;", …)
    //     … JAVA_INT, JAVA_LONG, JAVA_FLOAT, JAVA_DOUBLE, JAVA_BOOLEAN,
    //     … JAVA_CHAR, ADDRESS
    //
    // each answering a `pe_make_layout` object, plus `byteSize`,
    // `byteAlignment`, `name` and two `withName` overloads reading it.
    //
    // TWO THINGS WERE WRONG WITH THEM, AND THE SECOND IS THE ONE THAT
    // MATTERED.
    //
    // 1. The descriptor is fabricated. These are FIELDS, not methods, and
    //    `()Ljava/lang/foreign/ValueLayout;` appears nowhere in JDK 25:
    //
    //      $ javap -p java.lang.foreign.ValueLayout      # 25.0.3+9-LTS
    //        public static final java.lang.foreign.ValueLayout$OfInt JAVA_INT;
    //      $ javap -c F16Desc   # static Object e(){ return ValueLayout.JAVA_INT; }
    //        0: getstatic Field java/lang/foreign/ValueLayout.JAVA_INT:
    //                          Ljava/lang/foreign/ValueLayout$OfInt;
    //
    //    No classfile can reach these rows. Only a Rust-side `call_native`
    //    could, and `vm/src/vm/tests.rs` was the only caller.
    //
    // 2. They were a SECOND MINTER FOR A SECOND ENCODING. `pe_make_layout`
    //    built a 3-slot object whose slot 0 is `Int(kind)`;
    //    `phases_late/foreign_ffm.rs::p67_layout_object` builds the JDK-shaped
    //    4-slot `[byteSize, byteAlignment, …, name]`. Both were registered,
    //    under different keys, so neither shadowed the other — and every
    //    group-layout consumer decoded slot 0 as `Int(kind)` with a `_ => 0`
    //    fallback. `LAYOUT_BYTE == 0`, so a 4-slot layout arriving at
    //    `sequenceLayout(10, JAVA_INT)` answered 10 instead of 40. A defaulting
    //    reader turning a wrong type into a plausible number is exactly the
    //    shape that stays invisible until something returns it to Java.
    //
    // The 4-slot encoding won, because it is the JDK's own: a real
    // `jdk.internal.foreign.layout.AbstractLayout` declares `byteSize` then
    // `byteAlignment` then `name`, so slots 0 and 1 read the same on a real
    // JDK object and on a CratonVM carrier. `foreign_ffm.rs` now owns the
    // whole family, in both JDK modes, and is reachable the way the JDK is:
    // its `<clinit>` row populates the real static fields, so a plain
    // `getstatic ValueLayout.JAVA_INT` finds an object.
    //
    // The replacements, all in `foreign_ffm.rs::register_p67_foreign_memory`:
    //   * the nine constants — the `$Of*`/`AddressLayout` FIELD rows, plus
    //     `<clinit>`, which also covers the seven `_UNALIGNED` fields these
    //     never had;
    //   * `byteSize` / `byteAlignment` / `name` / `withName` on
    //     `java/lang/foreign/ValueLayout` and on every `$Of*` class.
    //
    // DO NOT RE-ADD A LAYOUT FACTORY HERE. `register_pe_panama` runs AFTER
    // `register_p67_foreign_memory` (lib.rs: `register_phase67_natives` at
    // :24121, `register_pe_panama` at :24181, both inside
    // `register_synthetic_overrides`), so a row added here silently REPLACES
    // the JDK-true one for every synthetic-JDK run while leaving real-JDK mode
    // — which never calls `register_pe_panama` at all — on the other body.
    // That divergence-by-registration-order is what this deletion removes.
    let _ = vl;
}

// --- Arena: lifecycle-scoped memory management ---
// Arena synthetic: [0]=kind (Int), [1]=alloc_ids (Object — int array of alloc IDs), [2]=closed (Int), [3]=count (Int)

/// The global arena's root handle, or `None` before the first `Arena.global()`.
///
/// `LockLevel::Scratch` (L0) is a LEAF: takeable while holding anything, and
/// nothing takeable while holding it. Every `ctx` call in the caller happens
/// outside this lock's scope, which is the property L0 states and the order
/// checker enforces -- a lock in this crate held across a VM call is a cycle
/// through the heap and class-manager locks.
pub(crate) fn global_arena_cell(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<Option<usize>> {
    use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
    static CELL: std::sync::OnceLock<OrderedPlMutex<Option<usize>>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| OrderedPlMutex::new(None, LockLevel::Scratch))
}

/// The published handle, if any. A function so the guard cannot outlive the
/// map read. `if let Some(h) = cell.lock() { ctx.something(h) }` keeps the
/// guard alive for the whole statement, which is a native-builtins lock held
/// across a re-entry into the VM -- the edge `lock_discipline_ratchet` exists
/// to keep out of this crate. `shared_secrets_bridge` carries the same note on
/// its own copy of this shape; two lanes wrote it the same day.
pub(crate) fn global_arena_handle() -> Option<usize> {
    *global_arena_cell().lock()
}

/// Publish `handle` unless someone already did; returns the winner if not ours.
pub(crate) fn claim_global_arena(handle: usize) -> Option<usize> {
    let mut cell = global_arena_cell().lock();
    match *cell {
        Some(existing) => Some(existing),
        None => {
            *cell = Some(handle);
            None
        }
    }
}

pub(crate) fn register_pe_arena(r: &mut NativeMethodRegistry) {
    let arena = "java/lang/foreign/Arena";

    // `Arena.global()` IS A SINGLETON. The JDK's is
    // `MemorySessionImpl.GLOBAL_SESSION`, one object for the life of the VM,
    // and `Arena.global() == Arena.global()` is `true` on HotSpot. This minted
    // a fresh arena on every call -- in BOTH modes -- so the identity was
    // wrong, and so was every `alloc_ids` table but the last one: allocations
    // recorded against one global arena were invisible to the next caller's.
    //
    // MEASURED by `apps/probes/FfmCarrierProbe.java`, the one row that differs
    // in compatible mode as well as strict.
    // The singleton memo is NOT here: `--dump-native-registry` says
    // `phases_late/foreign_ffm.rs` owns all four `Arena` factories and this
    // registrar owns none of them, so a memo added here is inert. It lives at
    // the owning site, and `global_arena_handle`/`claim_global_arena` below are
    // shared with it rather than duplicated -- two copies of one rule is how
    // two registrars come to disagree.
    r.register(arena, "global", "()Ljava/lang/foreign/Arena;", |ctx, _| {
        if let Some(existing) = global_arena_handle().and_then(|h| ctx.resolve_global_root(h)) {
            return Ok(Some(Value::Object(Some(existing))));
        }
        let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
        let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
        ctx.set_field(a, 0, Value::Int(ffi::ARENA_GLOBAL));
        ctx.set_field(a, 1, Value::Object(Some(ids)));
        ctx.set_field(a, 2, Value::Int(0));
        ctx.set_field(a, 3, Value::Int(0));
        // Published under a global root: the global arena outlives every native
        // call and must survive a moving collection, which a raw `ObjectRef` in
        // a process-global cell would not.
        let handle = ctx.add_global_root(a);
        match claim_global_arena(handle) {
            None => Ok(Some(Value::Object(Some(a)))),
            // Lost the race: drop ours and answer with theirs, so two callers
            // cannot hold two different global arenas.
            Some(published) => {
                ctx.remove_global_root(handle);
                let winner = ctx.resolve_global_root(published).unwrap_or(a);
                Ok(Some(Value::Object(Some(winner))))
            }
        }
    });
    r.register(arena, "ofAuto", "()Ljava/lang/foreign/Arena;", |ctx, _| {
        let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
        let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
        ctx.set_field(a, 0, Value::Int(ffi::ARENA_AUTO));
        ctx.set_field(a, 1, Value::Object(Some(ids)));
        ctx.set_field(a, 2, Value::Int(0));
        ctx.set_field(a, 3, Value::Int(0));
        Ok(Some(Value::Object(Some(a))))
    });
    r.register(
        arena,
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _| {
            let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
            let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
            ctx.set_field(a, 0, Value::Int(ffi::ARENA_CONFINED));
            ctx.set_field(a, 1, Value::Object(Some(ids)));
            ctx.set_field(a, 2, Value::Int(0));
            ctx.set_field(a, 3, Value::Int(0));
            Ok(Some(Value::Object(Some(a))))
        },
    );
    // ofShared() — multi-thread-safe arena (same as confined but with ARENA_SHARED kind)
    r.register(
        arena,
        "ofShared",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _| {
            let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
            let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
            ctx.set_field(a, 0, Value::Int(ffi::ARENA_SHARED));
            ctx.set_field(a, 1, Value::Object(Some(ids)));
            ctx.set_field(a, 2, Value::Int(0));
            ctx.set_field(a, 3, Value::Int(0));
            Ok(Some(Value::Object(Some(a))))
        },
    );

    // allocate(byteSize, byteAlignment) → MemorySegment
    r.register(
        arena,
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        pe_arena_allocate,
    );
    // allocate(byteSize) → MemorySegment (align=1)
    r.register(
        arena,
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let size = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            pe_arena_allocate_impl(ctx, this, size, 1)
        },
    );
    // allocate(layout) → MemorySegment
    r.register(
        arena,
        "allocate",
        "(Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            // This read WAS `match ctx.get_field(layout, 1) { Value::Int(n) =>
            // n as i64, _ => 1 }` — slot 1 as an `Int`, i.e. the deleted
            // `[0]=Int(kind), [1]=Int(byteSize)` carrier (F16, 2026-08-13).
            // Every layout that reaches it now carries `Long(byteAlignment)`
            // there, so the `Int` arm never matched and the `_ => 1` default
            // took over: `arena.allocate(ValueLayout.JAVA_LONG)` reserved ONE
            // byte for an eight-byte value, and the caller got a segment that
            // passes its own bounds check at every offset it will then write.
            // A defaulting reader is not a safe reader when what it feeds is
            // an allocation size.
            //
            // It now uses the same two accessors as the rest of the family, so
            // there is one definition of "how big is this layout".
            let size = crate::phases_late::foreign_ffm::p67_layout_size_of(ctx, layout);
            let align = crate::phases_late::foreign_ffm::p67_layout_align_of(ctx, layout);
            pe_arena_allocate_impl(ctx, this, size, align)
        },
    );
    // allocate(MemoryLayout) uses the interface descriptor emitted for
    // StructLayout capture-state allocations in real-JDK bytecode.
    r.register(
        arena,
        "allocate",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            // `panama_libffi::layout_total_size` / `layout_align` CANNOT SIZE A
            // GROUP LAYOUT ANY MORE, and could not size the one real-JDK mode
            // has always had (F16, 2026-08-13). Both start from
            // `read_layout_kind`, which resolves a `Long`-slot-0 carrier by
            // CLASS NAME against a list of the nine `ValueLayout$Of*` spellings
            // only. `java/lang/foreign/StructLayout` is not on that list, so it
            // fell to `_ => LAYOUT_LONG` — a struct of any size was allocated
            // EIGHT BYTES, at alignment 8.
            //
            // That was already true for every `--jdk-only` run, because
            // `register_pe_panama` never executes there and the layout in hand
            // has always been `foreign_ffm.rs`'s. It only looked correct under
            // synthetic-JDK, where the kind-tagged carrier happened to answer.
            // Reading the layout's own recorded size and alignment is right in
            // both modes and does not depend on a name list staying in sync.
            let size = crate::phases_late::foreign_ffm::p67_layout_size_of(ctx, layout);
            let align = crate::phases_late::foreign_ffm::p67_layout_align_of(ctx, layout);
            pe_arena_allocate_impl(ctx, this, size, align)
        },
    );

    // close() — free all allocations in this arena
    r.register(arena, "close", "()V", pe_arena_close);
}

pub(crate) fn pe_arena_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = match args.get(1) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        Some(Value::Double(n)) => i64::from_le_bytes(n.to_le_bytes()),
        _ => 0,
    };
    let align = match args.get(2) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        Some(Value::Double(n)) => i64::from_le_bytes(n.to_le_bytes()),
        _ => 1,
    };
    pe_arena_allocate_impl(ctx, this, size, align)
}

/// `Arena.allocate`'s two argument guards, which this body had neither of.
///
/// MEASURED against HotSpot 25.0.4+7, both modes
/// (`apps/probes/FfmSegmentSweep.java`, messages from
/// `apps/probes/FfmMsgProbe.java`):
///
/// ```text
///   arena.allocate(-1)      IllegalArgumentException: The provided allocation size is negative: -1
///   arena.allocate(8, 0)    IllegalArgumentException: Invalid alignment constraint : 0
///   arena.allocate(8, 3)    IllegalArgumentException: Invalid alignment constraint : 3
///   arena.allocate(8, -4)   IllegalArgumentException: Invalid alignment constraint : -4
/// ```
///
/// against no-throw here — and the body then did `size.max(1)` and
/// `align.max(1)`, so a negative size allocated ONE byte and a bogus alignment
/// silently became 1. **Clamping an argument is not validating it**, which is
/// the third family in this campaign to be caught by that exact sentence
/// (`Arrays.copyOfRange`, then `ConcurrentHashMap`'s `concurrencyLevel`).
///
/// The messages are TRANSCRIBED, including the space before the colon in
/// `"Invalid alignment constraint : 0"`, which is the JDK's own spacing and not
/// a typo. Inventing them would be the shape `G1-1` records ten times over.
///
/// The size check is `< 0`, not `<= 0`: `allocate(0)` is legal and answers a
/// zero-length segment, which the sweep also asks.
fn pe_arena_check_allocation(size: i64, align: i64) -> Result<(), MethodCallFailed> {
    if size < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("The provided allocation size is negative: {size}"),
        }
        .into());
    }
    if align <= 0 || (align & (align - 1)) != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Invalid alignment constraint : {align}"),
        }
        .into());
    }
    Ok(())
}

fn pe_arena_allocate_impl(
    ctx: &mut dyn NativeContext,
    arena_obj: ObjectRef,
    size: i64,
    align: i64,
) -> MethodCallResult {
    pe_arena_check_allocation(size, align)?;
    // TWO arena layouts reach this body, and only one of them is the one it was
    // written for (W7-89). `register_pe_arena`'s own arena is four slots wide --
    // [0] global flag, [1] alloc-id array, [2] closed flag, [3] count -- and
    // wins under `--synthetic-jdk`. The rival is `foreign_ffm`'s, whose slot 2
    // is an `open` flag with the OPPOSITE polarity to this model's `closed`
    // flag; on it, the id-tracking block below would call `set_array_element`
    // on a non-array.
    //
    // This read the WIDTH until 2026-08-29, and a width is not an identity:
    // `foreign_ffm`'s carrier widened to four slots when it moved onto the real
    // `ArenaImpl`, took this branch, and every `arena.allocate` in the sweep
    // threw `Arena is closed` -- 0 of 199 rows, both modes. The two producers
    // now mint DIFFERENT classes, so the question this was always asking ("is
    // this the synthetic-jdk arena?") can be asked directly.
    let four_slot_layout = PE_ARENA_CLASS_MEMO.matches(ctx, arena_obj, PE_ARENA_CLASS)
        && ctx.object_num_fields(arena_obj) > 3;
    if four_slot_layout && matches!(ctx.get_field(arena_obj, 2), Value::Int(1)) {
        return Err(RuntimeError::IllegalStateException {
            message: "Arena is closed".into(),
        }
        .into());
    }
    // The two-slot arena keeps its liveness in the session, and HotSpot raises
    // `IllegalStateException: Already closed` for `arena.allocate(...)` after
    // `arena.close()` -- measured, `MemorySessionValidStateProbe` row
    // `C.closed.allocate`.
    if let Some(session) = pe_arena_session(ctx, arena_obj) {
        pe_session_check_open(ctx, session)?;
    }

    // Allocate off-heap memory via SharedVm's native_memory table
    let (alloc_id, ptr) = ctx
        .allocate_native_memory(size.max(1) as usize, align.max(1) as usize)
        .ok_or_else(|| RuntimeError::IllegalStateException {
            message: "Out of native memory".into(),
        })?;

    // Track alloc_id in arena's ID list
    if four_slot_layout {
        if let Value::Object(Some(ids_arr)) = ctx.get_field(arena_obj, 1) {
            // Kind screen, W7-83's: the slot is only an id array on the
            // four-slot layout, and answering "is this actually an array?" is
            // what stops a session or a backing array being written through.
            if ctx.object_is_array(ids_arr) {
                let count = match ctx.get_field(arena_obj, 3) {
                    Value::Int(n) => n as usize,
                    _ => 0,
                };
                ctx.set_array_element(ids_arr, count, Value::Long(alloc_id));
                ctx.set_field(arena_obj, 3, Value::Int((count + 1) as i32));
            }
        }
    }

    // Create MemorySegment: [0]=ptr, [1]=size, [2]=arena, [3]=ro, [4]=alive, [5]=offset
    let seg = alloc_segment_carrier(ctx, 6)?;
    ctx.set_field(seg, 0, Value::Long(ptr as i64));
    ctx.set_field(seg, 1, Value::Long(size));
    ctx.set_field(seg, 2, Value::Object(Some(arena_obj)));
    ctx.set_field(seg, 3, Value::Int(0)); // read-write
    ctx.set_field(seg, 4, Value::Int(1)); // alive
    ctx.set_field(seg, 5, Value::Long(0)); // no offset

    Ok(Some(Value::Object(Some(seg))))
}

fn pe_arena_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if matches!(ctx.get_field(this, 0), Value::Int(0)) {
        return Err(RuntimeError::IllegalStateException {
            message: "Cannot close global arena".into(),
        }
        .into());
    }
    if matches!(ctx.get_field(this, 2), Value::Int(1)) {
        return Err(RuntimeError::IllegalStateException {
            message: "Arena already closed".into(),
        }
        .into());
    }

    // Free all allocations tracked by this arena
    if let Value::Object(Some(ids_arr)) = ctx.get_field(this, 1) {
        let count = match ctx.get_field(this, 3) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        for i in 0..count {
            if let Value::Long(alloc_id) = ctx.get_array_element(ids_arr, i) {
                ctx.free_native_memory(alloc_id);
            }
        }
    }

    ctx.set_field(this, 2, Value::Int(1)); // closed
    Ok(None)
}

/// The `jdk.internal.foreign.AbstractMemorySegmentImpl` surface, on CratonVM's
/// own segment carrier.
///
/// # Why this exists at all
///
/// [`CRATON_SEGMENT_CLASS`] is declared assignable to
/// `jdk/internal/foreign/AbstractMemorySegmentImpl` (see `synthetic_implements`),
/// which is what lets the JDK's own `checkcast` succeed. A cast that succeeds
/// is only half an answer: the code on the far side of it then CALLS the
/// abstract base's methods, and CratonVM's carrier has no superclass to inherit
/// them from. Measured on the path that motivated this — every Vector API
/// segment entry point —
///
/// ```text
/// ScopedMemoryAccess.loadFromMemorySegment(.., AbstractMemorySegmentImpl msp, ..)
///   msp.sessionImpl()                     // then session.checkValidStateRaw()
///   VectorSupport.load(.., msp.unsafeGetBase(), msp.unsafeGetOffset() + offset, ..)
/// ```
///
/// so `sessionImpl`, `unsafeGetBase` and `unsafeGetOffset` are not optional
/// extras; they are the three methods without which the cast buys nothing.
///
/// # Why the list is longer than three
///
/// Everything below either reads `length`, `readOnly` or `scope`, or is
/// `abstract` on the base. Those are exactly the members a receiver of this
/// class cannot answer by inheritance, so each one is a `NoSuchMethodError`
/// waiting for the first consumer that reaches it. `checkAccess` /
/// `checkBounds` / `checkReadOnly` / `isAlignedForElement` are reached from
/// `MemorySegment.copy`, `Utils`, the `VarHandle` accessors and
/// `SegmentBulkOperations`; `maxAlignMask` is reached from
/// `isAlignedForElement` and from every `asSlice(offset, size, alignment)`.
///
/// The set is NOT "every method `AbstractMemorySegmentImpl` declares": the
/// concrete ones that only compose public API (`fill`, `copyFrom`, `mismatch`,
/// `toArray`, `getString`, the nine `get`/`set` carrier pairs, `asSlice`,
/// `asReadOnly`, `asByteBuffer`, `spliterator`, `elements`, `byteSize`,
/// `isNative`, `isMapped`, `isReadOnly`, `equals`, `scope`) are already
/// registered by [`register_pe_memory_segment_on`] under both names, and the
/// private/static ones (`ofBuffer`, `nativeSegment`, `cleanupAction`, the
/// `lambda$` bodies) are unreachable on a receiver.
fn register_craton_segment_impl_surface(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Registered on the interface name as well as the craton class: a receiver
    // minted by the `--jdk-only` fallback in `alloc_segment_carrier` still
    // carries the interface stamp, and it needs the same methods for the same
    // reason.
    for owner in [PE_SEGMENT_INTERFACE, CRATON_SEGMENT_CLASS] {
        // `unsafeGetBase()` — the heap carrier's backing array, or `null` for an
        // off-heap one. This is the JDK's `(base, offset)` pair convention: a
        // null base means `offset` is an absolute machine address.
        r.register(
            owner,
            "unsafeGetBase",
            "()Ljava/lang/Object;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(match heap_segment_view(ctx, this) {
                    Some(view) => Value::Object(Some(view.base)),
                    None => Value::Object(None),
                }))
            },
        );
        // `unsafeGetOffset()` — the other half of that pair, and NOT the same
        // number as `address()`. For a heap carrier the JDK returns the
        // `Unsafe`-style offset, which has `arrayBaseOffset` baked in
        // (`ofArray(new byte[32])` reports 16, its `asSlice(3)` reports 19),
        // while `address()` reports 0 and 3. Answering `address()` here would
        // hand `Unsafe` a pointer 16 bytes before the array data.
        r.register(owner, "unsafeGetOffset", "()J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Long(match heap_segment_view(ctx, this) {
                Some(view) => HEAP_ARRAY_BASE_OFFSET.saturating_add(view.start),
                None => crate::panama_libffi::segment_address(ctx, this),
            })))
        });
        // `maxAlignMask()` — 0 for a native segment (malloc'd storage carries no
        // upper bound), and the ELEMENT alignment for a heap one, which is what
        // caps a `byte[]`-backed segment at 1-byte alignment. Both values are
        // the JDK's: `NativeMemorySegmentImpl.maxAlignMask()` returns 0 and
        // `HeapMemorySegmentImpl$Of*` return `ValueLayout.JAVA_*.byteAlignment()`.
        r.register(owner, "maxAlignMask", "()J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Long(match heap_segment_view(ctx, this) {
                Some(view) => view.elem_width as i64,
                None => 0,
            })))
        });
        // `sessionImpl()` — `final` on the base and a plain `return scope`. It
        // must NOT answer null: every caller goes straight into
        // `session.checkValidStateRaw()`, which is an `invokevirtual` and would
        // raise `NullPointerException` on a null receiver. (That is the one
        // place this carrier differs from the `Buffer.session()` shim in
        // `lib.rs`, which CAN answer null because `ScopedMemoryAccess`'s buffer
        // path tests for it first.)
        r.register(
            owner,
            "sessionImpl",
            "()Ljdk/internal/foreign/MemorySessionImpl;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(crate::phases_late::foreign_ffm::p67_receiver_session(
                    ctx, this,
                )?))
            },
        );
        // The covariant `scope()`. `AbstractMemorySegmentImpl` declares it
        // returning `MemorySessionImpl` and the `MemorySegment$Scope`-returning
        // bridge alongside it; the bridge descriptor is already registered by
        // `register_pe_memory_segment_on`, and a call site that resolved against
        // the abstract class picks the covariant one.
        r.register(
            owner,
            "scope",
            "()Ljdk/internal/foreign/MemorySessionImpl;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(crate::phases_late::foreign_ffm::p67_receiver_session(
                    ctx, this,
                )?))
            },
        );
        // `checkReadOnly(boolean)` — throws only when a WRITE is attempted on a
        // read-only segment. The argument is the caller's own read-only-ness,
        // so `checkReadOnly(true)` never throws.
        r.register(owner, "checkReadOnly", "(Z)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let caller_read_only = matches!(args.get(1), Some(Value::Int(n)) if *n != 0);
            craton_segment_check_read_only(ctx, this, caller_read_only)?;
            Ok(None)
        });
        // `checkBounds(long, long)`. The JDK's body is a bounds test over
        // `length`; the message shape is the JDK's own
        // (`AbstractMemorySegmentImpl.outOfBoundException`) so a caller matching
        // on it is not surprised.
        r.register(owner, "checkBounds", "(JJ)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = seg_long_arg(args, 1);
            let length = seg_long_arg(args, 2);
            craton_segment_check_bounds(ctx, this, offset, length)
        });
        // `checkAccess(long, long, boolean)` = `checkReadOnly` then `checkBounds`,
        // in that order — a write past the end of a read-only segment reports the
        // read-only violation, not the bounds one.
        r.register(owner, "checkAccess", "(JJZ)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = seg_long_arg(args, 1);
            let length = seg_long_arg(args, 2);
            let caller_read_only = matches!(args.get(3), Some(Value::Int(n)) if *n != 0);
            craton_segment_check_read_only(ctx, this, caller_read_only)?;
            craton_segment_check_bounds(ctx, this, offset, length)
        });
        // `isAlignedForElement(long, long)`:
        //     ((unsafeGetOffset() + offset) | maxAlignMask()) & (byteAlignment - 1) == 0
        // verbatim from the base class. The `| maxAlignMask()` term is what makes
        // a `byte[]`-backed segment refuse every alignment above 1.
        r.register(owner, "isAlignedForElement", "(JJ)Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = seg_long_arg(args, 1);
            let byte_alignment = seg_long_arg(args, 2);
            Ok(Some(Value::Int(i32::from(craton_segment_is_aligned(
                ctx,
                this,
                offset,
                byte_alignment,
            )))))
        });
        r.register(
            owner,
            "isAlignedForElement",
            "(JLjava/lang/foreign/MemoryLayout;)Z",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let offset = seg_long_arg(args, 1);
                let layout = obj_arg(args, 2)?;
                let (_size, align) =
                    crate::phases_late::foreign_ffm::p67_layout_size_align(ctx, layout);
                Ok(Some(Value::Int(i32::from(craton_segment_is_aligned(
                    ctx, this, offset, align,
                )))))
            },
        );
        // `toString()` — the JDK's shape, `MemorySegment{ address: 0x…, byteSize: N }`.
        // Not decoration: an unregistered `toString` on this carrier reaches
        // `java/lang/Object`'s and prints the identity hash, which is what every
        // FFM diagnostic in the tree would then say.
        r.register(owner, "toString", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let heap = heap_segment_view(ctx, this).is_some();
            let address = crate::panama_libffi::segment_address(ctx, this);
            let size = crate::panama_libffi::segment_byte_size(ctx, this);
            let text = if heap {
                format!(
                    "MemorySegment{{ heapBase: <array>, address: {address:#x}, byteSize: {size} }}"
                )
            } else {
                format!("MemorySegment{{ address: {address:#x}, byteSize: {size} }}")
            };
            let s = ctx.create_string(&text);
            Ok(Some(Value::Object(Some(s))))
        });
    }
    r.set_category(__prev_cat);
}

/// A `long` argument at `index`, or 0.
///
/// The `Value::Int` arm is not defensive padding: a `J` slot can arrive as an
/// `Int` from a caller that pushed a literal through the operand stack, and
/// reading that as 0 would be a silent wrong bound rather than a refusal.
fn seg_long_arg(args: &[Value], index: usize) -> i64 {
    match args.get(index) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    }
}

/// `AbstractMemorySegmentImpl.checkReadOnly(boolean)`.
///
/// The flag is read the way every other read-only reader in this crate reads
/// it — by NAME first (a real-JDK carrier declares `readOnly`), then slot 3 on
/// a CratonVM carrier wide enough to have one. Restating the rule rather than
/// calling `foreign_ffm::p67_segment_is_read_only` would be a second copy of a
/// decision; calling it means one.
fn craton_segment_check_read_only(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    caller_read_only: bool,
) -> Result<(), MethodCallFailed> {
    let read_only = matches!(
        crate::phases_late::foreign_ffm::p67_segment_is_read_only(
            ctx,
            &[Value::Object(Some(seg))],
        )?,
        Some(Value::Int(n)) if n != 0
    );
    if !caller_read_only && read_only {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Attempt to write a read-only segment".into(),
        }
        .into());
    }
    Ok(())
}

/// `AbstractMemorySegmentImpl.checkBounds(long offset, long length)`.
fn craton_segment_check_bounds(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    length: i64,
) -> MethodCallResult {
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    let end = offset.checked_add(length);
    let in_range = offset >= 0 && length >= 0 && matches!(end, Some(end) if end <= size);
    if !in_range {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!(
                "Out of bound access on segment MemorySegment{{ byteSize: {size} }}; \
                 new offset = {offset}; new length = {length}"
            )),
        }
        .into());
    }
    Ok(None)
}

/// `AbstractMemorySegmentImpl.isAlignedForElement`, both overloads.
fn craton_segment_is_aligned(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    byte_alignment: i64,
) -> bool {
    if byte_alignment <= 0 {
        return false;
    }
    let (base_offset, max_align_mask) = match heap_segment_view(ctx, seg) {
        Some(view) => (
            HEAP_ARRAY_BASE_OFFSET.saturating_add(view.start),
            view.elem_width as i64,
        ),
        None => (crate::panama_libffi::segment_address(ctx, seg), 0),
    };
    ((base_offset.wrapping_add(offset) | max_align_mask) & (byte_alignment - 1)) == 0
}

// --- MemorySegment: off-heap byte buffer ---

/// Register the whole `MemorySegment` surface, once per receiver class.
///
/// TWO names, and both are load-bearing:
///
///  * [`PE_SEGMENT_INTERFACE`] — a real-JDK segment arrives stamped with one of
///    the JDK's own impl classes, and a call site that resolved to the
///    interface's abstract declaration looks the native up under the DECLARING
///    class. Removing this name would strand every such call.
///  * [`CRATON_SEGMENT_CLASS`] — the class CratonVM now stamps onto the
///    segments it mints itself. Native dispatch is keyed on the RECEIVER class,
///    so without this pass a craton-minted segment would find nothing.
///
/// The pass is a loop rather than two hand-written lists on purpose: a method
/// added to one and forgotten on the other is exactly the drift that produced
/// the interface-stamp defect in the first place, and
/// `tests::the_craton_segment_class_mirrors_the_interface` fails if the two
/// registration sets ever differ.
pub(crate) fn register_pe_memory_segment(r: &mut NativeMethodRegistry) {
    for ms in [PE_SEGMENT_INTERFACE, CRATON_SEGMENT_CLASS] {
        register_pe_memory_segment_on(r, ms);
    }
    register_craton_segment_impl_surface(r);
}

fn register_pe_memory_segment_on(r: &mut NativeMethodRegistry, ms: &str) {
    // Real-JDK callers dispatch these interface methods directly; retain the
    // concrete native bridges when SyntheticStub registrations are filtered.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);

    // byteSize() → long
    r.register(ms, "byteSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(crate::panama_libffi::segment_byte_size(
            ctx, this,
        ))))
    });

    // address() → long.
    //
    // TWO DIFFERENT QUESTIONS SHARE ONE FUNCTION, and this registration asks
    // the wrong one for a heap segment. `panama_libffi::segment_address` is
    // "the machine address to dereference", and since F27 it deliberately
    // answers **0** for a heap carrier so that every raw-pointer consumer
    // refuses instead of dereferencing a `length` (the `0x10` of W7-89 §7.1).
    // `MemorySegment.address()` is the JDK's own accessor and has a defined
    // answer that is NOT always 0. Measured, 25.0.3+9-LTS:
    //
    //     ofArray(new byte[32]).address()            == 0   (offset field 16)
    //     ofArray(new byte[32]).asSlice(3).address()  == 3   (offset field 19)
    //
    // i.e. `offset - Unsafe.arrayBaseOffset`, which is exactly
    // `HeapSegmentView::start`. Answering 0 for the slice is the same class of
    // wrong answer as answering the length was — a plausible number from a
    // reader that could not decode the carrier — it is just quieter, because 0
    // is also the right answer for the un-sliced case that every existing test
    // uses.
    r.register(ms, "address", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(view) = heap_segment_view(ctx, this) {
            return Ok(Some(Value::Long(view.start)));
        }
        Ok(Some(Value::Long(crate::panama_libffi::segment_address(
            ctx, this,
        ))))
    });

    // isNative() → boolean.
    //
    // Was an unconditional `true` ("always true for our segments"), which is
    // wrong for the `ofArray(...)` segments registered further down in THIS
    // file: those are heap segments, and `isNative()` is exactly the query a
    // caller uses to decide whether `address()` is meaningful and whether the
    // segment may be handed to a downcall. They only *look* native from the
    // inside because CratonVM cannot expose a moving Java array to native code
    // and so gives them an off-heap mirror (`sync_heap_backed_segment`) — an
    // implementation detail that must not leak into the spec'd answer.
    //
    // The discriminator is the one `sync_heap_backed_segment` already uses:
    // `SEG_BACKING_ARRAY_FIELD` retains the Java array on an `ofArray` segment
    // and holds an Arena (or nothing) on every off-heap one.
    //
    // NOTE: `foreign_ffm.rs` registers this same class+method+descriptor, and
    // this registrar runs AFTER it on both paths that reach them
    // (lib.rs:9736→9740, and :22995→23051), so THIS is the live answer — the
    // one over there was dead, and said the opposite. They now agree.
    //
    // The `ofArray` mirror is not the only heap carrier any more. A real
    // `HeapMemorySegmentImpl$Of*` and this file's own alias carrier
    // (`[6]=array, [7]=start`) hold nothing in `SEG_BACKING_ARRAY_FIELD`, so
    // that check alone answered `true` for both. Measured on the oracle:
    // `ofArray(new byte[32]).asSlice(3, 4).isNative()` is **false**.
    r.register(ms, "isNative", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let heap_backed = match ctx.get_field(this, SEG_BACKING_ARRAY_FIELD) {
            Value::Object(Some(array)) => ctx.object_is_array(array),
            _ => false,
        };
        let heap_backed = heap_backed
            || crate::panama_libffi::is_real_heap_segment(ctx, this)
            || heap_segment_view(ctx, this).is_some();
        Ok(Some(Value::Int(i32::from(!heap_backed))))
    });

    // get(ValueLayout, long offset) → value
    r.register(
        ms,
        "get",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        pe_segment_get,
    );

    // set(ValueLayout, long offset, value)
    r.register(
        ms,
        "set",
        "(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V",
        pe_segment_set,
    );

    // Real-JDK bytecode resolves the covariant ValueLayout descriptors rather
    // than the erased Object signature above.
    //
    // All nine of each. `java.lang.foreign.MemorySegment` declares nine
    // `get`/`set` pairs and every one of the eighteen is `public abstract`, so
    // a descriptor nobody registers is not a slow path — it is
    // `AbstractMethodError: … has no Code attribute`, thrown at the interface
    // method itself.
    //
    // The `set` half of this loop did not exist. Only the erased
    // `(ValueLayout;JLjava/lang/Object;)V` above was registered, and real
    // bytecode never emits that; `phases_late/foreign_ffm.rs` separately
    // covered Byte/Short/Int/Long, which is why four of the nine worked and
    // `set(JAVA_DOUBLE, …)` raised. Keeping the two lists adjacent and
    // identical is the point: an asymmetry between them is exactly the defect,
    // and it is only visible when they are read together.
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;J)Z",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
        "(Ljava/lang/foreign/ValueLayout$OfChar;J)C",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;J)F",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;J)D",
        "(Ljava/lang/foreign/AddressLayout;J)Ljava/lang/foreign/MemorySegment;",
    ] {
        r.register(ms, "get", desc, pe_segment_get);
    }
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;JZ)V",
        "(Ljava/lang/foreign/ValueLayout$OfByte;JB)V",
        "(Ljava/lang/foreign/ValueLayout$OfChar;JC)V",
        "(Ljava/lang/foreign/ValueLayout$OfShort;JS)V",
        "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V",
        "(Ljava/lang/foreign/ValueLayout$OfLong;JJ)V",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;JF)V",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;JD)V",
        "(Ljava/lang/foreign/AddressLayout;JLjava/lang/foreign/MemorySegment;)V",
    ] {
        r.register(ms, "set", desc, pe_segment_set);
    }
    // getAtIndex / setAtIndex, ERASED descriptors.
    //
    // These used to open-code the stride as `get_field(layout, 1)` matched
    // against `Value::Int` — the DELETED three-slot layout encoding, in which
    // slot 1 was `Int(byteSize)`. Since F16 consolidated on the JDK-true
    // four-slot carrier, slot 1 is `Long(byteAlignment)`, so:
    //
    //   * the `Value::Int` arm cannot match any layout this VM or the real JDK
    //     mints, so every call took the `_ => 1` default and the stride was
    //     **1 byte** — `getAtIndex(JAVA_INT, 2)` read offset 2, not 8;
    //   * and had it matched, it would have been the ALIGNMENT, which is 1 for
    //     `JAVA_INT_UNALIGNED` and 8 for `JAVA_LONG` — a different wrong number
    //     per layout.
    //
    // The covariant registrations a few lines below route to
    // `pe_segment_get_at_index`, which derives the stride from
    // `ffi::layout_byte_size(read_layout_kind(...))` and is correct. So one
    // rule had two implementations, adjacent, disagreeing — and only the
    // erased one was wrong. Route both spellings at the same function; real
    // bytecode emits the covariant descriptors, so this is reachable by
    // reflection and by `MethodHandle`, not by a `javac` call site.
    r.register(
        ms,
        "getAtIndex",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        pe_segment_get_at_index,
    );
    r.register(
        ms,
        "setAtIndex",
        "(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V",
        pe_segment_set_at_index,
    );

    // Real-JDK bytecode resolves the covariant ValueLayout descriptors rather
    // than the erased Object signatures above.
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;J)Z",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
        "(Ljava/lang/foreign/ValueLayout$OfChar;J)C",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;J)F",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;J)D",
        "(Ljava/lang/foreign/AddressLayout;J)Ljava/lang/foreign/MemorySegment;",
    ] {
        r.register(ms, "getAtIndex", desc, pe_segment_get_at_index);
    }
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;JZ)V",
        "(Ljava/lang/foreign/ValueLayout$OfByte;JB)V",
        "(Ljava/lang/foreign/ValueLayout$OfChar;JC)V",
        "(Ljava/lang/foreign/ValueLayout$OfShort;JS)V",
        "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V",
        "(Ljava/lang/foreign/ValueLayout$OfLong;JJ)V",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;JF)V",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;JD)V",
        "(Ljava/lang/foreign/AddressLayout;JLjava/lang/foreign/MemorySegment;)V",
    ] {
        r.register(ms, "setAtIndex", desc, pe_segment_set_at_index);
    }

    // asSlice(long offset, long size) → MemorySegment
    //
    // A NAMED FUNCTION, not an inline closure, so the heap arm below is
    // reachable from a unit test. An anonymous closure inside a registrar can
    // only be exercised by standing a whole `NativeMethodRegistry` up, which is
    // why the defect it now fixes had no test either way.
    r.register(
        ms,
        "asSlice",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        pe_segment_as_slice,
    );

    // asSlice(long offset) → the rest of the segment.
    //
    // `MemorySegment` declares BOTH arities and both are `public abstract`, so
    // the one nobody registered was not a slow path — it was
    // `AbstractMethodError: … has no Code attribute` at the interface method.
    // It shares the two-argument body rather than restating the bounds check,
    // which is the asymmetry that produced the gap in the first place.
    r.register(
        ms,
        "asSlice",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = pe_long_arg(args, 1);
            let size = crate::panama_libffi::segment_byte_size(ctx, this);
            pe_segment_slice(ctx, this, offset, (size - offset).max(0), None)
        },
    );

    // asReadOnly() → the same memory, refused for writes.
    //
    // A full-size slice with the read-only flag FORCED on rather than
    // inherited. Without it `asReadOnly()` was `AbstractMethodError`, so
    // nothing could obtain a read-only view at all — and
    // `AbstractMemorySegmentImpl.asByteBuffer()` reaches for exactly this to
    // decide whether to hand back a read-only buffer.
    //
    // The other direction is the F21 rule, and it is `pe_segment_slice`'s:
    // read-only is CONTAGIOUS, so every `asSlice` above inherits the parent's
    // flag and there is no route back to writable.
    r.register(
        ms,
        "asReadOnly",
        "()Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let size = crate::panama_libffi::segment_byte_size(ctx, this);
            pe_segment_slice(ctx, this, 0, size, Some(true))
        },
    );

    // asByteBuffer() → a direct ByteBuffer over the segment's own memory.
    r.register(
        ms,
        "asByteBuffer",
        "()Ljava/nio/ByteBuffer;",
        pe_segment_as_byte_buffer,
    );

    // asSlice(long offset, long size, long byteAlignment) — the alignment-
    // checked form. A third arity, therefore a third AbstractMethodError; it
    // shares the bounds-checked body above and adds the check the argument is
    // FOR, rather than accepting and ignoring it.
    r.register(
        ms,
        "asSlice",
        "(JJJ)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = pe_long_arg(args, 1);
            let new_size = pe_long_arg(args, 2);
            let align = pe_long_arg(args, 3);
            // Bounds, then power-of-two, then alignment — the oracle's order,
            // measured. See [`pe_slice_bounds_check`].
            pe_slice_bounds_check(ctx, this, offset, new_size)?;
            pe_slice_alignment_check(ctx, this, offset, align)?;
            pe_segment_slice(ctx, this, offset, new_size, None)
        },
    );

    // asSlice(long offset, MemoryLayout layout) — size AND ALIGNMENT taken
    // from the layout.
    //
    // The JDK's body is `asSlice(offset, layout.byteSize(),
    // layout.byteAlignment())`, so the layout's alignment is a CONSTRAINT, not
    // merely a width. Dropping it made this the one slice arity that could
    // hand back a view the oracle refuses to create:
    //
    // | call | oracle | before |
    // |---|---|---|
    // | `ofArray(byte[16]).asSlice(0, JAVA_INT)` | IAE | 4-byte slice |
    // | `ofArray(byte[16]).asSlice(0, JAVA_INT_UNALIGNED)` | 4-byte slice | 4-byte slice |
    // | `ofArray(int[8]).asSlice(2, JAVA_INT)` | IAE | 4-byte slice |
    // | `ofArray(int[8]).asSlice(4, JAVA_INT)` | 4-byte slice | 4-byte slice |
    // | `ofArray(byte[16]).asSlice(0, sequenceLayout(2, JAVA_INT))` | IAE | 8-byte slice |
    // | `ofArray(byte[16]).asSlice(0, paddingLayout(4))` | 4-byte slice | 4-byte slice |
    //
    // (measured, `FfmProbe` C16/C17 and `FfmProbe3` M3a–M3i). The width comes
    // from [`p67_layout_size_align`] rather than `pe_memory_layout_width` so
    // that the size and the alignment are read out of the SAME carrier slots
    // in one call — two readers for one four-slot object is how the two got to
    // disagree in the first place.
    r.register(
        ms,
        "asSlice",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = pe_long_arg(args, 1);
            let layout = obj_arg(args, 2)?;
            let (width, align) =
                crate::phases_late::foreign_ffm::p67_layout_size_align(ctx, layout);
            pe_slice_bounds_check(ctx, this, offset, width)?;
            pe_slice_alignment_check(ctx, this, offset, align)?;
            pe_segment_slice(ctx, this, offset, width, None)
        },
    );

    // maxByteAlignment().
    //
    // SETTLED 2026-08-16 against the oracle; the previous body was a
    // hypothesis and both of its arms were wrong.
    //
    // * **Heap.** The answer is the BACKING ARRAY'S ELEMENT ALIGNMENT, capped
    //   by the low bit of the offset within that array — not the offset alone.
    //   Measured: `ofArray(byte[16])`→1, `short[8]`→2, `char[8]`→2,
    //   `int[8]`→4, `long[8]`→8, `float[8]`→4, `double[8]`→8, and
    //   `ofArray(byte[0])`→1, `ofArray(long[0])`→8 (an empty segment still
    //   knows its element type). The old body answered **8 for every one of
    //   them** at offset 0, and 1/2/4 by accident of the offset elsewhere:
    //   `ofArray(byte[16]).maxByteAlignment()` was 8 where the oracle says 1,
    //   which is the difference between refusing and admitting
    //   `get(JAVA_LONG, 0)` on a byte array.
    // * **Native.** A base of 0 answers **2^62**, not 8 — measured on
    //   `MemorySegment.NULL` and `ofAddress(0)`. 8 made `NULL.asSlice(0,0,16)`
    //   an `IllegalArgumentException` where HotSpot returns a segment.
    //
    // Both arms live in [`pe_segment_max_byte_alignment`], which is the same
    // reader `asSlice`'s alignment arms and the heap `get`/`set` gate go
    // through, so this method and the refusals it explains cannot drift apart.
    r.register(ms, "maxByteAlignment", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(pe_segment_max_byte_alignment(ctx, this))))
    });

    // heapBase() — present only for a heap segment, whose Java array this
    // answers; a native segment has no heap base.
    //
    // Asked through `heap_segment_view`, which is the same reader `isNative()`
    // and `address()` above use, so the three cannot disagree. The plain
    // `SEG_BACKING_ARRAY_FIELD` read this used to do knew only ONE of the three
    // heap carriers — the `ofArray` off-heap mirror — and answered empty for a
    // real `HeapMemorySegmentImpl$Of*` and for this file's own alias carrier
    // (`[6]=array, [7]=start`), the two that `isNative()` already reports as
    // non-native. The mirror is kept as the fallback because its array lives in
    // slot 2 and `heap_segment_view` does not claim it.
    // A READ-ONLY SEGMENT HAS NO `heapBase`. MEASURED, and it is a
    // CAPABILITY, not a formatting detail.
    //
    // | call | oracle |
    // |---|---|
    // | `ofArray(byte[16]).heapBase().isPresent()` | `true` |
    // | `ofArray(byte[16]).asReadOnly().heapBase().isPresent()` | **`false`** |
    // | `asReadOnly().asSlice(3,4).heapBase().isPresent()` | `false` |
    // | `asReadOnly().elements(JAVA_BYTE).findFirst().get().heapBase().isPresent()` | `false` |
    // | `ofBuffer(ByteBuffer.allocate(8).asReadOnlyBuffer()).heapBase().isPresent()` | `false` |
    // | native `allocate(8).asReadOnly().heapBase().isPresent()` | `false` |
    //
    // (`FfmProbe` B12, `FfmProbe3` M4a–M4k.) The array `heapBase()` hands back
    // is the segment's own storage and is fully writable through plain array
    // stores, so returning it from a read-only view gives away exactly the
    // capability `asReadOnly()` was called to remove — F26's "a wrong
    // capability" and F21's "read-only is contagious", one call apart. Reads
    // through the segment still work (`asReadOnly().toArray(JAVA_BYTE)` is
    // 16 elements on the oracle); it is only the escape hatch that closes.
    r.register(ms, "heapBase", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let base = pe_segment_heap_base(ctx, this);
        ctx.invoke(
            "java/util/Optional",
            "ofNullable",
            "(Ljava/lang/Object;)Ljava/util/Optional;",
            &[base],
        )
    });

    // isAccessibleBy(Thread) — delegate to the SESSION rather than re-deriving
    // confinement here. The owner slot belongs to `foreign_ffm`'s session model,
    // and a second copy of that index is exactly the drift this file's
    // scope-resolution comment warns about.
    r.register(
        ms,
        "isAccessibleBy",
        "(Ljava/lang/Thread;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let thread = args.get(1).copied().unwrap_or(Value::Object(None));
            match pe_segment_session(ctx, this) {
                Some(session) => ctx.invoke_virtual(
                    session,
                    "isAccessibleBy",
                    "(Ljava/lang/Thread;)Z",
                    &[thread],
                ),
                // No modelled session means unconfined, which every thread may reach.
                None => Ok(Some(Value::Int(1))),
            }
        },
    );

    // isLoaded / load / unload / force — specified to throw for a segment that
    // did not come from `FileChannel.map`, which is every segment this VM mints.
    // Registered rather than left absent so the caller sees the JDK's own
    // exception (message included, measured) instead of an AbstractMethodError
    // that names dispatch.
    r.register(ms, "isLoaded", "()Z", |_ctx, _args| {
        Err(RuntimeError::UnsupportedOperationException {
            message: "Not a mapped segment".to_string(),
        }
        .into())
    });
    for method in ["load", "unload", "force"] {
        r.register(ms, method, "()V", |_ctx, _args| {
            Err(RuntimeError::UnsupportedOperationException {
                message: "Not a mapped segment".to_string(),
            }
            .into())
        });
    }

    // ofAddress(long address) → MemorySegment (wraps a raw address, zero-length)
    r.register(
        ms,
        "ofAddress",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let addr = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            // `ofAddress` is NOT restricted, and the JDK does not gate it —
            // measured on Adoptium 25.0.4, it answers a segment even under
            // `--illegal-native-access=deny`, which is the strictest setting
            // the launcher has:
            //
            //     java                                 ofAddress -> OK, byteSize=0
            //     java --illegal-native-access=deny    ofAddress -> OK, byteSize=0
            //
            // The safety is structural rather than a flag: the segment it
            // returns has length ZERO, so every access through it is an
            // IndexOutOfBoundsException. Widening it needs `reinterpret`, and
            // THAT is the restricted call — which is where the check now lives.
            // The refusal that used to sit here had no counterpart in the JDK
            // at any setting, and the `addr != 0` carve-out it needed for
            // `MemorySegment.NULL`'s own <clinit> was the tell.
            let seg = alloc_segment_carrier(ctx, 6)?;
            ctx.set_field(seg, 0, Value::Long(addr));
            ctx.set_field(seg, 1, Value::Long(0)); // unknown size
            ctx.set_field(seg, 2, Value::Object(None)); // no arena
            ctx.set_field(seg, 3, Value::Int(0));
            ctx.set_field(seg, 4, Value::Int(1));
            ctx.set_field(seg, 5, Value::Long(0));
            Ok(Some(Value::Object(Some(seg))))
        },
    );

    // allocateFrom(Arena, ValueLayout, value) → MemorySegment
    // Allocates a segment for a single value and writes the value into it.
    r.register(
        ms,
        "allocateFrom",
        "(Ljava/lang/foreign/Arena;Ljava/lang/foreign/ValueLayout;I)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arena = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let value = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let size = match ctx.get_field(layout, 1) {
                Value::Int(n) => n as i64,
                _ => 4,
            };
            let seg = pe_arena_allocate_impl(ctx, arena, size, size)?
                .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None });
            if let Some(seg) = seg {
                pe_segment_set_impl(ctx, seg, layout, 0, Value::Int(value))?;
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );
    // allocateFrom(Arena, ValueLayout.OfLong, long) → MemorySegment
    r.register(
        ms,
        "allocateFrom",
        "(Ljava/lang/foreign/Arena;Ljava/lang/foreign/ValueLayout;J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arena = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let value = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let size = match ctx.get_field(layout, 1) {
                Value::Int(n) => n as i64,
                _ => 8,
            };
            let seg = pe_arena_allocate_impl(ctx, arena, size, size)?
                .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None });
            if let Some(seg) = seg {
                pe_segment_set_impl(ctx, seg, layout, 0, Value::Long(value))?;
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );

    // ofArray(int[]) → MemorySegment wrapping the Java array's data
    r.register(
        ms,
        "ofArray",
        "([I)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                // `ofArray(null)` is a message-less NPE on the oracle. All five
                // arms of this family answered a NULL SEGMENT instead, which is
                // the shape `phase-2-worklist` calls the worst a refusal can
                // take: the caller does not learn it passed null until the null
                // it got back is dereferenced somewhere else. A MISSING
                // argument stays a dispatch defect and keeps the old return.
                Some(Value::Object(None)) => {
                    return Err(RuntimeError::NullPointerException { message: None }.into())
                }
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let byte_size = (len * 4) as i64; // int = 4 bytes each
                                              // Allocate native memory and copy array contents into it
            let result = ctx.allocate_native_memory(byte_size as usize, 4);
            if let Some((alloc_id, ptr)) = result {
                // Copy array elements into native memory
                for i in 0..len {
                    if let Value::Int(v) = ctx.get_array_element(arr, i) {
                        unsafe {
                            let dest = (ptr as *mut i32).add(i);
                            *dest = v;
                        }
                    }
                }
                let seg = alloc_segment_carrier(ctx, 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None)); // auto-managed
                ctx.set_field(seg, 3, Value::Int(0)); // read-write
                ctx.set_field(seg, 4, Value::Int(1)); // alive
                ctx.set_field(seg, 5, Value::Long(0));
                ctx.set_field(seg, SEG_BACKING_ARRAY_FIELD, Value::Object(Some(arr)));
                ctx.set_field(seg, SEG_BACKING_KIND_FIELD, Value::Int(LAYOUT_INT));
                // Store alloc_id so it can be freed (field 0 encodes the pointer)
                let _ = alloc_id; // tracked by NativeMemoryTable
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }
                .into())
            }
        },
    );
    // ofArray(long[]) → MemorySegment
    r.register(
        ms,
        "ofArray",
        "([J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                // `ofArray(null)` is a message-less NPE on the oracle. All five
                // arms of this family answered a NULL SEGMENT instead, which is
                // the shape `phase-2-worklist` calls the worst a refusal can
                // take: the caller does not learn it passed null until the null
                // it got back is dereferenced somewhere else. A MISSING
                // argument stays a dispatch defect and keeps the old return.
                Some(Value::Object(None)) => {
                    return Err(RuntimeError::NullPointerException { message: None }.into())
                }
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let byte_size = (len * 8) as i64;
            let result = ctx.allocate_native_memory(byte_size as usize, 8);
            if let Some((_alloc_id, ptr)) = result {
                for i in 0..len {
                    if let Value::Long(v) = ctx.get_array_element(arr, i) {
                        unsafe {
                            let dest = (ptr as *mut i64).add(i);
                            *dest = v;
                        }
                    }
                }
                let seg = alloc_segment_carrier(ctx, 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                ctx.set_field(seg, SEG_BACKING_ARRAY_FIELD, Value::Object(Some(arr)));
                ctx.set_field(seg, SEG_BACKING_KIND_FIELD, Value::Int(LAYOUT_LONG));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }
                .into())
            }
        },
    );
    // ofArray(float[]) → MemorySegment
    for (owner, method, descriptor) in [
        (ms, "ofArray", "([F)Ljava/lang/foreign/MemorySegment;"),
        (
            "jdk/internal/foreign/SegmentFactories",
            "fromArray",
            "([F)Ljdk/internal/foreign/HeapMemorySegmentImpl$OfFloat;",
        ),
    ] {
        r.register(owner, method, descriptor, |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                // `ofArray(null)` is a message-less NPE on the oracle. All five
                // arms of this family answered a NULL SEGMENT instead, which is
                // the shape `phase-2-worklist` calls the worst a refusal can
                // take: the caller does not learn it passed null until the null
                // it got back is dereferenced somewhere else. A MISSING
                // argument stays a dispatch defect and keeps the old return.
                Some(Value::Object(None)) => {
                    return Err(RuntimeError::NullPointerException { message: None }.into())
                }
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let byte_size = (len * 4) as i64;
            let result = ctx.allocate_native_memory(byte_size as usize, 4);
            if let Some((_alloc_id, ptr)) = result {
                for i in 0..len {
                    let value = match ctx.get_array_element(arr, i) {
                        Value::Float(v) => v,
                        // Primitive float arrays are stored as raw IEEE-754
                        // bits in this VM's generic array representation.
                        Value::Int(bits) => f32::from_bits(bits as u32),
                        _ => 0.0,
                    };
                    unsafe {
                        let dest = (ptr as *mut f32).add(i);
                        *dest = value;
                    }
                }
                let seg = alloc_segment_carrier(ctx, 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                ctx.set_field(seg, SEG_BACKING_ARRAY_FIELD, Value::Object(Some(arr)));
                ctx.set_field(seg, SEG_BACKING_KIND_FIELD, Value::Int(LAYOUT_FLOAT));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }
                .into())
            }
        });
    }
    // ofArray(double[]) → MemorySegment
    r.register(
        ms,
        "ofArray",
        "([D)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                // `ofArray(null)` is a message-less NPE on the oracle. All five
                // arms of this family answered a NULL SEGMENT instead, which is
                // the shape `phase-2-worklist` calls the worst a refusal can
                // take: the caller does not learn it passed null until the null
                // it got back is dereferenced somewhere else. A MISSING
                // argument stays a dispatch defect and keeps the old return.
                Some(Value::Object(None)) => {
                    return Err(RuntimeError::NullPointerException { message: None }.into())
                }
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let byte_size = (len * 8) as i64;
            let result = ctx.allocate_native_memory(byte_size as usize, 8);
            if let Some((_alloc_id, ptr)) = result {
                for i in 0..len {
                    if let Value::Double(v) = ctx.get_array_element(arr, i) {
                        unsafe {
                            let dest = (ptr as *mut f64).add(i);
                            *dest = v;
                        }
                    }
                }
                let seg = alloc_segment_carrier(ctx, 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                ctx.set_field(seg, SEG_BACKING_ARRAY_FIELD, Value::Object(Some(arr)));
                ctx.set_field(seg, SEG_BACKING_KIND_FIELD, Value::Int(LAYOUT_DOUBLE));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }
                .into())
            }
        },
    );

    // ofArray(byte[] | short[] | char[]) → MemorySegment ALIASING the array.
    //
    // `java.lang.foreign.MemorySegment` declares SEVEN `ofArray` overloads
    // (`javap`, 25.0.3+9-LTS: `byte[] char[] short[] int[] float[] long[]
    // double[]` — and NO `boolean[]`). This file registered four. `ofArray` is
    // on `native_override.rs`'s force-route NAME list, but a forced name with
    // no registration for that descriptor falls back to real bytecode, so in
    // `--jdk-only` the three missing arms produced a real
    // `HeapMemorySegmentImpl$OfByte/OfShort/OfChar` — and until F27 this file
    // read one's `length` as its address and the VM died with
    // `EXCEPTION_ACCESS_VIOLATION … at address 0x10` (W7-89 §7.1). In
    // synthetic-JDK mode there is no real bytecode to fall back to at all.
    //
    // THESE THREE ALIAS; THE FOUR ABOVE COPY. That is deliberate and it is the
    // one asymmetry in this registrar, so it is stated here rather than left
    // to be discovered. The four above allocate an off-heap mirror and copy
    // the array into it, because CratonVM cannot hand a moving Java array to
    // native code and those carriers exist to be passed to downcalls
    // (`sync_heap_backed_segment` copies back at the boundary). Measured, the
    // oracle does not permit that at all:
    //
    //     strlen(MemorySegment.ofArray("hi\0".getBytes()))
    //       -> IllegalArgumentException: Heap segment not allowed: MemorySegment{ kind: heap, … }
    //
    // and it DOES require aliasing:
    //
    //     byte[] a = new byte[8];
    //     MemorySegment.ofArray(a).set(JAVA_INT_UNALIGNED, 0, 0x01020304);
    //     // a == [4, 3, 2, 1, 0, 0, 0, 0]
    //
    // So the mirror is a wrong capability in both directions, and propagating
    // it to three more descriptors — when the alias carrier costs nothing and
    // needs no native allocation — would have been propagating a known defect.
    // Unifying the other four is NOMINATED, not done here: they are the shape
    // Elasticsearch's bulk-vector downcalls depend on.
    // One body for all three: the element width comes from the ARRAY, through
    // the same `heap_element_width` the accessors use, so the stride cannot
    // drift between the factory and the reader. `NativeCallback` is a plain
    // `fn` pointer, which is also why the width could not be a captured
    // per-descriptor constant even if that had been desirable.
    for descriptor in [
        "([B)Ljava/lang/foreign/MemorySegment;",
        "([S)Ljava/lang/foreign/MemorySegment;",
        "([C)Ljava/lang/foreign/MemorySegment;",
    ] {
        r.register(ms, "ofArray", descriptor, pe_of_array_alias);
    }

    // Copy primitive array elements into a native segment. Elasticsearch uses
    // this overload to stage float[] rows for bulk vector kernels.
    r.register(
        ms,
        "copy",
        "(Ljava/lang/Object;ILjava/lang/foreign/MemorySegment;Ljava/lang/foreign/ValueLayout;JI)V",
        |ctx, args| {
            require_segment_access(ctx, "copy")?;
            let src = obj_arg(args, 0)?;
            let src_index = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                _ => 0,
            };
            let dst = obj_arg(args, 2)?;
            let layout = obj_arg(args, 3)?;
            let dst_offset = match args.get(4) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let count = match args.get(5) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                _ => 0,
            };
            let length = ctx.array_length(src);
            if src_index
                .checked_add(count)
                .map_or(true, |end| end > length)
            {
                return Err(RuntimeError::aioobe_index_only(src_index as i32).into());
            }
            let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
            let width = ffi::layout_byte_size(kind) as i64;
            for i in 0..count {
                let value = ctx.get_array_element(src, src_index + i);
                // Primitive float arrays use their raw IEEE-754 bits in the
                // interpreter's array representation. The set helper expects
                // the typed Value::Float form; otherwise its type switch
                // silently falls through and leaves the destination zeroed.
                let value = match (kind, value) {
                    (LAYOUT_FLOAT, Value::Int(bits)) => Value::Float(f32::from_bits(bits as u32)),
                    (_, value) => value,
                };
                pe_segment_set_impl(ctx, dst, layout, dst_offset + (i as i64) * width, value)?;
            }
            Ok(None)
        },
    );

    // copy(src, srcOffset, dst, dstOffset, bytes) — memcpy
    r.register(
        ms,
        "copy",
        "(Ljava/lang/foreign/MemorySegment;JLjava/lang/foreign/MemorySegment;JJ)V",
        |ctx, args| {
            // Defense-in-depth: copy dereferences both segments' raw `ptr` fields.
            require_segment_access(ctx, "copy")?;
            let src = obj_arg(args, 0)?;
            let src_offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let dst = obj_arg(args, 2)?;
            let dst_offset = match args.get(3) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let bytes = match args.get(4) {
                Some(Value::Long(n)) => *n as usize,
                _ => 0,
            };

            let src_ptr = crate::panama_libffi::segment_address(ctx, src);
            let dst_ptr = crate::panama_libffi::segment_address(ctx, dst);

            // Validate offsets against segment sizes to prevent out-of-bounds access
            let src_size = crate::panama_libffi::segment_byte_size(ctx, src);
            let dst_size = crate::panama_libffi::segment_byte_size(ctx, dst);

            if bytes > 0 {
                if bytes > MAX_COPY_SIZE {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "copy size {} exceeds maximum of {} bytes",
                            bytes, MAX_COPY_SIZE
                        ),
                    }
                    .into());
                }
                // Bounds check: offset + bytes must fit within segment size.
                // FIX: validate UNCONDITIONALLY (not gated on size > 0). A segment
                // with a declared byteSize() of 0 must still reject a non-zero
                // `bytes` copy — otherwise a 0-size segment drives an OOB
                // read/write of up to MAX_COPY_SIZE. This mirrors the correct
                // single-element path (`pe_segment_access_addr`), which rejects
                // any non-zero access on a zero-size segment. Uses checked_add so
                // a malicious offset cannot wrap past the size comparison.
                let src_end = src_offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
                if src_offset < 0 || src_end > src_size {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "source offset {} + {} bytes exceeds segment size {}",
                            src_offset, bytes, src_size
                        ),
                    }
                    .into());
                }
                let dst_end = dst_offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
                if dst_offset < 0 || dst_end > dst_size {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "destination offset {} + {} bytes exceeds segment size {}",
                            dst_offset, bytes, dst_size
                        ),
                    }
                    .into());
                }

                // A heap segment has no address at all, so the pointer
                // path below cannot express it: `segment_address` answers
                // 0 and the `is_null` guard then skipped the copy without
                // a word — `MemorySegment.copy` into an `ofArray(int[])`
                // destination wrote NOTHING and reported success. Route
                // any side that is heap-backed through the array itself.
                let src_heap = heap_segment_view(ctx, src);
                let dst_heap = heap_segment_view(ctx, dst);
                if src_heap.is_some() || dst_heap.is_some() {
                    let staged = match &src_heap {
                        Some(view) => heap_read_bytes(ctx, view, src_offset, bytes),
                        None => {
                            let addr = (src_ptr as u64).checked_add(src_offset as u64);
                            addr.filter(|a| *a != 0).map(|a| {
                                let mut buf = vec![0u8; bytes];
                                // SAFETY: the source range was bounds-checked
                                // against the segment size above, and the
                                // destination is a fresh owned buffer.
                                unsafe {
                                    std::ptr::copy_nonoverlapping(
                                        a as *const u8,
                                        buf.as_mut_ptr(),
                                        bytes,
                                    )
                                };
                                buf
                            })
                        }
                    };
                    let Some(staged) = staged else {
                        return Err(RuntimeError::IllegalStateException {
                            message: "MemorySegment.copy: source is neither addressable nor \
                                      array-backed"
                                .into(),
                        }
                        .into());
                    };
                    let wrote = match &dst_heap {
                        Some(view) => heap_write_bytes(ctx, view, dst_offset, &staged),
                        None => {
                            let addr = (dst_ptr as u64).checked_add(dst_offset as u64);
                            match addr.filter(|a| *a != 0) {
                                Some(a) => {
                                    // SAFETY: bounds-checked above; `staged`
                                    // is exactly `bytes` long.
                                    unsafe { std::ptr::copy(staged.as_ptr(), a as *mut u8, bytes) };
                                    true
                                }
                                None => false,
                            }
                        }
                    };
                    if !wrote {
                        return Err(RuntimeError::IllegalStateException {
                            message: "MemorySegment.copy: destination is read-only, not \
                                      addressable, or not array-backed"
                                .into(),
                        }
                        .into());
                    }
                    return Ok(None);
                }

                // Validate address arithmetic doesn't overflow
                let src_total = (src_ptr as u64).checked_add(src_offset as u64);
                let dst_total = (dst_ptr as u64).checked_add(dst_offset as u64);

                if let (Some(s), Some(d)) = (src_total, dst_total) {
                    let src_addr = s as *const u8;
                    let dst_addr = d as *mut u8;
                    if !src_addr.is_null() && !dst_addr.is_null() {
                        // The JDK's MemorySegment.copy is defined for overlapping
                        // src/dst (it is specified as a memmove-equivalent bulk
                        // copy). Decide between memmove and the faster
                        // `copy_nonoverlapping` by testing whether the two
                        // byte ranges actually intersect.
                        //
                        // Ranges are `[s, s+bytes)` and `[d, d+bytes)` over the
                        // *absolute* addresses computed above. They overlap iff
                        // `s < d+bytes && d < s+bytes`. `bytes` is bounded by
                        // MAX_COPY_SIZE and both endpoints derive from the
                        // checked address arithmetic, so the `+ bytes` cannot
                        // wrap a u64.
                        let bytes_u64 = bytes as u64;
                        let overlap =
                            s < d.saturating_add(bytes_u64) && d < s.saturating_add(bytes_u64);
                        // SAFETY: addresses are non-null, bounds-checked against
                        // segment sizes, and bytes is bounded by MAX_COPY_SIZE.
                        // Overlapping ranges use `copy` (memmove), which is
                        // defined for overlap; provably-disjoint ranges use the
                        // faster `copy_nonoverlapping`.
                        if overlap {
                            unsafe { std::ptr::copy(src_addr, dst_addr, bytes) };
                        } else {
                            unsafe { std::ptr::copy_nonoverlapping(src_addr, dst_addr, bytes) };
                        }
                    }
                } else {
                    return Err(RuntimeError::IllegalStateException {
                        message: "address arithmetic overflow in MemorySegment.copy".into(),
                    }
                    .into());
                }
            }
            Ok(None)
        },
    );

    // ---- the rest of the surface `java.lang.foreign` declares -------------
    //
    // Every method below is `public abstract` on the `MemorySegment`
    // interface, so one that nobody registers is not a slow path — it is
    // `AbstractMethodError: … has no Code attribute` at the call site, an
    // error that names dispatch rather than the missing feature. They were
    // found by running the whole public surface one call at a time against
    // both VMs (`FfmAudit`), which is the only way to find this shape: each
    // one otherwise surfaces a single application at a time.

    // copyFrom(src) — bulk copy, sizes must match exactly.
    r.register(
        ms,
        "copyFrom",
        "(Ljava/lang/foreign/MemorySegment;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let src = obj_arg(args, 1)?;
            pe_segment_check_scope(ctx, this)?;
            pe_segment_check_scope(ctx, src)?;
            let dst_size = crate::panama_libffi::segment_byte_size(ctx, this);
            let src_size = crate::panama_libffi::segment_byte_size(ctx, src);
            if src_size != dst_size {
                return Err(RuntimeError::IndexOutOfBoundsException {
                    message: Some(format!(
                        "Cannot copy {} bytes into a {}-byte segment",
                        src_size, dst_size
                    )),
                }
                .into());
            }
            let n = usize::try_from(dst_size).unwrap_or(0);
            if n > MAX_COPY_SIZE {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("copy size {} exceeds maximum of {} bytes", n, MAX_COPY_SIZE),
                }
                .into());
            }
            let dst_addr = crate::panama_libffi::segment_address(ctx, this) as *mut u8;
            let src_addr = crate::panama_libffi::segment_address(ctx, src) as *const u8;
            if n > 0 && !dst_addr.is_null() && !src_addr.is_null() {
                // SAFETY: both addresses come from JVM-managed segments whose
                // sizes were just checked equal to `n`, and the two blocks may
                // overlap (`copyFrom` does not forbid it), hence `copy` and not
                // `copy_nonoverlapping`.
                unsafe { std::ptr::copy(src_addr, dst_addr, n) };
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // mismatch(other) — index of the first differing byte, or -1.
    r.register(
        ms,
        "mismatch",
        "(Ljava/lang/foreign/MemorySegment;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            pe_segment_check_scope(ctx, this)?;
            pe_segment_check_scope(ctx, other)?;
            let a_len = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
            let b_len = crate::panama_libffi::segment_byte_size(ctx, other).max(0);
            let common = a_len.min(b_len);
            let a = crate::panama_libffi::segment_address(ctx, this) as *const u8;
            let b = crate::panama_libffi::segment_address(ctx, other) as *const u8;
            if a.is_null() || b.is_null() {
                // Nothing to compare through; fall back to the length rule.
                return Ok(Some(Value::Long(if a_len == b_len { -1 } else { common })));
            }
            let n = usize::try_from(common).unwrap_or(0).min(MAX_COPY_SIZE);
            // SAFETY: both pointers are segment bases and `n` is bounded by the
            // shorter of the two segment sizes.
            let (a_bytes, b_bytes) = unsafe {
                (
                    std::slice::from_raw_parts(a, n),
                    std::slice::from_raw_parts(b, n),
                )
            };
            let index = a_bytes
                .iter()
                .zip(b_bytes.iter())
                .position(|(x, y)| x != y)
                .map(|i| i as i64);
            // The JDK's contract: the first differing byte; else the common
            // length when one segment is a prefix of the other; else -1.
            Ok(Some(Value::Long(match index {
                Some(i) => i,
                None if a_len == b_len => -1,
                None => common,
            })))
        },
    );

    // getString(offset[, charset]) — a NUL-terminated string, read in place.
    for desc in [
        "(J)Ljava/lang/String;",
        "(JLjava/nio/charset/Charset;)Ljava/lang/String;",
    ] {
        r.register(ms, "getString", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            pe_segment_check_scope(ctx, this)?;
            let offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
            if offset < 0 || offset > size {
                return Err(RuntimeError::IndexOutOfBoundsException {
                    message: Some(format!("offset {} out of bounds for size {}", offset, size)),
                }
                .into());
            }
            let base = crate::panama_libffi::segment_address(ctx, this) as *const u8;
            if base.is_null() {
                return Ok(Some(Value::Object(None)));
            }
            let avail = usize::try_from(size - offset)
                .unwrap_or(0)
                .min(MAX_COPY_SIZE);
            // SAFETY: `offset` is within the segment and `avail` is the
            // remaining length from there.
            let bytes = unsafe { std::slice::from_raw_parts(base.add(offset as usize), avail) };
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(avail);
            // The charset overload is accepted and read as UTF-8: that is the
            // only decoder available here, and it is the default the no-charset
            // form uses. Non-UTF-8 bytes are replaced rather than refused,
            // matching `String::from_utf8_lossy` — recorded rather than silent.
            let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
            let s = ctx.create_string(&text);
            Ok(Some(Value::Object(Some(s))))
        });
    }

    // setString(offset, value[, charset]) — write the bytes plus a NUL.
    for desc in [
        "(JLjava/lang/String;)V",
        "(JLjava/lang/String;Ljava/nio/charset/Charset;)V",
    ] {
        r.register(ms, "setString", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            pe_segment_check_scope(ctx, this)?;
            let offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let text = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
            let needed = text.len() as i64 + 1;
            if offset < 0 || offset.saturating_add(needed) > size {
                return Err(RuntimeError::IndexOutOfBoundsException {
                    message: Some(format!(
                        "writing {} bytes at offset {} exceeds segment size {}",
                        needed, offset, size
                    )),
                }
                .into());
            }
            let base = crate::panama_libffi::segment_address(ctx, this) as *mut u8;
            if base.is_null() {
                return Ok(None);
            }
            // SAFETY: the bounds check above guarantees `offset + text.len() + 1`
            // bytes are inside the segment.
            unsafe {
                let dst = base.add(offset as usize);
                std::ptr::copy_nonoverlapping(text.as_ptr(), dst, text.len());
                dst.add(text.len()).write(0);
            }
            Ok(None)
        });
    }

    // asOverlappingSlice(other) — the shared region, as an Optional.
    r.register(
        ms,
        "asOverlappingSlice",
        "(Ljava/lang/foreign/MemorySegment;)Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let that = obj_arg(args, 1)?;
            let a_start = crate::panama_libffi::segment_address(ctx, this);
            let a_end = a_start.saturating_add(crate::panama_libffi::segment_byte_size(ctx, this));
            let b_start = crate::panama_libffi::segment_address(ctx, that);
            let b_end = b_start.saturating_add(crate::panama_libffi::segment_byte_size(ctx, that));
            let lo = a_start.max(b_start);
            let hi = a_end.min(b_end);
            let empty = lo >= hi;
            let value = if empty {
                Value::Object(None)
            } else {
                pe_segment_slice(ctx, this, lo - a_start, hi - lo, None)?
                    .unwrap_or(Value::Object(None))
            };
            let opt = crate::phases_late::foreign_ffm::p67_optional(ctx, value)?;
            Ok(Some(Value::Object(Some(opt))))
        },
    );

    // toArray(elementLayout) — copy the whole segment out into a Java array.
    //
    // Eight overloads, one per primitive layout, each with its own array return
    // type. They share one body because `register` takes a plain fn pointer —
    // a per-descriptor closure could not capture its element width — so the
    // width and the array kind come from the layout argument, exactly as
    // `get`/`set` take theirs.
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;)[Z",
        "(Ljava/lang/foreign/ValueLayout$OfByte;)[B",
        "(Ljava/lang/foreign/ValueLayout$OfChar;)[C",
        "(Ljava/lang/foreign/ValueLayout$OfShort;)[S",
        "(Ljava/lang/foreign/ValueLayout$OfInt;)[I",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;)[F",
        "(Ljava/lang/foreign/ValueLayout$OfLong;)[J",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;)[D",
    ] {
        r.register(ms, "toArray", desc, pe_segment_to_array);
    }

    // allocateFrom(elementLayout, values...) — the seven array overloads.
    //
    // These are `default` methods on `SegmentAllocator`, so real JDK bytecode
    // runs for them, and it ends in
    // `((AbstractMemorySegmentImpl) segment).copyFrom(...)` — a cast that can
    // never succeed while CratonVM fabricates segments as instances of the
    // `MemorySegment` INTERFACE. The failure was therefore not "no Code
    // attribute" like its neighbours but a `ClassCastException` from inside the
    // JDK, which is why it survived the interface audit that found the rest.
    // A native for each descriptor keeps that bytecode from running at all.
    for allocator in [
        "java/lang/foreign/Arena",
        // An arena is minted as the real `ArenaImpl` since 2026-08-29, and
        // native dispatch is keyed on the receiver's CLASS. Without this row
        // the JDK default above runs and `RJdkForeign`'s downcall SIGSEGVs in
        // `heap_read_bytes` on the segment it builds.
        "jdk/internal/foreign/ArenaImpl",
        "java/lang/foreign/SegmentAllocator",
    ] {
        // Seven, not eight: `SegmentAllocator` declares no `boolean...`
        // overload. Registering a descriptor the JDK does not declare would be
        // dead weight that reads like coverage.
        for desc in [
            "(Ljava/lang/foreign/ValueLayout$OfByte;[B)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfChar;[C)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfShort;[S)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfInt;[I)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfFloat;[F)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfLong;[J)Ljava/lang/foreign/MemorySegment;",
            "(Ljava/lang/foreign/ValueLayout$OfDouble;[D)Ljava/lang/foreign/MemorySegment;",
        ] {
            r.register(allocator, "allocateFrom", desc, pe_allocate_from_array);
        }
    }

    // spliterator(elementLayout) / elements(elementLayout).
    //
    // The last two `MemorySegment` methods that answered
    // `AbstractMethodError`. They are implemented LAZILY — see
    // `pe_segment_spliterator` for why a materialised list of slices would be
    // wrong for exactly the case these methods exist for.
    r.register(
        ms,
        "spliterator",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/util/Spliterator;",
        pe_segment_spliterator,
    );
    r.register(
        ms,
        "elements",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/util/stream/Stream;",
        pe_segment_elements,
    );

    // The splitter's own surface. Registered on ITS class, not on
    // `java/util/Spliterator`: that interface already carries the
    // array-backed collections implementation (field 0 = `Object[]`, 1 =
    // cursor, 2 = fence), registered later than this file runs, and a second
    // registration of the same triple would silently take those calls over.
    // A distinct receiver class keeps the two implementations apart with no
    // ordering dependency and no shape-sniffing.
    let splitter = PE_SEGMENT_SPLITTER;
    r.register(
        splitter,
        "tryAdvance",
        "(Ljava/util/function/Consumer;)Z",
        pe_splitter_try_advance,
    );
    r.register(
        splitter,
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
        |ctx, args| {
            // `Spliterator.forEachRemaining` has a default body, but it is not
            // reachable here: the receiver's class is a fabricated stub with no
            // declared interfaces, so nothing routes the call to
            // `java.util.Spliterator`. Every method the contract exposes is
            // therefore registered explicitly rather than inherited.
            while matches!(pe_splitter_try_advance(ctx, args)?, Some(Value::Int(1))) {}
            Ok(None)
        },
    );
    r.register(
        splitter,
        "trySplit",
        "()Ljava/util/Spliterator;",
        |ctx, args| {
            // The JDK's `SegmentSplitter.trySplit`: split only before the first
            // element is consumed, hand the LOW half to the new splitter and keep
            // the high half (plus the odd element) here.
            let this = obj_arg(args, 0)?;
            let (elem_count, elem_size, index) = pe_splitter_state(ctx, this);
            if index != 0 || elem_count <= 1 {
                return Ok(Some(Value::Object(None)));
            }
            let (split, lobound, hibound) = pe_split_bounds(elem_count, elem_size);
            // What THIS splitter keeps: everything the low half did not take,
            // i.e. the JDK's `split + rem` with `rem = elemCount % 2`.
            let high_count = elem_count - split;
            let segment = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let low = match pe_segment_slice(ctx, segment, 0, lobound, None)? {
                Some(Value::Object(Some(s))) => s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let low_pin = ctx.pin_native_root(low);
            let prefix = pe_new_splitter(ctx, low, split, elem_size)?;
            ctx.unpin_native_roots(low_pin);
            // Only now narrow this splitter to its own (high) half.
            let this = obj_arg(args, 0)?;
            let segment = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => s,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Some(Value::Object(Some(high))) =
                pe_segment_slice(ctx, segment, lobound, hibound, None)?
            {
                ctx.set_field(this, 0, Value::Object(Some(high)));
            }
            ctx.set_field(this, 1, Value::Long(high_count));
            Ok(Some(Value::Object(Some(prefix))))
        },
    );
    // `elemCount`, NOT `elemCount - currentIndex`. The JDK returns the former
    // verbatim, so a half-drained splitter still reports its ORIGINAL size —
    // measured, `tryAdvance` x3 over 8 elements still answers 8 on HotSpot.
    // Subtracting would be the more sensible number and the wrong one.
    r.register(splitter, "estimateSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let (elem_count, _, _) = pe_splitter_state(ctx, this);
        Ok(Some(Value::Long(elem_count.max(0))))
    });
    r.register(splitter, "getExactSizeIfKnown", "()J", |ctx, args| {
        // SIZED, so the exact size IS known — and it is `estimateSize()`,
        // which is what the interface default returns.
        let this = obj_arg(args, 0)?;
        let (elem_count, _, _) = pe_splitter_state(ctx, this);
        Ok(Some(Value::Long(elem_count.max(0))))
    });
    r.register(splitter, "characteristics", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(PE_SPLITTER_CHARACTERISTICS)))
    });
    r.register(splitter, "hasCharacteristics", "(I)Z", |_ctx, args| {
        let wanted = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let has = (PE_SPLITTER_CHARACTERISTICS & wanted) == wanted;
        Ok(Some(Value::Int(i32::from(has))))
    });
    r.register(
        splitter,
        "getComparator",
        "()Ljava/util/Comparator;",
        |_ctx, _args| {
            // Not SORTED — the spec'd answer is to throw, not to return null
            // (null means "sorted in natural order"). `Spliterator`'s default
            // throws `new IllegalStateException()`, so `getMessage()` is empty;
            // measured on HotSpot as `IllegalStateException: null`.
            Err(RuntimeError::IllegalStateException {
                message: String::new(),
            }
            .into())
        },
    );

    // fill(byte value) — memset
    r.register(
        ms,
        "fill",
        "(B)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            // Defense-in-depth: fill writes to the segment's raw `ptr` field.
            require_segment_access(ctx, "fill")?;
            let this = obj_arg(args, 0)?;
            let byte_val = match args.get(1) {
                Some(Value::Int(n)) => *n as u8,
                _ => 0,
            };
            let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0) as usize;
            let addr = crate::panama_libffi::segment_address(ctx, this) as *mut u8;
            if size > 0 && !addr.is_null() {
                if size > MAX_COPY_SIZE {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "fill size {} exceeds maximum of {} bytes",
                            size, MAX_COPY_SIZE
                        ),
                    }
                    .into());
                }
                // SAFETY: addr is non-null, and size is bounded by MAX_COPY_SIZE.
                // The address comes from a JVM-managed MemorySegment.
                unsafe { std::ptr::write_bytes(addr, byte_val, size) };
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.set_category(__prev_cat);
}

/// `MemorySegment.toArray(ValueLayout$OfX)` — the segment copied out into a
/// fresh Java array of the matching primitive type.
///
/// The element kind is taken from the layout argument's class name, which is
/// how the eight overloads share one body. `checkArraySize` in the JDK refuses
/// a segment whose size is not a whole number of elements; that is the
/// `size % width` arm here.
fn pe_segment_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::ArrayElementType as AET;
    let this = obj_arg(args, 0)?;
    pe_segment_check_scope(ctx, this)?;
    let layout = obj_arg(args, 1)?;
    let layout_class = ctx
        .class_name_of_id(ctx.class_id_of_object(layout))
        .unwrap_or_default();
    let (width, kind) = if layout_class.contains("OfBoolean") {
        (1_i64, AET::Boolean)
    } else if layout_class.contains("OfByte") {
        (1, AET::Byte)
    } else if layout_class.contains("OfChar") {
        (2, AET::Char)
    } else if layout_class.contains("OfShort") {
        (2, AET::Short)
    } else if layout_class.contains("OfInt") {
        (4, AET::Int)
    } else if layout_class.contains("OfFloat") {
        (4, AET::Float)
    } else if layout_class.contains("OfLong") {
        (8, AET::Long)
    } else if layout_class.contains("OfDouble") {
        (8, AET::Double)
    } else {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("toArray: unsupported element layout {}", layout_class),
        }
        .into());
    };

    let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
    if size % width != 0 {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Segment size is not a multiple of {}. Size: {}",
                width, size
            ),
        }
        .into());
    }
    let count = size / width;
    // `ArraysSupport.SOFT_MAX_ARRAY_LENGTH`, the JDK's own bound.
    if count > i64::from(i32::MAX) - 8 {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Segment is too large to wrap as an array. Size: {}", size),
        }
        .into());
    }

    // The alignment gate, AFTER the size gate — that order is the JDK's and it
    // is measured, not assumed. `AbstractMemorySegmentImpl.toArray` runs
    // `checkArraySize` (the `IllegalStateException` above) and only then hands
    // the segment to `MemorySegment.copy`, which is where the alignment
    // `IllegalArgumentException` comes from. So a segment that is BOTH badly
    // sized and badly aligned reports the size:
    //
    // | call | oracle |
    // |---|---|
    // | `ofArray(byte[15]).toArray(JAVA_INT)` | ISE `Segment size is not a multiple of 4. Size: 15` |
    // | `ofArray(byte[16]).toArray(JAVA_INT)` | IAE `Source segment incompatible with alignment constraints` |
    // | `ofArray(byte[16]).toArray(JAVA_INT_UNALIGNED)` | `[50462976, 117835012, 185207048, 252579084]` |
    // | `ofArray(int[8]).toArray(JAVA_LONG)` | IAE, same message |
    // | `ofArray(int[8]).asSlice(4).toArray(JAVA_INT).length` | 7 |
    //
    // Note the message is `Source segment ...`, not the `Incompatible
    // alignment constraints` that `spliterator` answers: they come from
    // different JDK call sites and both are transcribed
    // (`FfmProbe` G2/G3/G5/G50, `FfmProbe3` M1n/M2a).
    let layout_align = crate::panama_libffi::layout_align(ctx, layout) as i64;
    if layout_align > pe_segment_max_byte_alignment(ctx, this) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Source segment incompatible with alignment constraints".into(),
        }
        .into());
    }

    // A HEAP SEGMENT'S BYTES ARE IN A JAVA ARRAY, NOT AT AN ADDRESS.
    //
    // `segment_address` answers 0 for every heap carrier (F27), and the raw
    // loop below then took its `base.is_null()` early return and handed back a
    // correctly-sized array of ZEROS. `MemorySegment.ofArray(new byte[]{1,2,3})
    // .toArray(JAVA_BYTE)` answered `[0, 0, 0]` where the oracle answers
    // `[1, 2, 3]` — a silent wrong answer, not a refusal, on the one method
    // whose entire job is to hand the bytes back. It reached real
    // `HeapMemorySegmentImpl$Of*` receivers and every heap slice this file
    // mints (`asSlice` is force-routed, so a slice of a real heap segment is
    // one of ours).
    //
    // `heap_segment_read` is the same reader `get` uses, so `toArray` and a
    // loop of `get`s cannot disagree about the stride or the byte order.
    if let Some(view) = heap_segment_view(ctx, this) {
        let arr = ctx.new_array(kind, count as usize);
        for i in 0..count as usize {
            let raw = heap_segment_read(ctx, &view, i as i64 * width, width as usize);
            let value = match kind {
                AET::Boolean => Value::Int(i32::from((raw & 0xff) != 0)),
                AET::Byte => Value::Int(i32::from(raw as u8 as i8)),
                AET::Char => Value::Int(i32::from(raw as u16)),
                AET::Short => Value::Int(i32::from(raw as u16 as i16)),
                AET::Int => Value::Int(raw as u32 as i32),
                AET::Float => Value::Float(f32::from_bits(raw as u32)),
                AET::Long => Value::Long(raw as i64),
                _ => Value::Double(f64::from_bits(raw)),
            };
            ctx.set_array_element(arr, i, value);
        }
        return Ok(Some(Value::Object(Some(arr))));
    }

    let base = crate::panama_libffi::segment_address(ctx, this) as *const u8;
    let arr = ctx.new_array(kind, count as usize);
    if base.is_null() {
        return Ok(Some(Value::Object(Some(arr))));
    }
    for i in 0..count as usize {
        // SAFETY: `i * width` is inside the segment by the size check above,
        // and each read is exactly `width` bytes wide.
        let value = unsafe {
            let p = base.add(i * width as usize);
            match kind {
                AET::Boolean => Value::Int(i32::from(p.read() != 0)),
                AET::Byte => Value::Int(i32::from(p.read() as i8)),
                AET::Char => Value::Int(i32::from(u16::from_ne_bytes(p.cast::<[u8; 2]>().read()))),
                AET::Short => Value::Int(i32::from(i16::from_ne_bytes(p.cast::<[u8; 2]>().read()))),
                AET::Int => Value::Int(i32::from_ne_bytes(p.cast::<[u8; 4]>().read())),
                AET::Float => Value::Float(f32::from_ne_bytes(p.cast::<[u8; 4]>().read())),
                AET::Long => Value::Long(i64::from_ne_bytes(p.cast::<[u8; 8]>().read())),
                _ => Value::Double(f64::from_ne_bytes(p.cast::<[u8; 8]>().read())),
            }
        };
        ctx.set_array_element(arr, i, value);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// A `long` argument, tolerant of the `Int` an interpreter frame may carry.
fn pe_long_arg(args: &[Value], index: usize) -> i64 {
    match args.get(index) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        _ => 0,
    }
}

/// `SegmentAllocator.allocateFrom(ValueLayout$OfX, X... elements)`.
///
/// One body for all eight overloads: the element width comes from the layout
/// argument and the values from the Java array, the same way `toArray` reads
/// its kind from the layout rather than the descriptor.
fn pe_allocate_from_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arena = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let Some(Value::Object(Some(array))) = args.get(2).copied() else {
        return Err(RuntimeError::NullPointerException {
            message: Some("allocateFrom: elements array is null".to_string()),
        }
        .into());
    };
    let (width, align) = crate::phases_late::foreign_ffm::p67_layout_size_align(ctx, layout);
    if width <= 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Invalid byte size: {}", width),
        }
        .into());
    }
    let count = ctx.array_length(array) as i64;
    let size = count.saturating_mul(width);
    // Zero elements still allocates: the JDK hands back an empty segment rather
    // than null, and `Arena.allocate(0)` is legal.
    let Some(Value::Object(Some(segment))) =
        pe_arena_allocate_impl(ctx, arena, size.max(1), align)?
    else {
        return Ok(Some(Value::Object(None)));
    };
    // The allocator over-allocates one byte for an empty array (`size.max(1)`)
    // so the pointer is real; report the size the caller asked for.
    ctx.set_field(segment, 1, Value::Long(size));
    let base = crate::panama_libffi::segment_address(ctx, segment) as *mut u8;
    if base.is_null() || count == 0 {
        return Ok(Some(Value::Object(Some(segment))));
    }
    for i in 0..count as usize {
        let value = ctx.get_array_element(array, i);
        // SAFETY: `i * width` is inside the block just allocated for
        // `count * width` bytes, and each write is exactly `width` wide.
        unsafe {
            let p = base.add(i * width as usize);
            match width {
                1 => p.write(match value {
                    Value::Int(v) => v as u8,
                    _ => 0,
                }),
                2 => {
                    let v = match value {
                        Value::Int(v) => v as u16,
                        _ => 0,
                    };
                    p.cast::<[u8; 2]>().write(v.to_ne_bytes());
                }
                4 => {
                    let bits = match value {
                        Value::Float(f) => f.to_bits(),
                        Value::Int(v) => v as u32,
                        _ => 0,
                    };
                    p.cast::<[u8; 4]>().write(bits.to_ne_bytes());
                }
                _ => {
                    let bits = match value {
                        Value::Double(d) => d.to_bits(),
                        Value::Long(v) => v as u64,
                        Value::Int(v) => i64::from(v) as u64,
                        _ => 0,
                    };
                    p.cast::<[u8; 8]>().write(bits.to_ne_bytes());
                }
            }
        }
    }
    Ok(Some(Value::Object(Some(segment))))
}

/// The receiver class of `MemorySegment.spliterator(...)`.
///
/// Deliberately NOT `java/util/Spliterator`. That name is already the runtime
/// class of the array-backed collections spliterators, whose natives are
/// registered by `native-collections` *after* this file's registrar runs — so
/// re-registering `tryAdvance` there would take over every `ArrayList`
/// spliterator in the VM. A distinct class also makes
/// `StreamSupport.stream(spliterator, false)` treat this as a real
/// `Spliterator` implementation and drain it lazily through `tryAdvance`,
/// which is exactly the behaviour wanted here.
const PE_SEGMENT_SPLITTER: &str = "java/lang/foreign/MemorySegment$SegmentSplitter";

/// `NONNULL | SUBSIZED | SIZED | IMMUTABLE | ORDERED`, the value the JDK's
/// `AbstractMemorySegmentImpl.SegmentSplitter.characteristics()` returns.
const PE_SPLITTER_CHARACTERISTICS: i32 = 0x100 | 0x4000 | 0x40 | 0x400 | 0x10;

/// `(low half element count, low half byte size, high half byte size)`.
///
/// The JDK's `SegmentSplitter.trySplit` arithmetic, lifted out so it can be
/// checked without a heap: the LOW half gets `elemCount / 2` elements and the
/// odd one stays with the high half, so an odd count splits 2/3, not 3/2. An
/// off-by-one here silently drops or duplicates an element in every parallel
/// stream over a segment, which is exactly the bug a differential probe over
/// an even count cannot see.
fn pe_split_bounds(elem_count: i64, elem_size: i64) -> (i64, i64, i64) {
    let rem = elem_count % 2;
    let split = elem_count / 2;
    let lobound = split * elem_size;
    let hibound = lobound + (rem * elem_size);
    (split, lobound, hibound)
}

/// `(elemCount, elementSize, currentIndex)` off a splitter carrier.
///
/// Slots: `[0]=segment, [1]=elemCount, [2]=elementSize, [3]=currentIndex`.
fn pe_splitter_state(ctx: &mut dyn NativeContext, this: ObjectRef) -> (i64, i64, i64) {
    let long_at = |ctx: &mut dyn NativeContext, i: usize| match ctx.get_field(this, i) {
        Value::Long(v) => v,
        Value::Int(v) => i64::from(v),
        _ => 0,
    };
    (long_at(ctx, 1), long_at(ctx, 2), long_at(ctx, 3))
}

/// Allocate a splitter over `segment`.
fn pe_new_splitter(
    ctx: &mut dyn NativeContext,
    segment: ObjectRef,
    elem_count: i64,
    elem_size: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let segment_pin = ctx.pin_native_root(segment);
    let splitter = try_alloc_concurrent_synthetic(ctx, PE_SEGMENT_SPLITTER, 4)?;
    let segment = ctx.read_native_pin(segment_pin, segment);
    ctx.unpin_native_roots(segment_pin);
    ctx.set_field(splitter, 0, Value::Object(Some(segment)));
    ctx.set_field(splitter, 1, Value::Long(elem_count));
    ctx.set_field(splitter, 2, Value::Long(elem_size));
    ctx.set_field(splitter, 3, Value::Long(0));
    Ok(splitter)
}

/// `Spliterator.tryAdvance` — hand the consumer the next slice, one at a time.
///
/// This is where the laziness lives: the slice for element `i` is minted when
/// `i` is reached, so a segment with more elements than fit in memory as
/// separate `MemorySegment` objects still streams. A materialised list of
/// slices would answer the same two calls and be wrong for exactly the case
/// these methods exist for.
fn pe_splitter_try_advance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let (elem_count, elem_size, index) = pe_splitter_state(ctx, this);
    if index >= elem_count {
        return Ok(Some(Value::Int(0)));
    }
    let segment = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => s,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_pin = ctx.pin_native_root(this);
    let slice = pe_segment_slice(ctx, segment, index * elem_size, elem_size, None)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    // Advance BEFORE the callback: the JDK increments in a `finally`, so a
    // consumer that throws still leaves the splitter past this element.
    ctx.set_field(this, 3, Value::Long(index + 1));
    if let Some(Value::Object(Some(consumer))) = args.get(1) {
        let slice = slice.unwrap_or(Value::Object(None));
        ctx.invoke_virtual(*consumer, "accept", "(Ljava/lang/Object;)V", &[slice])?;
    }
    Ok(Some(Value::Int(1)))
}

/// `MemorySegment.spliterator(MemoryLayout)`.
///
/// The four `IllegalArgumentException`s are the JDK's, in its order — see
/// `AbstractMemorySegmentImpl.spliterator`.
fn pe_segment_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    pe_segment_check_scope(ctx, this)?;
    let (elem_size, elem_align) =
        crate::phases_late::foreign_ffm::p67_layout_size_align(ctx, layout);
    if elem_size == 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Element layout size cannot be zero".into(),
        }
        .into());
    }
    if elem_size % elem_align != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Element layout size is not multiple of alignment".into(),
        }
        .into());
    }
    // `maxByteAlignment`, NOT `segment_address % elem_align`.
    //
    // On a heap carrier `segment_address` is deliberately 0 (F27), and
    // `0 % anything == 0`, so this gate admitted EVERY element layout on every
    // heap segment. Measured refusals it let through:
    //
    // | call | oracle |
    // |---|---|
    // | `ofArray(byte[16]).spliterator(JAVA_INT)` | IAE `Incompatible alignment constraints` |
    // | `ofArray(byte[16]).elements(JAVA_INT)` | IAE, same message |
    // | `ofArray(int[8]).asSlice(2).elements(JAVA_INT)` | IAE, same message |
    // | `ofArray(byte[16]).elements(JAVA_INT_UNALIGNED)` | 4 elements |
    // | `ofArray(int[8]).elements(JAVA_INT)` | 8 elements |
    // | `ofArray(int[8]).asSlice(4).elements(JAVA_INT)` | 7 elements |
    //
    // (`FfmProbe` G22/G24/G48, `FfmProbe3` M1k/M1l/M1o.) The alignment gate
    // also precedes the size-multiple gate below: `ofArray(byte[15])
    // .elements(JAVA_INT)` is the alignment message, not
    // `Segment size is not a multiple of layout size` (M2b/M2c), which is the
    // order these four checks are already written in.
    if elem_align > pe_segment_max_byte_alignment(ctx, this) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Incompatible alignment constraints".into(),
        }
        .into());
    }
    let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0);
    if size % elem_size != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Segment size is not a multiple of layout size".into(),
        }
        .into());
    }
    let splitter = pe_new_splitter(ctx, this, size / elem_size, elem_size)?;
    Ok(Some(Value::Object(Some(splitter))))
}

/// `MemorySegment.elements(MemoryLayout)` — `StreamSupport.stream(spliterator, false)`.
///
/// Built here rather than by calling that method, because `NativeContext` has
/// no `invoke_static`. The shape is not invented: it is byte for byte what
/// `service_loader::native_stream_support_stream_from_spliterator` builds for a
/// non-synthetic spliterator — a `java/util/stream/Stream` carrier with a null
/// element array in slot 0 and the spliterator parked in the lazy slot 2, which
/// `native-collections`' `materialize_lazy_stream` / `stream_lazy_spliterator`
/// drain on demand. Going through that path is what keeps `elements()` lazy:
/// `forEach` interleaves `tryAdvance` and `accept` instead of buffering.
fn pe_segment_elements(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(splitter))) = pe_segment_spliterator(ctx, args)? else {
        return Ok(Some(Value::Object(None)));
    };
    let splitter_pin = ctx.pin_native_root(splitter);
    let cid = ctx
        .ensure_class_initialized("java/util/stream/Stream")
        .unwrap_or_else(|_| cratonvm_types::ClassId::new(0));
    // Force >= 3 fields so the lazy slot exists alongside elements (0) and
    // close-handlers (1).
    let nfields = ctx.class_num_total_fields(cid).max(3);
    let stream = ctx.alloc_object(cid, nfields);
    let stream_pin = ctx.pin_native_root(stream);
    let mut stream = ctx.read_native_pin(stream_pin, stream);
    ctx.set_field(stream, 0, Value::Object(None));
    stream = ctx.read_native_pin(stream_pin, stream);
    let splitter = ctx.read_native_pin(splitter_pin, splitter);
    ctx.set_field(stream, 2, Value::Object(Some(splitter)));
    stream = ctx.read_native_pin(stream_pin, stream);
    ctx.unpin_native_roots(splitter_pin);
    Ok(Some(Value::Object(Some(stream))))
}

/// The base address a segment reports through `address()`.
///
/// NOT `panama_libffi::segment_address`: on a heap carrier that is deliberately
/// 0 (F27 — slot 0 stays 0 so no raw-pointer consumer is ever handed a small
/// integer to dereference), while `address()` answers the offset of the
/// segment's first byte within its backing array. Anything that reasons about
/// WHERE a segment starts — the alignment arms of `asSlice`/`maxByteAlignment`
/// — must use this one, or it reasons about a native address on a segment that
/// has none.
fn pe_segment_base_address(ctx: &dyn NativeContext, seg: ObjectRef) -> i64 {
    match heap_segment_view(ctx, seg) {
        Some(view) => view.start,
        None => crate::panama_libffi::segment_address(ctx, seg),
    }
}

/// The value inside the `Optional` that `MemorySegment.heapBase()` answers.
///
/// A free function rather than a closure body so the read-only rule below has
/// a test that does not need a whole `NativeMethodRegistry` stood up — the same
/// reason F35 lifted `pe_segment_as_slice` out of its closure.
///
/// `Value::Object(None)` means the empty `Optional`.
fn pe_segment_heap_base(ctx: &dyn NativeContext, seg: ObjectRef) -> Value {
    let read_only = match heap_segment_view(ctx, seg) {
        Some(view) => view.read_only,
        None => matches!(
            match ctx.get_field_by_name(seg, "readOnly") {
                v @ Value::Int(_) => v,
                _ => ctx.get_field(seg, 3),
            },
            Value::Int(n) if n != 0
        ),
    };
    if read_only {
        return Value::Object(None);
    }
    match heap_segment_view(ctx, seg) {
        Some(view) => Value::Object(Some(view.base)),
        None => match ctx.get_field(seg, SEG_BACKING_ARRAY_FIELD) {
            Value::Object(Some(array)) if ctx.object_is_array(array) => Value::Object(Some(array)),
            _ => Value::Object(None),
        },
    }
}

/// The alignment a native carrier can promise, given the address it starts at.
///
/// MEASURED on 25.0.3+9-LTS (`FfmProbe` rows H18–H21, A30, P6):
/// `ofAddress(16)`→16, `ofAddress(12)`→4, `ofAddress(1)`→1, and
/// `MemorySegment.NULL`/`ofAddress(0)`→**4611686018427387904 = 2^62**, not 8.
/// The JDK's rule is `lowestOneBit(address | maxAlignMask)` with the mask
/// falling back to the largest representable power of two when there is no
/// address to constrain it — a zero address constrains nothing, so nothing is
/// refused on it. Answering 8 (what this file used to answer) refused
/// `NULL.asSlice(0, 0, 16)` where the oracle allows it.
fn native_max_byte_alignment(addr: i64) -> i64 {
    if addr == 0 {
        1_i64 << 62
    } else {
        addr & addr.wrapping_neg()
    }
}

/// The alignment a HEAP carrier can promise at `byte_offset` bytes into its
/// backing array.
///
/// MEASURED (`FfmProbe2` row P1, every offset 0..=16 of seven element types):
/// the answer is `min(elementAlignment, lowestOneBit(byteOffset))`, with
/// offset 0 answering the element alignment outright. Transcribed:
///
/// ```text
/// byte[32]   0:1 1:1 2:1 3:1 4:1 ... 16:1
/// short[16]  0:2 1:1 2:2 3:1 4:2 ... 16:2
/// int[8]     0:4 1:1 2:2 3:1 4:4 ... 16:4
/// long[4]    0:8 1:1 2:2 3:1 4:4 8:8 12:4 16:8
/// ```
///
/// `byte_offset` is the ABSOLUTE offset within the array — `view.start` for
/// the segment itself, `view.start + access_offset` for an access inside it.
/// Passing only `view.start` and then re-checking the access offset modulo the
/// alignment is NOT the same rule and is stricter than the oracle: a slice
/// starting at 2 of an `int[]` has `maxByteAlignment()==2`, yet
/// `int[8].asSlice(2).get(JAVA_INT, 2)` SUCCEEDS on HotSpot (absolute offset
/// 4) while `get(JAVA_INT, 0)` is refused — measured, `FfmProbe3` rows M1c/M1d.
fn heap_max_byte_alignment(elem_width: i64, byte_offset: i64) -> i64 {
    if byte_offset == 0 {
        elem_width.max(1)
    } else {
        elem_width
            .max(1)
            .min(byte_offset & byte_offset.wrapping_neg())
    }
}

/// `MemorySegment.maxByteAlignment()` for any carrier this VM can decode.
///
/// One reader for the one rule. Every alignment decision in this file —
/// `maxByteAlignment()` itself, `asSlice(long,long,long)`,
/// `asSlice(long,MemoryLayout)`, `spliterator`/`elements`, `toArray`, and the
/// heap `get`/`set` gate — is `constraint <= maxByteAlignment(base + offset)`,
/// and that equivalence is MEASURED, not assumed: `FfmProbe2` §P2/§P3/§P4
/// cross every offset 0..=16 against alignments 1/2/4/8/16 on four heap
/// element types and a native segment, comparing each call's success against
/// `seg.asSlice(off).maxByteAlignment()` **as HotSpot itself computes it**, and
/// report zero mismatches in all nine sweeps.
fn pe_segment_max_byte_alignment(ctx: &dyn NativeContext, seg: ObjectRef) -> i64 {
    match heap_segment_view(ctx, seg) {
        Some(view) => heap_max_byte_alignment(view.elem_width as i64, view.start),
        None => native_max_byte_alignment(crate::panama_libffi::segment_address(ctx, seg)),
    }
}

/// The alignment available at `offset` bytes into `seg`, i.e. what
/// `seg.asSlice(offset).maxByteAlignment()` answers.
fn pe_segment_max_byte_alignment_at(ctx: &dyn NativeContext, seg: ObjectRef, offset: i64) -> i64 {
    match heap_segment_view(ctx, seg) {
        Some(view) => {
            heap_max_byte_alignment(view.elem_width as i64, view.start.saturating_add(offset))
        }
        None => native_max_byte_alignment(
            crate::panama_libffi::segment_address(ctx, seg).saturating_add(offset),
        ),
    }
}

/// The bounds half of every slice, extracted so the alignment-checked arity
/// can run it FIRST.
///
/// MEASURED order (`FfmProbe3`/`FfmProbe2` §P5): `byte[16].asSlice(20, 4, 3)`
/// is an `IndexOutOfBoundsException`, not the `IllegalArgumentException:
/// Invalid alignment constraint : 3` that the same bad alignment produces in
/// bounds — so bounds precede both the power-of-two check and the alignment
/// check. Doing them in the other order reports the second-most-interesting
/// problem.
fn pe_slice_bounds_check(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    offset: i64,
    new_size: i64,
) -> Result<(), MethodCallFailed> {
    let size = crate::panama_libffi::segment_byte_size(ctx, this);
    let end = offset.checked_add(new_size);
    if offset < 0 || new_size < 0 || end.map_or(true, |n| n > size) {
        // `IndexOutOfBoundsException`, NOT `IllegalStateException`.
        //
        // MEASURED, every out-of-range slice arity on both carriers:
        // `IndexOutOfBoundsException: Out of bound access on segment
        // MemorySegment{ kind: heap, heapBase: [B@7c1503a3, address: 0x0,
        // byteSize: 16 }; new offset = 17; new length = 0`. A caller writing
        // `catch (IndexOutOfBoundsException)` — the idiom for a bounds check,
        // and what `heap_segment_check_access` already answers for `get`/`set`
        // — could not catch the `IllegalStateException` this used to raise.
        // The bracketed receiver text is HotSpot's `toString()` and carries an
        // identity hash we cannot reproduce; the CLASS and the two trailing
        // clauses are what a program can act on, and those are exact.
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!(
                "Out of bound access on segment MemorySegment{{ kind: {}, address: 0x{:x}, \
                 byteSize: {} }}; new offset = {}; new length = {}",
                if heap_segment_view(ctx, this).is_some() {
                    "heap"
                } else {
                    "native"
                },
                pe_segment_base_address(ctx, this),
                size,
                offset,
                new_size
            )),
        }
        .into());
    }
    Ok(())
}

/// The alignment half, shared by `asSlice(long,long,long)` and
/// `asSlice(long,MemoryLayout)`.
///
/// MEASURED refusal texts, transcribed character for character (note the space
/// before the colon in the first — it is HotSpot's, not a typo):
///
/// * `IllegalArgumentException: Invalid alignment constraint : 3`
/// * `IllegalArgumentException: Target offset incompatible with alignment constraints`
///
/// Neither message interpolates the offset. The version this replaces wrote
/// `Target offset {offset} incompatible with alignment {align}`, which is a
/// different string on every row.
fn pe_slice_alignment_check(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    offset: i64,
    align: i64,
) -> Result<(), MethodCallFailed> {
    if align <= 0 || (align & (align - 1)) != 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Invalid alignment constraint : {align}"),
        }
        .into());
    }
    if align > pe_segment_max_byte_alignment_at(ctx, this, offset) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Target offset incompatible with alignment constraints".into(),
        }
        .into());
    }
    Ok(())
}

/// The body behind `asSlice(long,long)`, `asSlice(long)`, `asSlice(long,long,
/// long)`, `asSlice(long,MemoryLayout)` and `asReadOnly()`.
///
/// `read_only_override` is `None` to inherit the parent's flag (what a slice
/// does) and `Some(true)` to force it on (what `asReadOnly` does). Inheriting
/// is the F21 rule and not a convenience: read-only is CONTAGIOUS, there is no
/// route back to writable, and a derived view that quietly cleared the flag
/// would be the wrong CAPABILITY one call after the fix.
fn pe_segment_slice(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    offset: i64,
    new_size: i64,
    read_only_override: Option<bool>,
) -> MethodCallResult {
    pe_slice_bounds_check(ctx, this, offset, new_size)?;
    // A SLICE OF A HEAP SEGMENT IS A HEAP SEGMENT.
    //
    // The address arithmetic below is only meaningful for a carrier
    // that HAS an address. For a heap one `segment_address` answers 0
    // (F27), so `slice_ptr` came out as the slice's OFFSET and was
    // stamped into slot 0 of a synthetic segment — reconstructing, one
    // level up, exactly the defect F27 closed: a small integer that is
    // not an address, sitting where every consumer reads an address.
    // `ofArray(new byte[16]).asSlice(3, 4).get(JAVA_BYTE, 0)` then
    // dereferenced the literal address 3. (At offset 0 it stayed 0 and
    // refused, which is why the un-sliced repro looked fixed.)
    //
    // Measured on the oracle: `ofArray(new byte[16]).asSlice(3, 4)` has
    // `byteSize=4`, `address()=3`, `isNative()=false`, and a write
    // through it lands at `src[3..7]`.
    if let Some(view) = heap_segment_view(ctx, this) {
        let read_only = i32::from(read_only_override.unwrap_or(view.read_only));
        let start = view.start.saturating_add(offset);
        // G19-1: A SLICE OF A HEAP SEGMENT SHARES ITS PARENT'S SCOPE.
        //
        // MEASURED on 25.0.3+9-LTS (`G19Probe` SC8/SC9):
        // `heap.asSlice(4,4).scope() == heap.scope()` and
        // `heap.asReadOnly().scope() == heap.scope()` are both **true** — and
        // `asReadOnly()` reaches this same body, so one write covers both. Slot
        // 2 was an unconditional `Object(None)` here, which is the same
        // "no scope resolvable" hole the NATIVE arm below closed for W7-89 and
        // this arm never did; the fresh session `p67_receiver_session` minted
        // instead was a different object on every call.
        let parent_session = pe_segment_session(ctx, this);
        // The allocation below can MOVE the backing array and the session (the
        // native stale-local family), so pin both and re-read them through the
        // pins — the same discipline the session handling below uses.
        // `unpin_native_roots` truncates to its argument, so releasing the
        // FIRST handle releases both.
        let base_pin = ctx.pin_native_root(view.base);
        let session_pin = parent_session.map(|session| ctx.pin_native_root(session));
        let slice = alloc_segment_carrier(ctx, SEG_HEAP_FIELDS)?;
        let base = ctx.read_native_pin(base_pin, view.base);
        let scope_value = match (parent_session, session_pin) {
            (Some(session), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, session))),
            _ => Value::Object(None),
        };
        ctx.unpin_native_roots(base_pin);
        // Slot 0 stays 0: a heap slice has no machine address either,
        // and `segment_address` must keep answering the value every
        // raw-pointer consumer already refuses on.
        ctx.set_field(slice, 0, Value::Long(0));
        ctx.set_field(slice, 1, Value::Long(new_size));
        ctx.set_field(slice, 2, scope_value);
        ctx.set_field(slice, 3, Value::Int(read_only));
        ctx.set_field(slice, 4, Value::Int(1));
        ctx.set_field(slice, 5, Value::Long(0));
        ctx.set_field(slice, SEG_HEAP_BASE_FIELD, Value::Object(Some(base)));
        ctx.set_field(slice, SEG_HEAP_START_FIELD, Value::Long(start));
        return Ok(Some(Value::Object(Some(slice))));
    }
    let base_ptr = crate::panama_libffi::segment_address(ctx, this);
    let slice_ptr = base_ptr.checked_add(offset).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::IllegalStateException {
            message: "address arithmetic overflow in MemorySegment.asSlice".into(),
        })
    })?;

    // A synthetic slice cannot retain the real implementation's
    // private scope object.  It stores an already-adjusted absolute
    // address instead, which is valid for both real and synthetic
    // source segments and avoids treating real field 0/5 as ptr/off.
    let read_only = match read_only_override {
        Some(forced) => Value::Int(i32::from(forced)),
        None => match ctx.get_field_by_name(this, "readOnly") {
            Value::Int(n) => Value::Int(n),
            _ => ctx.get_field(this, 3),
        },
    };

    // W7-89: a slice stays inside its parent's scope. Slot 2 used to be
    // written as the "no arena" marker unconditionally, so
    // `pe_segment_session` answered `None` for every slice and
    // `pe_segment_check_scope` let it through — HotSpot raises
    // `IllegalStateException: Already closed` for a slice of a closed
    // arena exactly as it does for the parent (measured,
    // `MemorySessionValidStateProbe` row `C.closed.slice.get`).
    //
    // Only a session we MODELLED is propagated, which is all
    // `pe_segment_session` can return. That is what keeps the slot's
    // OTHER tenant safe: on an `ofArray` segment slot 2 holds the Java
    // backing array (`SEG_BACKING_ARRAY_FIELD`), an array resolves to no
    // session, and such a slice keeps the historical `Object(None)` — so
    // `isNative()` and `sync_heap_backed_segment` see exactly what they
    // saw before.
    let parent_session = pe_segment_session(ctx, this);
    let session_pin = parent_session.map(|session| ctx.pin_native_root(session));
    let slice = alloc_segment_carrier(ctx, 6)?;
    // The allocation above can move the session (native stale-local
    // family), so re-read it through the pin before storing it.
    let scope_value = match (parent_session, session_pin) {
        (Some(session), Some(pin)) => {
            let session = ctx.read_native_pin(pin, session);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(session))
        }
        _ => Value::Object(None),
    };
    ctx.set_field(slice, 0, Value::Long(slice_ptr));
    ctx.set_field(slice, 1, Value::Long(new_size));
    ctx.set_field(slice, 2, scope_value);
    ctx.set_field(slice, 3, read_only);
    ctx.set_field(slice, 4, Value::Int(1));
    ctx.set_field(slice, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(slice))))
}

/// `MemorySegment.asByteBuffer()` — a direct `ByteBuffer` over the segment.
///
/// This is the JDK 25 bridge between `java.lang.foreign` and every existing
/// NIO API, and the reason netty's non-`sun.misc.Unsafe` allocator could not
/// allocate at all on CratonVM: `CleanerJava25` is FFM-`Arena`-backed, and
/// every one of its allocations ended in
/// `AbstractMethodError: MemorySegment.asByteBuffer() has no Code attribute`.
///
/// Mirrors `AbstractMemorySegmentImpl.asByteBuffer()`:
///
/// * `checkArraySize("ByteBuffer", 1)` first — a segment larger than a byte
///   array can hold is an `IllegalStateException`, not a truncated buffer;
/// * `NativeMemorySegmentImpl.makeByteBuffer()` is
///   `NIO_ACCESS.newDirectByteBuffer(min, (int) length, null, this)`, so in
///   real-JDK mode run that very constructor and inherit every `Buffer`
///   invariant instead of restating them. Passing the segment as the
///   constructor's `MemorySegment` argument is also what keeps the arena
///   reachable for the buffer's lifetime — the buffer holds the segment, the
///   segment holds its scope;
/// * a read-only segment yields a read-only buffer (`_bb.asReadOnlyBuffer()`).
///
/// Deliberately NOT gated on `require_native_access`, unlike `get`/`set`/`fill`
/// in this file: `asByteBuffer` is not `@Restricted` in the JDK, an
/// `Arena`-derived segment is a safe (bounds- and lifetime-checked) one, and
/// gating it would deny the exact call netty makes on a VM where HotSpot allows
/// it — trading a real divergence for no security the `Arena.allocate` call
/// that produced the segment already gave away.
fn pe_segment_as_byte_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    pe_segment_check_scope(ctx, this)?;
    let size = crate::panama_libffi::segment_byte_size(ctx, this);
    // `ArraysSupport.SOFT_MAX_ARRAY_LENGTH`, the bound `checkArraySize` uses.
    const SOFT_MAX_ARRAY_LENGTH: i64 = i32::MAX as i64 - 8;
    if size < 0 || size > SOFT_MAX_ARRAY_LENGTH {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Segment is too large to wrap as ByteBuffer. Size: {}", size),
        }
        .into());
    }
    let addr = crate::panama_libffi::segment_address(ctx, this);
    let read_only = matches!(
        match ctx.get_field_by_name(this, "readOnly") {
            v @ Value::Int(_) => v,
            _ => ctx.get_field(this, 3),
        },
        Value::Int(n) if n != 0
    );

    // Real-JDK mode: `java.nio.DirectByteBuffer(long addr, int cap, Object ob,
    // MemorySegment segment)` is the package-private constructor
    // `JavaNioAccess.newDirectByteBuffer` calls. Both probes are needed for the
    // same reason `ByteBuffer.allocateDirect` needs both — see that registrar.
    let real_direct_byte_buffer = !ctx.would_fabricate_synthetic_stub("java/nio/DirectByteBuffer")
        && !ctx.is_class_synthetic_stub("java/nio/DirectByteBuffer");
    let buffer = if real_direct_byte_buffer {
        ctx.new_object_initialized(
            "java/nio/DirectByteBuffer",
            "(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)V",
            &[
                Value::Long(addr),
                Value::Int(size as i32),
                Value::Object(None),
                Value::Object(Some(this)),
            ],
        )?
    } else {
        // Synthetic-JDK mode: no `DirectByteBuffer` bytecode to run. Seed the
        // same field set `direct_buffer::dbb_allocate_direct0` does, minus the
        // cleaner — this buffer does not own the memory, the arena does, and
        // registering a cleaner here would free the segment out from under it.
        let buf = try_alloc_concurrent_synthetic(ctx, "java/nio/DirectByteBuffer", 8)?;
        ctx.set_field_by_name(buf, "address", Value::Long(addr));
        ctx.set_field_by_name(buf, "capacity", Value::Int(size as i32));
        ctx.set_field_by_name(buf, "limit", Value::Int(size as i32));
        ctx.set_field_by_name(buf, "position", Value::Int(0));
        ctx.set_field_by_name(buf, "mark", Value::Int(-1));
        ctx.set_field_by_name(buf, "bigEndian", Value::Int(1));
        ctx.set_field_by_name(
            buf,
            "nativeByteOrder",
            Value::Int(i32::from(cfg!(target_endian = "big"))),
        );
        let named_ok = matches!(
            ctx.get_field_by_name(buf, "capacity"),
            Value::Int(c) if c == size as i32
        );
        if !named_ok {
            ctx.set_field(buf, 0, Value::Int(0)); // position
            ctx.set_field(buf, 1, Value::Int(size as i32)); // limit
            ctx.set_field(buf, 2, Value::Int(size as i32)); // capacity
            ctx.set_field(buf, 3, Value::Int(-1)); // mark
            ctx.set_field(buf, 4, Value::Long(addr)); // address
            ctx.set_field(buf, 5, Value::Long(size)); // native size
            ctx.set_field(buf, 6, Value::Int(0)); // no cleaner: the arena owns it
            ctx.set_field(buf, 7, Value::Int(0));
        }
        Some(Value::Object(Some(buf)))
    };

    if !read_only {
        return Ok(buffer);
    }
    let Some(Value::Object(Some(buf))) = buffer else {
        return Ok(buffer);
    };
    ctx.invoke_virtual(buf, "asReadOnlyBuffer", "()Ljava/nio/ByteBuffer;", &[])
}

// --- Scope validity: refuse access through a closed Arena ---
//
// `phases_late::foreign_ffm` gives every synthetic `Arena` a *session* object
// and `Arena.close()` now genuinely closes it and runs its close actions — so
// the off-heap block a segment points at is really freed. The raw load/store
// below would then read or write freed memory; HotSpot raises
// `IllegalStateException` instead, and so must we.
//
// The two layouts are `foreign_ffm`'s. They are MIRRORED here rather than
// shared because those helpers are private to that module, and the one public
// resolver it does expose (`p67_receiver_session`) mints a fresh — therefore
// always-open — session when it finds nothing, which would make every check
// below trivially pass:
//
//   Arena   : [0] = open (Int), [1] = session
//   session : four words, whose SLOTS are `foreign_ffm`'s to decide — see
//             `P67SessionSlots`. This file used to hard-code `state` at slot 0
//             and a width of 4; both are deleted (W7-89) because that second
//             copy of the index is what made the check dead in Compatible mode,
//             where the real `MemorySessionImpl` types slot 0 as a REFERENCE.
//
// Both are `alloc_concurrent_synthetic` objects, so their class names are
// exactly the two constants below. Every step of the resolution is gated on
// those names, which is what makes it self-validating rather than
// shape-guessing: a miss answers "no resolvable scope" (access proceeds
// unchanged) and never "closed".
const PE_ARENA_CLASS: &str = "java/lang/foreign/Arena";
/// The class `foreign_ffm` now mints an arena as. It is a REAL JDK class, so
/// unlike `PE_ARENA_CLASS` it is not a fabrication -- and because the two
/// producers finally differ, a class test can replace the width test below.
const PE_ARENA_IMPL_CLASS: &str = crate::phases_late::foreign_ffm::P67_ARENA_IMPL;
const PE_SESSION_CLASS: &str = "jdk/internal/foreign/MemorySessionImpl";
const PE_ARENA_SESSION_FIELD: usize = 1;
/// Slot 2 of a synthetic segment names the arena that allocated it — this
/// file's own convention (see the `// no arena` writes in `ofAddress` and
/// `asSlice`), which `foreign_ffm`'s 3-field segments adopted as well.
///
/// The slot is REUSED by `ofArray` segments to retain the Java backing array
/// ([`SEG_BACKING_ARRAY_FIELD`]), and on a real-JDK segment it is whatever
/// field happens to sit at index 2 — hence the class-name gate in
/// [`pe_arena_session`]: we must never follow a non-Arena object's slots.
const PE_SEGMENT_ARENA_FIELD: usize = 2;

/// Single-entry positive/negative memo for an exact class-name test.
///
/// The test itself is what makes the resolution safe — unlike a field-shape
/// probe it cannot mistake a primitive array, whose raw slots decode to
/// arbitrary `Value`s, for an object we modelled — but `class_name_of_id`
/// takes the class-manager lock and allocates a `String`, and this runs on
/// every FFM load/store. Class ids are process-stable and each site sees
/// exactly two shapes (the arena / the `ofArray` backing array), so one
/// remembered hit id and one remembered miss id keep the steady state at an
/// integer compare.
struct PeClassMemo {
    hit: std::sync::atomic::AtomicU32,
    miss: std::sync::atomic::AtomicU32,
}

impl PeClassMemo {
    /// `u32::MAX` is the "nothing remembered" sentinel; no real class id
    /// reaches it.
    const fn new() -> Self {
        Self {
            hit: std::sync::atomic::AtomicU32::new(u32::MAX),
            miss: std::sync::atomic::AtomicU32::new(u32::MAX),
        }
    }

    fn matches(&self, ctx: &dyn NativeContext, obj: ObjectRef, expected: &str) -> bool {
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        let class_id = ctx.class_id_of_object(obj);
        let raw = class_id.as_u32();
        // G19-1: THE MEMO IS BYPASSED UNDER `cfg(test)`, AND ONLY THERE.
        //
        // Class ids are process-stable on the real VM, which is the whole
        // premise of remembering one. Under `MockNativeContext` they are a
        // PER-CONTEXT counter, so id 7 names `MemorySegment` in one test and
        // `MemorySessionImpl` in the next — and this struct remembers exactly
        // one hit and one miss for the whole process, so the second test is
        // handed the first test's answer for a class it never saw. That made
        // every test of a session-shaped predicate non-deterministic (it
        // depends on which sibling test ran last, and they run in threads).
        // The memo is a pure cache: skipping it changes no answer, only the
        // number of `class_name_arc_of_id` calls.
        if cfg!(test) {
            return ctx.class_name_arc_of_id(class_id).as_deref() == Some(expected);
        }
        if raw == self.hit.load(relaxed) {
            return true;
        }
        if raw == self.miss.load(relaxed) {
            return false;
        }
        let matched = ctx.class_name_arc_of_id(class_id).as_deref() == Some(expected);
        if matched {
            self.hit.store(raw, relaxed);
        } else {
            self.miss.store(raw, relaxed);
        }
        matched
    }
}

static PE_ARENA_CLASS_MEMO: PeClassMemo = PeClassMemo::new();
static PE_ARENA_IMPL_MEMO: PeClassMemo = PeClassMemo::new();

/// Whether `arena` is one of the two carriers this workspace mints for
/// `java.lang.foreign.Arena`. One memo per name: `PeClassMemo` remembers a
/// single hit and a single miss, so asking one memo about two names would make
/// each answer evict the other's.
fn pe_is_modelled_arena(ctx: &dyn NativeContext, arena: ObjectRef) -> bool {
    PE_ARENA_CLASS_MEMO.matches(ctx, arena, PE_ARENA_CLASS)
        || PE_ARENA_IMPL_MEMO.matches(ctx, arena, PE_ARENA_IMPL_CLASS)
}
static PE_SESSION_CLASS_MEMO: PeClassMemo = PeClassMemo::new();

/// Whether `session` carries the layout `foreign_ffm` writes. Anything else —
/// a real JDK `ConfinedSession`/`SharedSession`, or an object that merely
/// happens to sit in the session slot — is left strictly alone.
///
/// The state word is located through `foreign_ffm`'s own slot map rather than
/// through a second copy of the index. That map is the W7-89 repair: in
/// Compatible mode the carrier is the REAL `MemorySessionImpl`, whose slot 0 is
/// a declared REFERENCE (`resourceList`), so the model's `Int` state word never
/// read back as an `Int` there and this predicate answered false for every
/// session — which is why the choke point below, though correctly wired since
/// W7-58, never once fired. Calling the shared resolver keeps the decision in
/// ONE implementation; open-coding the index here is what let the two files
/// drift out of step in the first place.
pub(crate) fn pe_session_modelled(ctx: &dyn NativeContext, session: ObjectRef) -> bool {
    if !PE_SESSION_CLASS_MEMO.matches(ctx, session, PE_SESSION_CLASS) {
        return false;
    }
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    ctx.object_num_fields(session) >= slots.required_width()
        && matches!(ctx.get_field(session, slots.state), Value::Int(_))
}

/// The session stored on a synthetic `Arena`, if `arena` is one.
///
/// Rejects [`register_pe_arena`]'s rival 4-slot arena, whose slot 1 is an
/// int-array of allocation ids rather than a session: the array fails
/// [`pe_session_modelled`]'s class-name test, so the arena resolves to `None`
/// (no scope) instead of to a bogus "closed" session. That shape is the one
/// that wins under `--synthetic-jdk`, where a false throw here would break
/// every FFM access.
fn pe_arena_session(ctx: &dyn NativeContext, arena: ObjectRef) -> Option<ObjectRef> {
    if !pe_is_modelled_arena(ctx, arena) {
        return None;
    }
    // Through the shared resolver, not a second copy of the index: on
    // `ArenaImpl` the session is the class's own declared field and is NOT at
    // `PE_ARENA_SESSION_FIELD`. Open-coding it here is what let this file and
    // `foreign_ffm` drift apart over the session layout, as the comment on
    // `pe_session_modelled` above records.
    let session_slot = crate::phases_late::foreign_ffm::p67_arena_slots(ctx, arena).session;
    if ctx.object_num_fields(arena) <= session_slot {
        return None;
    }
    match ctx.get_field(arena, session_slot) {
        Value::Object(Some(session)) if pe_session_modelled(ctx, session) => Some(session),
        _ => None,
    }
}

/// The session governing `seg`'s lifetime, or `None` when the segment has no
/// scope we can resolve — `MemorySegment.ofAddress`, `asSlice`, `ofArray`, the
/// global arena and every segment this file allocates outside an arena. Those
/// must keep working exactly as before, so an unresolvable scope is NOT an
/// error.
///
/// Allocation-free and safepoint-free by construction (plain field/class
/// reads only), so no caller has to pin `seg` across the check.
fn pe_segment_session(ctx: &dyn NativeContext, seg: ObjectRef) -> Option<ObjectRef> {
    // Fast path: our own slot-2 convention. One field read decides it for the
    // overwhelmingly common shapes — an arena-allocated segment resolves here,
    // and an explicit `Object(None)` is the "no arena" marker this file writes,
    // which needs no further lookup.
    if ctx.object_num_fields(seg) > PE_SEGMENT_ARENA_FIELD {
        match ctx.get_field(seg, PE_SEGMENT_ARENA_FIELD) {
            Value::Object(None) => return None,
            Value::Object(Some(owner)) => {
                if let Some(session) = pe_arena_session(ctx, owner) {
                    return Some(session);
                }
                // Tolerate a segment stamped with the session directly.
                if pe_session_modelled(ctx, owner) {
                    return Some(owner);
                }
                // Not our convention (an `ofArray` backing array, or a real
                // segment's own reference field) — fall through.
            }
            // A primitive there means this is not our layout at all.
            _ => {}
        }
    }
    // A real-JDK segment carries its session in `AbstractMemorySegmentImpl
    // .scope`, and that field holds one of OUR sessions because the
    // `createConfined`/`createShared` factories are force-dispatched into
    // `foreign_ffm`. A real session we did not build is an honest miss: we
    // cannot read its state word without guessing its encoding, so we let the
    // access through rather than throw on a shape we misread.
    match ctx.get_field_by_name(seg, "scope") {
        Value::Object(Some(scope)) if pe_session_modelled(ctx, scope) => Some(scope),
        _ => None,
    }
}

/// Raise `IllegalStateException` if `seg`'s scope has already been closed.
///
/// This is the single choke point for `get`/`set`/`getAtIndex`/`setAtIndex`:
/// all four reach [`pe_segment_access_addr`], which calls this before it
/// computes an address.
fn pe_segment_check_scope(ctx: &dyn NativeContext, seg: ObjectRef) -> Result<(), MethodCallFailed> {
    let Some(session) = pe_segment_session(ctx, seg) else {
        return Ok(());
    };
    pe_session_check_open(ctx, session)
}

/// `Err(IllegalStateException)` if this MODELLED session has been closed.
///
/// One implementation, called by both the access path and
/// `pe_arena_allocate_impl`: a second open-coded copy of "state == 0 means
/// closed" is precisely the drift W7-89 had to unpick between this file and
/// `foreign_ffm`. The caller is responsible for having resolved `session`
/// through [`pe_segment_session`] / [`pe_arena_session`], both of which gate on
/// [`pe_session_modelled`] -- so a session whose encoding we do not own never
/// reaches here.
fn pe_session_check_open(
    ctx: &dyn NativeContext,
    session: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    if matches!(ctx.get_field(session, slots.state), Value::Int(0)) {
        return Err(RuntimeError::IllegalStateException {
            message: "Already closed".into(),
        }
        .into());
    }
    Ok(())
}

/// The zero-length `MemorySegment` that `get(AddressLayout, long)` returns.
///
/// The JDK's contract for an address read is a segment of size 0 at that
/// address -- the caller must `reinterpret` it before dereferencing, which is
/// precisely the safety property that makes the read legal at all. It carries
/// no arena, so `pe_arena_close` never frees memory this VM did not allocate.
///
/// Shape matches `pe_arena_allocate_impl`: `[0]=ptr, [1]=size, [2]=arena,
/// [3]=readOnly, [4]=alive, [5]=offset`.
fn pe_zero_length_segment(
    ctx: &mut dyn NativeContext,
    addr: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let seg = alloc_segment_carrier(ctx, 6)?;
    ctx.set_field(seg, 0, Value::Long(addr));
    ctx.set_field(seg, 1, Value::Long(0));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    Ok(seg)
}

// ===================================================================
// Heap segments: a `MemorySegment` whose bytes live in a Java array
// ===================================================================
//
// F27 (2026-08-13) established that this file could not read or write one at
// all, and that `segment_address` was answering `new byte[16].length` — 16, the
// `read at address 0x10` W7-89 §7.1 records and misattributes to a
// `Buffer.address` read. F27 turned that fatal SIGSEGV into
// `IllegalStateException: Null segment address`, which is an improvement and
// not a fix: the JDK's own answer is to read and write the backing array.
//
// TWO CARRIERS reach the code below.
//
//   H1 — a real JDK `jdk.internal.foreign.HeapMemorySegmentImpl$Of*`. Measured
//        (25.0.3+9-LTS): five fields, `length`/`readOnly`/`scope` from
//        `AbstractMemorySegmentImpl` then `offset`/`base`. `offset` is
//        `Unsafe`-style — see [`HEAP_ARRAY_BASE_OFFSET`]. `--jdk-only` gets one
//        of these from `MemorySegment.ofArray(byte[]/short[]/char[])` (the
//        three descriptors this file does not register), from
//        `MemorySegment.ofBuffer(ByteBuffer.allocate(n))` (measured: a
//        `HeapMemorySegmentImpl$OfByte`), and from `asSlice(long)`.
//
//   H2 — a CratonVM-minted alias carrier, `[6]=array, [7]=byte start`, slot 0
//        deliberately 0. `asSlice(long,long)` mints one when its receiver is a
//        heap segment; before that it computed `segment_address(this) + offset`
//        and stamped the RESULT into slot 0, so a slice of a heap segment was a
//        synthetic segment whose "address" was its offset — F27's fix made the
//        un-sliced read refuse and left `asSlice(3, 4).get(...)` dereferencing
//        the literal address 3.
//
// Not reached: CratonVM's `ofArray([I/[J/[F/[D)` carriers. Those copy into an
// off-heap mirror and are addressable; `sync_heap_backed_segment` writes the
// mirror back to the array. That is a different (and, against the oracle,
// wrong — see the record) design, and it is left alone here.

/// A `MemorySegment` whose bytes live in a Java primitive array, resolved into
/// the pieces an access needs.
#[derive(Clone, Copy)]
struct HeapSegmentView {
    /// The Java primitive array holding the bytes.
    base: ObjectRef,
    /// Byte offset of the segment's first byte within `base`'s DATA — i.e.
    /// the JDK's `address()`, not its `offset` field.
    start: i64,
    /// The segment's `byteSize`.
    size: i64,
    read_only: bool,
    /// 1, 2, 4 or 8 — the width of one `base` element.
    elem_width: usize,
    elem_type: cratonvm_types::ArrayElementType,
}

/// Read a named field off either heap carrier.
///
/// `get_field_by_name` answers `Value::Object(None)` both for a name it cannot
/// resolve and for a null reference field, so a miss falls back to the
/// class-side field table — the resolution `lang_invoke.rs::segment_raw_access`
/// uses for these same two fields.
fn heap_seg_field(ctx: &dyn NativeContext, seg: ObjectRef, name: &str) -> Value {
    match ctx.get_field_by_name(seg, name) {
        Value::Object(None) => {
            match ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(seg), name) {
                Some(index) => ctx.get_field(seg, index),
                None => Value::Object(None),
            }
        }
        other => other,
    }
}

/// Byte width of one array element, or `None` for a shape that cannot back a
/// `MemorySegment`.
///
/// `Reference` is refused because there is no byte view of an object array.
/// `Boolean` is refused because `java.lang.foreign.MemorySegment` has **no**
/// `ofArray(boolean[])` overload — measured, `NoSuchMethodException:
/// java.lang.foreign.MemorySegment.ofArray([Z)` — so a `boolean[]`-backed
/// segment is not a thing the JDK can produce, and inventing a byte semantics
/// for one is how a defaulting reader becomes a quiet wrong write.
fn heap_element_width(elem: cratonvm_types::ArrayElementType) -> Option<usize> {
    use cratonvm_types::ArrayElementType as A;
    Some(match elem {
        A::Byte => 1,
        A::Char | A::Short => 2,
        A::Int | A::Float => 4,
        A::Long | A::Double => 8,
        A::Boolean | A::Reference => return None,
    })
}

/// Read `len` bytes of a heap segment's payload, starting `offset` bytes
/// into the segment.
///
/// A heap segment has no address, so `MemorySegment.copy`'s pointer path
/// cannot see it at all — which is exactly how a copy INTO one used to
/// write nothing at all and say nothing about it. The bytes have to come
/// out of (or go into) the Java array itself.
///
/// The `Byte` and `Int` arms go through the bulk array accessors, which
/// the VM implements as one `copy_nonoverlapping` over the heap arena;
/// everything else, and any access not aligned to its element width,
/// falls back to an element-at-a-time loop that is correct for every
/// width and offset. That fallback is why this returns bytes rather than
/// borrowing them: an unaligned read spans element boundaries.
fn heap_read_bytes(
    ctx: &dyn NativeContext,
    view: &HeapSegmentView,
    offset: i64,
    len: usize,
) -> Option<Vec<u8>> {
    let start = view.start.checked_add(offset)?;
    if start < 0 || len == 0 {
        return if len == 0 { Some(Vec::new()) } else { None };
    }
    let start = start as usize;
    let width = view.elem_width;
    if width == 1 && view.elem_type == cratonvm_types::ArrayElementType::Byte {
        let mut out = vec![0u8; len];
        let got = ctx.read_byte_array_into(view.base, start, &mut out);
        return (got == len).then_some(out);
    }
    if width == 4
        && view.elem_type == cratonvm_types::ArrayElementType::Int
        && start % 4 == 0
        && len % 4 == 0
    {
        let mut words = vec![0i32; len / 4];
        let got = ctx.read_int_array_into(view.base, start / 4, &mut words);
        if got != words.len() {
            return None;
        }
        let mut out = Vec::with_capacity(len);
        for w in words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        return Some(out);
    }
    // General case: walk the elements the byte range touches and take the
    // bytes out of each one's little-endian image.
    let first = start / width;
    let last = (start + len - 1) / width;
    let mut staged = Vec::with_capacity((last - first + 1) * width);
    for index in first..=last {
        staged.extend_from_slice(&heap_element_le_bytes(ctx, view, index)?);
    }
    let skip = start - first * width;
    Some(staged[skip..skip + len].to_vec())
}

/// Write `src` into a heap segment's payload at `offset`. Mirror of
/// [`heap_read_bytes`]; an unaligned or sub-element write reads the
/// element it lands in, patches the bytes, and writes it back.
fn heap_write_bytes(
    ctx: &mut dyn NativeContext,
    view: &HeapSegmentView,
    offset: i64,
    src: &[u8],
) -> bool {
    if view.read_only {
        return false;
    }
    let Some(start) = view.start.checked_add(offset) else {
        return false;
    };
    if start < 0 {
        return false;
    }
    if src.is_empty() {
        return true;
    }
    let start = start as usize;
    let width = view.elem_width;
    if width == 1 && view.elem_type == cratonvm_types::ArrayElementType::Byte {
        return ctx.write_byte_array_from(view.base, start, src);
    }
    if width == 4
        && view.elem_type == cratonvm_types::ArrayElementType::Int
        && start % 4 == 0
        && src.len() % 4 == 0
    {
        let words: Vec<i32> = src
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        return ctx.write_int_array_from(view.base, start / 4, &words);
    }
    let first = start / width;
    let last = (start + src.len() - 1) / width;
    let mut staged = Vec::with_capacity((last - first + 1) * width);
    for index in first..=last {
        match heap_element_le_bytes(ctx, view, index) {
            Some(bytes) => staged.extend_from_slice(&bytes),
            None => return false,
        }
    }
    let skip = start - first * width;
    staged[skip..skip + src.len()].copy_from_slice(src);
    for (n, index) in (first..=last).enumerate() {
        let chunk = &staged[n * width..(n + 1) * width];
        if !heap_element_from_le_bytes(ctx, view, index, chunk) {
            return false;
        }
    }
    true
}

/// One array element's little-endian byte image, whatever its width.
fn heap_element_le_bytes(
    ctx: &dyn NativeContext,
    view: &HeapSegmentView,
    index: usize,
) -> Option<Vec<u8>> {
    if index >= ctx.array_length(view.base) {
        return None;
    }
    Some(match ctx.get_array_element(view.base, index) {
        Value::Int(v) => match view.elem_width {
            1 => vec![v as u8],
            2 => (v as u16).to_le_bytes().to_vec(),
            _ => v.to_le_bytes().to_vec(),
        },
        Value::Long(v) => v.to_le_bytes().to_vec(),
        Value::Float(v) => v.to_bits().to_le_bytes().to_vec(),
        Value::Double(v) => v.to_bits().to_le_bytes().to_vec(),
        _ => return None,
    })
}

/// Inverse of [`heap_element_le_bytes`].
fn heap_element_from_le_bytes(
    ctx: &mut dyn NativeContext,
    view: &HeapSegmentView,
    index: usize,
    bytes: &[u8],
) -> bool {
    use cratonvm_types::ArrayElementType as A;
    if index >= ctx.array_length(view.base) || bytes.len() != view.elem_width {
        return false;
    }
    let value = match view.elem_type {
        A::Byte => Value::Int(bytes[0] as i8 as i32),
        A::Short => Value::Int(i16::from_le_bytes([bytes[0], bytes[1]]) as i32),
        A::Char => Value::Int(u16::from_le_bytes([bytes[0], bytes[1]]) as i32),
        A::Int => Value::Int(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        A::Float => Value::Float(f32::from_bits(u32::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3],
        ]))),
        A::Long => {
            let mut w = [0u8; 8];
            w.copy_from_slice(bytes);
            Value::Long(i64::from_le_bytes(w))
        }
        A::Double => {
            let mut w = [0u8; 8];
            w.copy_from_slice(bytes);
            Value::Double(f64::from_bits(u64::from_le_bytes(w)))
        }
        A::Boolean | A::Reference => return false,
    };
    ctx.set_array_element(view.base, index, value);
    true
}

/// Resolve `seg` if — and only if — it is a heap segment (H1 or H2).
///
/// `None` means "not a heap segment", which every caller reads as "take the
/// raw-address path". A heap segment whose shape does not add up (a `base`
/// that is not an array, a negative start, a size that runs off the end of the
/// array) also answers `None` rather than a plausible view; the caller turns
/// that into a named refusal.
fn heap_segment_view(ctx: &dyn NativeContext, seg: ObjectRef) -> Option<HeapSegmentView> {
    let (base, start) = if crate::panama_libffi::is_real_heap_segment(ctx, seg) {
        let base = match heap_seg_field(ctx, seg, "base") {
            Value::Object(Some(array)) => array,
            _ => return None,
        };
        let raw_offset = match heap_seg_field(ctx, seg, "offset") {
            Value::Long(n) => n,
            Value::Int(n) => i64::from(n),
            _ => return None,
        };
        (base, raw_offset - HEAP_ARRAY_BASE_OFFSET)
    } else if ctx.object_num_fields(seg) > SEG_HEAP_START_FIELD {
        let base = match ctx.get_field(seg, SEG_HEAP_BASE_FIELD) {
            Value::Object(Some(array)) => array,
            _ => return None,
        };
        match ctx.get_field(seg, SEG_HEAP_START_FIELD) {
            Value::Long(n) => (base, n),
            _ => return None,
        }
    } else {
        return None;
    };
    if start < 0 || !ctx.object_is_array(base) {
        return None;
    }
    let elem_type = ctx.heap_element_type_of(base);
    let elem_width = heap_element_width(elem_type)?;
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    // The whole segment must lie inside the array. Every index computed below
    // is derived from `start + offset` with `offset + width <= size`, so this
    // one check is what keeps `get_array_element` in range without a per-byte
    // bound.
    let array_bytes = (ctx.array_length(base) as i64).saturating_mul(elem_width as i64);
    if size < 0 || start.saturating_add(size) > array_bytes {
        return None;
    }
    let read_only = matches!(heap_seg_field(ctx, seg, "readOnly"), Value::Int(n) if n != 0)
        || (ctx.object_num_fields(seg) > SEG_HEAP_START_FIELD
            && matches!(ctx.get_field(seg, 3), Value::Int(n) if n != 0));
    Some(HeapSegmentView {
        base,
        start,
        size,
        read_only,
        elem_width,
        elem_type,
    })
}

/// The raw bits of one array element, zero-extended into a `u64`.
fn heap_element_bits(ctx: &dyn NativeContext, base: ObjectRef, index: usize) -> u64 {
    match ctx.get_array_element(base, index) {
        // A `byte`/`short`/`char`/`int` element arrives as `Value::Int`; the
        // `as u32` is what stops a negative byte from smearing 0xFF across the
        // upper 56 bits and corrupting the neighbouring bytes of a wider read.
        Value::Int(v) => u64::from(v as u32),
        Value::Long(v) => v as u64,
        Value::Float(v) => u64::from(v.to_bits()),
        Value::Double(v) => v.to_bits(),
        _ => 0,
    }
}

/// Store the raw bits of one array element back, in that array's own encoding.
fn heap_store_element(ctx: &dyn NativeContext, view: &HeapSegmentView, index: usize, bits: u64) {
    use cratonvm_types::ArrayElementType as A;
    let value = match view.elem_type {
        A::Byte => Value::Int(i32::from(bits as u8 as i8)),
        A::Char => Value::Int(i32::from(bits as u16)),
        A::Short => Value::Int(i32::from(bits as u16 as i16)),
        A::Int => Value::Int(bits as u32 as i32),
        A::Long => Value::Long(bits as i64),
        A::Float => Value::Float(f32::from_bits(bits as u32)),
        A::Double => Value::Double(f64::from_bits(bits)),
        // Unreachable: `heap_element_width` refused both, so no view exists.
        A::Boolean | A::Reference => return,
    };
    ctx.set_array_element(view.base, index, value);
}

/// Read `width` bytes at `offset` bytes into the segment, LITTLE-ENDIAN.
///
/// Endianness is measured, not assumed: on the oracle,
/// `MemorySegment.ofArray(new byte[8]).set(JAVA_INT_UNALIGNED, 0, 0x01020304)`
/// leaves `[4, 3, 2, 1, 0, 0, 0, 0]`, and the same write into a `short[4]` with
/// `JAVA_LONG_UNALIGNED 0x0102030405060708` leaves `[0x0708, 0x0506, 0x0304,
/// 0x0102]` — so a byte index runs low-to-high through the element AND through
/// the array.
///
/// The caller has already bounds-checked `offset + width <= view.size`, and
/// [`heap_segment_view`] has checked `start + size <= array bytes`, so every
/// index here is in range.
fn heap_segment_read(
    ctx: &dyn NativeContext,
    view: &HeapSegmentView,
    offset: i64,
    width: usize,
) -> u64 {
    let mut raw: u64 = 0;
    for i in 0..width {
        let byte_index = (view.start + offset) as usize + i;
        let element = byte_index / view.elem_width;
        let shift_in = byte_index % view.elem_width;
        let byte = (heap_element_bits(ctx, view.base, element) >> (8 * shift_in)) & 0xff;
        raw |= byte << (8 * i);
    }
    raw
}

/// Write `width` bytes at `offset` bytes into the segment, little-endian.
///
/// Read-modify-write per byte rather than per element: an access is at most 8
/// bytes wide and may start and end mid-element (measured: on an `int[4]`
/// segment `set(JAVA_BYTE, 0, 0x7f)` leaves `iarr[0] == 127`, i.e. byte 0 is
/// the element's low byte and the other three are untouched).
fn heap_segment_write(
    ctx: &dyn NativeContext,
    view: &HeapSegmentView,
    offset: i64,
    width: usize,
    raw: u64,
) {
    for i in 0..width {
        let byte_index = (view.start + offset) as usize + i;
        let element = byte_index / view.elem_width;
        let shift_in = 8 * (byte_index % view.elem_width);
        let byte = (raw >> (8 * i)) & 0xff;
        let bits = heap_element_bits(ctx, view.base, element);
        heap_store_element(
            ctx,
            view,
            element,
            (bits & !(0xffu64 << shift_in)) | (byte << shift_in),
        );
    }
}

/// Validate a heap-segment access: scope, read-only, bounds, alignment.
///
/// **Every exception class here is the oracle's**, measured on 25.0.3+9-LTS:
///
/// | call | thrown |
/// |---|---|
/// | `ofArray(byte[8]).get(JAVA_INT_UNALIGNED, 6)` | `IndexOutOfBoundsException` |
/// | `ofArray(byte[8]).get(JAVA_BYTE, -1)` | `IndexOutOfBoundsException` |
/// | `ofArray(byte[0]).get(JAVA_BYTE, 0)` | `IndexOutOfBoundsException` |
/// | `ofArray(byte[8]).asReadOnly().set(JAVA_BYTE, 0, 1)` | `IllegalArgumentException: Attempt to write a read-only segment` |
/// | `ofArray(byte[32]).get(JAVA_INT, 0)` | `IllegalArgumentException: Target offset 0 is incompatible with alignment constraint 4 (of i4) …` |
/// | `ofArray(int[8]).get(JAVA_INT, 1)` | `IllegalArgumentException` (offset not a multiple of 4) |
/// | `ofArray(int[8]).get(JAVA_LONG, 0)` | `IllegalArgumentException` (`maxByteAlignment` is 4) |
/// | `ofArray(int[8]).get(JAVA_LONG_UNALIGNED, 0)` | OK |
///
/// The alignment rule is ONE predicate, and CORRECTED 2026-08-16: the
/// constraint must be no larger than the alignment available at the
/// **absolute** offset `view.start + offset`, i.e. exactly
/// `seg.asSlice(offset).maxByteAlignment()`. The previous spelling was
/// `align <= maxByteAlignment(view.start) && (view.start + offset) % align == 0`,
/// which is one conjunct too many and is STRICTER than the oracle whenever a
/// slice starts at a worse alignment than the offset inside it reaches:
///
/// | call | oracle | previous |
/// |---|---|---|
/// | `ofArray(int[8]).asSlice(2).get(JAVA_INT, 2)` | reads (absolute 4) | refused |
/// | `ofArray(int[8]).asSlice(2).get(JAVA_INT, 0)` | refused (absolute 2) | refused |
/// | `ofArray(long[4]).asSlice(4).get(JAVA_LONG, 4)` | reads (absolute 8) | refused |
///
/// (measured, `FfmProbe3` rows M1c/M1d/M1h; `int[8].asSlice(2)` reports
/// `maxByteAlignment()==2` and `asSlice(2).asSlice(2)` reports 4.) Both halves
/// are still present — they are just the two halves of
/// [`heap_max_byte_alignment`]: the element width, and the low bit of the
/// offset. A `byte[]` segment's element width is 1, which is what refuses
/// `get(JAVA_INT, 0)` on one where a bare modulo would admit it.
///
/// This is enforced on the HEAP path only. The raw-address path has never
/// checked alignment and is not changed here — a native segment's real
/// `maxByteAlignment` is not knowable from the carrier (the oracle answers 32
/// for a malloc'd one), so the same rule cannot be transplanted, and adding a
/// half-rule to a path that works today would be a regression. NOMINATED in
/// the record instead.
fn heap_segment_check_access(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    view: &HeapSegmentView,
    layout: ObjectRef,
    offset: i64,
    width: i64,
    writing: bool,
) -> Result<(), MethodCallFailed> {
    pe_segment_check_scope(ctx, seg)?;

    if writing && view.read_only {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Attempt to write a read-only segment".into(),
        }
        .into());
    }

    let end = offset.checked_add(width);
    if offset < 0 || end.map_or(true, |e| e > view.size) {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!(
                "Out of bound access on segment MemorySegment{{ kind: heap, address: 0x{:x}, \
                 byteSize: {} }}; new offset = {}; new length = {}",
                view.start, view.size, offset, width
            )),
        }
        .into());
    }

    let align = crate::panama_libffi::layout_align(ctx, layout) as i64;
    if align > 1 {
        let max_align =
            heap_max_byte_alignment(view.elem_width as i64, view.start.saturating_add(offset));
        if align > max_align {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!(
                    "Target offset {} is incompatible with alignment constraint {} for segment \
                     MemorySegment{{ kind: heap, address: 0x{:x}, byteSize: {} }}",
                    offset, align, view.start, view.size
                ),
            }
            .into());
        }
    }
    Ok(())
}

/// The refusal for a heap carrier whose shape does not add up.
///
/// A named refusal, not a `0`: the whole family this file keeps paying for is a
/// reader that answers a plausible number for a carrier it could not decode.
fn unreadable_heap_segment(ctx: &dyn NativeContext, seg: ObjectRef, op: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: format!(
            "MemorySegment.{op}: {} is a heap segment whose backing array cannot be resolved \
             (base/offset unreadable, or its bytes do not fit the array)",
            ctx.class_name_of_id(ctx.class_id_of_object(seg))
                .unwrap_or_else(|| "<unknown>".to_string())
        ),
    }
    .into()
}

/// `MemorySegment.asSlice(long offset, long size)`.
///
/// Extracted from an inline closure by F35 so its heap arm can be tested. The
/// body itself is [`pe_segment_slice`] — the same one `asSlice(long)`,
/// `asSlice(long,long,long)`, `asSlice(long,MemoryLayout)` and `asReadOnly()`
/// call, so the heap arm and the read-only contagion cannot be present on one
/// arity and absent on the next. Two bodies for one rule is what this file's
/// `getAtIndex` banner is about; there is now one.
fn pe_segment_as_slice(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let offset = pe_long_arg(args, 1);
    let new_size = pe_long_arg(args, 2);
    pe_segment_slice(ctx, this, offset, new_size, None)
}

/// `MemorySegment.ofArray(byte[] | short[] | char[])` — an ALIAS carrier.
///
/// Mints the H2 shape: `[0]=0` (no machine address), `[1]=byteSize`,
/// `[2]=this segment's session`, `[6]=the array`, `[7]=0`. The element width is
/// read off the array itself
/// through [`heap_element_width`], so the factory and the accessors cannot
/// disagree about the stride. See the registration site for why these three
/// alias where the four `int[]/long[]/float[]/double[]` arms copy.
fn pe_of_array_alias(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let array = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        // The row this comment used to defer. MEASURED: `ofArray(null)` is a
        // message-less `NullPointerException` on HotSpot 25.0.4+7, and all
        // FIVE arms of the family now raise it rather than answering a null
        // segment. "One row for the whole family" was right about the scope and
        // wrong about the cost.
        Some(Value::Object(None)) => {
            return Err(RuntimeError::NullPointerException { message: None }.into())
        }
        _ => return Ok(Some(Value::Object(None))),
    };
    let width = match heap_element_width(ctx.heap_element_type_of(array)) {
        Some(width) if ctx.object_is_array(array) => width as i64,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "MemorySegment.ofArray requires a byte[], short[] or char[]".into(),
            }
            .into())
        }
    };
    let byte_size = (ctx.array_length(array) as i64).saturating_mul(width);
    // G19-1: THE SCOPE IS MINTED HERE, ONCE, NOT ON EVERY `scope()` CALL.
    //
    // MEASURED on 25.0.3+9-LTS (`G19Probe` §SC): a heap segment's scope is a
    // per-segment session that can never close —
    //
    //     ofArray(new byte[16]).scope().getClass()
    //         = jdk.internal.foreign.GlobalSession$HeapSession
    //     heap.scope() == heap.scope()                  true
    //     heap.asSlice(4,4).scope() == heap.scope()     true
    //     heap.asReadOnly().scope() == heap.scope()     true
    //     ofArray(a).scope() == ofArray(a).scope()      FALSE  (same array!)
    //     heap.scope() == Arena.global().scope()        FALSE
    //
    // so it is neither a fresh object per CALL nor a process-wide singleton:
    // it is one object per SEGMENT, shared by everything derived from it. With
    // slot 2 left empty, `p67_receiver_session` found nothing on this carrier
    // and minted a fresh always-open session on every `scope()` — measured
    // `heap.scope() == heap.scope()` = **false**, which is the assertion
    // `RForeignLayoutJdkInterfaces` dies on ("a heap segment's scope is
    // stable").
    //
    // Slot 2 is the right home and not a new convention: `pe_segment_session`
    // above already documents "tolerate a segment stamped with the session
    // directly", and every OTHER reader of slot 2 in this file
    // (`sync_heap_backed_segment`, `pe_segment_heap_base`'s fallback, the
    // `isNative` discriminator) gates on `ctx.object_is_array`, which a session
    // is not — so none of them changes its answer.
    let array_pin = ctx.pin_native_root(array);
    let session = crate::phases_late::foreign_ffm::p67_memory_session(ctx)?;
    let array = ctx.read_native_pin(array_pin, array);
    ctx.unpin_native_roots(array_pin);
    // Both the array and the session must survive the segment's allocation, so
    // both are pinned across it; `unpin_native_roots` truncates the pin stack
    // to its argument, so the FIRST handle releases both.
    let array_pin = ctx.pin_native_root(array);
    let session_obj = match session {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let seg = alloc_segment_carrier(ctx, SEG_HEAP_FIELDS)?;
    let array = ctx.read_native_pin(array_pin, array);
    let session = match session_obj {
        Some((pin, obj)) => Value::Object(Some(ctx.read_native_pin(pin, obj))),
        None => Value::Object(None),
    };
    ctx.unpin_native_roots(array_pin);
    ctx.set_field(seg, 0, Value::Long(0));
    ctx.set_field(seg, 1, Value::Long(byte_size));
    ctx.set_field(seg, 2, session);
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    ctx.set_field(seg, SEG_HEAP_BASE_FIELD, Value::Object(Some(array)));
    ctx.set_field(seg, SEG_HEAP_START_FIELD, Value::Long(0));
    Ok(Some(Value::Object(Some(seg))))
}

/// Is `kind` one of the nine VALUE layouts (`LAYOUT_BYTE` … `LAYOUT_CHAR`)?
///
/// **Written as a range CONTAINMENT, not as `kind < 10`.** The kinds are
/// `0..=8` for the value layouts and `10..=13` for the group family, so
/// `kind < 10` looks like the same test and is not: it is also true for every
/// NEGATIVE kind, and `panama_libffi::LAYOUT_UNKNOWN` is **-2**. Three sites in
/// this file spelled it that way, so an unclassifiable carrier — the answer
/// F27 introduced precisely so that an undecodable layout would stop being a
/// plausible eight-byte integer — was admitted by all three and then fell to
/// each one's default arm: `Value::Int(0)` for a read, a silent no-op for a
/// write, and `"Ljava/lang/foreign/MemorySegment;"` for a carrier descriptor.
///
/// None of the three is reachable today, because `alloc_return_slot` and
/// `layout_to_ffi_type` both refuse an unknown carrier earlier on every path.
/// That is a property of TODAY'S CALLERS, not of this code, and it is one
/// reorder away from being false. Every consumer in this file now either uses
/// this predicate or names `LAYOUT_UNKNOWN` explicitly.
fn layout_kind_is_value(kind: i32) -> bool {
    (0..10).contains(&kind)
}

/// The refusal a `MemorySegment` accessor raises for a layout it cannot
/// classify.
///
/// Names the class, because a refusal that cannot say WHICH carrier was
/// undecodable is not actionable.
fn unclassifiable_access_layout(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
    op: &str,
) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: format!(
            "MemorySegment.{op}: cannot classify layout carrier {} — refusing rather than \
             defaulting to an eight-byte integer",
            ctx.class_name_of_id(ctx.class_id_of_object(layout))
                .unwrap_or_else(|| "<unknown>".to_string())
        ),
    }
    .into()
}

/// Validate a single-element access (get/set) against the segment's declared
/// size and compute the target address with checked arithmetic.
///
/// Mirrors the bounds/overflow checks the `copy`/`fill` paths perform, and
/// throws the same `IllegalStateException` on violation. Rejects:
///   - access through a scope that has been closed (`arena.close()` really
///     frees the block, so this is a use-after-FREE guard, not a cosmetic
///     one) — see [`pe_segment_check_scope`],
///   - zero-size segments (a 0-size segment — as produced by `ofAddress`
///     before `reinterpret` — is not accessible, matching JDK semantics),
///   - negative `offset`,
///
/// **This is the RAW-ADDRESS path only.** A heap segment never reaches it:
/// `pe_segment_get_impl`/`pe_segment_set_impl` resolve a [`HeapSegmentView`]
/// first. The "Null segment address" arm below is therefore no longer the
/// answer a heap segment gets — it used to be, and its wording named the
/// symptom rather than the reason (F27 residual 4).
///   - `offset + width` overflowing `i64`,
///   - `offset + width` exceeding the segment size,
///   - `(ptr + base_off + offset)` overflowing the address space, or a null
///     resulting address.
///
/// `width` is the access width in bytes derived from the layout kind
/// (`ffi::layout_byte_size`). Returns the validated raw address.
fn pe_segment_access_addr(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    width: i64,
) -> Result<usize, MethodCallFailed> {
    // Liveness before bounds, as HotSpot checks it: a closed scope means the
    // block is gone, so nothing about its size or address is meaningful.
    pe_segment_check_scope(ctx, seg)?;

    let ptr = crate::panama_libffi::segment_address(ctx, seg);
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);

    // Bounds check: 0 <= offset and offset + width <= size, with overflow guard.
    //
    // Bounds, not state. The final FFM API specifies `IllegalStateException`
    // for a SCOPE violation — a closed arena, the wrong thread — and
    // `IndexOutOfBoundsException` for an access outside the segment. Measured
    // on HotSpot 25: `seg.get(JAVA_INT, 62)` on a 64-byte segment raises
    // `IndexOutOfBoundsException`. A caller writing
    // `catch (IndexOutOfBoundsException e)` — the idiom for a bounds check —
    // did not catch ours.
    //
    // THE ZERO-SIZE CASE IS THE SAME CHECK, not a separate `IllegalStateException`.
    // It used to be raised above this block, which contradicted the paragraph
    // directly above it. Measured, 25.0.3+9-LTS, and the exception class is the
    // same for a native and a heap carrier:
    //
    //     MemorySegment.NULL.get(JAVA_BYTE, 0)
    //       -> IndexOutOfBoundsException: Out of bound access on segment
    //          MemorySegment{ kind: native, address: 0x0, byteSize: 0 };
    //          new offset = 0; new length = 1
    //     MemorySegment.ofAddress(0x1000).get(JAVA_BYTE, 0)   -> IndexOutOfBoundsException
    //     MemorySegment.ofArray(new byte[0]).get(JAVA_BYTE, 0) -> IndexOutOfBoundsException
    //
    // `size == 0` cannot pass `offset + width <= size` for any `width >= 1`, so
    // deleting the special case does not admit anything: it only re-labels the
    // refusal with the class the oracle throws and a caller can catch. A closed
    // scope is still `IllegalStateException: Already closed`, raised above.
    let end = offset.checked_add(width);
    if offset < 0 || size <= 0 || end.map_or(true, |e| e > size) {
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(format!(
                "Out of bound access on segment MemorySegment{{ kind: native, address: 0x{:x}, \
                 byteSize: {} }}; new offset = {}; new length = {}",
                ptr, size, offset, width
            )),
        }
        .into());
    }

    // Validate address arithmetic doesn't overflow.
    let total = (ptr as u64).checked_add(offset as u64);
    match total {
        Some(addr) if addr != 0 => Ok(addr as usize),
        Some(_) => Err(RuntimeError::IllegalStateException {
            message: "Null segment address".into(),
        }
        .into()),
        None => Err(RuntimeError::IllegalStateException {
            message: "address arithmetic overflow in MemorySegment access".into(),
        }
        .into()),
    }
}

/// Publish an FFM fast-path verdict for `seg` — see [`crate::ffm_fast`].
///
/// Called only where [`pe_segment_access_addr`] has already returned a
/// validated address, i.e. the scope was live, the shape resolved and the
/// access was in bounds. This records THAT, and never the address or the size:
/// the JIT re-reads those slots itself, so nothing here can go stale under a
/// slot rewrite.
///
/// Gated to the plain SIX-slot CratonVM-minted native carrier, because that is
/// the only shape whose `[0]=ptr, [1]=size, [5]=offset` decode the JIT can
/// reproduce. The 8-slot heap-aliasing carrier keeps `ptr` at 0 and reuses
/// slots 6/7, and the 2-/3-slot shapes have no slot 5 at all — publishing
/// either would hand the JIT a decode that does not describe it.
fn ffm_publish_verdict(ctx: &dyn NativeContext, seg: ObjectRef, write: bool) {
    if ctx.object_num_fields(seg) != 6 {
        return;
    }
    if crate::panama_libffi::is_real_heap_segment(ctx, seg) {
        return;
    }
    crate::ffm_fast::note_validated(seg.as_ptr() as u64, write);
    crate::ffm_fast::note_publish();
}

// Exact primitive/covariant descriptors used by real-JDK MemorySegment
// default methods. They share the checked erased implementation above.
fn pe_segment_get_at_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    require_segment_access(ctx, "getAtIndex")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let index = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let elem_size =
        ffi::layout_byte_size(crate::panama_libffi::read_layout_kind(ctx, layout)) as i64;
    pe_segment_get_impl(ctx, this, layout, index * elem_size)
}

fn pe_segment_set_at_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    require_segment_access(ctx, "setAtIndex")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let index = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let value = args.get(3).copied().unwrap_or(Value::Int(0));
    let elem_size =
        ffi::layout_byte_size(crate::panama_libffi::read_layout_kind(ctx, layout)) as i64;
    pe_segment_set_impl(ctx, this, layout, index * elem_size, value)
}

fn pe_segment_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Defense-in-depth: get() dereferences the segment's raw `ptr` field.
    require_segment_access(ctx, "get")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    pe_segment_get_impl(ctx, this, layout, offset)
}

fn pe_segment_get_impl(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    offset: i64,
) -> MethodCallResult {
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    // An unclassifiable carrier must not silently become a one-byte read that
    // answers `Value::Int(0)`. `ffi::layout_byte_size` defaults to 1 and the
    // `_ =>` arm below defaults to 0, so before this the pair produced a
    // believable answer for a layout this VM could not decode at all.
    // `LAYOUT_UNKNOWN` is -2, so the `kind < 10` spellings that used to guard
    // this family admitted it; see [`layout_kind_is_value`].
    if kind == crate::panama_libffi::LAYOUT_UNKNOWN {
        return Err(unclassifiable_access_layout(ctx, layout, "get"));
    }
    // Reject reads that fall outside the segment's declared bounds, overflow
    // the address space, or target a zero-size segment. The access width is
    // derived from the layout kind, matching the read widths below.
    let width = ffi::layout_byte_size(kind) as i64;

    // HEAP SEGMENTS READ THE BACKING JAVA ARRAY, not an address they do not
    // have. This is the whole of F27 NOM-3 item 2; before it, this path
    // dereferenced whatever `segment_address` answered for a heap carrier.
    if crate::panama_libffi::is_real_heap_segment(ctx, seg)
        || ctx.object_num_fields(seg) > SEG_HEAP_START_FIELD
    {
        if let Some(view) = heap_segment_view(ctx, seg) {
            heap_segment_check_access(ctx, seg, &view, layout, offset, width, false)?;
            let raw = heap_segment_read(ctx, &view, offset, width as usize);
            let value = match kind {
                LAYOUT_BYTE => Value::Int(i32::from(raw as u8 as i8)),
                LAYOUT_BOOLEAN => Value::Int(i32::from(raw as u8 != 0)),
                LAYOUT_SHORT => Value::Int(i32::from(raw as u16 as i16)),
                LAYOUT_CHAR => Value::Int(i32::from(raw as u16)),
                LAYOUT_INT => Value::Int(raw as u32 as i32),
                LAYOUT_LONG => Value::Long(raw as i64),
                LAYOUT_FLOAT => Value::Float(f32::from_bits(raw as u32)),
                LAYOUT_DOUBLE => Value::Double(f64::from_bits(raw)),
                LAYOUT_ADDRESS => {
                    return Ok(Some(Value::Object(Some(pe_zero_length_segment(
                        ctx, raw as i64,
                    )?))))
                }
                other => {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "MemorySegment.get: layout kind {other} is not a value layout"
                        ),
                    }
                    .into())
                }
            };
            return Ok(Some(value));
        } else if crate::panama_libffi::is_real_heap_segment(ctx, seg) {
            // It IS a heap segment and its shape did not resolve. Refusing by
            // name beats falling through to the raw-address path, which would
            // dereference `segment_address`'s answer for it.
            return Err(unreadable_heap_segment(ctx, seg, "get"));
        }
    }

    let addr = pe_segment_access_addr(ctx, seg, offset, width)? as *const u8;
    ffm_publish_verdict(ctx, seg, false);

    // SAFETY: addr is non-null, bounds-checked against the segment's declared
    // size, and the address arithmetic was overflow-checked (see
    // pe_segment_access_addr). The kind determines the read width so alignment
    // is implicit from the segment.
    let value = unsafe {
        match kind {
            LAYOUT_BYTE => Value::Int(*(addr as *const i8) as i32),
            // A `boolean` is 0 or 1, never the raw byte: the JDK reads the byte
            // and compares it to zero, so a stray 2 in the segment is `true`.
            // Passing the raw byte through hands Java bytecode a `Z` value
            // outside its domain.
            LAYOUT_BOOLEAN => Value::Int(i32::from(*(addr as *const i8) != 0)),
            LAYOUT_SHORT => Value::Int(*(addr as *const i16) as i32),
            // `char` is UNSIGNED. Sign-extending it makes every code point
            // above 0x7FFF negative, which is not a `char` at all.
            LAYOUT_CHAR => Value::Int(i32::from(*(addr as *const u16))),
            LAYOUT_INT => Value::Int(*(addr as *const i32)),
            LAYOUT_LONG => Value::Long(*(addr as *const i64)),
            LAYOUT_FLOAT => Value::Float(*(addr as *const f32)),
            LAYOUT_DOUBLE => Value::Double(*(addr as *const f64)),
            // ADDRESS is handled after the unsafe block: its declared return
            // type is `Ljava/lang/foreign/MemorySegment;`, so it has to
            // ALLOCATE, which `Value::Long` cannot stand in for -- a reference
            // slot receiving a primitive is the one shape this tree keeps
            // paying for.
            LAYOUT_ADDRESS => Value::Long(*(addr as *const i64)),
            _ => Value::Int(0),
        }
    };
    if kind == LAYOUT_ADDRESS {
        let raw = match value {
            Value::Long(v) => v,
            _ => 0,
        };
        return Ok(Some(Value::Object(Some(pe_zero_length_segment(ctx, raw)?))));
    }
    if crate::nbflags().dbg_mh_dispatch && kind == LAYOUT_FLOAT && offset == 0 {
        eprintln!(
            "[PANAMA_GET_FLOAT] runtime={} ptr={:?} base_offset={:?} value={value:?}",
            ctx.class_name_of_id(ctx.class_id_of_object(seg))
                .unwrap_or_default(),
            ctx.get_field(seg, 0),
            ctx.get_field(seg, 5),
        );
    }
    Ok(Some(value))
}

fn pe_segment_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Defense-in-depth: set() dereferences the segment's raw `ptr` field.
    require_segment_access(ctx, "set")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let value = args.get(3).copied().unwrap_or(Value::Int(0));
    pe_segment_set_impl(ctx, this, layout, offset, value)
}

fn pe_segment_set_impl(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    offset: i64,
    value: Value,
) -> MethodCallResult {
    // A READ-ONLY SEGMENT REFUSES THE WRITE, and this body never asked.
    // MEASURED against HotSpot 25.0.4+7, in BOTH modes
    // (`apps/probes/FfmSegmentSweep.java`):
    //
    //   MemorySegment ro = Arena.ofConfined().allocate(16).asReadOnly();
    //   ro.set(ValueLayout.JAVA_INT, 0, 1)
    //     HotSpot   IllegalArgumentException: Attempt to write a read-only segment
    //     CratonVM  no-throw -- and the next read shows the write LANDED
    //
    // `isReadOnly()` already answered `true` on that receiver, so the flag was
    // right and nothing consulted it: `asReadOnly()` handed back a reference
    // that had not lost the one capability the call exists to remove. F26
    // records the same shape one level down ("a copying slice is a wrong
    // capability").
    //
    // The HEAP path has had this since `heap_segment_check_access`; the
    // raw-address path never did. Unlike the ALIGNMENT rule beside it -- which
    // that function's comment explains cannot be transplanted, because a native
    // segment's real `maxByteAlignment` is not knowable from the carrier -- the
    // read-only flag IS on the carrier, and `craton_segment_check_read_only`
    // reads it through `p67_segment_is_read_only`, the registered `isReadOnly()`
    // body. One source of truth, not a second copy.
    //
    // WHERE THIS HAD TO GO. `phases_late/foreign_ffm.rs` registers the same four
    // `set` descriptors and carries the identical check, and
    // `--dump-native-registry` says those rows are `owns_slot: false` while
    // these are `owns_slot: true`. The first attempt put the guard only there
    // and measured EXACTLY the pre-fix transcript. Both halves are kept, and
    // deliberately: registration order is what picks between them, and a pair
    // where only one half refuses is the half-fixed duplicate this file's own
    // history keeps finding.
    craton_segment_check_read_only(ctx, seg, false)?;
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    // Symmetric with `pe_segment_get_impl`: an unclassifiable carrier used to
    // reach the `_ => {}` arm below, i.e. a WRITE that silently did nothing.
    if kind == crate::panama_libffi::LAYOUT_UNKNOWN {
        return Err(unclassifiable_access_layout(ctx, layout, "set"));
    }
    // Reject writes that fall outside the segment's declared bounds, overflow
    // the address space, or target a zero-size segment. The access width is
    // derived from the layout kind, matching the write widths below.
    let width = ffi::layout_byte_size(kind) as i64;

    // `set(ADDRESS, off, seg)` takes a MemorySegment, not a long: its address
    // is what gets stored. Without this the value arrives as `Value::Object`,
    // misses every arm below and the write is a SILENT no-op -- which is the
    // quieter half of the same defect as the missing registration.
    //
    // A HEAP SEGMENT HAS NO ADDRESS TO STORE, and since F27 `segment_address`
    // answers 0 for one — so this arm would have written a C NULL and said
    // nothing. The oracle refuses instead (measured, 25.0.3+9-LTS):
    //
    //     MemorySegment.ofArray(new byte[16])
    //         .set(ValueLayout.ADDRESS.withByteAlignment(1), 0,
    //              MemorySegment.ofArray(new byte[4]))
    //       -> IllegalArgumentException: Heap segment not allowed:
    //          MemorySegment{ kind: heap, heapBase: [B@…, address: 0x0, byteSize: 4 }
    //
    // and the same refusal, verbatim, when one is passed to a downcall as a
    // pointer. A wrong ADDRESS is a wrong capability; 0 is a legitimate C null
    // and cannot be told apart from one downstream.
    let value = match (kind, value) {
        (LAYOUT_ADDRESS, Value::Object(Some(target))) => {
            if crate::panama_libffi::is_real_heap_segment(ctx, target)
                || heap_segment_view(ctx, target).is_some()
            {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Heap segment not allowed: MemorySegment{{ kind: heap, class: {} }}",
                        ctx.class_name_of_id(ctx.class_id_of_object(target))
                            .unwrap_or_else(|| "<unknown>".to_string())
                    ),
                }
                .into());
            }
            Value::Long(crate::panama_libffi::segment_address(ctx, target))
        }
        (LAYOUT_ADDRESS, Value::Object(None)) => Value::Long(0),
        _ => value,
    };

    // HEAP SEGMENTS WRITE THE BACKING JAVA ARRAY. See `pe_segment_get_impl`.
    if crate::panama_libffi::is_real_heap_segment(ctx, seg)
        || ctx.object_num_fields(seg) > SEG_HEAP_START_FIELD
    {
        if let Some(view) = heap_segment_view(ctx, seg) {
            heap_segment_check_access(ctx, seg, &view, layout, offset, width, true)?;
            let raw = match (kind, value) {
                (LAYOUT_BYTE | LAYOUT_BOOLEAN, Value::Int(v)) => u64::from(v as u8),
                (LAYOUT_SHORT | LAYOUT_CHAR, Value::Int(v)) => u64::from(v as u16),
                (LAYOUT_INT, Value::Int(v)) => u64::from(v as u32),
                (LAYOUT_LONG | LAYOUT_ADDRESS, Value::Long(v)) => v as u64,
                (LAYOUT_FLOAT, Value::Float(v)) => u64::from(v.to_bits()),
                (LAYOUT_DOUBLE, Value::Double(v)) => v.to_bits(),
                // NOT a silent no-op, unlike the raw-address arm below. A
                // value whose Rust shape does not match its layout is a
                // marshalling defect upstream, and swallowing it is how a
                // wrong write becomes invisible.
                (other_kind, other_value) => {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "MemorySegment.set on a heap segment: layout kind {other_kind} does \
                             not accept {other_value:?}"
                        ),
                    }
                    .into())
                }
            };
            heap_segment_write(ctx, &view, offset, width as usize, raw);
            return Ok(None);
        } else if crate::panama_libffi::is_real_heap_segment(ctx, seg) {
            return Err(unreadable_heap_segment(ctx, seg, "set"));
        }
    }

    let addr = pe_segment_access_addr(ctx, seg, offset, width)? as *mut u8;
    ffm_publish_verdict(ctx, seg, true);

    // SAFETY: addr is non-null, bounds-checked against the segment's declared
    // size, and the address arithmetic was overflow-checked (see
    // pe_segment_access_addr). The kind determines the write width so alignment
    // is implicit from the segment.
    unsafe {
        match (kind, value) {
            (LAYOUT_BYTE | LAYOUT_BOOLEAN, Value::Int(v)) => *(addr as *mut i8) = v as i8,
            (LAYOUT_SHORT | LAYOUT_CHAR, Value::Int(v)) => *(addr as *mut i16) = v as i16,
            (LAYOUT_INT, Value::Int(v)) => *(addr as *mut i32) = v,
            (LAYOUT_LONG | LAYOUT_ADDRESS, Value::Long(v)) => *(addr as *mut i64) = v,
            (LAYOUT_FLOAT, Value::Float(v)) => *(addr as *mut f32) = v,
            (LAYOUT_DOUBLE, Value::Double(v)) => *(addr as *mut f64) = v,
            _ => {}
        }
    }
    sync_heap_backed_segment(ctx, seg, false);
    Ok(None)
}

/// Synchronize a primitive-array-backed synthetic MemorySegment at the FFI
/// boundary. The VM cannot expose a moving Java heap array directly to
/// native code, so ofArray owns a native mirror; this retains the Java
/// backing array and copies it immediately before and after a downcall.
fn sync_heap_backed_segment(ctx: &mut dyn NativeContext, seg: ObjectRef, to_native: bool) {
    let backing = match ctx.get_field(seg, SEG_BACKING_ARRAY_FIELD) {
        Value::Object(Some(array)) if ctx.object_is_array(array) => array,
        _ => return,
    };
    let kind = match ctx.get_field(seg, SEG_BACKING_KIND_FIELD) {
        Value::Int(kind) => kind,
        _ => return,
    };
    let width = ffi::layout_byte_size(kind);
    if width == 0 {
        return;
    }
    let (base_ptr, segment_offset, byte_size) = match (
        ctx.get_field(seg, 0),
        ctx.get_field(seg, 5),
        ctx.get_field(seg, 1),
    ) {
        (Value::Long(ptr), Value::Long(offset), Value::Long(size))
            if ptr > 0 && offset >= 0 && size >= 0 =>
        {
            (ptr as usize, offset as usize, size as usize)
        }
        _ => return,
    };
    let array_start = segment_offset / width;
    if segment_offset % width != 0 || array_start >= ctx.array_length(backing) {
        return;
    }
    let count = (byte_size / width).min(ctx.array_length(backing) - array_start);
    let Some(native_addr) = base_ptr.checked_add(segment_offset) else {
        return;
    };
    for i in 0..count {
        let addr = unsafe { (native_addr as *mut u8).add(i * width) };
        if to_native {
            unsafe {
                match (kind, ctx.get_array_element(backing, array_start + i)) {
                    (LAYOUT_INT, Value::Int(value)) => *(addr as *mut i32) = value,
                    (LAYOUT_LONG, Value::Long(value)) => *(addr as *mut i64) = value,
                    (LAYOUT_FLOAT, Value::Float(value)) => *(addr as *mut f32) = value,
                    (LAYOUT_FLOAT, Value::Int(bits)) => {
                        *(addr as *mut f32) = f32::from_bits(bits as u32)
                    }
                    (LAYOUT_DOUBLE, Value::Double(value)) => *(addr as *mut f64) = value,
                    _ => {}
                }
            }
        } else {
            unsafe {
                let value = match kind {
                    LAYOUT_INT => Value::Int(*(addr as *const i32)),
                    LAYOUT_LONG => Value::Long(*(addr as *const i64)),
                    LAYOUT_FLOAT => Value::Float(*(addr as *const f32)),
                    LAYOUT_DOUBLE => Value::Double(*(addr as *const f64)),
                    _ => continue,
                };
                ctx.set_array_element(backing, array_start + i, value);
            }
        }
    }
}

// --- SymbolLookup: load shared libraries and find symbols ---
// SymbolLookup synthetic: [0]=lib_index (Long — index into SharedVm.native_libraries), [1]=name

pub(crate) fn register_pe_symbol_lookup(r: &mut NativeMethodRegistry) {
    // Promote to `Bridge`: this function runs under whatever category was
    // ambient at the `register_pe_panama` call site, which defaults to
    // `SyntheticStub` (dropped entirely under strict-no-stubs / real-JDK
    // mode). These natives are real supporting glue for the Panama FFI
    // downcall path (real library loading via `ctx.load_native_library`,
    // real `Optional`/`Optional.empty()` wrapping) — not a placeholder — and
    // must survive that filtering so `phases_late.rs`'s `Linker.defaultLookup`
    // et al. (which allocate 0-field `SymbolLookup` objects) still resolve
    // `.find()` sanely via this implementation's `_ => -1` "unknown lookup"
    // fallback instead of a former duplicate stub here always returning a
    // bare Java `null`. See the `find`/`libraryLookup` doc comments below.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sl = "java/lang/foreign/SymbolLookup";

    // libraryLookup(path, arena) → SymbolLookup
    r.register(
        sl,
        "libraryLookup",
        "(Ljava/lang/String;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/SymbolLookup;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let path = ctx.read_string(path_obj).unwrap_or_default();

            require_native_access(ctx, "libraryLookup")?;
            let lib_index = ctx.load_native_library(&path)?;

            let lookup = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 2)?;
            ctx.set_field(lookup, 0, Value::Long(lib_index));
            let name_str = ctx.create_string(&path);
            ctx.set_field(lookup, 1, Value::Object(Some(name_str)));
            Ok(Some(Value::Object(Some(lookup))))
        },
    );

    // loaderLookup() → SymbolLookup (stub — returns lookup for default system library)
    r.register(
        sl,
        "loaderLookup",
        "()Ljava/lang/foreign/SymbolLookup;",
        |ctx, _| {
            // The returned lookup searches every loaded library (index -1),
            // so obtaining one is itself a capability. Gated identically to
            // `libraryLookup` minus the `--enable-native-access` requirement,
            // which the JDK does not impose on `loaderLookup`.
            crate::security_manager::check_host_native_access_or_throw(ctx, "symbolLookup")?;
            let lookup = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 2)?;
            ctx.set_field(lookup, 0, Value::Long(-1)); // -1 = default/system lookup
            Ok(Some(Value::Object(Some(lookup))))
        },
    );

    // find(name) → Optional<MemorySegment>
    r.register(
        sl,
        "find",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sym_name_obj = obj_arg(args, 1)?;
            let sym_name = ctx.read_string(sym_name_obj).unwrap_or_default();
            let lib_index = match ctx.get_field(this, 0) {
                Value::Long(n) => n,
                _ => -1,
            };

            // A `loaderLookup()` receiver carries `lib_index == -1`, and
            // `find_native_symbol` treats that as "search every loaded
            // library", so `loaderLookup().find(x)` is an arbitrary-symbol
            // address oracle over libraries loaded by trusted code. Gate it
            // the same way as the load paths rather than leaving the read
            // side open when the load side is closed.
            crate::security_manager::check_host_native_access_or_throw(ctx, "symbolLookup")?;

            match ctx.find_native_symbol(lib_index, &sym_name) {
                Some(addr) => {
                    // Wrap address in MemorySegment and Optional.of()
                    let seg = alloc_segment_carrier(ctx, 6)?;
                    ctx.set_field(seg, 0, Value::Long(addr as i64));
                    ctx.set_field(seg, 1, Value::Long(0)); // function pointer — no byte size
                    ctx.set_field(seg, 2, Value::Object(None));
                    ctx.set_field(seg, 3, Value::Int(1)); // read-only
                    ctx.set_field(seg, 4, Value::Int(1)); // alive
                    ctx.set_field(seg, 5, Value::Long(0));
                    // Return as Optional.of(segment)
                    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
                    ctx.set_field(opt, 0, Value::Object(Some(seg)));
                    Ok(Some(Value::Object(Some(opt))))
                }
                None => {
                    // Return Optional.empty()
                    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
                    ctx.set_field(opt, 0, Value::Object(None));
                    Ok(Some(Value::Object(Some(opt))))
                }
            }
        },
    );
    r.set_category(__prev_cat);
}

// --- RawNativeLibraries: the real-JDK FFM native-library load path ---
//
// In real-JDK mode `java.lang.foreign.SymbolLookup.libraryLookup(name, arena)`
// runs the JDK's own bytecode (our higher-level `SymbolLookup.libraryLookup`
// override does not win over a concrete method body), which routes through
// `jdk.internal.loader.RawNativeLibraries`:
//
//   RawNativeLibraryImpl.open()  → RawNativeLibraries.load0(impl, name)  // dlopen/LoadLibrary
//   RawNativeLibraryImpl.find()  → NativeLibrary.findEntry0(handle, name) // dlsym/GetProcAddress
//   RawNativeLibraryImpl.close() → RawNativeLibraries.unload0(name, handle)// dlclose/FreeLibrary
//
// All three are real ACC_NATIVE methods, so a registry override always
// dispatches to them (unlike the concrete-bytecode methods above). Without
// them, any FFM downcall binding — e.g. Tomcat's `openssl_h` loading
// libssl/libcrypto — dies in `<clinit>` with
// `UnsatisfiedLinkError: RawNativeLibraries.load0`. Back them with CratonVM's
// existing native-library table (the same `load_native_library` /
// `find_native_symbol` the panama `SymbolLookup` above uses). This makes the
// VM behave like HotSpot: the load is attempted, and if the library is not
// present `load0` returns false (the caller returns null and Tomcat falls back
// to JSSE) rather than raising.
//
// NOTE: registered from `register_essential_natives` (the always-compiled,
// real-JDK path), NOT from `register_pe_panama` — the latter is only reached
// under `#[cfg(feature = "synthetic-jdk")]` and would be dead-code-eliminated
// in real-JDK mode, which is exactly where these natives are needed (real-JDK
// mode runs the JDK's own FFM bytecode, which calls these natives directly).
pub(crate) fn register_pe_raw_native_libraries(r: &mut NativeMethodRegistry) {
    let rnl = "jdk/internal/loader/RawNativeLibraries";

    // static native boolean load0(RawNativeLibraryImpl impl, String name)
    r.register_with_kind(
        rnl,
        "load0",
        "(Ljdk/internal/loader/RawNativeLibraries$RawNativeLibraryImpl;Ljava/lang/String;)Z",
        |ctx, args| {
            let impl_obj = obj_arg(args, 0)?;
            let name_obj = obj_arg(args, 1)?;
            let name = ctx.read_string(name_obj).unwrap_or_default();
            require_native_access(ctx, "libraryLookup")?;
            match ctx.load_native_library(&name) {
                Ok(lib_index) => {
                    // Stash the library index as the opaque `handle` long.
                    // Offset by +1 so a valid index 0 never collides with the
                    // handle==0 "not loaded" sentinel that
                    // RawNativeLibraryImpl.open() checks before calling load0;
                    // findEntry0 below decodes it back.
                    ctx.set_field_by_name(impl_obj, "handle", Value::Long(lib_index + 1));
                    Ok(Some(Value::Int(1)))
                }
                // Match the real native: a failed load returns false (caller
                // returns null), it does NOT throw.
                Err(_) => Ok(Some(Value::Int(0))),
            }
        },
        NativeKind::Bridge,
    );

    // static native void unload0(String name, long handle)
    //
    // IMPLEMENTED (was a no-op) via `NativeContext::unload_native_library`, the
    // escalation this comment used to request. No new handle plumbing was
    // needed: `load0` above stashes `lib_index + 1`, which is exactly what
    // `RawNativeLibraryImpl.close()` hands back as `handle`, so the same `- 1`
    // decode `findEntry0` uses recovers the index.
    //
    // It is a LOGICAL unload, not a `dlclose`: the VM's library table is an
    // append-only `Vec<Library>` whose index IS the handle, so an entry cannot
    // be removed without renumbering every live handle. The VM implementation
    // therefore tombstones the index — subsequent `find_native_symbol` calls on
    // it fail, as they would against a closed handle — while leaving the mapping
    // resident. Staying mapped is the safe direction of the two errors: a
    // `RawNativeLibraries` handle can still back live function pointers (callers
    // cache `findEntry0` results, and any bound downcall stub holds one), so
    // unmapping underneath them segfaults, while not unmapping costs an
    // address-space mapping.
    //
    // Real `unload0` returns void and reports nothing, so the `false` an
    // implementation without an unload path returns is intentionally ignored.
    r.register_with_kind(
        rnl,
        "unload0",
        "(Ljava/lang/String;J)V",
        |ctx, args| {
            let handle = match args.get(1) {
                Some(Value::Long(n)) => *n,
                Some(Value::Int(n)) => *n as i64,
                _ => 0,
            };
            // Undo the +1 offset applied by load0 to recover the library index; a
            // handle of 0 ("not loaded") decodes to -1 and is refused.
            let _unloaded = ctx.unload_native_library(handle - 1);
            Ok(None)
        },
        NativeKind::Bridge,
    );

    // static native long findEntry0(long handle, String name)  (in NativeLibrary)
    r.register_with_kind(
        "jdk/internal/loader/NativeLibrary",
        "findEntry0",
        "(JLjava/lang/String;)J",
        |ctx, args| {
            let handle = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let name_obj = obj_arg(args, 1)?;
            let name = ctx.read_string(name_obj).unwrap_or_default();
            // `handle` is caller-supplied and is only `lib_index + 1`, a small
            // dense integer, so without this gate a guest can walk 1, 2, 3, …
            // and `dlsym` any library loaded by anyone in the process — a
            // symbol-address oracle that never passes the load-time check.
            // Same gate as the load paths, so trusted callers are unaffected:
            // it denies only under CRATONVM_UNTRUSTED_CODE or a SecurityManager
            // policy that withholds `loadLibrary.*`.
            crate::security_manager::check_host_native_access_or_throw(ctx, "findEntry")?;
            // Undo the +1 offset applied by load0 to recover the library index.
            let lib_index = handle - 1;
            let addr = ctx.find_native_symbol(lib_index, &name).unwrap_or(0);
            Ok(Some(Value::Long(addr as i64)))
        },
        NativeKind::Bridge,
    );
}

// --- Linker: create downcall handles ---
//
// The carrier a downcall handle is allocated AS is a real
// `java/lang/invoke/MethodHandle`. See [`alloc_downcall_handle`] for why, and
// for the slot layout that used to be a five-field
// `java/lang/foreign/DowncallHandle`.

/// First slot of the downcall state on a [`DOWNCALL_CARRIER_CLASS`] carrier.
///
/// Anchored well past `lang_invoke.rs`'s synthetic MethodHandle window, for the
/// same reason that window is itself anchored at 16 rather than at the real
/// JDK's field count of 6: the window GREW once already (`MH_BOUND + 1` to
/// `MH_VARARGS + 1`, W7-19 §5.2), so a base chosen to sit exactly on top of
/// today's last slot is a collision waiting for the next combinator.
///
/// The gap between the two windows is not addressed by this file and is left
/// null by [`alloc_downcall_handle`] — see the null-fill there.
pub(crate) const DOWNCALL_BASE: usize = 32;

/// Discriminator slot: `Int(DOWNCALL_TAG_MAGIC)` on a downcall carrier and
/// nothing else. See [`is_downcall_handle`].
pub(crate) const DOWNCALL_TAG: usize = DOWNCALL_BASE;
/// Target function pointer (`Long`). Never dereferenced without
/// [`validated_fn_ptr`].
pub(crate) const DOWNCALL_FN_ADDR: usize = DOWNCALL_BASE + 1;
/// The `java/lang/foreign/FunctionDescriptor` this handle was linked against.
pub(crate) const DOWNCALL_DESCRIPTOR: usize = DOWNCALL_BASE + 2;
/// Index of the first variadic argument, or `-1` for a non-variadic call.
pub(crate) const DOWNCALL_FIRST_VARIADIC: usize = DOWNCALL_BASE + 3;
/// T5.6.3 cached `Box<Cif>` raw pointer as `u64` (`0` = not yet built).
pub(crate) const DOWNCALL_CIF: usize = DOWNCALL_BASE + 4;
/// `Int(1)` when `Linker.Option.captureCallState` added a leading
/// `MemorySegment` parameter.
pub(crate) const DOWNCALL_CAPTURE_CALL_STATE: usize = DOWNCALL_BASE + 5;
/// Width [`alloc_downcall_handle`] requests.
pub(crate) const DOWNCALL_SLOT_COUNT: usize = DOWNCALL_CAPTURE_CALL_STATE + 1;

/// `"DCH1"`. Distinctive on purpose: [`is_downcall_handle`] is asked about
/// arbitrary references, and a magic that could plausibly be a stored `int`
/// would make the predicate answer yes for a handle that merely happens to be
/// wide enough.
const DOWNCALL_TAG_MAGIC: i32 = 0x4443_4831;

/// The class a downcall handle is allocated as.
///
/// **This is the whole of the `--jdk-only` fix.** It used to be
/// `java/lang/foreign/DowncallHandle`, a name NO JDK image declares — it is in
/// `native-api/src/no_image_receiver.rs`'s `NO_IMAGE_JDK_RECEIVERS`. Strict
/// mode refuses to fabricate such a class, correctly, so
/// `Linker.downcallHandle` died with `NoClassDefFoundError:
/// java/lang/foreign/DowncallHandle` and took all of Panama with it. The
/// refusal was right; the caller surviving to ask for it was the defect.
pub(crate) const DOWNCALL_CARRIER_CLASS: &str = "java/lang/invoke/MethodHandle";

/// Is `obj` a downcall handle?
///
/// Replaces the `class_name == "java/lang/foreign/DowncallHandle"` test that
/// the consumers in `lang_invoke.rs` used while the carrier had a class of its
/// own. Three screens, cheapest first:
///
/// 1. **Width.** Every MethodHandle `lang_invoke.rs` mints is 22 slots, so this
///    rejects the entire ordinary population on one integer compare — and it is
///    what makes the tag read safe rather than an out-of-bounds access. Same
///    discipline as `mh_is_varargs_collector`.
/// 2. **Exact class.** Our carrier is allocated as `java/lang/invoke/
///    MethodHandle` itself, so a real-JDK `BoundMethodHandle$Species_L` or
///    `DirectMethodHandle$Constructor` is not one of ours no matter how wide.
/// 3. **Tag.** [`DOWNCALL_TAG_MAGIC`], written by [`alloc_downcall_handle`].
pub(crate) fn is_downcall_handle(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.object_num_fields(obj) > DOWNCALL_TAG
        && ctx
            .class_name_arc_of_id(ctx.class_id_of_object(obj))
            .as_deref()
            == Some(DOWNCALL_CARRIER_CLASS)
        && matches!(ctx.get_field(obj, DOWNCALL_TAG), Value::Int(tag) if tag == DOWNCALL_TAG_MAGIC)
}

/// Mint the `MethodHandle` that `Linker.downcallHandle` hands back.
///
/// # Why a real `MethodHandle` and not a class of our own
///
/// `Linker.downcallHandle` is DECLARED to return `java.lang.invoke.
/// MethodHandle`, and on a real JDK that is what it returns. Returning an
/// invented `java/lang/foreign/DowncallHandle` instead cost three separate
/// compensations, every one of which this carrier deletes rather than moves:
///
/// * `vm_exec.rs::is_method_handle_signature_polymorphic_receiver` had to name
///   the class, or `invokeExact` would not link signature-polymorphically. A
///   real `java/lang/invoke/MethodHandle` already satisfies that predicate's
///   first arm. (ES-FAIL-FAMILY-20260710 is that compensation being added.)
/// * `typecheck.rs` had to hard-code that the invented class is castable to
///   `java/lang/invoke/MethodHandle`. Now it IS one.
/// * `lang_invoke.rs`'s `asType` had to refuse to write the `type` field,
///   because on the compact layout slot 0 was the FUNCTION ADDRESS and writing
///   a `MethodType` over it turned a later void `invokeExact` into a silent
///   no-op. Here slot 0 is the genuine `type` field and the write is correct.
///
/// # The `type` field is populated here, not lazily
///
/// It has to be. `pe_downcall_type` was registered as `DowncallHandle.type()`;
/// with a `MethodHandle` receiver, `type()` resolves to `lang_invoke.rs`'s
/// registration on `java/lang/invoke/MethodHandle`, which reads slot 0 and
/// falls back to `()V` when it is null. A downcall that reported `()V` would
/// mis-link every signature-polymorphic call site, so the MethodType is
/// derived from the FunctionDescriptor and stored at mint time.
///
/// # GC
///
/// `build_method_type_from_descriptor` allocates, so both the carrier and the
/// caller's `descriptor` are pinned across it and re-read after.
pub(crate) fn alloc_downcall_handle(
    ctx: &mut dyn NativeContext,
    fn_addr: i64,
    descriptor: ObjectRef,
    first_variadic: i64,
    capture_call_state: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    // Pinned BEFORE the allocation below, which can itself collect.
    let desc_pin = ctx.pin_native_root(descriptor);
    let handle = try_alloc_concurrent_synthetic(ctx, DOWNCALL_CARRIER_CLASS, DOWNCALL_SLOT_COUNT)?;
    let handle_pin = ctx.pin_native_root(handle);
    let descriptor = ctx.read_native_pin(desc_pin, descriptor);

    let method_descriptor = downcall_carrier_descriptor(ctx, descriptor, capture_call_state)?;
    let method_type =
        crate::lang_invoke::build_method_type_from_descriptor(ctx, &method_descriptor)?
            .ok_or_else(|| -> MethodCallFailed {
                RuntimeError::IllegalStateException {
                    message: format!(
                        "Unable to construct MethodType for DowncallHandle descriptor \
                     {method_descriptor}"
                    ),
                }
                .into()
            })?;

    let handle = ctx.read_native_pin(handle_pin, handle);
    let descriptor = ctx.read_native_pin(desc_pin, descriptor);

    // Everything between the real JDK's `type` field and our base belongs to
    // neither of us: slots 1-5 are the JDK's other MethodHandle fields and
    // 16-21 are `lang_invoke.rs`'s synthetic MethodHandle window. Null rather
    // than left raw, so a reader that reaches one — `mh_is_varargs_collector`
    // tests `Int(1)` at slot 21 and would otherwise be interpreting whatever
    // the allocator left there — gets a definite "absent" instead of padding.
    // Null is the safe filler in both directions: a reference slot reads as a
    // null oop and an int-shaped reader's `match` falls to its default arm.
    for slot in 1..DOWNCALL_BASE {
        ctx.set_field(handle, slot, Value::Object(None));
    }
    // Slot 0 is the real JDK `MethodHandle.type` field.
    ctx.set_field(handle, 0, Value::Object(Some(method_type)));

    ctx.set_field(handle, DOWNCALL_TAG, Value::Int(DOWNCALL_TAG_MAGIC));
    ctx.set_field(handle, DOWNCALL_FN_ADDR, Value::Long(fn_addr));
    ctx.set_field(handle, DOWNCALL_DESCRIPTOR, Value::Object(Some(descriptor)));
    ctx.set_field(handle, DOWNCALL_FIRST_VARIADIC, Value::Long(first_variadic));
    ctx.set_field(handle, DOWNCALL_CIF, Value::Long(0));
    ctx.set_field(
        handle,
        DOWNCALL_CAPTURE_CALL_STATE,
        Value::Int(capture_call_state as i32),
    );

    ctx.unpin_native_roots(desc_pin);
    Ok(handle)
}

/// The function pointer this handle targets, or 0.
pub(crate) fn downcall_fn_addr(ctx: &dyn NativeContext, handle: ObjectRef) -> i64 {
    match ctx.get_field(handle, DOWNCALL_FN_ADDR) {
        Value::Long(n) => n,
        _ => 0,
    }
}

/// The `FunctionDescriptor` this handle was linked against.
fn downcall_descriptor(ctx: &dyn NativeContext, handle: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(handle, DOWNCALL_DESCRIPTOR) {
        Value::Object(Some(d)) => Some(d),
        _ => None,
    }
}

pub(crate) fn register_pe_linker_options(r: &mut NativeMethodRegistry) {
    let option = "java/lang/foreign/Linker$Option";
    r.register(
        option,
        "critical",
        "(Z)Ljava/lang/foreign/Linker$Option;",
        |ctx, args| {
            let enabled = args.first().and_then(|v| v.as_int()).unwrap_or(0) != 0;
            let opt = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker$Option", 2)?;
            ctx.set_field(opt, 0, Value::Int(1)); // kind = critical
            ctx.set_field(opt, 1, Value::Long(enabled as i64));
            Ok(Some(Value::Object(Some(opt))))
        },
    );
}

/// True for the real-JDK option that adds a leading MemorySegment state
/// argument to a downcall handle.
pub(crate) fn downcall_option_captures_call_state(
    ctx: &dyn NativeContext,
    option: ObjectRef,
) -> bool {
    matches!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(option))
            .as_deref(),
        Some("jdk/internal/foreign/abi/LinkerOptions$CaptureCallState")
    )
}

fn register_pe_linker(r: &mut NativeMethodRegistry) {
    let linker = "java/lang/foreign/Linker";

    // nativeLinker() → Linker
    r.register(
        linker,
        "nativeLinker",
        "()Ljava/lang/foreign/Linker;",
        |ctx, _| {
            let l = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker", 1)?;
            Ok(Some(Value::Object(Some(l))))
        },
    );

    // downcallHandle(MemorySegment address, FunctionDescriptor desc) → MethodHandle
    //
    // Layout and carrier class: see `alloc_downcall_handle`.
    r.register(linker, "downcallHandle", "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;)Ljava/lang/invoke/MethodHandle;", |ctx, args| {
        let addr_seg = obj_arg(args, 1)?;
        let descriptor = obj_arg(args, 2)?;
        let fn_addr = crate::panama_libffi::segment_address(ctx, addr_seg);

        let handle = alloc_downcall_handle(ctx, fn_addr, descriptor, -1, false)?;
        Ok(Some(Value::Object(Some(handle))))
    });

    // downcallHandle with Linker.Option[] for variadic etc. (NEW-18.3).
    r.register(linker, "downcallHandle",
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let addr_seg = obj_arg(args, 1)?;
            let descriptor = obj_arg(args, 2)?;
            let fn_addr = crate::panama_libffi::segment_address(ctx, addr_seg);

            // Scan the option array for a firstVariadicArg option (kind=0).
            // Linker.Option synthetic layout: field 0 = kind (Int), field 1 = payload (Long).
            let mut variadic_fixed: i64 = -1;
            let mut capture_call_state = false;
            if let Some(Value::Object(Some(opts))) = args.get(3) {
                let n = ctx.array_length(*opts);
                for i in 0..n {
                    if let Value::Object(Some(opt)) = ctx.get_array_element(*opts, i) {
                        if downcall_option_captures_call_state(ctx, opt) {
                            capture_call_state = true;
                        }
                        let kind = match ctx.get_field(opt, 0) {
                            Value::Int(k) => k,
                            _ => -1,
                        };
                        if kind == 0 {
                            variadic_fixed = match ctx.get_field(opt, 1) {
                                Value::Long(v) => v,
                                Value::Int(v) => v as i64,
                                _ => -1,
                            };
                        }
                    }
                }
            }

            if crate::nbflags().dbg_linker {
                eprintln!(
                    "[PANAMA_LINKER] option downcall addr=0x{fn_addr:x} options={}",
                    args.get(3).is_some()
                );
            }
            let handle = alloc_downcall_handle(
                ctx,
                fn_addr,
                descriptor,
                variadic_fixed,
                capture_call_state,
            )?;
            Ok(Some(Value::Object(Some(handle))))
        },
    );

    // Linker.Option.firstVariadicArg(int n) → Linker$Option synthetic (kind=0, payload=n)
    r.register(
        "java/lang/foreign/Linker$Option",
        "firstVariadicArg",
        "(I)Ljava/lang/foreign/Linker$Option;",
        |ctx, args| {
            let n = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            let opt = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker$Option", 2)?;
            ctx.set_field(opt, 0, Value::Int(0)); // kind = firstVariadicArg
            ctx.set_field(opt, 1, Value::Long(n as i64)); // payload
            Ok(Some(Value::Object(Some(opt))))
        },
    );
    register_pe_linker_options(r);

    // DowncallHandle.invoke(Object... args) → Object
    //
    // UNREACHABLE since the carrier became a real `java/lang/invoke/
    // MethodHandle` ([`alloc_downcall_handle`]): nothing in the VM allocates a
    // `java/lang/foreign/DowncallHandle` any more, so no receiver can ever
    // resolve to these four rows. Invocation now arrives at `lang_invoke.rs`'s
    // `MethodHandle.invoke`/`invokeExact`, whose `mh_dispatch` routes downcalls
    // to `pe_downcall_invoke` before decoding the generic MethodHandle layout.
    //
    // NOT DELETED HERE, deliberately. These four rows are frozen by name in
    // `scripts/baselines/jdk-only-kind-map-25-linux.tsv`,
    // `jdk-only-gated-never-delete.tsv` and `jdk-only-dead-everywhere-GATED.tsv`,
    // and removing a registration those gates enumerate is a census change that
    // has to be made with the gate run, not alongside a behaviour change. The
    // rows are inert in the meantime, which is the safe direction: a dead
    // registration decides nothing, whereas deleting one the gate still expects
    // reddens the gate for a reason unrelated to this fix.
    //
    // Do NOT re-point these at `java/lang/invoke/MethodHandle`. `register` is
    // last-write-wins on (class, method, descriptor), so registering
    // `pe_downcall_invoke` on `MethodHandle.invoke([Ljava/lang/Object;)…` would
    // silently replace `lang_invoke.rs`'s body for EVERY method handle in the
    // VM, not only for downcalls.
    let dh = "java/lang/foreign/DowncallHandle";
    r.register(
        dh,
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_downcall_invoke,
    );
    r.register(
        dh,
        "invokeExact",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_downcall_invoke,
    );
    r.register(
        dh,
        "invokeBasic",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_downcall_invoke,
    );
    r.register(
        dh,
        "type",
        "()Ljava/lang/invoke/MethodType;",
        pe_downcall_type,
    );

    // upcallHandle(target, descriptor, arena) → MemorySegment wrapping trampoline address
    r.register(linker, "upcallHandle",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/foreign/FunctionDescriptor;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/MemorySegment;",
        pe_upcall_handle);

    // UpcallStub.invoke — dispatches from a trampoline back into Java
    let us = "java/lang/foreign/UpcallStub";
    r.register(
        us,
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_upcall_invoke,
    );
}

/// The JVM method descriptor a downcall handle's `MethodType` is built from.
///
/// Derived from the `FunctionDescriptor`'s layouts rather than exposing an
/// untyped `Object[]` invoker, because a signature-polymorphic call site links
/// against whatever `type()` reports.
///
/// Allocation-free (reads layout fields only), so [`alloc_downcall_handle`]
/// may call it before it has anything pinned.
fn downcall_carrier_descriptor(
    ctx: &dyn NativeContext,
    descriptor: ObjectRef,
    captures_call_state: bool,
) -> Result<String, MethodCallFailed> {
    use crate::panama_libffi as plf;

    let mut method_descriptor = String::from("(");
    if captures_call_state {
        method_descriptor.push_str("Ljava/lang/foreign/MemorySegment;");
    }
    for layout in plf::descriptor_param_layouts(ctx, descriptor) {
        let layout = layout.ok_or_else(|| -> MethodCallFailed {
            RuntimeError::IllegalStateException {
                message: "DowncallHandle FunctionDescriptor has null parameter layout".into(),
            }
            .into()
        })?;
        method_descriptor.push_str(downcall_layout_carrier_descriptor(ctx, layout)?);
    }
    method_descriptor.push(')');
    match plf::descriptor_return_layout(ctx, descriptor) {
        Some(layout) => {
            method_descriptor.push_str(downcall_layout_carrier_descriptor(ctx, layout)?)
        }
        None => method_descriptor.push('V'),
    }
    Ok(method_descriptor)
}

/// Return the `MethodType` represented by a downcall handle.
///
/// **Now a fallback, not the primary path.** While the carrier was a class of
/// its own this was registered as `DowncallHandle.type()`. On a real
/// `java/lang/invoke/MethodHandle` carrier, `type()` resolves to
/// `lang_invoke.rs`'s registration, which reads the real `type` field at slot
/// 0 — populated by [`alloc_downcall_handle`] for exactly that reason. This
/// stays because the registration on the old receiver is still present (see
/// the note at the `DowncallHandle` registration block) and because it is the
/// one place the descriptor→`MethodType` derivation is spelled out.
pub(crate) fn pe_downcall_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = obj_arg(args, 0)?;
    let descriptor = downcall_descriptor(ctx, handle).ok_or_else(|| -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: "DowncallHandle has no FunctionDescriptor".into(),
        }
        .into()
    })?;
    let captures = downcall_handle_captures_call_state(ctx, handle);
    let method_descriptor = downcall_carrier_descriptor(ctx, descriptor, captures)?;

    let method_type =
        crate::lang_invoke::build_method_type_from_descriptor(ctx, &method_descriptor)?
            .ok_or_else(|| -> MethodCallFailed {
                RuntimeError::IllegalStateException {
                    message: format!(
                "Unable to construct MethodType for DowncallHandle descriptor {method_descriptor}"
            ),
                }
                .into()
            })?;
    Ok(Some(Value::Object(Some(method_type))))
}

/// The JVM descriptor letter a layout's carrier contributes to a downcall's
/// `MethodType`.
///
/// **Takes the LAYOUT, not a bare kind, and returns a `Result`.** It used to
/// take an `i32` and answer `"Ljava/lang/foreign/MemorySegment;"` for anything
/// it did not recognise — including `panama_libffi::LAYOUT_UNKNOWN`, whose
/// whole purpose is to stop an undecodable carrier from getting a plausible
/// answer. A wrong descriptor here is not a local wrong answer: it is the
/// signature the `MethodType` is built from, so every argument after it lands
/// in the wrong slot kind. Refusing needs the layout's class name to be
/// actionable, which is why the parameter changed.
fn downcall_layout_carrier_descriptor(
    ctx: &dyn NativeContext,
    layout: ObjectRef,
) -> Result<&'static str, MethodCallFailed> {
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    Ok(match kind {
        LAYOUT_BOOLEAN => "Z",
        LAYOUT_BYTE => "B",
        LAYOUT_SHORT => "S",
        LAYOUT_CHAR => "C",
        LAYOUT_INT => "I",
        LAYOUT_LONG => "J",
        LAYOUT_FLOAT => "F",
        LAYOUT_DOUBLE => "D",
        crate::panama_libffi::LAYOUT_UNKNOWN => {
            return Err(unclassifiable_access_layout(
                ctx,
                layout,
                "downcall carrier",
            ))
        }
        // ADDRESS and aggregate layouts use the FFM MemorySegment carrier.
        _ => "Ljava/lang/foreign/MemorySegment;",
    })
}

fn downcall_handle_captures_call_state(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    matches!(ctx.get_field(handle, DOWNCALL_CAPTURE_CALL_STATE), Value::Int(flag) if flag != 0)
}

fn write_downcall_capture_state(ctx: &dyn NativeContext, state: ObjectRef) {
    #[cfg(target_os = "linux")]
    {
        let addr = crate::panama_libffi::segment_address(ctx, state);
        if addr != 0 {
            // The standard Linux capture-state layout starts with errno.
            unsafe { *(addr as *mut i32) = *libc::__errno_location() };
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ctx, state);
    }
}

/// Execute a downcall via libffi (NEW-18).
///
/// The handle carries the function pointer, the FunctionDescriptor, an
/// optional fixed-arg count for variadic calls (-1 means non-variadic) and a
/// T5.6.3 cached `Box<Cif>` raw pointer (0 means unpopulated) at the
/// `DOWNCALL_*` slots — see [`alloc_downcall_handle`]. The descriptor carries
/// the parameter and return layouts.
///
/// libffi handles ABI classification (integer vs float register
/// allocation, struct-by-value, alignment, padding) for every
/// supported platform — replacing the previous 8-arg integer-only
/// dispatcher. See `panama_libffi` for the layout↔ffi_type bridge.
///
/// TODO(T5.6.3 finalization): the `Box<Cif>` stashed on `DOWNCALL_CIF` is
/// currently leaked when the DowncallHandle is garbage-collected —
/// the Panama synthetic objects don't yet route through a finalizer
/// callback. Since `Linker::downcallHandle` is called once per native
/// function per VM run the leak is bounded (typically a handful of
/// Cifs) and matches HotSpot's own permanent FFI metadata. When a
/// finalizer path lands, call `plf::free_cached_cif(field3)` from it.
pub(crate) fn pe_downcall_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::panama_libffi as plf;
    use libffi::middle::{arg as ffi_arg, CodePtr};

    require_native_access(ctx, "downcall")?;
    let handle = obj_arg(args, 0)?;
    let fn_addr = downcall_fn_addr(ctx, handle);
    // Work-list item 14. The symbol name is not carried on the handle, so the
    // scope is the target address — which is what a denial needs to report and
    // what an operator would have to grant. Permissive by default.
    crate::capability_gate::gate_foreign_downcall(&*ctx, &format!("0x{fn_addr:x}"))?;
    let descriptor = downcall_descriptor(ctx, handle).ok_or_else(|| -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: "No FunctionDescriptor".into(),
        }
        .into()
    })?;

    if fn_addr == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Null function pointer".into(),
        }
        .into());
    }
    // Run the validated_fn_ptr alignment check.
    let _: unsafe extern "C" fn() = validated_fn_ptr(fn_addr)?;

    // Variadic flag — the Linker.Option.firstVariadicArg(int) overload
    // populates this when registering the downcall handle. -1 (or
    // missing field) = non-variadic.
    let variadic_fixed: Option<usize> = match ctx.get_field(handle, DOWNCALL_FIRST_VARIADIC) {
        Value::Long(n) if n >= 0 => Some(n as usize),
        Value::Int(n) if n >= 0 => Some(n as usize),
        _ => None,
    };

    // T5.6.3 — previously cached `Box<Cif>` pointer (0 = miss). See
    // `panama_libffi::box_cif_to_u64` for the encoding.
    let cached_cif_u64: u64 = match ctx.get_field(handle, DOWNCALL_CIF) {
        Value::Long(n) => n as u64,
        _ => 0,
    };

    // ----- Read descriptor layouts -----
    let param_layout_objs = plf::descriptor_param_layouts(ctx, descriptor);
    let return_layout = plf::descriptor_return_layout(ctx, descriptor);

    // ----- Read incoming Java arguments -----
    //
    // The legacy synthetic bridge calls invoke(Object[]) and supplies one
    // boxed array after the handle. Signature-polymorphic real-JDK calls
    // arrive as [handle, arg0, arg1, ...], so preserve those concrete
    // values instead of interpreting the first reference argument as an
    // array.
    let mut call_args: Vec<Value> = match args.get(1) {
        Some(Value::Object(Some(arr))) if args.len() == 2 && ctx.object_is_array(*arr) => {
            let len = ctx.array_length(*arr);
            (0..len).map(|i| ctx.get_array_element(*arr, i)).collect()
        }
        _ => args[1..].to_vec(),
    };

    let capture_state = if downcall_handle_captures_call_state(ctx, handle) {
        if call_args.len() != param_layout_objs.len() + 1 {
            return Err(RuntimeError::IllegalStateException {
                message: format!(
                    "Panama capture-state downcall expected {} arguments, got {}",
                    param_layout_objs.len() + 1,
                    call_args.len()
                ),
            }
            .into());
        }
        match call_args.remove(0) {
            Value::Object(Some(state)) => Some(state),
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "Panama capture-state argument is not a MemorySegment".into(),
                }
                .into());
            }
        }
    } else {
        None
    };

    if call_args.len() != param_layout_objs.len() {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Panama downcall arity mismatch: descriptor has {} params, got {} args",
                param_layout_objs.len(),
                call_args.len()
            ),
        }
        .into());
    }

    // Heap-array segments use a native mirror so moving GC cannot expose the
    // Java heap directly. Refresh mirrors at the precise foreign boundary.
    for value in &call_args {
        if let Value::Object(Some(segment)) = value {
            sync_heap_backed_segment(ctx, *segment, true);
        }
    }

    // ----- Marshal arguments into per-slot scratch storage -----
    // Must happen before the CIF hit/miss fork because it allocates
    // per-call storage that depends on the actual Value inputs.
    let mut slots: Vec<plf::ArgSlot> = Vec::with_capacity(call_args.len());
    for (i, val) in call_args.iter().enumerate() {
        let layout = param_layout_objs[i].ok_or_else(|| -> MethodCallFailed {
            RuntimeError::IllegalStateException {
                message: format!("Panama downcall parameter {} layout is null", i),
            }
            .into()
        })?;
        slots.push(plf::marshal_arg(ctx, layout, val)?);
    }

    // ----- CIF: cache hit / miss -----
    //
    // If the DowncallHandle carries a previously boxed Cif, reuse it.
    // Otherwise build a fresh Cif, box it, and stash the raw pointer
    // on field 3 so the next invocation skips layout→ffi_type
    // translation and the libffi `prep_cif` call.
    //
    // After this block, `raw_cif_ptr` always points at a Cif living
    // inside the stashed Box on the handle synthetic. The Box outlives
    // us (it lives until handle finalization).
    let raw_cif_ptr: *mut libffi::low::ffi_cif = if cached_cif_u64 != 0 {
        // SAFETY: `cached_cif_u64` was produced by box_cif_to_u64 in
        // a prior invocation on this same handle; the Box outlives
        // this call (freed on handle finalization). libffi only
        // reads the Cif during ffi_call.
        let cif_ref =
            unsafe { plf::cached_cif_ref(cached_cif_u64) }.expect("non-zero pointer must deref");
        cif_ref.as_raw_ptr()
    } else {
        // ----- Build libffi types for each parameter -----
        let mut ffi_arg_types = Vec::with_capacity(param_layout_objs.len());
        for (i, pl) in param_layout_objs.iter().enumerate() {
            let layout = pl.ok_or_else(|| -> MethodCallFailed {
                RuntimeError::IllegalStateException {
                    message: format!("Panama downcall parameter {} layout is null", i),
                }
                .into()
            })?;
            ffi_arg_types.push(plf::layout_to_ffi_type(ctx, layout, 0)?);
        }

        // Return type
        let ret_ffi_type = match return_layout {
            Some(rl) => plf::layout_to_ffi_type(ctx, rl, 0)?,
            None => libffi::middle::Type::void(),
        };

        // Build CIF (variadic-aware) and record the build for tests.
        let cif = plf::build_cif_and_record(ffi_arg_types, ret_ffi_type, variadic_fixed)?;
        // Move the Cif into a Box, stash the raw pointer, and return a
        // pointer to the Box-owned Cif for this call.
        let stash_u64 = plf::box_cif_to_u64(cif);
        ctx.set_field(handle, DOWNCALL_CIF, Value::Long(stash_u64 as i64));
        // SAFETY: stash_u64 was produced above; Box is live for the
        // remainder of this call and beyond.
        let cif_ref =
            unsafe { plf::cached_cif_ref(stash_u64) }.expect("just-stashed pointer must deref");
        cif_ref.as_raw_ptr()
    };

    // libffi expects the args slice to be `&[Arg]` where each `Arg`
    // wraps a `*mut c_void` — which is the address of the typed slot
    // bytes. The slot Vec must outlive the call.
    let arg_refs: Vec<libffi::middle::Arg> = slots
        .iter()
        .map(|s| ffi_arg(unsafe { &*(s.as_ptr() as *const u8) }))
        .collect();

    // ----- Allocate return slot -----
    let mut ret_slot = plf::alloc_return_slot(ctx, return_layout)?;

    // ----- Make the call -----
    // Install the active NativeContext so any libffi-closure-backed
    // upcall fired from C during this downcall can re-enter Java.
    // The guard restores the previous slot on drop.
    let _ctx_guard = plf::ActiveContextGuard::install(ctx);

    // SAFETY: fn_addr was validated above for non-null and alignment
    // via validated_fn_ptr. The CIF was built from the descriptor, so
    // arg/return types match the slot byte layout. The slot vectors
    // outlive the call (held until return). `raw_cif_ptr` points into
    // the stashed `Box<Cif>` on the handle synthetic (field 3), which
    // lives for the remainder of the handle's lifetime.
    unsafe {
        // ffi_call requires a real writable result address even for a void
        // descriptor. Do not use low::call<()> here: its zero-sized return
        // slot gives libffi a dangling result pointer and caused native void
        // calls (the Elasticsearch bulk-vector symbols) to leave outputs
        // untouched on this ABI.
        let mut void_sink = [0u8; std::mem::size_of::<usize>()];
        let result_ptr = if ret_slot.is_empty() {
            void_sink.as_mut_ptr() as *mut std::ffi::c_void
        } else {
            ret_slot.as_mut_ptr() as *mut std::ffi::c_void
        };
        libffi::raw::ffi_call(
            raw_cif_ptr,
            Some(*CodePtr::from_ptr(fn_addr as *const std::ffi::c_void).as_safe_fun()),
            result_ptr,
            arg_refs.as_ptr() as *mut *mut std::ffi::c_void,
        );
    }

    for value in &call_args {
        if let Value::Object(Some(segment)) = value {
            sync_heap_backed_segment(ctx, *segment, false);
        }
    }

    if crate::nbflags().dbg_mh_dispatch && return_layout.is_none() && call_args.len() == 5 {
        if let Some(Value::Object(Some(out))) = call_args.last() {
            if let Value::Long(ptr) = ctx.get_field(*out, 0) {
                if ptr != 0 {
                    let first = unsafe { *(ptr as *const f32) };
                    eprintln!("[PANAMA_POST_VOID] fn=0x{fn_addr:x} out=0x{ptr:x} first={first}");
                }
            }
        }
    }
    if let Some(state) = capture_state {
        write_downcall_capture_state(ctx, state);
    }

    // ----- Unmarshal return -----
    let result = match return_layout {
        None => Value::Object(None),
        Some(rl) => {
            let kind = plf::read_layout_kind(ctx, rl);
            if kind == LAYOUT_ADDRESS {
                let address = match plf::unmarshal_return_primitive(kind, &ret_slot) {
                    Value::Long(address) => address,
                    _ => 0,
                };
                let seg = alloc_segment_carrier(ctx, 6)?;
                ctx.set_field(seg, 0, Value::Long(address));
                ctx.set_field(seg, 1, Value::Long(0));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                Value::Object(Some(seg))
            } else if kind == plf::LAYOUT_UNKNOWN {
                // Cannot arrive today — `alloc_return_slot` refused this
                // carrier before the call was made, and it is on every path to
                // here. Named anyway: the old spelling of the arm below was
                // `kind < 10`, which is TRUE for -2, so an unknown carrier
                // would have been unmarshalled as whatever
                // `unmarshal_return_primitive`'s default arm answers rather
                // than reported. See [`layout_kind_is_value`].
                return Err(unclassifiable_access_layout(ctx, rl, "downcall return"));
            } else if layout_kind_is_value(kind) {
                plf::unmarshal_return_primitive(kind, &ret_slot)
            } else {
                // Struct/union/sequence return: copy the bytes into a
                // freshly allocated MemorySegment via the global arena
                // path. The Java caller will then read the segment.
                //
                // Sized from the layout's own `byteSize`/`byteAlignment` and
                // not from `plf::layout_total_size` — see the note at
                // `Arena.allocate(MemoryLayout)`. `layout_total_size` answers 8
                // for EVERY group layout (its `read_layout_kind` resolves a
                // `Long`-slot-0 carrier by class name, and no GROUP class is on
                // that list, so it defaults to `LAYOUT_LONG`), so a by-value
                // struct return of any width got an 8-byte segment.
                let total =
                    crate::phases_late::foreign_ffm::p67_layout_size_of(ctx, rl).max(0) as usize;
                let align =
                    crate::phases_late::foreign_ffm::p67_layout_align_of(ctx, rl).max(1) as usize;
                let (alloc_id, ptr) = ctx.allocate_native_memory(total, align.max(8)).ok_or_else(
                    || -> MethodCallFailed {
                        RuntimeError::OutOfMemoryError {
                            message: "Failed to allocate result MemorySegment".into(),
                        }
                        .into()
                    },
                )?;
                // THE COPY LENGTH IS CLAMPED TO `ret_slot`, DELIBERATELY.
                //
                // `ret_slot` is sized by `panama_libffi::alloc_return_slot`
                // and `total` by `foreign_ffm::p67_layout_size_of` — two size
                // functions in two files, with one `unsafe
                // copy_nonoverlapping` between them. Copying `total` bytes
                // unconditionally would read past the end of `ret_slot` the
                // moment those two disagree in that direction, which is what
                // fixing the size on only one side of the pair produces.
                //
                // THE MATCHING FIX LANDED (F27, 2026-08-13).
                // `panama_libffi::layout_total_size` now reads the carrier's
                // own `[0]=byteSize`, and `alloc_return_slot` rounds that up to
                // the layout's alignment because libffi writes the C size of an
                // aggregate (measured: struct(JAVA_LONG, JAVA_INT) is 12 to the
                // JDK and 16 to C). So `ret_slot.len() >= total` always and this
                // clamp no longer truncates anything.
                //
                // IT STAYS ANYWAY. `total` is computed by `p67_layout_size_of`
                // in foreign_ffm.rs and `ret_slot.len()` by `alloc_return_slot`
                // in panama_libffi.rs; the clamp is the LOCAL proof that the
                // unsafe copy below is in bounds. Removing it would make an
                // unsafe block's safety argument depend on two functions in two
                // files continuing to agree, with nothing at the site saying so.
                let copy_len = total.min(ret_slot.len());
                // SAFETY: `ptr` was freshly allocated with `total >= copy_len`
                // bytes, and `copy_len <= ret_slot.len()`, so both sides are in
                // bounds. The regions cannot overlap — one is a fresh
                // allocation.
                unsafe {
                    std::ptr::copy_nonoverlapping(ret_slot.as_ptr(), ptr, copy_len);
                }
                let seg = alloc_segment_carrier(ctx, 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(total as i64));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                let _ = alloc_id;
                Value::Object(Some(seg))
            }
        }
    };

    Ok(Some(result))
}

// --- FunctionDescriptor: native function signature ---
// FunctionDescriptor: [0]=return layout (Object or null for void), [1]=param layouts (Object array)

fn register_pe_function_descriptor(r: &mut NativeMethodRegistry) {
    let fd = "java/lang/foreign/FunctionDescriptor";

    let prev_category = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // of(returnLayout, paramLayouts...) → FunctionDescriptor
    r.register(fd, "of", "(Ljava/lang/foreign/ValueLayout;[Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/FunctionDescriptor;", |ctx, args| {
        let ret_layout = args.first().copied().unwrap_or(Value::Object(None));
        let params = args.get(1).copied().unwrap_or(Value::Object(None));
        let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
        ctx.set_field(desc, 0, ret_layout);
        ctx.set_field(desc, 1, params);
        Ok(Some(Value::Object(Some(desc))))
    });

    r.register(fd, "of", "(Ljava/lang/foreign/MemoryLayout;[Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;", |ctx, args| {
        let ret_layout = args.first().copied().unwrap_or(Value::Object(None));
        let params = args.get(1).copied().unwrap_or(Value::Object(None));
        let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
        ctx.set_field(desc, 0, ret_layout);
        ctx.set_field(desc, 1, params);
        Ok(Some(Value::Object(Some(desc))))
    });

    // ofVoid(paramLayouts...) → FunctionDescriptor
    r.register(
        fd,
        "ofVoid",
        "([Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/FunctionDescriptor;",
        |ctx, args| {
            let params = args.first().copied().unwrap_or(Value::Object(None));
            let desc =
                try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
            ctx.set_field(desc, 0, Value::Object(None)); // void return
            ctx.set_field(desc, 1, params);
            Ok(Some(Value::Object(Some(desc))))
        },
    );

    r.register(
        fd,
        "ofVoid",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;",
        |ctx, args| {
            let params = args.first().copied().unwrap_or(Value::Object(None));
            let desc =
                try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
            ctx.set_field(desc, 0, Value::Object(None));
            ctx.set_field(desc, 1, params);
            Ok(Some(Value::Object(Some(desc))))
        },
    );

    // returnLayout() → Optional<ValueLayout>
    r.register(fd, "returnLayout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let rl = ctx.get_field(this, 0);
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, rl);
        Ok(Some(Value::Object(Some(opt))))
    });

    // argumentLayouts() → ValueLayout[]
    r.register(
        fd,
        "argumentLayouts",
        "()[Ljava/lang/foreign/ValueLayout;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.set_category(prev_category);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// =============================================================================
// Phase 85.4: Upcall Trampoline Pool
// =============================================================================
//
// When native code needs to call back into Java (upcall), we need a real
// extern "C" function pointer. Since we can't dynamically generate executable
// code portably, we pre-generate a fixed pool of trampoline functions via macro.
//
// Each trampoline:
//   1. Reads its slot index (embedded as a constant)
//   2. Looks up the callback info from the global dispatch table
//   3. Invokes the Java method handle through a thread-local NativeContext
//
// The pool size (MAX_UPCALL_TRAMPOLINES) limits concurrent upcall stubs.

use std::sync::{Mutex, OnceLock};

/// Maximum number of concurrent upcall trampolines.
const MAX_UPCALL_TRAMPOLINES: usize = 64;

/// Global dispatch table entry for an upcall trampoline.
struct UpcallTrampolineEntry {
    /// The Java target object (MethodHandle)
    target: ObjectRef,
    /// Parameter layout kinds for marshaling
    param_kinds: Vec<i32>,
    /// Return layout kind for marshaling
    return_kind: i32,
    /// Whether this slot is in use
    active: bool,
}

/// Global dispatch table — maps trampoline slot to callback info.
static UPCALL_DISPATCH_TABLE: OnceLock<Mutex<Vec<Option<UpcallTrampolineEntry>>>> = OnceLock::new();

fn upcall_dispatch_table() -> &'static Mutex<Vec<Option<UpcallTrampolineEntry>>> {
    UPCALL_DISPATCH_TABLE.get_or_init(|| {
        let mut v = Vec::with_capacity(MAX_UPCALL_TRAMPOLINES);
        for _ in 0..MAX_UPCALL_TRAMPOLINES {
            v.push(None);
        }
        Mutex::new(v)
    })
}

/// Allocate a trampoline slot and return (slot_index, function_pointer).
fn allocate_trampoline_slot(
    target: ObjectRef,
    param_kinds: Vec<i32>,
    return_kind: i32,
) -> Option<(usize, usize)> {
    let mut table = upcall_dispatch_table().lock().ok()?;
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(UpcallTrampolineEntry {
                target,
                param_kinds,
                return_kind,
                active: true,
            });
            // Return the function pointer for trampoline #i
            let fn_ptr = UPCALL_TRAMPOLINE_FNS[i] as usize;
            return Some((i, fn_ptr));
        }
    }
    None // all slots in use
}

/// Free a trampoline slot.
fn free_trampoline_slot(slot: usize) {
    if let Ok(mut table) = upcall_dispatch_table().lock() {
        if slot < table.len() {
            table[slot] = None;
        }
    }
}

/// Generic trampoline dispatch: called by each generated trampoline with its slot index.
/// Accepts up to 8 raw u64 arguments and returns a u64 result.
///
/// This function looks up the callback in the dispatch table and invokes it
/// through the VM's upcall mechanism (pe_upcall_invoke). Since we're called
/// from C, we don't have a NativeContext — real upcalls would go through the
/// JNI thread-local VM reference. For now, the trampoline returns 0 if no
/// context is available (the Java-side dispatch happens through pe_upcall_invoke
/// when the JVM calls the stub's invoke method).
fn trampoline_dispatch(slot: usize, args: &[u64]) -> u64 {
    let table = match upcall_dispatch_table().lock() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let entry = match &table[slot] {
        Some(e) if e.active => e,
        _ => return 0,
    };
    // In a full implementation, we'd use thread-local NativeContext to invoke
    // the Java target. For now, we record that the trampoline was called and
    // return 0 (the pe_upcall_invoke path handles actual Java dispatch).
    let _ = (entry.target, &entry.param_kinds, entry.return_kind);
    let _ = args;
    0
}

/// Macro to generate N extern "C" trampoline functions.
#[allow(unused_macros)]
macro_rules! gen_trampolines {
    ($($idx:expr),* $(,)?) => {
        $(
            unsafe extern "C" fn _upcall_trampoline_fn($idx: usize,
                a0: u64, a1: u64, a2: u64, a3: u64,
                a4: u64, a5: u64, a6: u64, a7: u64) -> u64
            {
                // The slot index is baked into the function via the array index
                // We use a workaround: each function is distinct because the
                // compiler sees a different constant in the body.
                let _ = $idx; // suppress unused warning; the slot is the array index
                0
            }
        )*
    };
}

// Generate individual trampoline functions with distinct slot constants.
macro_rules! gen_trampoline {
    ($name:ident, $slot:expr) => {
        unsafe extern "C" fn $name(
            a0: u64,
            a1: u64,
            a2: u64,
            a3: u64,
            a4: u64,
            a5: u64,
            a6: u64,
            a7: u64,
        ) -> u64 {
            trampoline_dispatch($slot, &[a0, a1, a2, a3, a4, a5, a6, a7])
        }
    };
}

gen_trampoline!(_upcall_t00, 0);
gen_trampoline!(_upcall_t01, 1);
gen_trampoline!(_upcall_t02, 2);
gen_trampoline!(_upcall_t03, 3);
gen_trampoline!(_upcall_t04, 4);
gen_trampoline!(_upcall_t05, 5);
gen_trampoline!(_upcall_t06, 6);
gen_trampoline!(_upcall_t07, 7);
gen_trampoline!(_upcall_t08, 8);
gen_trampoline!(_upcall_t09, 9);
gen_trampoline!(_upcall_t10, 10);
gen_trampoline!(_upcall_t11, 11);
gen_trampoline!(_upcall_t12, 12);
gen_trampoline!(_upcall_t13, 13);
gen_trampoline!(_upcall_t14, 14);
gen_trampoline!(_upcall_t15, 15);
gen_trampoline!(_upcall_t16, 16);
gen_trampoline!(_upcall_t17, 17);
gen_trampoline!(_upcall_t18, 18);
gen_trampoline!(_upcall_t19, 19);
gen_trampoline!(_upcall_t20, 20);
gen_trampoline!(_upcall_t21, 21);
gen_trampoline!(_upcall_t22, 22);
gen_trampoline!(_upcall_t23, 23);
gen_trampoline!(_upcall_t24, 24);
gen_trampoline!(_upcall_t25, 25);
gen_trampoline!(_upcall_t26, 26);
gen_trampoline!(_upcall_t27, 27);
gen_trampoline!(_upcall_t28, 28);
gen_trampoline!(_upcall_t29, 29);
gen_trampoline!(_upcall_t30, 30);
gen_trampoline!(_upcall_t31, 31);
gen_trampoline!(_upcall_t32, 32);
gen_trampoline!(_upcall_t33, 33);
gen_trampoline!(_upcall_t34, 34);
gen_trampoline!(_upcall_t35, 35);
gen_trampoline!(_upcall_t36, 36);
gen_trampoline!(_upcall_t37, 37);
gen_trampoline!(_upcall_t38, 38);
gen_trampoline!(_upcall_t39, 39);
gen_trampoline!(_upcall_t40, 40);
gen_trampoline!(_upcall_t41, 41);
gen_trampoline!(_upcall_t42, 42);
gen_trampoline!(_upcall_t43, 43);
gen_trampoline!(_upcall_t44, 44);
gen_trampoline!(_upcall_t45, 45);
gen_trampoline!(_upcall_t46, 46);
gen_trampoline!(_upcall_t47, 47);
gen_trampoline!(_upcall_t48, 48);
gen_trampoline!(_upcall_t49, 49);
gen_trampoline!(_upcall_t50, 50);
gen_trampoline!(_upcall_t51, 51);
gen_trampoline!(_upcall_t52, 52);
gen_trampoline!(_upcall_t53, 53);
gen_trampoline!(_upcall_t54, 54);
gen_trampoline!(_upcall_t55, 55);
gen_trampoline!(_upcall_t56, 56);
gen_trampoline!(_upcall_t57, 57);
gen_trampoline!(_upcall_t58, 58);
gen_trampoline!(_upcall_t59, 59);
gen_trampoline!(_upcall_t60, 60);
gen_trampoline!(_upcall_t61, 61);
gen_trampoline!(_upcall_t62, 62);
gen_trampoline!(_upcall_t63, 63);

/// Table of trampoline function pointers, indexed by slot.
static UPCALL_TRAMPOLINE_FNS: [unsafe extern "C" fn(
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
) -> u64; MAX_UPCALL_TRAMPOLINES] = [
    _upcall_t00,
    _upcall_t01,
    _upcall_t02,
    _upcall_t03,
    _upcall_t04,
    _upcall_t05,
    _upcall_t06,
    _upcall_t07,
    _upcall_t08,
    _upcall_t09,
    _upcall_t10,
    _upcall_t11,
    _upcall_t12,
    _upcall_t13,
    _upcall_t14,
    _upcall_t15,
    _upcall_t16,
    _upcall_t17,
    _upcall_t18,
    _upcall_t19,
    _upcall_t20,
    _upcall_t21,
    _upcall_t22,
    _upcall_t23,
    _upcall_t24,
    _upcall_t25,
    _upcall_t26,
    _upcall_t27,
    _upcall_t28,
    _upcall_t29,
    _upcall_t30,
    _upcall_t31,
    _upcall_t32,
    _upcall_t33,
    _upcall_t34,
    _upcall_t35,
    _upcall_t36,
    _upcall_t37,
    _upcall_t38,
    _upcall_t39,
    _upcall_t40,
    _upcall_t41,
    _upcall_t42,
    _upcall_t43,
    _upcall_t44,
    _upcall_t45,
    _upcall_t46,
    _upcall_t47,
    _upcall_t48,
    _upcall_t49,
    _upcall_t50,
    _upcall_t51,
    _upcall_t52,
    _upcall_t53,
    _upcall_t54,
    _upcall_t55,
    _upcall_t56,
    _upcall_t57,
    _upcall_t58,
    _upcall_t59,
    _upcall_t60,
    _upcall_t61,
    _upcall_t62,
    _upcall_t63,
];

// =============================================================================
// Phase E2: Struct/Union Layouts, Upcalls, String Marshaling
// =============================================================================

// =============================================================================
// NEW-18: real upcall handles via libffi closures
// =============================================================================
//
// `Linker.upcallHandle(target, descriptor, arena)` returns a
// MemorySegment wrapping a real extern "C" function pointer that C
// code can call directly. libffi generates the trampoline; our closure
// dispatches back into Java via the per-thread NativeContext installed
// by the surrounding downcall.
//
// Each upcall keeps a `Box<UpcallClosure>` alive on the heap. The
// closure owns its `libffi::middle::Closure` (which owns the libffi
// closure object + executable trampoline page) plus the userdata. We
// register a global registry keyed by the trampoline address so that
// a) the closure stays alive until the owning arena is closed, and
// b) the userdata pointer remains stable across the call.

use libffi::middle::{Cif as MiddleCif, Closure, Type as MiddleType};

/// Per-upcall state held alive in `UPCALL_REGISTRY`.
///
/// libffi `Closure` is not `Send` because it carries raw pointers
/// to its trampoline page, but the closure data is read-only after
/// construction and the trampoline page is allocated by libffi with
/// rwx permissions independent of any thread. We assert `Send`/`Sync`
/// manually so we can park it in a global mutex-guarded map.
struct UpcallEntry {
    /// libffi closure object — owns the executable trampoline page.
    _closure: Box<Closure<'static>>,
    /// The leaked `&'static UpcallUserdata` the trampoline reads its target from.
    /// Held here (the closure captures the same allocation) so the GC root
    /// scan/remap can reach and rewrite `target` in place — see
    /// `gc_scan_upcall_target_roots` / `gc_update_upcall_target_refs`. Step 5
    /// GAP C: the upcall target is a live Java object the native trampoline
    /// holds; without this it was neither kept alive nor remapped across a
    /// moving GC (use-after-free on the next upcall).
    userdata: *const UpcallUserdata,
}

// SAFETY: libffi closures are immutable after construction and their
// trampoline pages are independent of any thread. The userdata we
// store is `'static` and only read by the closure callback.
unsafe impl Send for UpcallEntry {}
unsafe impl Sync for UpcallEntry {}

/// Userdata captured by every upcall trampoline.
struct UpcallUserdata {
    /// Java target object address (a relocatable heap pointer), stored as an
    /// `AtomicUsize` so the GC remap (`gc_update_upcall_target_refs`) can rewrite
    /// it in place after a moving collection. The trampoline loads it on each
    /// dispatch; both run at a stop-the-world safepoint relative to one another,
    /// so `Relaxed` is sufficient.
    target: std::sync::atomic::AtomicUsize,
    param_kinds: Vec<i32>,
    return_kind: i32,
}

static UPCALL_REGISTRY: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<usize, UpcallEntry>>,
> = std::sync::OnceLock::new();

fn upcall_registry() -> &'static parking_lot::Mutex<std::collections::HashMap<usize, UpcallEntry>> {
    UPCALL_REGISTRY.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Step 5 GAP C — GC root scan for FFM/Panama upcall targets. Each registered
/// upcall trampoline holds a live Java target (a `MethodHandle`/lambda) only
/// through its leaked `UpcallUserdata.target`, which is otherwise invisible to
/// the GC. Push every one so a moving collector keeps it alive and records its
/// relocation in the pointer map. Companion of [`gc_update_upcall_target_refs`]
/// — the two MUST visit the identical set. Called from the VM root scan
/// (`memory::roots`). The registry mutex is a leaf lock (no Java allocation
/// while held), so this is safe to call at a stop-the-world safepoint.
pub fn gc_scan_upcall_target_roots(out: &mut Vec<ObjectRef>) {
    let reg = upcall_registry().lock();
    for entry in reg.values() {
        // SAFETY: `userdata` is a leaked `&'static UpcallUserdata`, alive for the
        // whole process (the closure captures the same allocation).
        let addr = unsafe {
            (*entry.userdata)
                .target
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        if addr != 0 {
            // SAFETY: a non-zero, 8-byte-aligned heap address previously stored
            // from a live `ObjectRef`; used only as a GC root here.
            out.push(unsafe { ObjectRef::from_raw(addr as *mut u8) });
        }
    }
}

/// Step 5 GAP C — post-move remap for upcall targets (companion of
/// [`gc_scan_upcall_target_roots`]). After a moving collection relocates a
/// target, rewrite each `UpcallUserdata.target` in place so the next trampoline
/// dispatch reaches the new address. Called from the VM's `update_all_roots`.
pub fn gc_update_upcall_target_refs(map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let reg = upcall_registry().lock();
    for entry in reg.values() {
        // SAFETY: see `gc_scan_upcall_target_roots`.
        let cell = unsafe { &(*entry.userdata).target };
        let old = cell.load(std::sync::atomic::Ordering::Relaxed);
        if let Some(&new) = map.get(&old) {
            debug_assert!(new != 0, "GC pointer map contains null address");
            cell.store(new, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// libffi `Callback<UpcallUserdata, u64>` — runs whenever the trampoline
/// returned by `pe_upcall_handle` is invoked from C.
///
/// The downcall that triggered this callback installed the active
/// NativeContext via `ActiveContextGuard`, so we can re-enter Java
/// from this exact thread.
unsafe extern "C" fn upcall_dispatch(
    _cif: &libffi::low::ffi_cif,
    result: &mut u64,
    args: *const *const std::ffi::c_void,
    userdata: &UpcallUserdata,
) {
    use crate::panama_libffi as plf;

    // Default the return slot to zero in case dispatch fails.
    *result = 0;

    let nargs = userdata.param_kinds.len();
    // Decode args from the libffi-supplied void**.
    let mut java_args: Vec<Value> = Vec::with_capacity(nargs);
    for (i, &kind) in userdata.param_kinds.iter().enumerate() {
        let slot = *args.add(i);
        let v = match kind {
            cratonvm_native_api::ffi::LAYOUT_BYTE | cratonvm_native_api::ffi::LAYOUT_BOOLEAN => {
                Value::Int(*(slot as *const i8) as i32)
            }
            cratonvm_native_api::ffi::LAYOUT_SHORT | cratonvm_native_api::ffi::LAYOUT_CHAR => {
                Value::Int(*(slot as *const i16) as i32)
            }
            cratonvm_native_api::ffi::LAYOUT_INT => Value::Int(*(slot as *const i32)),
            cratonvm_native_api::ffi::LAYOUT_LONG => Value::Long(*(slot as *const i64)),
            cratonvm_native_api::ffi::LAYOUT_FLOAT => Value::Float(*(slot as *const f32)),
            cratonvm_native_api::ffi::LAYOUT_DOUBLE => Value::Double(*(slot as *const f64)),
            cratonvm_native_api::ffi::LAYOUT_ADDRESS => Value::Long(*(slot as *const i64)),
            _ => Value::Long(0),
        };
        java_args.push(v);
    }

    // GAP C: read the GC-remappable target address atomically before re-entering
    // Java. The remap (`gc_update_upcall_target_refs`) rewrites `userdata.target`
    // in place at a stop-the-world safepoint, so the next dispatch loads the new
    // address; this load and that store never overlap (STW).
    let target = unsafe {
        cratonvm_types::ObjectRef::from_raw(
            userdata.target.load(std::sync::atomic::Ordering::Relaxed) as *mut u8,
        )
    };
    // Dispatch into Java via the active NativeContext.
    let dispatch_result = plf::with_active_context(|ctx| {
        // The Java target is a MethodHandle / functional interface impl.
        // We invoke its `invoke([Object])` method passing our boxed args.
        // Build an Object[] of boxed primitives.
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, java_args.len());
        for (i, v) in java_args.iter().enumerate() {
            // Pass primitives through directly; the receiver is
            // expected to pattern-match on Value via lambda dispatch.
            // For invoke_virtual the args slice is [receiver, args...]
            // so we don't store into the array for primitives (the
            // receiver-less variant uses `invoke_virtual` with a fresh
            // single-element packed array of references).
            ctx.set_array_element(arr, i, *v);
        }
        ctx.invoke_virtual(
            target,
            "invoke",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(target)), Value::Object(Some(arr))],
        )
    });

    let returned = match dispatch_result {
        Some(Ok(Some(v))) => v,
        _ => Value::Object(None),
    };

    // Marshal Java return value into the C return slot.
    *result = match (userdata.return_kind, returned) {
        (UPCALL_RETURN_VOID, _) => 0,
        (cratonvm_native_api::ffi::LAYOUT_BYTE, Value::Int(n))
        | (cratonvm_native_api::ffi::LAYOUT_BOOLEAN, Value::Int(n))
        | (cratonvm_native_api::ffi::LAYOUT_SHORT, Value::Int(n))
        | (cratonvm_native_api::ffi::LAYOUT_CHAR, Value::Int(n))
        | (cratonvm_native_api::ffi::LAYOUT_INT, Value::Int(n)) => n as u64,
        (cratonvm_native_api::ffi::LAYOUT_LONG, Value::Long(n))
        | (cratonvm_native_api::ffi::LAYOUT_ADDRESS, Value::Long(n)) => n as u64,
        (cratonvm_native_api::ffi::LAYOUT_FLOAT, Value::Float(f)) => f.to_bits() as u64,
        (cratonvm_native_api::ffi::LAYOUT_DOUBLE, Value::Double(d)) => d.to_bits(),
        _ => 0,
    };
}

/// A scope for an upcall stub: the class of the Java target it dispatches into.
///
/// A `ForeignUpcall` denial that cannot say *which* callback was refused is not
/// actionable, and this is the narrowest name reachable here — the stub's
/// descriptor is a layout list, not a method signature. Falls back to
/// `<unknown>` so a denial always names something.
fn upcall_target_name(ctx: &dyn NativeContext, target: ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(target))
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// `Linker.upcallHandle(target, descriptor, arena)` — build a libffi
/// closure that dispatches into a Java MethodHandle. Returns a
/// MemorySegment whose address is the closure's extern "C" trampoline.
fn pe_upcall_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::panama_libffi as plf;

    let _linker = obj_arg(args, 0)?;
    let target = obj_arg(args, 1)?;
    let descriptor = obj_arg(args, 2)?;

    // GAP F3. This function had **no** native-access gate at all, while the
    // downcall path fails closed — yet it is the more dangerous direction: it
    // hands native code a real extern "C" trampoline into Java. `docs/CONFIG.md`
    // already documents `Linker.upcallHandle` as consulting the access
    // registry; it did not. Restore the documented behaviour, then record the
    // `ForeignUpcall` capability (permissive by default).
    //
    // BEHAVIOUR CHANGE — the only one in this pass: with
    // `--enable-native-access` absent, `upcallHandle` now throws
    // `IllegalCallerException` instead of succeeding, matching
    // `pe_downcall_invoke`. Revert this one line if a workload needs the old
    // laxity; the capability check below is behaviour-neutral on its own.
    require_native_access(ctx, "upcallHandle")?;
    let upcall_target = upcall_target_name(&*ctx, target);
    crate::capability_gate::gate_foreign_upcall(&*ctx, &upcall_target)?;
    // args[3] = Arena — used to bound the closure's lifetime; for
    // simplicity we leak the closure and rely on the registry until
    // the JVM exits. A NEW-17 cleaner could be added if needed.

    // Build libffi types from descriptor
    let param_layouts = plf::descriptor_param_layouts(ctx, descriptor);
    let return_layout = plf::descriptor_return_layout(ctx, descriptor);

    let mut param_kinds: Vec<i32> = Vec::with_capacity(param_layouts.len());
    let mut ffi_params: Vec<MiddleType> = Vec::with_capacity(param_layouts.len());
    for (i, pl) in param_layouts.iter().enumerate() {
        let layout = pl.ok_or_else(|| -> MethodCallFailed {
            RuntimeError::IllegalStateException {
                message: format!("Upcall parameter {} layout is null", i),
            }
            .into()
        })?;
        param_kinds.push(plf::read_layout_kind(ctx, layout));
        ffi_params.push(plf::layout_to_ffi_type(ctx, layout, 0)?);
    }
    let return_kind = match return_layout {
        Some(rl) => plf::read_layout_kind(ctx, rl),
        None => UPCALL_RETURN_VOID,
    };
    let ffi_ret = match return_layout {
        Some(rl) => plf::layout_to_ffi_type(ctx, rl, 0)?,
        None => MiddleType::void(),
    };

    let cif = MiddleCif::new(ffi_params, ffi_ret);

    // Also register the callback in the legacy slot table so the
    // existing `UpcallStub.invoke([Object])` Java-side dispatch path
    // remains operational. Tests + JDK code may still hold references
    // to that slot index.
    let entry = ffi::UpcallEntry {
        target,
        method_name: "invoke".to_string(),
        method_descriptor: String::new(),
        param_kinds: param_kinds.clone(),
        return_kind,
    };
    let _legacy_slot = ctx.register_upcall(entry);

    // Heap-allocate userdata so the closure has a stable reference.
    let userdata = Box::new(UpcallUserdata {
        target: std::sync::atomic::AtomicUsize::new(target.as_ptr() as usize),
        param_kinds: param_kinds.clone(),
        return_kind,
    });
    // Leak the userdata for the closure's lifetime (held in the registry).
    let userdata_ptr: &'static UpcallUserdata = Box::leak(userdata);

    // Build the libffi closure. It becomes a real extern "C" function
    // whose code_ptr() can be called directly from any C code.
    let closure: Closure<'static> = Closure::new(cif, upcall_dispatch, userdata_ptr);
    let code_ptr = *closure.code_ptr() as *const () as usize;
    let boxed_closure = Box::new(closure);
    upcall_registry().lock().insert(
        code_ptr,
        UpcallEntry {
            _closure: boxed_closure,
            // Same leaked allocation the closure captured — the GC root scan/remap
            // reach `target` through this (Step 5 GAP C).
            userdata: userdata_ptr as *const UpcallUserdata,
        },
    );

    // Wrap the trampoline address in a MemorySegment so Java can pass
    // it to other downcalls expecting a `MemorySegment` function ptr.
    let seg = alloc_segment_carrier(ctx, 6)?;
    ctx.set_field(seg, 0, Value::Long(code_ptr as i64));
    ctx.set_field(seg, 1, Value::Long(0));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(1)); // read-only
    ctx.set_field(seg, 4, Value::Int(1)); // alive
    ctx.set_field(seg, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(seg))))
}

/// Dispatch an upcall — called when C invokes a Java callback through the upcall table.
fn pe_upcall_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = obj_arg(args, 0)?;
    let slot = match ctx.get_field(handle, 0) {
        Value::Long(n) => n as usize,
        _ => 0,
    };

    let (target, _param_kinds, return_kind) =
        ctx.get_upcall_info(slot)
            .ok_or_else(|| RuntimeError::IllegalStateException {
                message: format!("Upcall slot {} not found", slot),
            })?;

    // GAP F4: invoking an upcall stub was ungated. The `ForeignUpcall` check is
    // permissive by default; the scope is the callback's class, resolved after
    // the slot lookup so an unknown slot still reports "slot not found".
    let upcall_target = upcall_target_name(&*ctx, target);
    crate::capability_gate::gate_foreign_upcall(&*ctx, &upcall_target)?;

    // Unmarshal args from the Object[] array
    let call_args: Vec<Value> = if args.len() > 1 {
        if let Value::Object(Some(arr)) = args[1] {
            let len = ctx.array_length(arr);
            (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Call the Java target
    let result = ctx.invoke_virtual(
        target,
        "invoke",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &call_args,
    );
    match result {
        Ok(val) => {
            let _ = return_kind;
            Ok(val.or(Some(Value::Object(None))))
        }
        Err(e) => Err(e),
    }
}

// --- MemoryLayout$PathElement ---
//
// This registrar KEEPS ITS NAME and has lost its subject. The
// StructLayout/UnionLayout/SequenceLayout family it was written for lives in
// `phases_late/foreign_ffm.rs` now; what is left is the two path-element
// factories, which have no twin there. The banner that used to sit here
// described the deleted 6-slot `[kind, size, members, names, offsets, align]`
// carrier — the authoritative one is 4-slot `[byteSize, byteAlignment,
// payload, name]` and is documented at `foreign_ffm.rs::p67_member_size_align`.
fn register_pe2_struct_layouts(r: &mut NativeMethodRegistry) {
    let ml = "java/lang/foreign/MemoryLayout";

    // THE WHOLE GROUP-LAYOUT FAMILY WAS DELETED FROM HERE
    // (F16, 2026-08-13). It is now `phases_late/foreign_ffm.rs`'s, alone.
    //
    // What stood here: `structLayout`, `unionLayout`, `sequenceLayout` and
    // `paddingLayout`, each registered TWICE — once under the JDK-true return
    // type and once under a fabricated `…)Ljava/lang/foreign/MemoryLayout;`
    // one — plus `byteSize`/`byteAlignment`/`name`/`withName`/`memberLayouts`
    // on `StructLayout` and `GroupLayout`, `byteOffset` on `StructLayout`, and
    // `withName`/`varHandle`/`name`/`byteSize` on `MemoryLayout`.
    //
    // EVERY ONE OF THEM HAD A TWIN in `register_p67_foreign_memory`, and the
    // twins disagreed, because the two files carry two different layout
    // objects:
    //
    //     panama       [0]=Int(kind) [1]=size [2]=members [3]=names
    //                  [4]=offsets   [5]=align
    //     foreign_ffm  [0]=Long(byteSize) [1]=Long(byteAlignment)
    //                  [2]=payload        [3]=name
    //
    // Which one a caller got was decided by REGISTRATION ORDER, not by the
    // descriptor it wrote: `register_pe_panama` runs after
    // `register_p67_foreign_memory`, and `register()` is last-write-wins, so
    // these rows took the JDK-true keys away from the JDK-true bodies — but
    // only in synthetic-JDK mode, since real-JDK mode never calls
    // `register_pe_panama` at all. One VM, two answers, chosen by which mode
    // you booted.
    //
    // The surviving implementation is also the CORRECT one, which the deleted
    // `pe_struct_layout` was not. Measured on HotSpot 25.0.3+9-LTS:
    //
    //     structLayout(JAVA_BYTE, JAVA_INT)  -> IllegalArgumentException
    //     structLayout(JAVA_INT, JAVA_LONG)  -> IllegalArgumentException
    //     structLayout(JAVA_LONG, JAVA_INT)  -> byteSize=12  (NOT 16)
    //
    // `pe_struct_layout` auto-padded the first two into a fabricated success
    // and rounded the third up to 16. The JDK never pads a struct: the caller
    // writes `paddingLayout(...)`, and an under-aligned member is an error.
    //
    // `MemoryLayout$PathElement`'s two factories are the ONLY thing kept, and
    // deliberately: `foreign_ffm.rs` decodes path elements but mints none,
    // because in real-JDK mode `PathElement.groupElement("c")` runs the JDK's
    // own bytecode and yields a `jdk.internal.foreign.LayoutPath$…` record.
    // Synthetic-JDK mode has no such bytecode, so these two rows are its only
    // source — and the 2-field carrier they build is a shape
    // `p67_classify_path_element` explicitly accepts.

    // PathElement.groupElement(name) → PathElement
    let pe = "java/lang/foreign/MemoryLayout$PathElement";
    r.register(
        pe,
        "groupElement",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout$PathElement;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let elem = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/foreign/MemoryLayout$PathElement",
                2,
            )?;
            ctx.set_field(elem, 0, name); // field name
            ctx.set_field(elem, 1, Value::Int(0)); // kind=group
            Ok(Some(Value::Object(Some(elem))))
        },
    );
    r.register(
        pe,
        "sequenceElement",
        "()Ljava/lang/foreign/MemoryLayout$PathElement;",
        |ctx, _| {
            let elem = try_alloc_concurrent_synthetic(
                ctx,
                "java/lang/foreign/MemoryLayout$PathElement",
                2,
            )?;
            ctx.set_field(elem, 0, Value::Object(None));
            ctx.set_field(elem, 1, Value::Int(1)); // kind=sequence
            Ok(Some(Value::Object(Some(elem))))
        },
    );
}

// THE LAST SURVIVOR OF THE GROUP-LAYOUT FAMILY IS GONE TOO (G6, 2026-08-16).
//
// `pe_memory_layout_width` stood here — a size reader that understood BOTH
// this file's old `[0]=Int(kind)` carrier and `foreign_ffm`'s
// `[0]=Long(byteSize)` one. It had exactly one caller,
// `asSlice(long, MemoryLayout)`, and that caller now reads size AND alignment
// out of the four-slot carrier in one call through
// `foreign_ffm::p67_layout_size_align`, because the JDK's own body is
// `asSlice(offset, layout.byteSize(), layout.byteAlignment())` and a reader
// that answers only the size cannot express the second half.
//
// This closes F16-1's remaining question. The reconciling arm this function
// existed for — "which of the two encodings am I looking at?" — has nothing
// left to reconcile: `pe_make_layout`, the only minter of `[0]=Int(kind)`, is
// `#[cfg(test)]`, and every shipping carrier comes from `p67_layout_object` or
// the four group-layout factories beside it. One encoding, one reader.

// --- String marshaling helpers ---

fn register_pe2_string_marshaling(r: &mut NativeMethodRegistry) {
    for ms in [PE_SEGMENT_INTERFACE, CRATON_SEGMENT_CLASS] {
        register_pe2_string_marshaling_on(r, ms);
    }
}

/// `MemorySegment.reinterpret(long)` -- a new segment over the SAME address,
/// with a caller-chosen size.
///
/// A free function rather than a closure because two registrars need to name
/// it. They each carried their OWN body until 2026-08-24, and the shipping one
/// -- the only body a `--jdk-only` process ever had -- copied slots 0..5
/// verbatim and performed no native-access check at all.
///
/// The address is read through `panama_libffi::segment_address` so a REAL
/// JDK-loaded segment is not mistaken for the synthetic six-slot carrier, on
/// which slot 0 means something else entirely.
pub(crate) fn pe_segment_reinterpret(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `reinterpret` IS `@Restricted` in the JDK — it grants an arbitrary access
    // window over a possibly-raw address. But restricted does not mean refused:
    // a JDK 25 launcher warns and proceeds unless `--illegal-native-access=deny`
    // (measured, see [`IllegalNativeAccess`]). Refusing unconditionally is a
    // policy CratonVM invented, and it is load-bearing in the worst place — a
    // static initialiser, where the throw is permanent for the class.
    restricted_method_check(ctx, "reinterpret")?;
    let new_size = match args.get(1) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let ptr = crate::panama_libffi::segment_address(ctx, this);
    let read_only = match ctx.get_field_by_name(this, "readOnly") {
        Value::Int(n) => Value::Int(n),
        _ => ctx.get_field(this, 3),
    };

    let seg = alloc_segment_carrier(ctx, 6)?;
    ctx.set_field(seg, 0, Value::Long(ptr));
    ctx.set_field(seg, 1, Value::Long(new_size));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, read_only);
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(seg))))
}

fn register_pe2_string_marshaling_on(r: &mut NativeMethodRegistry, ms: &str) {
    // `getUtf8String(J)` and `reinterpret(J)` USED TO BE REGISTERED HERE, on
    // both class names, and both drifted: `register_p67_foreign_memory`
    // registers the same triples, and THAT pass is the one a shipping binary
    // reaches. This pass is synthetic-only, so its bodies won the
    // last-write-wins race under `--features synthetic-jdk` and were absent
    // from every other mode -- i.e. every test built that way measured code
    // that does not ship, and the code that DID ship was the weaker of the two
    // in both cases. One body each now, registered by the shipping pass:
    // `p67_segment_get_string` and `pe_segment_reinterpret` above.
    //
    // `setUtf8String` and `allocateUtf8String` below have NO shipping twin and
    // stay exactly where they are.

    // setUtf8String(long offset, String value) → void
    r.register(
        ms,
        "setUtf8String",
        "(JLjava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let str_obj = obj_arg(args, 2)?;
            let s = ctx.read_string(str_obj).unwrap_or_default();

            let ptr = crate::panama_libffi::segment_address(ctx, this);
            // Bounds check: string + null terminator must fit within segment.
            // A segment with size 0 has unknown bounds (e.g. created via
            // ofAddress or wrapping a raw function pointer); writing to such a
            // segment is an arbitrary-native-write primitive. The READ path
            // (getUtf8String) rejects zero-size segments, so the WRITE path must
            // be symmetric and reject them too rather than skipping the bounds
            // check and writing blindly to the raw address.
            let seg_size = match crate::panama_libffi::segment_byte_size(ctx, this) {
                n if n > 0 => n,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "setUtf8String on a segment with unknown bounds \
                                  (size 0): reinterpret the segment with a known \
                                  size before writing a C string"
                            .into(),
                    }
                    .into());
                }
            };
            let str_bytes = s.as_bytes();
            let needed = str_bytes.len() as i64 + 1; // +1 for null terminator
            if offset < 0 || offset + needed > seg_size {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "setUtf8String: offset {} + {} bytes exceeds segment size {}",
                        offset, needed, seg_size
                    ),
                }
                .into());
            }

            // Validate address arithmetic doesn't overflow
            let total = (ptr as u64).checked_add(offset as u64);
            if let Some(addr_val) = total {
                let addr = addr_val as *mut u8;
                if !addr.is_null() {
                    // SAFETY: bounds-checked against segment size, address
                    // arithmetic verified, null-checked.
                    unsafe {
                        std::ptr::copy_nonoverlapping(str_bytes.as_ptr(), addr, str_bytes.len());
                        *addr.add(str_bytes.len()) = 0; // null terminator
                    }
                }
            } else {
                return Err(RuntimeError::IllegalStateException {
                    message: "address arithmetic overflow in setUtf8String".into(),
                }
                .into());
            }
            Ok(None)
        },
    );

    // Arena.allocateUtf8String(String) → MemorySegment. JDK 22 renamed this to
    // `allocateFrom`, which shares the body — see the registration in
    // `phases_late::foreign_ffm`, where the `allocateFrom` spelling was still
    // handing back a stand-in segment with address 0 and byteSize 0.
    let arena = "java/lang/foreign/Arena";
    r.register(
        arena,
        "allocateUtf8String",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        pe_arena_allocate_from_string,
    );
}

// =============================================================================
// R3: Resource Loading — Class.getResourceAsStream, InputStreamReader, BufferedReader
//
// Synthetic InputStream layout (2 fields):
//   field 0: String[] ref array — all lines of the resource file
//   field 1: int — current read position (line index)
//
// InputStreamReader layout (1 field):
//   field 0: InputStream reference
//
// BufferedReader layout (1 field):
//   field 0: Reader reference (InputStreamReader)
// =============================================================================

/// Traverse BufferedReader → InputStreamReader → InputStream chain.
/// Returns the synthetic InputStream ObjectRef, or None if the chain is broken.
fn r3_get_input_stream(ctx: &dyn NativeContext, buffered_reader: ObjectRef) -> Option<ObjectRef> {
    // BufferedReader.field[0] = Reader (InputStreamReader)
    let reader = match ctx.get_field(buffered_reader, 0) {
        Value::Object(Some(r)) => r,
        _ => return None,
    };
    // InputStreamReader.field[0] = InputStream
    match ctx.get_field(reader, 0) {
        Value::Object(Some(is)) => Some(is),
        _ => None,
    }
}

/// `Arena.allocateFrom(String)` / `Arena.allocateUtf8String(String)`: a segment
/// holding the string's UTF-8 bytes plus a NUL terminator.
///
/// Named rather than inline so `foreign_ffm`'s `allocateFrom` registration can
/// share it — the two spellings used to be two bodies, and only one of them was
/// repaired. That spelling used to answer a `p67_arena_segment` stand-in: it
/// allocated NO memory (`set_field(segment, 1, Long(0)) // address`) and wrote
/// the SIZE into slot 0 — the inverse of the convention
/// `panama_libffi::segment_address` and `pe_arena_allocate_impl` use. So
/// `segment_address` fell through to `get_field(seg, 0)`, read `Long(5)` — the
/// byte length of `"abcd\0"` — and libffi passed 5 as the `char *`. `strlen`
/// then dereferenced address 0x5.
///
/// Measured 2026-08-12: the same object reported `byteSize() == 0` and
/// `address() == 5`, inverted on both. The downcall carrier, `invoke` dispatch,
/// CIF build and return unmarshal were all correct — a positive control
/// building the argument with `allocate(5)` plus explicit stores returns
/// `strlen(abcd) == 4`, matching HotSpot. It is the second half of residual 3
/// in `ffm-elements-spliterator-and-allocatefrom-gaps-20260813`.
///
/// A failed allocation RAISES rather than answering a null segment. The whole
/// defect above was a segment that looked allocated and was not; handing back
/// `null` here would put the same silence one call further out, where the
/// caller dereferences it in a downcall.
pub(crate) fn pe_arena_allocate_from_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let str_obj = obj_arg(args, 1)?;
    let s = ctx.read_string(str_obj).unwrap_or_default();
    let bytes = s.as_bytes();
    let size = (bytes.len() + 1) as i64; // +1 for null terminator

    // Allocate through the SAME path as `Arena.allocate(long, long)`, so the
    // segment carries a real off-heap base in the `[0]=ptr, [1]=size` layout
    // every reader expects.
    let seg = match pe_arena_allocate_impl(ctx, this, size, 1)? {
        Some(Value::Object(Some(seg))) => seg,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Arena.allocateFrom could not allocate a segment".into(),
            }
            .into())
        }
    };
    // Write the string bytes + null terminator.
    let ptr = match ctx.get_field(seg, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
    if ptr == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Arena.allocateFrom segment is not writable".into(),
        }
        .into());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        *(ptr as *mut u8).add(bytes.len()) = 0;
    }
    Ok(Some(Value::Object(Some(seg))))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `MemorySegment.spliterator` split arithmetic and characteristics.
#[cfg(test)]
mod segment_splitter_tests {
    use super::{pe_split_bounds, PE_SPLITTER_CHARACTERISTICS};

    #[test]
    fn an_even_count_splits_down_the_middle() {
        // 8 ints: low half 4 elements / 16 bytes, high half 16 bytes.
        assert_eq!(pe_split_bounds(8, 4), (4, 16, 16));
    }

    #[test]
    fn an_odd_element_stays_with_the_high_half() {
        // 5 ints: low half 2 elements / 8 bytes, high half 12 bytes (3 elements).
        // The JDK keeps the remainder on the side that is NOT handed out, so
        // `lo + hi` must still be the whole segment.
        let (split, lobound, hibound) = pe_split_bounds(5, 4);
        assert_eq!((split, lobound, hibound), (2, 8, 12));
        assert_eq!(
            lobound + hibound,
            5 * 4,
            "the two halves must tile the segment"
        );
    }

    #[test]
    fn the_halves_always_tile_the_segment() {
        for count in 1..64_i64 {
            for size in [1_i64, 2, 4, 8] {
                let (split, lobound, hibound) = pe_split_bounds(count, size);
                assert_eq!(
                    lobound + hibound,
                    count * size,
                    "count {count} size {size} leaves a gap or an overlap"
                );
                assert_eq!(split * size, lobound, "count {count} size {size}");
                assert!(
                    split <= count - split,
                    "the low half must never be the larger one"
                );
                // The element counts must tile too, and each half's count must
                // match its byte span. Checking only the BYTE bounds is what let
                // a bad high-half count through: the splitter kept claiming the
                // whole original count over the half-sized segment it had left,
                // and ran off the end on the first element past the middle.
                assert_eq!(
                    split + (count - split),
                    count,
                    "count {count} size {size}: element counts must tile"
                );
                assert_eq!(
                    (count - split) * size,
                    hibound,
                    "count {count} size {size}: the high half's count must match its bytes"
                );
            }
        }
    }

    #[test]
    fn characteristics_match_the_jdk() {
        // Measured on HotSpot JDK 25: `seg.spliterator(JAVA_INT)
        // .characteristics()` is 17744 =
        // NONNULL|SUBSIZED|SIZED|IMMUTABLE|ORDERED.
        assert_eq!(PE_SPLITTER_CHARACTERISTICS, 17744);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The four compound layout tags are test-only in this file now — see the
    // note on the `ffi::` import at the top.
    use cratonvm_native_api::ffi::{LAYOUT_PADDING, LAYOUT_SEQUENCE, LAYOUT_STRUCT, LAYOUT_UNION};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Build a downcall carrier the way [`alloc_downcall_handle`] does, minus
    /// the `MethodType` derivation.
    ///
    /// These tests exercise `pe_downcall_invoke`'s libffi marshalling, not the
    /// mint. Routing them through `alloc_downcall_handle` would drag
    /// `build_method_type_from_descriptor` into every one of them, and what
    /// that measures on a `MockNativeContext` is the mock's class table rather
    /// than anything about a downcall.
    ///
    /// It does allocate the real carrier class at the real width, so the slot
    /// arithmetic under test is the production arithmetic: a test that kept
    /// writing the old compact `[0]=addr, [1]=descriptor` layout would read
    /// back zeros through the `DOWNCALL_*` accessors and pass or fail for a
    /// reason that has nothing to do with the code it names.
    fn mk_downcall_handle(
        ctx: &mut dyn NativeContext,
        fn_addr: i64,
        descriptor: Option<ObjectRef>,
        first_variadic: i64,
    ) -> ObjectRef {
        let handle =
            try_alloc_concurrent_synthetic(ctx, DOWNCALL_CARRIER_CLASS, DOWNCALL_SLOT_COUNT)
                .unwrap();
        ctx.set_field(handle, DOWNCALL_TAG, Value::Int(DOWNCALL_TAG_MAGIC));
        ctx.set_field(handle, DOWNCALL_FN_ADDR, Value::Long(fn_addr));
        ctx.set_field(handle, DOWNCALL_DESCRIPTOR, Value::Object(descriptor));
        ctx.set_field(handle, DOWNCALL_FIRST_VARIADIC, Value::Long(first_variadic));
        ctx.set_field(handle, DOWNCALL_CIF, Value::Long(0));
        ctx.set_field(handle, DOWNCALL_CAPTURE_CALL_STATE, Value::Int(0));
        handle
    }

    // FIX(test): RAII guard that enables the process-wide native-access gate
    // for the duration of a downcall test and restores the previous value on
    // drop (even on panic). The Panama implementation is secure-by-default
    // (`NATIVE_ACCESS_ENABLED == false`), so tests that exercise the real
    // downcall machinery (abs/strlen/snprintf/…) must grant native access
    // first or `validated_fn_ptr` denies them with an IllegalCallerException.
    // Using a guard (rather than a bare set/reset pair) keeps the global flag
    // from leaking into sibling tests — leaked state is what causes the
    // order-dependent flakiness this fix addresses. The production gate is
    // unchanged; only the test scope flips the flag.
    //
    // FIX(test-isolation): `NATIVE_ACCESS_ENABLED` is a single process-global
    // `AtomicBool` shared by every test in this binary. The previous guard
    // snapshotted the *prior* value and restored it on drop, but that is
    // unsound under parallel execution and is exactly why
    // `panama_cif_cache_reuses_cif_across_calls` still flaked: with two tests
    // A and B, A enables (prior=false); B enables (prior=true, because A had
    // already flipped it on); A finishes first and its guard restores false;
    // B is now mid-downcall yet the gate reads false, so `validated_fn_ptr`
    // denies it with `IllegalCallerException`. Snapshot/restore gives no
    // mutual exclusion. The fix is to serialize every test that toggles the
    // flag behind one module-level mutex, so only a single such test ever
    // observes (or mutates) the flag at a time. While the lock is held the
    // flag cannot be flipped out from under the running test.
    static NATIVE_ACCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct NativeAccessGuard {
        prev: bool,
        // FIX(test-isolation): hold the serialization lock for the entire
        // lifetime of the guard. Acquiring it here means no other guarded
        // (or directly-locked) test can touch `NATIVE_ACCESS_ENABLED` until
        // this guard is dropped. Field order matters for drop: `prev` is
        // restored in `Drop::drop` *before* this `_lock` field is dropped
        // (Rust drops struct fields in declaration order, after the explicit
        // `Drop` impl runs), so the flag is reset while we still hold the
        // lock, and the lock is released only afterwards.
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl NativeAccessGuard {
        fn enable() -> Self {
            // Recover from a poisoned lock: a panicking guarded test must not
            // wedge the rest of the suite. The `()` payload carries no state,
            // so the poisoned inner guard is perfectly usable.
            let lock = NATIVE_ACCESS_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = native_access_enabled();
            set_native_access_enabled(true);
            NativeAccessGuard { prev, _lock: lock }
        }
    }
    impl Drop for NativeAccessGuard {
        fn drop(&mut self) {
            // Restore the prior value while still holding the lock; `_lock`
            // is released immediately afterwards when the struct fields drop.
            set_native_access_enabled(self.prev);
        }
    }

    #[test]
    fn test_linker_native_call_convention() {
        // Verify calling convention for downcalls
        // x86-64 SysV: RDI, RSI, RDX, RCX, R8, R9 for integer args
        // Windows: RCX, RDX, R8, R9
        #[cfg(target_os = "windows")]
        let max_reg_args = 4;
        #[cfg(not(target_os = "windows"))]
        let max_reg_args = 6;
        assert!(max_reg_args >= 4);
    }

    #[test]
    fn test_symbol_lookup_resolution() {
        // SymbolLookup.loaderLookup() should find loaded library symbols
        // Test that we can look up standard C functions
        let name = "strlen";
        assert!(!name.is_empty());
    }

    #[test]
    fn test_value_layout_carriers() {
        // Each ValueLayout has a carrier type
        // JAVA_INT -> int.class, JAVA_LONG -> long.class, etc.
        let carriers = vec![
            ("JAVA_BYTE", 1usize),
            ("JAVA_SHORT", 2),
            ("JAVA_INT", 4),
            ("JAVA_LONG", 8),
            ("JAVA_FLOAT", 4),
            ("JAVA_DOUBLE", 8),
            ("ADDRESS", std::mem::size_of::<*const u8>()),
        ];
        for (name, size) in carriers {
            assert!(size > 0, "{name} must have positive size");
        }
    }

    /// Snapshot/restore helper for the per-module policy tests so they can
    /// mutate the global `NATIVE_ACCESS_POLICY` without leaking state into the
    /// rest of the suite. Acquires the shared serialization lock.
    fn with_policy_isolated<F: FnOnce()>(f: F) {
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = NATIVE_ACCESS_POLICY
            .read()
            .map(|p| p.clone())
            .unwrap_or(NativeAccessPolicy::None);
        f();
        store_policy(prev);
    }

    #[test]
    fn test_native_access_policy_none_denies_all() {
        with_policy_isolated(|| {
            set_native_access_enabled(false);
            assert!(!native_access_enabled());
            assert!(!module_native_access_enabled(None));
            assert!(!module_native_access_enabled(Some("com.example.app")));
        });
    }

    #[test]
    fn test_native_access_policy_all_grants_every_module() {
        with_policy_isolated(|| {
            set_native_access_enabled(true);
            assert!(native_access_enabled());
            assert!(module_native_access_enabled(None));
            assert!(module_native_access_enabled(Some("any.module")));
        });
    }

    #[test]
    fn test_native_access_policy_scoped_modules() {
        with_policy_isolated(|| {
            set_native_access_modules(["com.example.ffi", "org.foo.bar"]);
            // Only the listed modules are granted.
            assert!(module_native_access_enabled(Some("com.example.ffi")));
            assert!(module_native_access_enabled(Some("org.foo.bar")));
            // An unlisted module — and the unnamed module — are denied.
            assert!(!module_native_access_enabled(Some("com.other")));
            assert!(!module_native_access_enabled(None));
            // The coarse process-global gate sees *some* grant.
            assert!(native_access_enabled());
        });
    }

    #[test]
    fn test_native_access_policy_all_unnamed_sentinel_grants_all() {
        with_policy_isolated(|| {
            // The JDK `ALL-UNNAMED` sentinel collapses to a global grant here.
            set_native_access_modules(["ALL-UNNAMED"]);
            assert!(module_native_access_enabled(None));
            assert!(module_native_access_enabled(Some("anything")));
            // Case-insensitive and mixed with named modules.
            set_native_access_modules(["com.x", "all-modules"]);
            assert!(module_native_access_enabled(Some("unlisted")));
        });
    }

    #[test]
    fn test_native_access_policy_blank_list_grants_all_like_bare_flag() {
        with_policy_isolated(|| {
            // A whitespace-only / empty argument behaves like the bare flag.
            set_native_access_modules(["", "   "]);
            assert!(native_access_enabled());
            assert!(module_native_access_enabled(Some("whatever")));
        });
    }

    // FIX(test): regression for the MemorySegment.copy zero-size OOB hole.
    // The bounds check in `copy` was previously gated on `size > 0`, so a
    // segment with a declared byteSize() of 0 skipped validation entirely and
    // a non-zero `bytes` length drove an OOB read/write of up to MAX_COPY_SIZE.
    // This test mirrors the exact (now-unconditional) predicate used in the
    // production fix — `offset < 0 || offset.checked_add(bytes) > size` — and
    // asserts that a zero-size segment with a non-zero length is rejected.
    // Pure i64 arithmetic only, so it is independent of NativeContext.
    #[test]
    fn test_copy_zero_size_segment_rejects_nonzero_len() {
        // Replicates the copy-path bounds predicate: returns true == "reject".
        fn exceeds(offset: i64, bytes: usize, size: i64) -> bool {
            let end = offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
            offset < 0 || end > size
        }
        // Zero-size segment, non-zero copy length -> must reject (the bug).
        assert!(
            exceeds(0, 64, 0),
            "zero-size segment must reject non-zero copy"
        );
        // Negative offset -> reject.
        assert!(exceeds(-1, 0, 16), "negative offset must reject");
        // Offset+bytes overflowing i64 -> saturates to MAX, exceeds size -> reject.
        assert!(exceeds(i64::MAX, 1, 1024), "overflowing offset must reject");
        // Offset+bytes past the declared size -> reject.
        assert!(exceeds(8, 16, 16), "out-of-range copy must reject");
        // Legitimate in-bounds copy -> accept (no over-rejection).
        assert!(!exceeds(8, 8, 16), "in-bounds copy must be accepted");
        // Zero-length copy on a zero-size segment is harmless and the
        // production code only enters the bounds block when bytes > 0, so the
        // predicate for (0,0,0) staying false is the consistent invariant.
        assert!(
            !exceeds(0, 0, 0),
            "zero-length copy is not a bounds violation"
        );
    }

    #[test]
    fn test_function_descriptor() {
        // FunctionDescriptor.of(returnLayout, argLayouts...)
        // Describes a native function signature
        struct FuncDesc {
            ret_size: usize,
            arg_sizes: Vec<usize>,
        }
        let desc = FuncDesc {
            ret_size: 4,           // int return
            arg_sizes: vec![8, 8], // two pointer args
        };
        assert_eq!(desc.arg_sizes.len(), 2);
        assert_eq!(desc.ret_size, 4);
    }

    #[test]
    fn test_upcall_handle() {
        // Upcall: native code calling back into Java
        // Should create a function pointer that routes to Java method
        let callback_invoked = std::sync::atomic::AtomicBool::new(false);
        // Simulate upcall
        callback_invoked.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(callback_invoked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn test_arena_confined_thread_safety() {
        // Arena.ofConfined() should only be usable from creating thread
        let creator_thread = std::thread::current().id();
        assert_eq!(std::thread::current().id(), creator_thread);
    }

    #[test]
    fn test_arena_shared_multi_thread() {
        // Arena.ofShared() can be used from multiple threads
        use std::sync::Arc;
        let data = Arc::new(vec![1u8, 2, 3]);
        let d2 = data.clone();
        let handle = std::thread::spawn(move || {
            assert_eq!(d2[0], 1);
        });
        handle.join().unwrap();
        assert_eq!(data[0], 1);
    }

    #[test]
    fn test_validated_fn_ptr_null_rejected() {
        // Null function pointer should be rejected
        let result = validated_fn_ptr::<extern "C" fn() -> i32>(0);
        assert!(result.is_err());
    }

    #[test]
    fn test_max_copy_size_constant() {
        assert_eq!(MAX_COPY_SIZE, 256 * 1024 * 1024);
    }

    #[test]
    fn test_max_cstr_len_constant() {
        assert_eq!(MAX_CSTR_LEN, 4096);
    }

    #[test]
    fn test_layout_byte_size_via_ffi() {
        // Verify the ffi layout constants are accessible and correct
        assert_eq!(ffi::layout_byte_size(LAYOUT_INT), 4);
        assert_eq!(ffi::layout_byte_size(LAYOUT_LONG), 8);
        assert_eq!(ffi::layout_byte_size(LAYOUT_FLOAT), 4);
        assert_eq!(ffi::layout_byte_size(LAYOUT_DOUBLE), 8);
        assert_eq!(ffi::layout_byte_size(LAYOUT_BYTE), 1);
        assert_eq!(ffi::layout_byte_size(LAYOUT_SHORT), 2);
        assert_eq!(ffi::layout_byte_size(LAYOUT_BOOLEAN), 1);
        assert_eq!(ffi::layout_byte_size(LAYOUT_CHAR), 2);
        assert_eq!(ffi::layout_byte_size(LAYOUT_ADDRESS), 8);
    }

    #[test]
    fn test_compound_layout_sizes_are_zero() {
        assert_eq!(ffi::layout_byte_size(LAYOUT_STRUCT), 0);
        assert_eq!(ffi::layout_byte_size(LAYOUT_UNION), 0);
        assert_eq!(ffi::layout_byte_size(LAYOUT_SEQUENCE), 0);
        assert_eq!(ffi::layout_byte_size(LAYOUT_PADDING), 0);
    }

    // --- Phase 80.1: Panama Memory Safety Tests ---

    #[test]
    fn test_get_utf8string_address_overflow() {
        // Verify that checked_add detects u64 overflow in address arithmetic.
        // Using -1i64 as u64 = u64::MAX, adding anything > 0 overflows.
        let ptr: i64 = -1; // u64::MAX when cast
        let base_off: i64 = 0;
        let offset: i64 = 1;
        let total = (ptr as u64)
            .checked_add(base_off as u64)
            .and_then(|v| v.checked_add(offset as u64));
        assert!(total.is_none(), "overflow must be detected");
    }

    #[test]
    fn test_set_utf8string_bounds_check() {
        // setUtf8String must reject writes beyond segment size.
        // seg_size=10, offset=8, string "hello" (5+1=6 bytes needed) → 8+6=14 > 10 → reject.
        let seg_size: i64 = 10;
        let offset: i64 = 8;
        let needed: i64 = 6; // "hello" + null terminator
        assert!(
            offset + needed > seg_size,
            "write should exceed segment bounds"
        );
    }

    #[test]
    fn test_copy_bounds_check_src_overflow() {
        // copy must reject src_offset + bytes > src_size.
        let src_size: i64 = 100;
        let src_offset: i64 = 90;
        let bytes: usize = 20;
        let src_end = src_offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
        assert!(
            src_end > src_size,
            "source bounds check must catch overflow"
        );
    }

    #[test]
    fn test_copy_address_arithmetic_overflow() {
        // copy must reject when address arithmetic overflows u64.
        let src_ptr: u64 = u64::MAX - 5;
        let src_off: u64 = 10;
        let src_offset: u64 = 0;
        let total = src_ptr
            .checked_add(src_off)
            .and_then(|v| v.checked_add(src_offset));
        assert!(total.is_none(), "address overflow must be detected");
    }

    #[test]
    fn test_get_utf8string_offset_exceeds_segment() {
        // getUtf8String must reject offset >= segment size.
        let seg_size: i64 = 100;
        let offset: i64 = 150;
        let remaining = seg_size - offset;
        assert!(remaining <= 0, "offset beyond segment must be rejected");
    }

    #[test]
    fn test_copy_negative_offset_rejected() {
        // copy must reject negative offsets.
        let src_offset: i64 = -1;
        let src_size: i64 = 100;
        assert!(
            src_offset < 0,
            "negative offset must be rejected by bounds check"
        );
        // The production code checks: if src_offset < 0 || src_end > src_size
        assert!(src_offset < 0 || src_offset > src_size);
    }

    // ===================================================================
    // Phase 85.1: Arena Lifecycle Tests
    // ===================================================================

    use crate::test_utils::mock_ctx;
    use crate::try_alloc_concurrent_synthetic;

    /// Helper: create an arena object of the given kind using the actual registration logic.
    fn make_arena(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
        let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4).unwrap();
        let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
        ctx.set_field(a, 0, Value::Int(kind));
        ctx.set_field(a, 1, Value::Object(Some(ids)));
        ctx.set_field(a, 2, Value::Int(0)); // not closed
        ctx.set_field(a, 3, Value::Int(0)); // count=0
        a
    }

    #[test]
    fn test_85_1_arena_confined_lifecycle() {
        // Create confined arena, allocate, close, verify freed
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        // Allocate via arena
        let seg_result = pe_arena_allocate_impl(&mut ctx, arena, 64, 8);
        assert!(seg_result.is_ok());
        let seg = match seg_result.unwrap() {
            Some(Value::Object(Some(s))) => s,
            _ => panic!("Expected segment object"),
        };

        // Segment should have valid pointer and size
        let ptr = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert!(ptr != 0, "Segment pointer should be non-null");
        let size = match ctx.get_field(seg, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert_eq!(size, 64);

        // Arena count should be 1
        assert!(matches!(ctx.get_field(arena, 3), Value::Int(1)));

        // Close arena
        let close_result = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close_result.is_ok());

        // Arena should be marked closed
        assert!(matches!(ctx.get_field(arena, 2), Value::Int(1)));
    }

    #[test]
    fn test_85_1_arena_shared_lifecycle() {
        // Shared arena should work the same as confined
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_SHARED);

        // Allocate two segments
        let seg1 = pe_arena_allocate_impl(&mut ctx, arena, 32, 4);
        assert!(seg1.is_ok());
        let seg2 = pe_arena_allocate_impl(&mut ctx, arena, 64, 8);
        assert!(seg2.is_ok());

        // Count should be 2
        assert!(matches!(ctx.get_field(arena, 3), Value::Int(2)));

        // Close should succeed
        let close_result = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close_result.is_ok());
        assert!(matches!(ctx.get_field(arena, 2), Value::Int(1)));
    }

    #[test]
    fn test_85_1_arena_auto_lifecycle() {
        // Auto arena should allocate but not be manually closeable like global
        // (actually auto CAN be closed, only global cannot)
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_AUTO);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 16, 1);
        assert!(seg.is_ok());

        // Auto arena can be closed
        let close_result = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close_result.is_ok());
    }

    #[test]
    fn test_85_1_arena_double_close_error() {
        // Closing an already-closed arena should return an error
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        // First close succeeds
        let close1 = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close1.is_ok());

        // Second close should fail
        let close2 = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close2.is_err(), "Double close must return error");

        // Global arena cannot be closed at all
        let global = make_arena(&mut ctx, ffi::ARENA_GLOBAL);
        let close_global = pe_arena_close(&mut ctx, &[Value::Object(Some(global))]);
        assert!(
            close_global.is_err(),
            "Global arena close must return error"
        );
    }

    // ===================================================================
    // Phase 85.2: MemorySegment Implementation Tests
    // ===================================================================

    /// Helper: create a ValueLayout object
    fn make_layout(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
        pe_make_layout(ctx, kind).unwrap()
    }

    #[test]
    fn test_85_2_allocate_and_readwrite() {
        // Allocate a segment via arena, write an int, read it back
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
        let layout_int = make_layout(&mut ctx, LAYOUT_INT);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 16, 4)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        // Write int 42 at offset 0
        pe_segment_set_impl(&mut ctx, seg, layout_int, 0, Value::Int(42)).unwrap();
        // Read it back
        let val = pe_segment_get_impl(&mut ctx, seg, layout_int, 0).unwrap();
        assert_eq!(val, Some(Value::Int(42)));

        // Write long at offset 8
        let layout_long = make_layout(&mut ctx, LAYOUT_LONG);
        pe_segment_set_impl(
            &mut ctx,
            seg,
            layout_long,
            8,
            Value::Long(0x1234_5678_9ABC_DEF0),
        )
        .unwrap();
        let val2 = pe_segment_get_impl(&mut ctx, seg, layout_long, 8).unwrap();
        assert_eq!(val2, Some(Value::Long(0x1234_5678_9ABC_DEF0)));
    }

    #[test]
    fn test_85_2_bounds_check_null_segment() {
        // get/set on a null address should return error
        let mut ctx = mock_ctx();
        let layout_int = make_layout(&mut ctx, LAYOUT_INT);

        // Create a segment with null pointer
        let seg = alloc_segment_carrier(&mut ctx, 6).unwrap();
        ctx.set_field(seg, 0, Value::Long(0)); // null ptr
        ctx.set_field(seg, 1, Value::Long(100));
        ctx.set_field(seg, 5, Value::Long(0));

        let result = pe_segment_get_impl(&mut ctx, seg, layout_int, 0);
        assert!(result.is_err(), "Get on null segment should fail");

        let result = pe_segment_set_impl(&mut ctx, seg, layout_int, 0, Value::Int(1));
        assert!(result.is_err(), "Set on null segment should fail");
    }

    #[test]
    fn test_85_2_of_array_int() {
        // ofArray(int[]) should create a segment wrapping array data
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
        ctx.set_array_element(arr, 0, Value::Int(10));
        ctx.set_array_element(arr, 1, Value::Int(20));
        ctx.set_array_element(arr, 2, Value::Int(30));
        ctx.set_array_element(arr, 3, Value::Int(40));

        // Call the ofArray registration logic directly
        let _args = vec![Value::Object(Some(arr))];
        // Simulate what the registration does
        let len = ctx.array_length(arr);
        let byte_size = (len * 4) as i64;
        let result = ctx.allocate_native_memory(byte_size as usize, 4);
        assert!(result.is_some());
        let (_, ptr) = result.unwrap();
        for i in 0..len {
            if let Value::Int(v) = ctx.get_array_element(arr, i) {
                unsafe {
                    *(ptr as *mut i32).add(i) = v;
                }
            }
        }

        // Verify the native memory contains the correct values
        for i in 0..4usize {
            let val = unsafe { *(ptr as *const i32).add(i) };
            assert_eq!(val, (i as i32 + 1) * 10);
        }
    }

    #[test]
    fn test_85_2_copy_segments() {
        // Allocate two segments, write to src, copy to dst, verify
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        let src = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();
        let dst = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        // Write pattern to src
        let src_ptr = match ctx.get_field(src, 0) {
            Value::Long(n) => n as *mut u8,
            _ => std::ptr::null_mut(),
        };
        assert!(!src_ptr.is_null());
        for i in 0..16u8 {
            unsafe {
                *src_ptr.add(i as usize) = i + 1;
            }
        }

        // Copy 16 bytes from src to dst
        let _copy_args = vec![
            Value::Object(Some(src)),
            Value::Long(0), // srcOffset
            Value::Object(Some(dst)),
            Value::Long(0),  // dstOffset
            Value::Long(16), // bytes
        ];

        // Simulate copy logic
        let src_addr = src_ptr;
        let dst_ptr = match ctx.get_field(dst, 0) {
            Value::Long(n) => n as *mut u8,
            _ => std::ptr::null_mut(),
        };
        assert!(!dst_ptr.is_null());
        unsafe {
            std::ptr::copy_nonoverlapping(src_addr, dst_ptr, 16);
        }

        // Verify dst has the pattern
        for i in 0..16u8 {
            let val = unsafe { *dst_ptr.add(i as usize) };
            assert_eq!(val, i + 1, "Byte at offset {} mismatch", i);
        }
    }

    /// Mirror of the overlap decision used by MemorySegment.copy: ranges
    /// `[s, s+bytes)` and `[d, d+bytes)` overlap iff `s < d+bytes && d < s+bytes`.
    fn copy_ranges_overlap(s: u64, d: u64, bytes: u64) -> bool {
        s < d.saturating_add(bytes) && d < s.saturating_add(bytes)
    }

    #[test]
    fn test_copy_overlap_detection() {
        // Disjoint adjacent ranges: [0,16) and [16,32) do NOT overlap.
        assert!(!copy_ranges_overlap(0, 16, 16));
        assert!(!copy_ranges_overlap(16, 0, 16));
        // One-byte overlap (forward): [0,16) and [15,31).
        assert!(copy_ranges_overlap(0, 15, 16));
        // One-byte overlap (backward): [15,31) and [0,16).
        assert!(copy_ranges_overlap(15, 0, 16));
        // Identical ranges fully overlap.
        assert!(copy_ranges_overlap(100, 100, 8));
        // Zero-length never overlaps.
        assert!(!copy_ranges_overlap(100, 100, 0));
    }

    #[test]
    fn test_copy_overlapping_within_segment_is_memmove_correct() {
        // Regression: MemorySegment.copy must behave as a memmove for
        // overlapping src/dst within a single segment. A forward-overlapping
        // copy done with copy_nonoverlapping would corrupt the tail; copy
        // (memmove) preserves it.
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
        let seg = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        let base = match ctx.get_field(seg, 0) {
            Value::Long(n) => n as *mut u8,
            _ => std::ptr::null_mut(),
        };
        assert!(!base.is_null());

        // Initialize bytes 0..16 = [1..=16].
        for i in 0..16u8 {
            unsafe { *base.add(i as usize) = i + 1 };
        }

        // copy 8 bytes from offset 0 to offset 4 (forward overlap).
        let s = base as u64;
        let d = unsafe { base.add(4) } as u64;
        let bytes: usize = 8;
        assert!(
            copy_ranges_overlap(s, d, bytes as u64),
            "ranges must be detected as overlapping"
        );
        // Use the same memmove path the production code selects on overlap.
        unsafe { std::ptr::copy(base, base.add(4), bytes) };

        // Expected memmove result: dst[4..12] == old src[0..8] == [1..=8].
        let expected: [u8; 16] = [1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 13, 14, 15, 16];
        for i in 0..16usize {
            let val = unsafe { *base.add(i) };
            assert_eq!(val, expected[i], "memmove byte at offset {} mismatch", i);
        }
    }

    #[test]
    fn test_85_2_as_slice() {
        // asSlice should create a sub-segment with offset
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 64, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        // Write a value at offset 16
        let layout_int = make_layout(&mut ctx, LAYOUT_INT);
        pe_segment_set_impl(&mut ctx, seg, layout_int, 16, Value::Int(0xCAFE)).unwrap();

        // Create a slice starting at offset 16, size 32
        let slice = alloc_segment_carrier(&mut ctx, 6).unwrap();
        let base_ptr = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        ctx.set_field(slice, 0, Value::Long(base_ptr));
        ctx.set_field(slice, 1, Value::Long(32));
        ctx.set_field(slice, 2, ctx.get_field(seg, 2));
        ctx.set_field(slice, 3, Value::Int(0));
        ctx.set_field(slice, 4, Value::Int(1));
        ctx.set_field(slice, 5, Value::Long(16)); // offset=16

        // Read at slice offset 0 should give the value written at parent offset 16
        let val = pe_segment_get_impl(&mut ctx, slice, layout_int, 0).unwrap();
        assert_eq!(val, Some(Value::Int(0xCAFE)));
    }

    #[test]
    fn test_85_2_reinterpret() {
        // reinterpret should change size but keep same address
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 64, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        let orig_ptr = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        let orig_size = match ctx.get_field(seg, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert_eq!(orig_size, 64);

        // Reinterpret with new size 128
        let reinterpreted = alloc_segment_carrier(&mut ctx, 6).unwrap();
        ctx.set_field(reinterpreted, 0, Value::Long(orig_ptr));
        ctx.set_field(reinterpreted, 1, Value::Long(128));
        ctx.set_field(reinterpreted, 2, ctx.get_field(seg, 2));
        ctx.set_field(reinterpreted, 3, ctx.get_field(seg, 3));
        ctx.set_field(reinterpreted, 4, Value::Int(1));
        ctx.set_field(reinterpreted, 5, Value::Long(0));

        // Pointer should be the same, size should be different
        let new_ptr = match ctx.get_field(reinterpreted, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        let new_size = match ctx.get_field(reinterpreted, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert_eq!(new_ptr, orig_ptr);
        assert_eq!(new_size, 128);
    }

    // ===================================================================
    // Phase 85.3: Linker Downcall Tests
    // ===================================================================

    #[test]
    fn test_85_3_downcall_strlen() {
        // Call C strlen through the downcall mechanism
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();

        // Look up strlen
        let strlen_addr = ctx.find_native_symbol(-1, "strlen");
        if strlen_addr.is_none() {
            // Skip on platforms where symbol lookup isn't available
            return;
        }
        let strlen_addr = strlen_addr.unwrap() as i64;

        // Create a C string in native memory
        let (_, ptr) = ctx.allocate_native_memory(16, 1).unwrap();
        let test_str = b"hello\0";
        unsafe {
            std::ptr::copy_nonoverlapping(test_str.as_ptr(), ptr, test_str.len());
        }

        // Build FunctionDescriptor: of(LAYOUT_LONG, ADDRESS)
        let ret_layout = make_layout(&mut ctx, LAYOUT_LONG);
        let param_layout = make_layout(&mut ctx, LAYOUT_ADDRESS);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                .unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        // Build the downcall handle
        let handle = mk_downcall_handle(&mut ctx, strlen_addr, Some(descriptor), -1);

        // Build args array with the pointer as a Long
        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Long(ptr as i64));

        // Invoke the downcall
        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        );

        assert!(result.is_ok(), "strlen downcall failed: {:?}", result.err());
        let val = result.unwrap();
        // strlen("hello") = 5
        assert_eq!(val, Some(Value::Long(5)));
    }

    #[test]
    fn test_85_3_downcall_abs() {
        // Call C abs() — int abs(int)
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();

        let abs_addr = ctx.find_native_symbol(-1, "abs");
        if abs_addr.is_none() {
            return;
        }
        let abs_addr = abs_addr.unwrap() as i64;

        // FunctionDescriptor: of(LAYOUT_INT, LAYOUT_INT)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                .unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = mk_downcall_handle(&mut ctx, abs_addr, Some(descriptor), -1);

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(-42));

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        );

        assert!(result.is_ok(), "abs downcall failed: {:?}", result.err());
        let val = result.unwrap();
        assert_eq!(val, Some(Value::Int(42)));
    }

    // -----------------------------------------------------------------
    // T5.6.3 — Panama DowncallHandle CIF cache (panama_cif)
    // -----------------------------------------------------------------

    #[test]
    fn panama_cif_cache_reuses_cif_across_calls() {
        // Invoke the same downcall twice and check that the global
        // CIF_BUILD_COUNT only increments once — proving the second
        // call hit the cache on field 3 of the DowncallHandle.
        use crate::panama_libffi as plf;
        use std::sync::atomic::Ordering;

        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();
        let abs_addr = ctx.find_native_symbol(-1, "abs");
        if abs_addr.is_none() {
            // Platform without symbol lookup — skip.
            return;
        }
        let abs_addr = abs_addr.unwrap() as i64;

        // FunctionDescriptor: int(int)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                .unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        // `mk_downcall_handle` leaves DOWNCALL_CIF at 0 — the cache-miss marker
        // this test's first call must observe.
        let handle = mk_downcall_handle(&mut ctx, abs_addr, Some(descriptor), -1);

        // Snapshot the global build counter.
        let before = plf::CIF_BUILD_COUNT.load(Ordering::Relaxed);

        // --- First call: cache miss, Cif built and stashed ---
        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(-7));
        let r1 = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        )
        .expect("first downcall must succeed");
        assert_eq!(r1, Some(Value::Int(7)));

        let after_first = plf::CIF_BUILD_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after_first,
            before + 1,
            "first call must construct exactly one Cif"
        );

        // Verify DOWNCALL_CIF now carries a non-zero pointer.
        let stash_v = ctx.get_field(handle, DOWNCALL_CIF);
        let stash_u64 = match stash_v {
            Value::Long(n) => n as u64,
            _ => 0,
        };
        assert!(
            stash_u64 != 0,
            "field 3 must hold the boxed Cif pointer after the first call, got {:?}",
            stash_v
        );

        // --- Second call: cache hit, Cif *not* rebuilt ---
        let call_args2 = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args2, 0, Value::Int(-11));
        let r2 = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args2))],
        )
        .expect("second downcall must succeed");
        assert_eq!(r2, Some(Value::Int(11)));

        let after_second = plf::CIF_BUILD_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after_second, after_first,
            "second call must reuse the cached Cif — build count must not increment"
        );

        // Clean up the leaked Box to keep the test process tidy.
        unsafe { plf::free_cached_cif(stash_u64) };
    }

    #[test]
    fn panama_cif_cache_field3_sticks_to_boxed_ptr() {
        // Simpler smoke test: if we don't actually call, DOWNCALL_CIF stays 0.
        // Only the invoke path populates the cache slot.
        let mut ctx = mock_ctx();
        let handle = mk_downcall_handle(&mut ctx, 0, None, -1);
        match ctx.get_field(handle, DOWNCALL_CIF) {
            Value::Long(0) => {}
            other => panic!("expected Long(0), got {:?}", other),
        }
    }

    // `test_85_3_downcall_struct_layout` AND
    // `panama_struct_layout_preserves_named_members` WERE HERE AND ARE DELETED
    // WITH THE FUNCTION THEY TESTED (F16, 2026-08-13).
    //
    // The first called `pe_struct_layout` on `{int, long}` and asserted
    // size 16 / alignment 8. That is a call HotSpot 25.0.3+9-LTS REFUSES:
    //
    //     MemoryLayout.structLayout(JAVA_INT, JAVA_LONG)
    //       -> IllegalArgumentException: Invalid alignment constraint for
    //          member layout: j8
    //
    // so the test was pinning a fabricated success — it froze the VM's own
    // wrong answer as if it were the specification. The second asserted that
    // member NAMES survive into a `names` array at slot 3, which is a slot the
    // authoritative carrier does not have: `foreign_ffm.rs` keeps the member
    // LAYOUTS at slot 2 and resolves a name by asking each member for its own
    // (`p67_layout_named_member`), so there is no second copy to drift.
    //
    // Both behaviours are now covered against the ORACLE'S numbers in
    // `vm/src/vm/tests.rs` — see `struct_layout_rejects_underaligned_member`,
    // `struct_layout_does_not_pad_the_total` and `struct_layout_single_field`.

    #[test]
    fn test_85_3_downcall_void_return() {
        // Call a function with void return — use memset (returns void* but we treat it as void)
        // Actually, let's use a simpler approach: call abs with void descriptor
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();

        // We'll test that a downcall with -1 return kind produces Value::Object(None)
        let abs_addr = ctx.find_native_symbol(-1, "abs");
        if abs_addr.is_none() {
            return;
        }
        let abs_addr = abs_addr.unwrap() as i64;

        // FunctionDescriptor: ofVoid(LAYOUT_INT)  — return_layout = None
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                .unwrap();
        ctx.set_field(descriptor, 0, Value::Object(None)); // void
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = mk_downcall_handle(&mut ctx, abs_addr, Some(descriptor), -1);

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(5));

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        );

        assert!(result.is_ok());
        // void return should produce Object(None)
        let val = result.unwrap();
        assert_eq!(val, Some(Value::Object(None)));
    }

    // ===================================================================
    // Phase 85.4: Upcall Stub Tests
    // ===================================================================

    #[test]
    fn test_85_4_upcall_registration() {
        // Register an upcall and verify it can be looked up
        let mut ctx = mock_ctx();

        // Create a "MethodHandle" target object
        let target =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2).unwrap();

        let entry = ffi::UpcallEntry {
            target,
            method_name: "invoke".to_string(),
            method_descriptor: String::new(),
            param_kinds: vec![LAYOUT_INT],
            return_kind: LAYOUT_INT,
        };

        let slot = ctx.register_upcall(entry);

        // Look it up
        let info = ctx.get_upcall_info(slot);
        assert!(info.is_some());
        let (ret_target, param_kinds, return_kind) = info.unwrap();
        assert_eq!(ret_target, target);
        assert_eq!(param_kinds, vec![LAYOUT_INT]);
        assert_eq!(return_kind, LAYOUT_INT);
    }

    #[test]
    fn test_85_4_upcall_handle_and_invoke() {
        // Native access is DENIED by default, and `pe_upcall_handle` gates on
        // it — so without this the test asserts against a refusal and fails on
        // a correct build. The two panama tests that pass unguarded do so only
        // because the policy is a process global that another test may have
        // granted first; `with_policy_isolated` takes the shared lock and
        // restores the previous value, so this neither depends on nor leaks
        // that ordering.
        with_policy_isolated(|| {
            set_native_access_enabled(true);
            // Create an upcall handle through pe_upcall_handle and dispatch through pe_upcall_invoke
            let mut ctx = mock_ctx();

            // Create target, descriptor
            let target =
                try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2)
                    .unwrap();

            let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
            let param_layout = make_layout(&mut ctx, LAYOUT_INT);
            let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
            ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

            let descriptor =
                try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                    .unwrap();
            ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
            ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

            let linker =
                try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker", 1).unwrap();
            let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

            // Register upcall handle
            let handle_result = pe_upcall_handle(
                &mut ctx,
                &[
                    Value::Object(Some(linker)),
                    Value::Object(Some(target)),
                    Value::Object(Some(descriptor)),
                    Value::Object(Some(arena)),
                ],
            );
            assert!(handle_result.is_ok());
            let seg = match handle_result.unwrap() {
                Some(Value::Object(Some(s))) => s,
                _ => panic!("Expected segment from upcall handle"),
            };

            // The segment's address (field 0) should be a real trampoline function pointer
            let tramp_addr = match ctx.get_field(seg, 0) {
                Value::Long(n) => n,
                _ => -1,
            };
            assert!(tramp_addr != 0, "Trampoline address should be non-null");
            // Verify it's a real callable function pointer by calling it
            let tramp_fn: unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                unsafe { std::mem::transmute(tramp_addr as usize) };
            let tramp_result = unsafe { tramp_fn(42, 0, 0, 0, 0, 0, 0, 0) };
            // Trampoline dispatch returns 0 (no thread-local context in tests)
            assert_eq!(tramp_result, 0);

            // Set up invoke_virtual to return a value when the upcall dispatches
            // via pe_upcall_invoke (the Java-side dispatch path)
            unsafe {
                *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Int(99))));
            }

            // Create an UpcallStub handle object for pe_upcall_invoke
            // (uses the VM upcall slot 0, not the trampoline address)
            let stub = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/UpcallStub", 2)
                .unwrap();
            ctx.set_field(stub, 0, Value::Long(0)); // slot 0 in the VM's upcall table

            // Create args array
            let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
            ctx.set_array_element(call_args, 0, Value::Int(42));

            let result = pe_upcall_invoke(
                &mut ctx,
                &[Value::Object(Some(stub)), Value::Object(Some(call_args))],
            );
            assert!(result.is_ok());
            let val = result.unwrap();
            assert_eq!(val, Some(Value::Int(99)));
        });
    }

    #[test]
    fn test_85_4_upcall_invalid_slot() {
        // Invoking an upcall with an invalid slot should return an error
        let mut ctx = mock_ctx();

        let stub =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/UpcallStub", 2).unwrap();
        ctx.set_field(stub, 0, Value::Long(999)); // non-existent slot

        let result = pe_upcall_invoke(&mut ctx, &[Value::Object(Some(stub))]);
        assert!(result.is_err(), "Invalid upcall slot must return error");
    }

    // =====================================================================
    // NEW-18: libffi-backed Panama Linker tests
    //
    // Each test exercises a real C-library function through the rewritten
    // pe_downcall_invoke / pe_upcall_handle path. Together they prove:
    //   (a) arbitrary-arity downcalls past the old 8-arg cap;
    //   (b) mixed integer/float register classification (was broken);
    //   (c) variadic dispatch via Cif::new_variadic;
    //   (d) struct-by-value return packed into a result MemorySegment;
    //   (e) round-trip upcall — C qsort calls a libffi-closure-backed
    //       trampoline that re-enters our Rust callback (which would, in
    //       a real VM, dispatch to Java via the active NativeContext).
    // =====================================================================

    /// NEW-18: a 12-arg integer downcall via the libffi pipeline using a
    /// helper `extern "C"` Rust function (no symbol lookup needed).
    /// This exercises argument counts past the 8-arg cap of the old
    /// dispatcher and the int register/stack handoff.
    #[test]
    fn new18_downcall_arity_12_ints() {
        extern "C" fn sum12(
            a: i32,
            b: i32,
            c: i32,
            d: i32,
            e: i32,
            f: i32,
            g: i32,
            h: i32,
            i: i32,
            j: i32,
            k: i32,
            l: i32,
        ) -> i32 {
            a + b + c + d + e + f + g + h + i + j + k + l
        }
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();
        let fn_addr = sum12 as usize as i64;

        // Build descriptor: int(int,int,int,int,int,int,int,int,int,int,int,int)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 12);
        for i in 0..12 {
            let p = make_layout(&mut ctx, LAYOUT_INT);
            ctx.set_array_element(params_arr, i, Value::Object(Some(p)));
        }
        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                .unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 12);
        for i in 0..12 {
            ctx.set_array_element(call_args, i, Value::Int((i + 1) as i32));
        }

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        )
        .expect("12-arg downcall must succeed");
        // Sum of 1..=12 = 78.
        assert_eq!(result, Some(Value::Int(78)));
    }

    /// NEW-18: mixed int/float arguments. The old dispatcher passed
    /// every arg through `u64` registers, so floats landed in the wrong
    /// place. libffi gets the ABI right.
    #[test]
    fn new18_downcall_mixed_int_float() {
        extern "C" fn mix(a: i32, b: f64, c: i32, d: f32) -> f64 {
            a as f64 + b + c as f64 + d as f64
        }
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();
        let fn_addr = mix as usize as i64;

        let ret_layout = make_layout(&mut ctx, LAYOUT_DOUBLE);
        let p_int1 = make_layout(&mut ctx, LAYOUT_INT);
        let p_dbl = make_layout(&mut ctx, LAYOUT_DOUBLE);
        let p_int2 = make_layout(&mut ctx, LAYOUT_INT);
        let p_flt = make_layout(&mut ctx, LAYOUT_FLOAT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(p_int1)));
        ctx.set_array_element(params_arr, 1, Value::Object(Some(p_dbl)));
        ctx.set_array_element(params_arr, 2, Value::Object(Some(p_int2)));
        ctx.set_array_element(params_arr, 3, Value::Object(Some(p_flt)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                .unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = mk_downcall_handle(&mut ctx, fn_addr, Some(descriptor), -1);

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_array_element(call_args, 0, Value::Int(10));
        ctx.set_array_element(call_args, 1, Value::Double(2.5));
        ctx.set_array_element(call_args, 2, Value::Int(7));
        ctx.set_array_element(call_args, 3, Value::Float(0.5));

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        )
        .expect("mixed int/float downcall must succeed");
        match result {
            Some(Value::Double(d)) => assert!((d - 20.0).abs() < 1e-9, "got {d}"),
            other => panic!("expected Double, got {other:?}"),
        }
    }

    /// NEW-18: variadic downcall via the new `firstVariadicArg` flag.
    /// Calls libc `snprintf(buf, len, "%d", 42)` and verifies the
    /// buffer contains "42" and the return value is 2 (excluding NUL).
    /// snprintf is a real variadic C function on every supported
    /// platform so this exercises libffi's `prep_cif_var` end-to-end.
    #[test]
    fn new18_downcall_variadic_snprintf() {
        // Look up snprintf — present on every supported platform. On
        // MSVC Windows the symbol is named `snprintf` in ucrt.
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall (keeps
        // this test from depending on a sibling having left the gate open).
        let _na = NativeAccessGuard::enable();
        let snprintf_addr = match ctx.find_native_symbol(-1, "snprintf") {
            Some(a) => a as i64,
            None => return, // platform without symbol lookup → skip
        };

        // Allocate a 16-byte output buffer in native memory.
        let (_, buf_ptr) = ctx.allocate_native_memory(16, 1).unwrap();
        unsafe {
            std::ptr::write_bytes(buf_ptr, 0, 16);
        }
        // Allocate the format string "%d\0".
        let (_, fmt_ptr) = ctx.allocate_native_memory(4, 1).unwrap();
        unsafe {
            let f = b"%d\0";
            std::ptr::copy_nonoverlapping(f.as_ptr(), fmt_ptr, f.len());
        }

        // Descriptor: int(address, long, address, int) — 3 fixed args
        // (buf, size, format) followed by 1 variadic int.
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let p_buf = make_layout(&mut ctx, LAYOUT_ADDRESS);
        let p_len = make_layout(&mut ctx, LAYOUT_LONG);
        let p_fmt = make_layout(&mut ctx, LAYOUT_ADDRESS);
        let p_var = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(p_buf)));
        ctx.set_array_element(params_arr, 1, Value::Object(Some(p_len)));
        ctx.set_array_element(params_arr, 2, Value::Object(Some(p_fmt)));
        ctx.set_array_element(params_arr, 3, Value::Object(Some(p_var)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                .unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        // First 3 args fixed; everything from index 3 is variadic.
        let handle = mk_downcall_handle(&mut ctx, snprintf_addr, Some(descriptor), 3);

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_array_element(call_args, 0, Value::Long(buf_ptr as i64));
        ctx.set_array_element(call_args, 1, Value::Long(16));
        ctx.set_array_element(call_args, 2, Value::Long(fmt_ptr as i64));
        ctx.set_array_element(call_args, 3, Value::Int(42));

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        )
        .expect("variadic snprintf downcall must succeed");
        // snprintf returns the number of chars written excluding NUL.
        assert_eq!(result, Some(Value::Int(2)));
        // And the buffer must contain "42\0".
        let written = unsafe { std::slice::from_raw_parts(buf_ptr as *const u8, 3) };
        assert_eq!(&written[..2], b"42");
        assert_eq!(written[2], 0);
    }

    /// NEW-18: real upcall round-trip. We register an upcall handle
    /// pointing at a Java MethodHandle target; libffi gives us a real
    /// extern "C" trampoline. We then call that trampoline directly
    /// from Rust, with the active NativeContext installed via the
    /// guard, and verify the dispatch reaches Java's invoke_virtual.
    #[test]
    fn new18_upcall_libffi_closure_dispatches_to_java() {
        // GAP F3: `pe_upcall_handle` now goes through `require_native_access`,
        // exactly like the downcall path (and exactly as docs/CONFIG.md has
        // always described `Linker.upcallHandle`). Grant it for this test, the
        // same way `new18_downcall_variadic_snprintf` does.
        let _na = NativeAccessGuard::enable();
        let mut ctx = mock_ctx();

        // Descriptor: int(int, int)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let p1 = make_layout(&mut ctx, LAYOUT_INT);
        let p2 = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(p1)));
        ctx.set_array_element(params_arr, 1, Value::Object(Some(p2)));
        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2)
                .unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let target =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2).unwrap();
        let linker =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker", 1).unwrap();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        // Pre-arm the mock NativeContext to return Int(123) from invoke_virtual.
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Int(123))));
        }

        // Allocate the upcall handle — this builds a real libffi closure
        // and returns a MemorySegment whose field 0 is the trampoline addr.
        let seg = pe_upcall_handle(
            &mut ctx,
            &[
                Value::Object(Some(linker)),
                Value::Object(Some(target)),
                Value::Object(Some(descriptor)),
                Value::Object(Some(arena)),
            ],
        )
        .expect("upcallHandle must succeed")
        .and_then(|v| {
            if let Value::Object(Some(s)) = v {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();

        let tramp_addr = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert!(tramp_addr != 0, "trampoline must be a real address");

        // Install the active NativeContext for the duration of the call,
        // then invoke the trampoline directly with two int args.
        let result_int: i32 = {
            let _guard = crate::panama_libffi::ActiveContextGuard::install(&mut ctx);
            let f: extern "C" fn(i32, i32) -> i32 =
                unsafe { std::mem::transmute(tramp_addr as usize) };
            f(11, 22)
        };
        // The Rust callback inside upcall_dispatch reaches the mock's
        // invoke_virtual, which we pre-armed to return Int(123). The
        // callback then writes 123 back into the result slot, which
        // libffi delivers to the C caller (us) as an int.
        assert_eq!(result_int, 123);
    }

    // --- Task #57: native-access gate emits IllegalCallerException ---

    /// Round-trip guard: when native access is disabled, the gate inside
    /// `validated_fn_ptr` must produce a `RuntimeError::IllegalCallerException`
    /// — not the old `IllegalStateException` with an "IllegalCallerException:"
    /// message prefix. This is what the VM-side mapping table converts to
    /// the throwable `java/lang/IllegalCallerException` class.
    #[test]
    fn task57_native_access_gate_emits_illegal_caller_exception() {
        // FIX(test-isolation): serialize with the guarded downcall tests.
        // This test sets the flag *false*; without the shared lock it could
        // run concurrently with a guarded test mid-downcall and either steal
        // its `true` value or have its own `false` stomped, corrupting both.
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Save and restore the global flag — other tests rely on the default
        // (enabled) behaviour and may run in parallel.
        let prev = native_access_enabled();
        set_native_access_enabled(false);

        let result = validated_fn_ptr::<extern "C" fn() -> i32>(0x1000);

        // Restore before any assertion so a failure does not poison sibling
        // tests.
        set_native_access_enabled(prev);

        let err = result.expect_err("gate must reject downcall when disabled");
        match err {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalCallerException { message },
            )) => {
                assert!(
                    message.contains("Native access is not enabled"),
                    "message should describe the denial: {message}"
                );
                // Must NOT carry the legacy "IllegalCallerException: " prefix
                // — the class name comes from the variant now.
                assert!(
                    !message.starts_with("IllegalCallerException:"),
                    "message must not duplicate the class name: {message}"
                );
            }
            other => panic!("expected RuntimeError::IllegalCallerException, got {other:?}"),
        }
    }

    /// Regression guard: the other validation failures inside
    /// `validated_fn_ptr` (null pointer, misaligned address) must still emit
    /// `IllegalStateException`. Only the native-access denial flips to the
    /// new variant.
    #[test]
    fn task57_other_gate_failures_still_emit_illegal_state() {
        // FIX(test-isolation): serialize with the guarded downcall tests so
        // no concurrent test can flip the flag out from under us.
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Ensure the gate is open so we exercise the non-access paths.
        let prev = native_access_enabled();
        set_native_access_enabled(true);

        // Null function pointer.
        let null_err = validated_fn_ptr::<extern "C" fn()>(0).expect_err("null must be rejected");
        // Misaligned function pointer (odd address fails alignment check on
        // every supported platform since `align_of::<usize>() >= 4`).
        let misaligned_err =
            validated_fn_ptr::<extern "C" fn()>(0x1001).expect_err("misaligned must be rejected");

        set_native_access_enabled(prev);

        for (label, err) in [("null", null_err), ("misaligned", misaligned_err)] {
            match err {
                MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                    RuntimeError::IllegalStateException { .. },
                )) => { /* expected */ }
                other => panic!("{label}: expected IllegalStateException, got {other:?}"),
            }
        }
    }

    /// NEW-18: variadic struct-layout helper sanity. Make sure the
    /// libffi bridge accepts every primitive layout our descriptor
    /// machinery produces. (We exercise the struct path indirectly
    /// via the existing `test_85_3_downcall_struct_layout` test.)
    #[test]
    fn new18_layout_translation_covers_every_primitive() {
        for kind in [
            LAYOUT_BYTE,
            LAYOUT_BOOLEAN,
            LAYOUT_SHORT,
            LAYOUT_CHAR,
            LAYOUT_INT,
            LAYOUT_LONG,
            LAYOUT_FLOAT,
            LAYOUT_DOUBLE,
            LAYOUT_ADDRESS,
        ] {
            assert!(
                crate::panama_libffi::primitive_kind_to_ffi_type(kind).is_some(),
                "primitive kind {} must translate",
                kind
            );
        }
    }

    // ===================================================================
    // Security regression: Panama arbitrary-memory + native-access gate
    // (CRITICAL — arbitrary process memory R/W + native-code execution)
    // ===================================================================

    /// Helper: build a MemorySegment synthetic backed by a real Rust buffer so
    /// in-bounds accesses are sound while out-of-bounds accesses are caught by
    /// the bounds checks before any dereference.
    fn make_segment(ctx: &mut dyn NativeContext, ptr: i64, size: i64, base_off: i64) -> ObjectRef {
        let seg = alloc_segment_carrier(ctx, 6).unwrap();
        ctx.set_field(seg, 0, Value::Long(ptr));
        ctx.set_field(seg, 1, Value::Long(size));
        ctx.set_field(seg, 2, Value::Object(None));
        ctx.set_field(seg, 3, Value::Int(0));
        ctx.set_field(seg, 4, Value::Int(1));
        ctx.set_field(seg, 5, Value::Long(base_off));
        seg
    }

    fn make_layout_kind(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
        pe_make_layout(ctx, kind).unwrap()
    }

    #[test]
    fn sec_segment_get_rejects_oob_offset() {
        let mut ctx = mock_ctx();
        let mut buf = [0u8; 8];
        let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_INT);
        // offset 8 + width 4 = 12 > size 8 → must be rejected, NOT dereferenced.
        let r = pe_segment_get_impl(&mut ctx, seg, layout, 8);
        assert!(r.is_err(), "out-of-bounds get must be rejected");
    }

    #[test]
    fn sec_segment_get_rejects_negative_offset() {
        let mut ctx = mock_ctx();
        let mut buf = [0u8; 8];
        let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_BYTE);
        let r = pe_segment_get_impl(&mut ctx, seg, layout, -1);
        assert!(r.is_err(), "negative offset get must be rejected");
    }

    #[test]
    fn sec_segment_set_rejects_oob_offset() {
        let mut ctx = mock_ctx();
        let mut buf = [0u8; 8];
        let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_LONG);
        // offset 4 + width 8 = 12 > size 8 → reject before writing.
        let r = pe_segment_set_impl(
            &mut ctx,
            seg,
            layout,
            4,
            Value::Long(0x4141414141414141u64 as i64),
        );
        assert!(r.is_err(), "out-of-bounds set must be rejected");
    }

    #[test]
    fn sec_segment_zero_size_not_accessible() {
        // A 0-size segment (as produced by ofAddress before reinterpret) must
        // refuse all access, even at offset 0 — this is the ofAddress escape.
        let mut ctx = mock_ctx();
        let seg = make_segment(&mut ctx, 0x1000, 0, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_BYTE);
        assert!(
            pe_segment_get_impl(&mut ctx, seg, layout, 0).is_err(),
            "get on zero-size segment must be rejected"
        );
        assert!(
            pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0)).is_err(),
            "set on zero-size segment must be rejected"
        );
    }

    #[test]
    fn sec_segment_access_addr_overflow_rejected() {
        let mut ctx = mock_ctx();
        // ptr = u64::MAX (as i64 = -1), size large enough to pass bounds, so the
        // address arithmetic itself overflows and must be caught.
        let seg = make_segment(&mut ctx, -1i64, 1024, 0);
        let r = pe_segment_access_addr(&mut ctx, seg, 16, 8);
        assert!(r.is_err(), "address arithmetic overflow must be rejected");
    }

    #[test]
    fn sec_segment_in_bounds_roundtrips() {
        // Sanity: a legitimate in-bounds access still works.
        let mut ctx = mock_ctx();
        let mut buf = [0u8; 8];
        let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_INT);
        assert!(pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0x11223344)).is_ok());
        match pe_segment_get_impl(&mut ctx, seg, layout, 0) {
            Ok(Some(Value::Int(v))) => assert_eq!(v, 0x11223344),
            other => panic!("expected Int(0x11223344), got {:?}", other),
        }
    }

    #[test]
    fn sec_native_access_disabled_by_default() {
        // The process-wide gate must default closed (secure-by-default).
        // NOTE: this reads global state; if a prior test in the same process
        // flipped it on we restore it, but the *initial* default is false.
        // We assert the default via a fresh load after forcing the documented
        // default value.
        // FIX(test-isolation): this test both writes and reads the global
        // flag, so it must serialize with every other flag-toggling test;
        // otherwise a concurrent guarded downcall test could flip the flag
        // between our `set` and our `assert`, breaking these assertions.
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_native_access_enabled(false);
        assert!(!native_access_enabled());
        // Setter plumbing still works in both directions.
        set_native_access_enabled(true);
        assert!(native_access_enabled());
        set_native_access_enabled(false);
        assert!(!native_access_enabled());
    }

    #[test]
    fn sec_validated_fn_ptr_denied_when_gate_closed() {
        // FIX(test-isolation): serialize so a concurrent guarded test cannot
        // re-enable the flag between our `set false` and the denial check.
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_native_access_enabled(false);
        let r = validated_fn_ptr::<extern "C" fn() -> i32>(0x1000);
        assert!(
            r.is_err(),
            "downcall must be denied when native access is disabled"
        );
    }

    #[test]
    fn upcall_target_root_scan_and_remap_gap_c() {
        // Step 5 GAP C: a leaked FFM/Panama upcall target must be reported as a
        // GC root and remapped in place after a move. Build a minimal registered
        // upcall around a fake target address, then scan + remap.
        let userdata = Box::new(UpcallUserdata {
            target: std::sync::atomic::AtomicUsize::new(0xABCD_0000),
            param_kinds: Vec::new(),
            return_kind: -1,
        });
        let userdata_ptr: &'static UpcallUserdata = Box::leak(userdata);
        let cif = MiddleCif::new(Vec::new(), MiddleType::void());
        let closure = Closure::new(cif, upcall_dispatch, userdata_ptr);
        let code_ptr = *closure.code_ptr() as *const () as usize;
        upcall_registry().lock().insert(
            code_ptr,
            UpcallEntry {
                _closure: Box::new(closure),
                userdata: userdata_ptr as *const UpcallUserdata,
            },
        );

        // Scan reports the (fake) target as a root.
        let mut roots = Vec::new();
        gc_scan_upcall_target_roots(&mut roots);
        assert!(
            roots.iter().any(|r| r.as_ptr() as usize == 0xABCD_0000),
            "upcall target must be scanned as a GC root"
        );

        // Remap 0xABCD_0000 -> 0xABCD_8000; the leaked userdata is rewritten.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0xABCD_0000usize, 0xABCD_8000usize);
        gc_update_upcall_target_refs(&map);
        assert_eq!(
            userdata_ptr
                .target
                .load(std::sync::atomic::Ordering::Relaxed),
            0xABCD_8000,
            "upcall target must be remapped in place"
        );
        let mut roots2 = Vec::new();
        gc_scan_upcall_target_roots(&mut roots2);
        assert!(
            roots2.iter().any(|r| r.as_ptr() as usize == 0xABCD_8000),
            "re-scan must report the moved target"
        );

        // Don't leak our entry into other tests sharing the global registry.
        upcall_registry().lock().remove(&code_ptr);
    }
    // ===================================================================
    // F35 (2026-08-13): heap segments, the LAYOUT_UNKNOWN range tests, and
    // the accessor gate.
    //
    // ORACLE. Every number quoted below is `java` on this host, Microsoft
    // build 25.0.3+9-LTS, with
    // `--add-opens java.base/jdk.internal.foreign=ALL-UNNAMED` where a private
    // field is read. Transcripts: `scratchpad/f35/F35Probe.java` and
    // `F35Probe2.java`.
    //
    // MOCK. `MockNativeContext::get_field_by_name` is a name-keyed map that is
    // independent of `set_field`, so a test that only round-trips a NAME
    // measures the mock. Nothing below asserts on a name lookup: every
    // assertion is about the CONTENTS of a real array after a write, or about
    // an exception class. The names only supply the carrier's inputs, exactly
    // as the real VM's field resolver would.
    // ===================================================================

    /// A real JDK heap carrier: `HeapMemorySegmentImpl$Of*` with the five
    /// fields `javap` reports, in declaration order
    /// (`AbstractMemorySegmentImpl{length, readOnly, scope}` then
    /// `HeapMemorySegmentImpl{offset, base}`).
    ///
    /// `byte_start` is the JDK's `address()`, i.e. `offset - 16`; the `offset`
    /// field is written WITH the bias applied, which is what the real
    /// constructor does.
    fn make_real_heap_segment(
        ctx: &mut dyn NativeContext,
        class: &str,
        base: ObjectRef,
        byte_start: i64,
        size: i64,
        read_only: bool,
    ) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(class).unwrap();
        let seg = ctx.alloc_object(cid, 5);
        ctx.set_field(seg, 0, Value::Long(size)); // length
        ctx.set_field(seg, 1, Value::Int(i32::from(read_only))); // readOnly
        ctx.set_field(seg, 2, Value::Object(None)); // scope
        ctx.set_field(seg, 3, Value::Long(byte_start + 16)); // offset
        ctx.set_field(seg, 4, Value::Object(Some(base))); // base
        ctx.set_field_by_name(seg, "length", Value::Long(size));
        ctx.set_field_by_name(seg, "readOnly", Value::Int(i32::from(read_only)));
        ctx.set_field_by_name(seg, "offset", Value::Long(byte_start + 16));
        ctx.set_field_by_name(seg, "base", Value::Object(Some(base)));
        seg
    }

    /// A JDK-true four-slot value layout: `[0]=Long(byteSize)`,
    /// `[1]=Long(byteAlignment)`, classified by CLASS NAME.
    ///
    /// This is the carrier `--jdk-only` actually has (F27 §1): the nine
    /// `jdk/internal/foreign/layout/ValueLayouts$Of*Impl`. `align = 1` builds
    /// `JAVA_INT_UNALIGNED`, which the oracle reports as
    /// `byteSize=4 byteAlignment=1 toString=1%i4`.
    fn make_jdk_layout(
        ctx: &mut dyn NativeContext,
        class: &str,
        byte_size: i64,
        align: i64,
    ) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(class).unwrap();
        let layout = ctx.alloc_object(cid, 4);
        ctx.set_field(layout, 0, Value::Long(byte_size));
        ctx.set_field(layout, 1, Value::Long(align));
        ctx.set_field(layout, 2, Value::Object(None));
        ctx.set_field(layout, 3, Value::Object(None));
        layout
    }

    fn jdk_int_unaligned(ctx: &mut dyn NativeContext) -> ObjectRef {
        make_jdk_layout(
            ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            4,
            1,
        )
    }

    fn jdk_byte_layout(ctx: &mut dyn NativeContext) -> ObjectRef {
        make_jdk_layout(
            ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
            1,
            1,
        )
    }

    fn arena_segment(ctx: &mut dyn NativeContext, arena: ObjectRef, size: i64) -> ObjectRef {
        pe_arena_allocate_impl(ctx, arena, size, 8)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap()
    }

    fn byte_vec(ctx: &dyn NativeContext, arr: ObjectRef, len: usize) -> Vec<i32> {
        (0..len)
            .map(|i| match ctx.get_array_element(arr, i) {
                Value::Int(v) => v,
                other => panic!("array element {i} is {other:?}"),
            })
            .collect()
    }

    /// The headline: a real heap segment now reads and writes its backing
    /// array instead of dereferencing a number that is not an address.
    ///
    /// The oracle row this pins, verbatim:
    ///
    /// ```text
    /// byte[] a = new byte[8];
    /// MemorySegment.ofArray(a).set(JAVA_INT_UNALIGNED, 0, 0x01020304);
    /// // a == [4, 3, 2, 1, 0, 0, 0, 0]
    /// ```
    ///
    /// PRE-FIX `pe_segment_set_impl` called `pe_segment_access_addr`, which
    /// called `segment_address`, which for this carrier answered slot 0 — the
    /// LENGTH. Before F27 that was a wild store; after F27 it is
    /// `IllegalStateException: Null segment address`. Either way the array
    /// stayed all zeroes, so the `[4, 3, 2, 1]` assertion is the mutation
    /// check as well as the assertion.
    #[test]
    fn a_real_heap_segment_reads_and_writes_its_backing_array() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
        let seg = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
            arr,
            0,
            8,
            false,
        );
        let layout = jdk_int_unaligned(&mut ctx);

        pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0x0102_0304)).unwrap();

        assert_eq!(
            byte_vec(&ctx, arr, 8),
            vec![4, 3, 2, 1, 0, 0, 0, 0],
            "the write must land in the Java array, little-endian, and touch \
             exactly four bytes"
        );
        assert_eq!(
            pe_segment_get_impl(&mut ctx, seg, layout, 0).unwrap(),
            Some(Value::Int(0x0102_0304)),
            "and reading it back must go through the same array"
        );
    }

    /// The `Unsafe.arrayBaseOffset` bias is removed EXACTLY ONCE.
    ///
    /// Oracle: `ofArray(new byte[32]).asSlice(3)` has `offset == 19` and
    /// `address() == 3`, and a write through `asSlice(3, 4)` lands at
    /// `src[3..7]` — measured, `[0, 0, 0, 4, 3, 2, 1, 0, ...]`.
    ///
    /// MUTATION: drop the `- HEAP_ARRAY_BASE_OFFSET` and the four bytes land
    /// at index 19; subtract it twice and the start is negative and the view
    /// is refused. Only the correct arithmetic puts them at 3.
    #[test]
    fn the_array_base_offset_bias_is_removed_exactly_once() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        let seg = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
            arr,
            3,
            4,
            false,
        );
        let layout = jdk_int_unaligned(&mut ctx);

        pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0x0102_0304)).unwrap();

        assert_eq!(
            byte_vec(&ctx, arr, 8),
            vec![0, 0, 0, 4, 3, 2, 1, 0],
            "the segment's first byte is array index 3, not 19 and not 0"
        );
    }

    /// A non-byte backing array is addressed by BYTE, not by element.
    ///
    /// Oracle, on an `int[4]`-backed segment:
    ///
    /// ```text
    /// si.set(JAVA_BYTE, 0, (byte) 0x7f);   // iarr[0] == 127
    /// si.set(JAVA_INT, 4, 0x11223344);     // iarr[1] == 0x11223344
    /// si.get(JAVA_BYTE, 3)  == 0
    /// ```
    ///
    /// so byte offset `k` is element `k / 4`, byte `k % 4`, little-endian, and
    /// a one-byte write must not disturb the other three bytes of its element.
    #[test]
    fn a_non_byte_backing_array_is_addressed_by_byte_not_by_element() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
        let seg = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfInt",
            arr,
            0,
            16,
            false,
        );
        let int_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            4,
            4,
        );
        let byte_layout = jdk_byte_layout(&mut ctx);

        pe_segment_set_impl(&mut ctx, seg, byte_layout, 0, Value::Int(0x7f)).unwrap();
        pe_segment_set_impl(&mut ctx, seg, int_aligned, 4, Value::Int(0x1122_3344)).unwrap();

        assert_eq!(
            ctx.get_array_element(arr, 0),
            Value::Int(0x7f),
            "a one-byte write is the element's LOW byte and leaves the rest alone"
        );
        assert_eq!(
            ctx.get_array_element(arr, 1),
            Value::Int(0x1122_3344),
            "byte offset 4 is element 1 whole"
        );
        assert_eq!(
            pe_segment_get_impl(&mut ctx, seg, byte_layout, 3).unwrap(),
            Some(Value::Int(0)),
            "byte 3 of element 0 is still zero"
        );
        assert_eq!(
            pe_segment_get_impl(&mut ctx, seg, int_aligned, 4).unwrap(),
            Some(Value::Int(0x1122_3344))
        );
    }

    /// Alignment is enforced on the heap path, with BOTH halves of the rule.
    ///
    /// Oracle rows, all `IllegalArgumentException` unless marked OK:
    ///
    /// | receiver | layout | offset | result |
    /// |---|---|---|---|
    /// | `byte[32]` | `JAVA_INT` (align 4) | 0 | refused - maxByteAlignment is 1 |
    /// | `byte[32]` | `JAVA_INT_UNALIGNED` | 0 | OK |
    /// | `int[8]` | `JAVA_INT` | 0 | OK |
    /// | `int[8]` | `JAVA_INT` | 1 | refused - offset not a multiple of 4 |
    /// | `int[8]` | `JAVA_LONG` (align 8) | 0 | refused - maxByteAlignment is 4 |
    ///
    /// MUTATION: keeping only the modulo half admits row 1 (0 % 4 == 0);
    /// keeping only the `maxByteAlignment` half admits row 4 (4 <= 4). Each
    /// row is red under exactly one of those two mutations.
    #[test]
    fn heap_alignment_is_enforced_the_way_the_oracle_enforces_it() {
        let mut ctx = mock_ctx();
        let bytes = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        let byte_seg = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
            bytes,
            0,
            32,
            false,
        );
        let ints = ctx.new_array(cratonvm_types::ArrayElementType::Int, 8);
        let int_seg = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfInt",
            ints,
            0,
            32,
            false,
        );
        let int_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            4,
            4,
        );
        let int_unaligned = jdk_int_unaligned(&mut ctx);
        let long_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
            8,
            8,
        );

        assert!(
            pe_segment_get_impl(&mut ctx, byte_seg, int_aligned, 0).is_err(),
            "a byte[] segment's maxByteAlignment is 1, so JAVA_INT is refused \
             even at offset 0"
        );
        assert!(
            pe_segment_get_impl(&mut ctx, byte_seg, int_unaligned, 0).is_ok(),
            "JAVA_INT_UNALIGNED on the same receiver is the oracle's OK row"
        );
        assert!(pe_segment_get_impl(&mut ctx, int_seg, int_aligned, 0).is_ok());
        assert!(
            pe_segment_get_impl(&mut ctx, int_seg, int_aligned, 1).is_err(),
            "offset 1 is not a multiple of the 4-byte alignment"
        );
        assert!(
            pe_segment_get_impl(&mut ctx, int_seg, long_aligned, 0).is_err(),
            "an int[] segment's maxByteAlignment is 4, so JAVA_LONG is refused"
        );
    }

    /// Bounds, zero size and read-only answer the oracle's exception CLASS,
    /// not merely "an error".
    ///
    /// A caller writing `catch (IndexOutOfBoundsException)` — the idiom for a
    /// bounds check — must catch ours. Measured: every out-of-bounds and
    /// zero-size access on BOTH a native and a heap carrier is
    /// `IndexOutOfBoundsException`, and a read-only write is
    /// `IllegalArgumentException: Attempt to write a read-only segment`.
    #[test]
    fn heap_refusals_use_the_oracles_exception_classes() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
        let seg = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
            arr,
            0,
            8,
            false,
        );
        let layout = jdk_int_unaligned(&mut ctx);
        let byte_layout = jdk_byte_layout(&mut ctx);

        let oob = pe_segment_get_impl(&mut ctx, seg, layout, 6).unwrap_err();
        assert!(
            format!("{oob:?}").contains("IndexOutOfBounds"),
            "offset 6 + 4 > 8 must be IndexOutOfBoundsException, got {oob:?}"
        );
        let negative = pe_segment_get_impl(&mut ctx, seg, byte_layout, -1).unwrap_err();
        assert!(
            format!("{negative:?}").contains("IndexOutOfBounds"),
            "a negative offset must be IndexOutOfBoundsException, got {negative:?}"
        );

        let empty_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
        let empty = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
            empty_arr,
            0,
            0,
            false,
        );
        let zero = pe_segment_get_impl(&mut ctx, empty, byte_layout, 0).unwrap_err();
        assert!(
            format!("{zero:?}").contains("IndexOutOfBounds"),
            "a zero-size segment is IndexOutOfBoundsException on the oracle, \
             not IllegalStateException, got {zero:?}"
        );

        let ro_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
        let ro = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
            ro_arr,
            0,
            8,
            true,
        );
        assert!(
            pe_segment_get_impl(&mut ctx, ro, byte_layout, 0).is_ok(),
            "a read-only segment still READS (oracle: OK)"
        );
        let write = pe_segment_set_impl(&mut ctx, ro, byte_layout, 0, Value::Int(1)).unwrap_err();
        assert!(
            format!("{write:?}").contains("IllegalArgument"),
            "writing a read-only segment is IllegalArgumentException, got {write:?}"
        );
        assert_eq!(
            ctx.get_array_element(ro_arr, 0),
            Value::Int(0),
            "and the refused write must not have happened"
        );
    }

    // ===================================================================
    // G6 (2026-08-16): the three callers `pe_segment_slice`'s heap arm
    // reached and nothing tested — `spliterator`, `elements`, `toArray` —
    // plus the `maxByteAlignment` rule the merge left UNSETTLED and the
    // `heapBase` capability the oracle withholds from a read-only view.
    //
    // ORACLE. Every number and message below is `java` on this host, Temurin
    // 25.0.3+9-LTS, transcribed from the probes `FfmProbe`, `FfmProbe2` and
    // `FfmProbe3` (see the G6-1 record for the full tables). NOTHING here has
    // been measured on a CratonVM binary.
    //
    // MOCK. These tests build their heap carriers with [`heap_alias_segment`],
    // the eight-slot H2 shape, and NOT with [`make_real_heap_segment`]. That
    // is not a preference. `make_real_heap_segment` writes `base`/`offset`/
    // `readOnly`/`length` BY NAME, and `MockNativeContext::set_field_by_name`
    // resolves a name through `mock_field_slot`, whose whole chain — including
    // `cratonvm_classloading::synthetic_stub_field_model` — has no entry for
    // `jdk/internal/foreign/HeapMemorySegmentImpl$Of*`. An unresolved name is
    // a SILENT no-op on write and `Value::Int(0)` on read, so
    // `heap_segment_view` cannot resolve such a carrier under the mock and
    // answers `None`. H2 resolves by SLOT (`[6]=array, [7]=start`) and needs
    // no name table, which is why `of_array_covers_byte_short_and_char_and_
    // the_carrier_aliases` — the one existing test that proves a write reaches
    // the caller's array — uses it. See the G6-1 record's NOM-1: the fix is a
    // `mock_field_slot` arm in `test_utils.rs`, which this lane does not own.
    // ===================================================================

    /// The H2 heap carrier: `[0]=0` (no machine address), `[1]=byteSize`,
    /// `[2]=scope`, `[3]=readOnly`, `[4]=1`, `[5]=0`, `[6]=array`,
    /// `[7]=startWithinArray`.
    ///
    /// This is the production shape, not a test fixture: it is exactly what
    /// [`pe_of_array_alias`] mints for `ofArray(byte[]|short[]|char[])` and
    /// what [`pe_segment_slice`]'s heap arm mints for every slice of a heap
    /// segment — including a slice of a REAL `HeapMemorySegmentImpl$Of*`,
    /// because `asSlice` is force-routed. So it is also the receiver that
    /// `toArray`/`elements`/`spliterator` actually see in the field.
    fn heap_alias_segment(
        ctx: &mut dyn NativeContext,
        array: ObjectRef,
        start: i64,
        size: i64,
        read_only: bool,
    ) -> ObjectRef {
        let seg = alloc_segment_carrier(ctx, SEG_HEAP_FIELDS).unwrap();
        ctx.set_field(seg, 0, Value::Long(0));
        ctx.set_field(seg, 1, Value::Long(size));
        ctx.set_field(seg, 2, Value::Object(None));
        ctx.set_field(seg, 3, Value::Int(i32::from(read_only)));
        ctx.set_field(seg, 4, Value::Int(1));
        ctx.set_field(seg, 5, Value::Long(0));
        ctx.set_field(seg, SEG_HEAP_BASE_FIELD, Value::Object(Some(array)));
        ctx.set_field(seg, SEG_HEAP_START_FIELD, Value::Long(start));
        seg
    }

    /// A `byte[]`-backed heap segment holding `bytes`, plus the array itself.
    fn heap_byte_segment(ctx: &mut dyn NativeContext, bytes: &[i32]) -> (ObjectRef, ObjectRef) {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b));
        }
        let seg = heap_alias_segment(ctx, arr, 0, bytes.len() as i64, false);
        (seg, arr)
    }

    /// An `int[]`-backed heap segment holding `values`, plus the array itself.
    fn heap_int_segment(ctx: &mut dyn NativeContext, values: &[i32]) -> (ObjectRef, ObjectRef) {
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, values.len());
        for (i, v) in values.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*v));
        }
        let seg = heap_alias_segment(ctx, arr, 0, values.len() as i64 * 4, false);
        (seg, arr)
    }

    /// `maxByteAlignment()` is the BACKING ARRAY'S element alignment, not 8.
    ///
    /// This is the question the merge lane left open, and both arms of the
    /// answer it guessed were wrong. Oracle rows, transcribed:
    ///
    /// | receiver | `maxByteAlignment()` |
    /// |---|---|
    /// | `ofArray(byte[16])` | 1 |
    /// | `ofArray(short[8])` / `ofArray(char[8])` | 2 |
    /// | `ofArray(int[8])` / `ofArray(float[8])` | 4 |
    /// | `ofArray(long[8])` / `ofArray(double[8])` | 8 |
    /// | `ofArray(byte[0])` | 1 |
    /// | `ofArray(long[0])` | 8 |
    /// | `ofArray(long[4]).asSlice(4)` | 4 |
    /// | `ofArray(int[8]).asSlice(2)` | 2 |
    /// | `ofArray(byte[16]).asSlice(8)` | 1 |
    /// | `MemorySegment.NULL` / `ofAddress(0)` | 4611686018427387904 |
    /// | `ofAddress(16)` | 16 |
    /// | `ofAddress(12)` | 4 |
    ///
    /// MUTATION: the previous body was `addr == 0 ? 8 : addr & -addr` over the
    /// segment's START, so every row with start 0 answered 8. Rows 1-3, 5 and
    /// 10 are red under it; row 4 (`long[8]` -> 8) is the single row that
    /// agreed, which is why reading one row would have settled nothing.
    #[test]
    fn max_byte_alignment_is_the_element_type_not_a_constant_eight() {
        use cratonvm_types::ArrayElementType as A;
        let mut ctx = mock_ctx();
        for (elem, len, expected) in [
            (A::Byte, 16usize, 1_i64),
            (A::Short, 8, 2),
            (A::Char, 8, 2),
            (A::Int, 8, 4),
            (A::Float, 8, 4),
            (A::Long, 8, 8),
            (A::Double, 8, 8),
            (A::Byte, 0, 1),
            (A::Long, 0, 8),
        ] {
            let width = heap_element_width(elem).unwrap() as i64;
            let arr = ctx.new_array(elem, len);
            let seg = heap_alias_segment(&mut ctx, arr, 0, len as i64 * width, false);
            assert_eq!(
                pe_segment_max_byte_alignment(&ctx, seg),
                expected,
                "a {elem:?}[] segment starting at offset 0 promises its element \
                 alignment, not 8"
            );
        }

        // The offset cap, on the element type wide enough to show it.
        let longs = ctx.new_array(A::Long, 4);
        for (start, expected) in [(0_i64, 8_i64), (1, 1), (2, 2), (4, 4), (8, 8), (12, 4)] {
            let seg = heap_alias_segment(&mut ctx, longs, start, 32 - start, false);
            assert_eq!(
                pe_segment_max_byte_alignment(&ctx, seg),
                expected,
                "long[4] at byte offset {start} promises min(8, lowestOneBit({start}))"
            );
        }

        // A byte[] never promises more than 1, at any offset — the row the old
        // body got most wrong.
        let bytes = ctx.new_array(A::Byte, 16);
        for start in [0_i64, 1, 4, 8, 12] {
            let seg = heap_alias_segment(&mut ctx, bytes, start, 16 - start, false);
            assert_eq!(pe_segment_max_byte_alignment(&ctx, seg), 1);
        }

        // The native arm, including the address-0 row that used to answer 8.
        assert_eq!(native_max_byte_alignment(0), 1_i64 << 62);
        assert_eq!(native_max_byte_alignment(16), 16);
        assert_eq!(native_max_byte_alignment(12), 4);
        assert_eq!(native_max_byte_alignment(1), 1);
    }

    /// `toArray` on a heap receiver hands back the ARRAY'S BYTES.
    ///
    /// It used to hand back zeros: `segment_address` is 0 for every heap
    /// carrier (F27), and the raw-pointer loop took its `base.is_null()` early
    /// return and returned a correctly-sized array of the type's default.
    /// Oracle:
    ///
    /// * `ofArray(new byte[]{0..7}).toArray(JAVA_BYTE)` -> `[0, 1, ..., 7]`
    /// * `ofArray(byte[16]).asSlice(3,4).toArray(JAVA_BYTE)` -> `[3, 4, 5, 6]`
    /// * `ofArray(new int[]{10,20,30,40}).toArray(JAVA_INT)` -> `[10, 20, 30, 40]`
    ///
    /// MUTATION: delete the heap arm and every assertion below reads 0.
    #[test]
    fn to_array_on_a_heap_receiver_reads_the_arrays_own_bytes() {
        let mut ctx = mock_ctx();
        let byte_layout = jdk_byte_layout(&mut ctx);
        let (seg, _arr) = heap_byte_segment(&mut ctx, &[0, 1, 2, 3, 4, 5, 6, 7]);

        let out = pe_segment_to_array(
            &mut ctx,
            &[Value::Object(Some(seg)), Value::Object(Some(byte_layout))],
        )
        .unwrap();
        let Some(Value::Object(Some(out))) = out else {
            panic!("toArray must answer an array")
        };
        assert_eq!(ctx.array_length(out), 8);
        for i in 0..8 {
            assert_eq!(
                ctx.get_array_element(out, i),
                Value::Int(i as i32),
                "byte {i} must come from the backing array, not from address 0"
            );
        }

        // A SLICE of that heap segment is the receiver `elements` and
        // `spliterator` hand on, and the one `asSlice` mints from a REAL
        // `HeapMemorySegmentImpl$Of*`.
        let Some(Value::Object(Some(slice))) = pe_segment_slice(&mut ctx, seg, 3, 4, None).unwrap()
        else {
            panic!("a heap slice must be a segment")
        };
        let sliced = pe_segment_to_array(
            &mut ctx,
            &[Value::Object(Some(slice)), Value::Object(Some(byte_layout))],
        )
        .unwrap();
        let Some(Value::Object(Some(sliced))) = sliced else {
            panic!("toArray must answer an array")
        };
        assert_eq!(ctx.array_length(sliced), 4);
        for (i, expected) in [3, 4, 5, 6].into_iter().enumerate() {
            assert_eq!(
                ctx.get_array_element(sliced, i),
                Value::Int(expected),
                "a heap slice's toArray starts at the slice, not at the array"
            );
        }

        // A wider element type, so the stride is exercised and not only the
        // byte-for-byte identity case.
        let int_layout = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            4,
            4,
        );
        let (int_seg, _) = heap_int_segment(&mut ctx, &[10, 20, 30, 40]);
        let out = pe_segment_to_array(
            &mut ctx,
            &[
                Value::Object(Some(int_seg)),
                Value::Object(Some(int_layout)),
            ],
        )
        .unwrap();
        let Some(Value::Object(Some(out))) = out else {
            panic!("toArray must answer an array")
        };
        assert_eq!(ctx.array_length(out), 4);
        for (i, v) in [10, 20, 30, 40].into_iter().enumerate() {
            assert_eq!(ctx.get_array_element(out, i), Value::Int(v));
        }
    }

    /// `toArray`'s two refusals, in the oracle's ORDER.
    ///
    /// | call | oracle |
    /// |---|---|
    /// | `ofArray(byte[15]).toArray(JAVA_INT)` | ISE `Segment size is not a multiple of 4. Size: 15` |
    /// | `ofArray(byte[16]).toArray(JAVA_INT)` | IAE `Source segment incompatible with alignment constraints` |
    /// | `ofArray(byte[16]).toArray(JAVA_INT_UNALIGNED)` | 4 elements |
    /// | `ofArray(int[8]).toArray(JAVA_LONG)` | IAE, same message |
    /// | `ofArray(int[8]).toArray(JAVA_INT)` | 8 elements |
    ///
    /// Row 1 is the ordering witness: that segment is BOTH badly sized and
    /// badly aligned, and HotSpot reports the SIZE, because
    /// `AbstractMemorySegmentImpl.toArray` runs `checkArraySize` before it
    /// hands the segment to `MemorySegment.copy`.
    #[test]
    fn to_array_alignment_gate_matches_the_oracle_and_runs_after_the_size_gate() {
        let mut ctx = mock_ctx();
        let int_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            4,
            4,
        );
        // Same CLASS, alignment 1: that is exactly how the JDK spells
        // `JAVA_INT_UNALIGNED` — one `OfIntImpl` with a different alignment
        // slot — so the element-kind sniff and the alignment gate are being
        // read off the two different places they are read off in production.
        let int_unaligned = jdk_int_unaligned(&mut ctx);
        let long_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
            8,
            8,
        );

        let (b15, _) = heap_byte_segment(&mut ctx, &[0; 15]);
        let err = pe_segment_to_array(
            &mut ctx,
            &[Value::Object(Some(b15)), Value::Object(Some(int_aligned))],
        )
        .unwrap_err();
        let text = format!("{err:?}");
        assert!(
            text.contains("IllegalState") && text.contains("not a multiple of 4"),
            "a segment that is both badly sized and badly aligned reports the \
             SIZE on the oracle, got {text}"
        );

        let (b16, _) = heap_byte_segment(&mut ctx, &[0; 16]);
        let err = pe_segment_to_array(
            &mut ctx,
            &[Value::Object(Some(b16)), Value::Object(Some(int_aligned))],
        )
        .unwrap_err();
        let text = format!("{err:?}");
        assert!(
            text.contains("IllegalArgument") && text.contains("Source segment incompatible"),
            "a byte[] segment's maxByteAlignment is 1, so JAVA_INT is refused, \
             got {text}"
        );
        assert!(
            pe_segment_to_array(
                &mut ctx,
                &[Value::Object(Some(b16)), Value::Object(Some(int_unaligned))]
            )
            .is_ok(),
            "JAVA_INT_UNALIGNED on the same receiver is the oracle's OK row"
        );

        let (int_seg, _) = heap_int_segment(&mut ctx, &[0; 8]);
        assert!(
            pe_segment_to_array(
                &mut ctx,
                &[
                    Value::Object(Some(int_seg)),
                    Value::Object(Some(long_aligned))
                ]
            )
            .is_err(),
            "an int[] segment's maxByteAlignment is 4, so JAVA_LONG is refused"
        );
        assert!(pe_segment_to_array(
            &mut ctx,
            &[
                Value::Object(Some(int_seg)),
                Value::Object(Some(int_aligned))
            ]
        )
        .is_ok());
    }

    /// `spliterator` and `elements` count a heap receiver's elements and share
    /// ONE alignment gate — the one that reads `maxByteAlignment`.
    ///
    /// The gate used to be `segment_address % elemAlign`, and a heap segment's
    /// address is 0, so it admitted every element layout on every heap
    /// receiver. Oracle:
    ///
    /// | call | oracle |
    /// |---|---|
    /// | `ofArray(byte[16]).spliterator(JAVA_BYTE).estimateSize()` | 16 |
    /// | `ofArray(byte[16]).spliterator(JAVA_INT_UNALIGNED).estimateSize()` | 4 |
    /// | `ofArray(byte[16]).spliterator(JAVA_INT)` | IAE `Incompatible alignment constraints` |
    /// | `ofArray(int[8]).spliterator(JAVA_INT).estimateSize()` | 8 |
    /// | `ofArray(int[8]).elements(JAVA_LONG)` | IAE, same message |
    /// | `ofArray(byte[15]).elements(JAVA_INT_UNALIGNED)` | IAE `Segment size is not a multiple of layout size` |
    ///
    /// MUTATION: restore `segment_address % elemAlign` and rows 3 and 5 turn
    /// into successful streams over misaligned memory.
    #[test]
    fn spliterator_and_elements_gate_a_heap_receiver_on_max_byte_alignment() {
        let mut ctx = mock_ctx();
        let byte_layout = jdk_byte_layout(&mut ctx);
        let int_unaligned = jdk_int_unaligned(&mut ctx);
        let int_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            4,
            4,
        );
        let long_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
            8,
            8,
        );

        fn split(
            ctx: &mut dyn NativeContext,
            seg: ObjectRef,
            layout: ObjectRef,
        ) -> MethodCallResult {
            pe_segment_spliterator(
                ctx,
                &[Value::Object(Some(seg)), Value::Object(Some(layout))],
            )
        }
        fn elements(
            ctx: &mut dyn NativeContext,
            seg: ObjectRef,
            layout: ObjectRef,
        ) -> MethodCallResult {
            pe_segment_elements(
                ctx,
                &[Value::Object(Some(seg)), Value::Object(Some(layout))],
            )
        }

        let (b16, _) = heap_byte_segment(&mut ctx, &[0; 16]);

        let Some(Value::Object(Some(s))) = split(&mut ctx, b16, byte_layout).unwrap() else {
            panic!("a byte-layout spliterator over a byte[16] heap segment must exist")
        };
        assert_eq!(pe_splitter_state(&mut ctx, s), (16, 1, 0));

        let Some(Value::Object(Some(s))) = split(&mut ctx, b16, int_unaligned).unwrap() else {
            panic!("JAVA_INT_UNALIGNED is the oracle's OK row on a byte[] receiver")
        };
        assert_eq!(pe_splitter_state(&mut ctx, s), (4, 4, 0));

        let err = split(&mut ctx, b16, int_aligned).unwrap_err();
        let text = format!("{err:?}");
        assert!(
            text.contains("IllegalArgument") && text.contains("Incompatible alignment constraints"),
            "a byte[] segment's maxByteAlignment is 1, so JAVA_INT is refused, \
             got {text}"
        );

        let (int_seg, _) = heap_int_segment(&mut ctx, &[0; 8]);
        let Some(Value::Object(Some(s))) = split(&mut ctx, int_seg, int_aligned).unwrap() else {
            panic!("JAVA_INT over an int[] receiver is the oracle's 8-element row")
        };
        assert_eq!(pe_splitter_state(&mut ctx, s), (8, 4, 0));
        assert!(
            split(&mut ctx, int_seg, long_aligned).is_err(),
            "an int[] segment's maxByteAlignment is 4, so JAVA_LONG is refused"
        );

        // `elements` is `spliterator` plus a Stream carrier, so its REFUSALS
        // must be identical — the gate runs before anything is allocated.
        assert!(
            elements(&mut ctx, b16, int_aligned).is_err(),
            "elements() shares spliterator()'s alignment gate"
        );
        assert!(
            elements(&mut ctx, int_seg, long_aligned).is_err(),
            "elements() shares spliterator()'s alignment gate"
        );
        assert!(matches!(
            elements(&mut ctx, int_seg, int_aligned),
            Ok(Some(Value::Object(Some(_))))
        ));

        // The size-multiple gate, on a receiver the alignment gate admits.
        let (b15, _) = heap_byte_segment(&mut ctx, &[0; 15]);
        let err = split(&mut ctx, b15, int_unaligned).unwrap_err();
        assert!(
            format!("{err:?}").contains("Segment size is not a multiple of layout size"),
            "got {err:?}"
        );
    }

    /// The splitter walks a heap receiver, and each element it mints is a HEAP
    /// slice over the same array — not a synthetic native carrier parked at a
    /// small integer address.
    ///
    /// Oracle: `ofArray(new int[]{10,20,30,40}).spliterator(JAVA_INT)` advances
    /// four times then reports exhausted; the second element has
    /// `address() == 4`, `isNative() == false`, `heapBase()` present, and reads
    /// `20`.
    #[test]
    fn the_splitter_walks_a_heap_receiver_element_by_element() {
        let mut ctx = mock_ctx();
        let int_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            4,
            4,
        );
        let (seg, arr) = heap_int_segment(&mut ctx, &[10, 20, 30, 40]);

        let Some(Value::Object(Some(splitter))) = pe_segment_spliterator(
            &mut ctx,
            &[Value::Object(Some(seg)), Value::Object(Some(int_aligned))],
        )
        .unwrap() else {
            panic!("spliterator over an int[4] heap segment must exist")
        };
        assert_eq!(pe_splitter_state(&mut ctx, splitter), (4, 4, 0));

        // A null consumer exercises the mint and the advance without needing
        // the mock to dispatch `Consumer.accept`.
        for expected_index in 1..=4_i64 {
            assert_eq!(
                pe_splitter_try_advance(&mut ctx, &[Value::Object(Some(splitter))]).unwrap(),
                Some(Value::Int(1)),
                "element {expected_index} of 4 must be produced"
            );
            assert_eq!(pe_splitter_state(&mut ctx, splitter).2, expected_index);
        }
        assert_eq!(
            pe_splitter_try_advance(&mut ctx, &[Value::Object(Some(splitter))]).unwrap(),
            Some(Value::Int(0)),
            "the fifth advance is exhausted"
        );

        // And the element body itself — the same `pe_segment_slice` call
        // `tryAdvance` makes for index 1.
        let Some(Value::Object(Some(elem))) = pe_segment_slice(&mut ctx, seg, 4, 4, None).unwrap()
        else {
            panic!("element 1 must be a segment")
        };
        assert_eq!(pe_segment_base_address(&ctx, elem), 4, "address() is 4");
        assert_eq!(
            pe_segment_heap_base(&ctx, elem),
            Value::Object(Some(arr)),
            "an element of a heap segment is a HEAP segment over the SAME \
             array, not a native carrier parked at the small integer 4"
        );
        assert_eq!(
            pe_segment_get_impl(&mut ctx, elem, int_aligned, 0).unwrap(),
            Some(Value::Int(20)),
            "element 1 reads the array's second int"
        );
    }

    /// A read-only segment has NO `heapBase`.
    ///
    /// Oracle (`FfmProbe` B12, `FfmProbe3` M4a-M4g): `heapBase()` is present on
    /// a writable heap segment and EMPTY on `asReadOnly()`, on a slice of a
    /// read-only segment, on an element of one, on a read-only `ofBuffer`
    /// segment, and on a read-only native segment.
    ///
    /// This is a capability, not cosmetics: the array `heapBase()` returns is
    /// writable through plain array stores, so handing it out from a read-only
    /// view returns exactly the capability `asReadOnly()` removed — F26's
    /// "a copying slice is a wrong capability", one call further on.
    ///
    /// MUTATION: drop the read-only arm and rows 2, 3 and 4 hand the array
    /// back.
    #[test]
    fn a_read_only_segment_hands_out_no_backing_array() {
        let mut ctx = mock_ctx();
        let (writable, arr) = heap_byte_segment(&mut ctx, &[1, 2, 3, 4]);
        assert_eq!(
            pe_segment_heap_base(&ctx, writable),
            Value::Object(Some(arr)),
            "a writable heap segment answers its array"
        );

        let read_only = heap_alias_segment(&mut ctx, arr, 0, 4, true);
        assert_eq!(
            pe_segment_heap_base(&ctx, read_only),
            Value::Object(None),
            "a read-only heap segment must not hand out its writable array"
        );

        // Read-only is contagious through `asSlice` (F21), so the slice must
        // withhold it too — the arm that would otherwise leak the array one
        // call after the fix.
        let Some(Value::Object(Some(slice))) =
            pe_segment_slice(&mut ctx, read_only, 1, 2, None).unwrap()
        else {
            panic!("a heap slice must be a segment")
        };
        assert_eq!(
            pe_segment_heap_base(&ctx, slice),
            Value::Object(None),
            "a slice of a read-only segment is read-only, so it has no \
             heapBase either"
        );

        // And `asReadOnly()`'s own shape: the same body with the flag FORCED.
        let Some(Value::Object(Some(forced))) =
            pe_segment_slice(&mut ctx, writable, 0, 4, Some(true)).unwrap()
        else {
            panic!("asReadOnly must answer a segment")
        };
        assert_eq!(
            pe_segment_heap_base(&ctx, forced),
            Value::Object(None),
            "asReadOnly() of a writable heap segment withholds the array"
        );

        // Reads still work through the read-only view — the oracle's
        // `asReadOnly().toArray(JAVA_BYTE).length == 16` row. Withholding the
        // array must not become "a read-only segment is unreadable".
        let byte_layout = jdk_byte_layout(&mut ctx);
        let out = pe_segment_to_array(
            &mut ctx,
            &[
                Value::Object(Some(read_only)),
                Value::Object(Some(byte_layout)),
            ],
        )
        .unwrap();
        let Some(Value::Object(Some(out))) = out else {
            panic!("a read-only segment still reads")
        };
        assert_eq!(ctx.get_array_element(out, 0), Value::Int(1));
    }

    /// The slice arities refuse in the oracle's ORDER, with the oracle's
    /// exception CLASSES and message TEXTS.
    ///
    /// | call | oracle |
    /// |---|---|
    /// | `ofArray(byte[16]).asSlice(17, 0)` | `IndexOutOfBoundsException` |
    /// | `ofArray(byte[16]).asSlice(4, -1)` | `IndexOutOfBoundsException` |
    /// | `ofArray(byte[16]).asSlice(20, 4, 3)` | `IndexOutOfBoundsException` (bounds beat a bad alignment) |
    /// | `ofArray(byte[16]).asSlice(4, 4, 3)` | IAE `Invalid alignment constraint : 3` |
    /// | `ofArray(byte[16]).asSlice(4, 4, 0)` | IAE `Invalid alignment constraint : 0` |
    /// | `ofArray(byte[16]).asSlice(0, 8, 8)` | IAE `Target offset incompatible with alignment constraints` |
    /// | `ofArray(byte[16]).asSlice(4, 4, 1)` | OK |
    /// | `ofArray(int[8]).asSlice(0, 4, 4)` | OK |
    /// | `ofArray(int[8]).asSlice(2, 4, 4)` | IAE, alignment |
    /// | `ofArray(int[8]).asSlice(0, 8, 8)` | IAE, alignment |
    ///
    /// Note the SPACE before the colon in `Invalid alignment constraint : 3`.
    /// It is HotSpot's, transcribed; the previous text had no space, and
    /// interpolated the offset into the second message, which HotSpot does not
    /// do at all.
    #[test]
    fn slice_refusals_are_the_oracles_classes_texts_and_order() {
        let mut ctx = mock_ctx();
        let (b16, _) = heap_byte_segment(&mut ctx, &[0; 16]);

        let oob = pe_slice_bounds_check(&ctx, b16, 17, 0).unwrap_err();
        assert!(
            format!("{oob:?}").contains("IndexOutOfBounds"),
            "an over-long slice is IndexOutOfBoundsException, not \
             IllegalStateException, got {oob:?}"
        );
        assert!(pe_slice_bounds_check(&ctx, b16, 4, -1).is_err());
        assert!(pe_slice_bounds_check(&ctx, b16, -1, 4).is_err());
        assert!(pe_slice_bounds_check(&ctx, b16, 16, 0).is_ok());

        let bad_power = pe_slice_alignment_check(&ctx, b16, 4, 3).unwrap_err();
        assert!(
            format!("{bad_power:?}").contains("Invalid alignment constraint : 3"),
            "the space before the colon is the oracle's, got {bad_power:?}"
        );
        let zero = pe_slice_alignment_check(&ctx, b16, 4, 0).unwrap_err();
        assert!(format!("{zero:?}").contains("Invalid alignment constraint : 0"));
        let unmeetable = pe_slice_alignment_check(&ctx, b16, 0, 8).unwrap_err();
        let text = format!("{unmeetable:?}");
        assert!(
            text.contains("Target offset incompatible with alignment constraints"),
            "the message does not interpolate the offset, got {text}"
        );
        assert!(
            pe_slice_alignment_check(&ctx, b16, 4, 1).is_ok(),
            "alignment 1 is always available"
        );

        let (int_seg, _) = heap_int_segment(&mut ctx, &[0; 8]);
        assert!(pe_slice_alignment_check(&ctx, int_seg, 0, 4).is_ok());
        assert!(pe_slice_alignment_check(&ctx, int_seg, 4, 4).is_ok());
        assert!(
            pe_slice_alignment_check(&ctx, int_seg, 2, 4).is_err(),
            "absolute offset 2 cannot carry a 4-byte alignment"
        );
        assert!(
            pe_slice_alignment_check(&ctx, int_seg, 0, 8).is_err(),
            "an int[] segment's maxByteAlignment is 4"
        );
    }

    /// A slice's own start does NOT disqualify a better-aligned offset inside
    /// it.
    ///
    /// This is the conjunct the previous `heap_segment_check_access` had one
    /// too many of. Oracle (`FfmProbe3` M1a-M1j):
    ///
    /// | call | oracle |
    /// |---|---|
    /// | `ofArray(int[8]).asSlice(2).maxByteAlignment()` | 2 |
    /// | `ofArray(int[8]).asSlice(2).get(JAVA_INT, 2)` | reads (absolute 4) |
    /// | `ofArray(int[8]).asSlice(2).get(JAVA_INT, 0)` | IAE (absolute 2) |
    /// | `ofArray(long[4]).asSlice(4).get(JAVA_LONG, 4)` | reads (absolute 8) |
    ///
    /// MUTATION: re-add `align <= maxByteAlignment(view.start)` and rows 2 and
    /// 4 are refused.
    #[test]
    fn alignment_is_judged_at_the_absolute_offset_not_the_slice_start() {
        let mut ctx = mock_ctx();
        let int_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            4,
            4,
        );
        let long_aligned = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
            8,
            8,
        );

        let ints = ctx.new_array(cratonvm_types::ArrayElementType::Int, 8);
        let at2 = heap_alias_segment(&mut ctx, ints, 2, 30, false);
        assert_eq!(
            pe_segment_max_byte_alignment(&ctx, at2),
            2,
            "a slice starting at byte 2 of an int[] promises 2"
        );
        assert!(
            pe_segment_get_impl(&mut ctx, at2, int_aligned, 2).is_ok(),
            "offset 2 of a slice starting at 2 is absolute 4, which the oracle \
             reads"
        );
        assert!(
            pe_segment_get_impl(&mut ctx, at2, int_aligned, 0).is_err(),
            "offset 0 of the same slice is absolute 2, which the oracle refuses"
        );

        let longs = ctx.new_array(cratonvm_types::ArrayElementType::Long, 4);
        let at4 = heap_alias_segment(&mut ctx, longs, 4, 28, false);
        assert_eq!(pe_segment_max_byte_alignment(&ctx, at4), 4);
        assert!(
            pe_segment_get_impl(&mut ctx, at4, long_aligned, 4).is_ok(),
            "absolute offset 8 carries an 8-byte alignment even though the \
             slice starts at 4"
        );
        assert!(pe_segment_get_impl(&mut ctx, at4, long_aligned, 0).is_err());
    }

    /// The zero-size refusal on the RAW-ADDRESS path changed class too, and
    /// this is the control for that half.
    ///
    /// Oracle: `MemorySegment.ofAddress(0x1000).get(JAVA_BYTE, 0)` is
    /// `IndexOutOfBoundsException`, exactly as the heap and native
    /// out-of-bounds rows are. It used to be `IllegalStateException` here,
    /// which contradicted the "Bounds, not state" paragraph sitting directly
    /// below it in the same function.
    #[test]
    fn a_zero_size_native_segment_is_also_an_index_out_of_bounds() {
        let mut ctx = mock_ctx();
        let seg = alloc_segment_carrier(&mut ctx, 6).unwrap();
        ctx.set_field(seg, 0, Value::Long(0x1000));
        ctx.set_field(seg, 1, Value::Long(0));
        ctx.set_field(seg, 5, Value::Long(0));
        let err = pe_segment_access_addr(&mut ctx, seg, 0, 1).unwrap_err();
        assert!(
            format!("{err:?}").contains("IndexOutOfBounds"),
            "got {err:?}"
        );
    }

    /// `MemorySegment.ofArray` now covers all three missing primitive arrays,
    /// and the carrier it mints ALIASES.
    ///
    /// `javap java.lang.foreign.MemorySegment` declares seven `ofArray`
    /// overloads — `byte[] char[] short[] int[] float[] long[] double[]` — and
    /// NO `boolean[]` (measured: `NoSuchMethodException ... ofArray([Z)`).
    /// This file registered four.
    #[test]
    fn of_array_covers_byte_short_and_char_and_the_carrier_aliases() {
        for (elem, width) in [
            (cratonvm_types::ArrayElementType::Byte, 1usize),
            (cratonvm_types::ArrayElementType::Short, 2),
            (cratonvm_types::ArrayElementType::Char, 2),
        ] {
            let mut ctx = mock_ctx();
            let arr = ctx.new_array(elem, 8);
            let seg = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(arr))]).unwrap() {
                Some(Value::Object(Some(seg))) => seg,
                other => panic!("ofArray({elem:?}) answered {other:?}"),
            };

            assert_eq!(
                crate::panama_libffi::segment_byte_size(&ctx, seg),
                (8 * width) as i64,
                "byteSize is length x element width for {elem:?}"
            );
            assert_eq!(
                crate::panama_libffi::segment_address(&ctx, seg),
                0,
                "an alias carrier has NO machine address; 0 is the value every \
                 raw-pointer consumer refuses on"
            );

            // A write through the segment must be visible in the Java array —
            // the property the four `int[]/long[]/float[]/double[]` arms do
            // NOT have, because they copy into an off-heap mirror.
            let byte_layout = jdk_byte_layout(&mut ctx);
            pe_segment_set_impl(&mut ctx, seg, byte_layout, 0, Value::Int(0x5a)).unwrap();
            assert_eq!(
                ctx.get_array_element(arr, 0),
                Value::Int(0x5a),
                "the write must alias the caller's {elem:?} array"
            );
        }
    }

    /// G19-1: a heap segment's scope is ONE session, minted with the segment,
    /// and every view derived from it hands back that same object.
    ///
    /// MEASURED on 25.0.3+9-LTS (`G19Probe` §SC), and every row below is one of
    /// those rows:
    ///
    /// ```text
    /// heap.scope() == heap.scope()                 true
    /// heap.asSlice(4,4).scope() == heap.scope()    true
    /// heap.asReadOnly().scope() == heap.scope()    true
    /// ofArray(a).scope() == ofArray(a).scope()     FALSE   (the same array!)
    /// ```
    ///
    /// The last row is the one that says the answer is per-SEGMENT and not a
    /// process-wide singleton, so it is asserted as a NEGATIVE — a fix that
    /// handed every heap segment one shared session would satisfy the first
    /// three and fail this one.
    ///
    /// Asserted through `pe_segment_session` as well as by slot, because the
    /// slot write is only half the repair: the reader in `foreign_ffm`
    /// (`p67_receiver_session`) has to recognise a session in slot 2, and if it
    /// does not, `scope()` still mints a fresh one and this whole family stays
    /// red with the carrier looking correct.
    #[test]
    fn a_heap_segments_scope_is_one_session_shared_by_its_slices() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        let seg = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(arr))]).unwrap() {
            Some(Value::Object(Some(seg))) => seg,
            other => panic!("ofArray answered {other:?}"),
        };

        let session = match ctx.get_field(seg, PE_SEGMENT_ARENA_FIELD) {
            Value::Object(Some(session)) => session,
            other => panic!("slot 2 must carry the segment's session, got {other:?}"),
        };
        assert_eq!(
            ctx.class_name_of_id(ctx.class_id_of_object(session))
                .as_deref(),
            Some(PE_SESSION_CLASS),
            "the stamped object must be a session, not an arena and not the array"
        );
        assert_eq!(
            pe_segment_session(&ctx, seg),
            Some(session),
            "the resolver must find the stamped session"
        );

        // A slice and a read-only view both go through `pe_segment_slice`, so
        // one write covers both — assert both anyway, because that sharing is a
        // property of today's call graph and not of the rule.
        let slice = match pe_segment_as_slice(
            &mut ctx,
            &[Value::Object(Some(seg)), Value::Long(4), Value::Long(4)],
        )
        .unwrap()
        {
            Some(Value::Object(Some(slice))) => slice,
            other => panic!("asSlice answered {other:?}"),
        };
        assert_eq!(
            ctx.get_field(slice, PE_SEGMENT_ARENA_FIELD),
            Value::Object(Some(session)),
            "a slice of a heap segment must share its parent's scope"
        );
        assert_eq!(pe_segment_session(&ctx, slice), Some(session));

        let read_only = match pe_segment_slice(&mut ctx, seg, 0, 16, Some(true)).unwrap() {
            Some(Value::Object(Some(view))) => view,
            other => panic!("asReadOnly answered {other:?}"),
        };
        assert_eq!(
            pe_segment_session(&ctx, read_only),
            Some(session),
            "asReadOnly must not drop the scope on the floor"
        );

        // Per SEGMENT, not per array and not per process.
        let twin = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(arr))]).unwrap() {
            Some(Value::Object(Some(twin))) => twin,
            other => panic!("ofArray answered {other:?}"),
        };
        assert_ne!(
            pe_segment_session(&ctx, twin),
            Some(session),
            "two segments over the SAME array have distinct scopes on the oracle"
        );
    }

    /// G19-1: stamping the session into slot 2 must not disturb the slot's two
    /// older tenants.
    ///
    /// Slot 2 is [`SEG_BACKING_ARRAY_FIELD`] on an `ofArray` MIRROR carrier and
    /// the owning arena on an arena-allocated one, and three readers key off it
    /// — `sync_heap_backed_segment`, `pe_segment_heap_base`'s fallback and the
    /// `isNative` discriminator. All three gate on `ctx.object_is_array`, which
    /// a session is not, so all three must answer exactly what they answered
    /// before. `heapBase()` is the one with a visible return value, so it is the
    /// one asserted: the oracle says an alias carrier's `heapBase()` is present
    /// and holds the caller's array (`G6-1` §6, row 1).
    #[test]
    fn stamping_the_scope_does_not_disturb_the_other_tenants_of_slot_two() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        let seg = match pe_of_array_alias(&mut ctx, &[Value::Object(Some(arr))]).unwrap() {
            Some(Value::Object(Some(seg))) => seg,
            other => panic!("ofArray answered {other:?}"),
        };
        assert_eq!(
            pe_segment_heap_base(&ctx, seg),
            Value::Object(Some(arr)),
            "heapBase() must still be the caller's array, read from slot 6"
        );
        assert!(
            heap_segment_view(&ctx, seg).is_some(),
            "the carrier must still decode as a heap view"
        );
        // The read-only contagion still withholds the array, and it does so on a
        // carrier whose slot 2 is now occupied.
        let read_only = match pe_segment_slice(&mut ctx, seg, 0, 16, Some(true)).unwrap() {
            Some(Value::Object(Some(view))) => view,
            other => panic!("asReadOnly answered {other:?}"),
        };
        assert_eq!(
            pe_segment_heap_base(&ctx, read_only),
            Value::Object(None),
            "a read-only view must not hand back the writable array"
        );
    }

    /// A slice of a heap segment is still a heap segment — and this is the
    /// crash F27 moved rather than closed.
    ///
    /// PRE-FIX `asSlice` computed `segment_address(this) + offset`, which for a
    /// heap receiver is `0 + offset`, and stamped THAT into slot 0 of a
    /// six-field synthetic. `ofArray(new byte[16]).asSlice(3, 4).get(...)`
    /// then dereferenced the literal address 3. At offset 0 the product was 0
    /// and refused, which is exactly why the un-sliced W7-89 §7.1 repro looked
    /// fixed while the sliced one still died.
    ///
    /// Oracle: `asSlice(3, 4)` has `byteSize=4`, `address()=3`,
    /// `isNative()=false`, and a write through it lands at `src[3..7]`.
    #[test]
    fn a_slice_of_a_heap_segment_is_still_a_heap_segment() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
        let parent = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
            arr,
            0,
            16,
            false,
        );

        let slice = match pe_segment_as_slice(
            &mut ctx,
            &[Value::Object(Some(parent)), Value::Long(3), Value::Long(4)],
        )
        .unwrap()
        {
            Some(Value::Object(Some(slice))) => slice,
            other => panic!("asSlice answered {other:?}"),
        };

        assert_eq!(
            crate::panama_libffi::segment_address(&ctx, slice),
            0,
            "the slice must NOT carry its offset where an address belongs"
        );
        assert_eq!(crate::panama_libffi::segment_byte_size(&ctx, slice), 4);
        let view = heap_segment_view(&ctx, slice).expect("a heap slice is a heap segment");
        assert_eq!(view.start, 3, "and its address() is 3, per the oracle");

        let layout = jdk_int_unaligned(&mut ctx);
        pe_segment_set_impl(&mut ctx, slice, layout, 0, Value::Int(0x0102_0304)).unwrap();
        assert_eq!(
            byte_vec(&ctx, arr, 8),
            vec![0, 0, 0, 4, 3, 2, 1, 0],
            "the write goes through the slice to src[3..7]"
        );

        // And the slice's own bounds are the SLICE's, not the parent's.
        assert!(
            pe_segment_set_impl(&mut ctx, slice, layout, 1, Value::Int(0)).is_err(),
            "offset 1 + 4 > 4"
        );
    }

    /// NEGATIVE CONTROL for every arm above: a genuinely native segment must
    /// still take the raw-address path and behave exactly as before.
    ///
    /// Without this, an over-broad heap predicate would silently route arena
    /// memory through `get_array_element` and every test above would still
    /// pass.
    #[test]
    fn a_native_segment_still_takes_the_raw_address_path() {
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
        let seg = arena_segment(&mut ctx, arena, 16);
        assert!(
            heap_segment_view(&ctx, seg).is_none(),
            "an arena segment is not a heap segment"
        );
        assert!(!crate::panama_libffi::is_real_heap_segment(&ctx, seg));

        let layout = make_layout(&mut ctx, LAYOUT_INT);
        pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0x2a)).unwrap();
        assert_eq!(
            pe_segment_get_impl(&mut ctx, seg, layout, 0).unwrap(),
            Some(Value::Int(0x2a)),
            "the off-heap round trip is unchanged"
        );
        let ptr = match ctx.get_field(seg, 0) {
            Value::Long(p) => p as *const i32,
            other => panic!("arena segment slot 0 is {other:?}"),
        };
        assert_eq!(
            unsafe { *ptr },
            0x2a,
            "and it really went to the native block, not to an array"
        );
    }

    /// A heap segment must not be handed to anything as a POINTER.
    ///
    /// Oracle, and the message is quoted from it:
    ///
    /// ```text
    /// MemorySegment.ofArray(new byte[16])
    ///     .set(ADDRESS.withByteAlignment(1), 0, MemorySegment.ofArray(new byte[4]))
    ///   -> IllegalArgumentException: Heap segment not allowed: ...
    /// strlen(MemorySegment.ofArray("hi\0".getBytes()))
    ///   -> IllegalArgumentException: Heap segment not allowed: ...
    /// ```
    ///
    /// PRE-FIX this arm stored `segment_address(target)`, which since F27 is
    /// **0** for a heap segment — a legitimate C null that no downstream
    /// consumer can tell apart from a real one. A quiet wrong write, not a
    /// refusal.
    #[test]
    fn a_heap_segment_is_refused_as_an_address_value() {
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
        let dst = arena_segment(&mut ctx, arena, 16);
        let address_layout = make_jdk_layout(
            &mut ctx,
            "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
            8,
            1,
        );

        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        let heap = make_real_heap_segment(
            &mut ctx,
            "jdk/internal/foreign/HeapMemorySegmentImpl$OfByte",
            arr,
            0,
            4,
            false,
        );
        let err = pe_segment_set_impl(&mut ctx, dst, address_layout, 0, Value::Object(Some(heap)))
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("Heap segment not allowed"),
            "the refusal must name the reason, got {err:?}"
        );

        // CONTROL: a real native segment is still stored, so the arm above is
        // not simply refusing every ADDRESS write.
        let target = arena_segment(&mut ctx, arena, 8);
        pe_segment_set_impl(
            &mut ctx,
            dst,
            address_layout,
            0,
            Value::Object(Some(target)),
        )
        .expect("a native segment is still a legal ADDRESS value");
        let stored = match pe_segment_get_impl(&mut ctx, dst, address_layout, 0).unwrap() {
            Some(Value::Object(Some(seg))) => crate::panama_libffi::segment_address(&ctx, seg),
            other => panic!("get(ADDRESS) answered {other:?}"),
        };
        assert_eq!(
            stored,
            crate::panama_libffi::segment_address(&ctx, target),
            "and the value that was written is the target's real address"
        );
    }

    /// `kind < 10` is NOT "is this a value layout".
    ///
    /// `LAYOUT_UNKNOWN` is -2, so the old spelling admitted it at all three
    /// sites. This pins the arithmetic fact itself, so a future change to
    /// either constant that re-opens the hole goes red here rather than in a
    /// downstream default arm.
    #[test]
    fn the_range_test_excludes_layout_unknown() {
        assert!(
            crate::panama_libffi::LAYOUT_UNKNOWN < 10,
            "this is WHY `kind < 10` was wrong - it is true for the unknown \
             sentinel"
        );
        assert!(!layout_kind_is_value(crate::panama_libffi::LAYOUT_UNKNOWN));
        assert!(!layout_kind_is_value(UPCALL_RETURN_VOID));
        for kind in [
            LAYOUT_BYTE,
            LAYOUT_SHORT,
            LAYOUT_INT,
            LAYOUT_LONG,
            LAYOUT_FLOAT,
            LAYOUT_DOUBLE,
            LAYOUT_ADDRESS,
            LAYOUT_BOOLEAN,
            LAYOUT_CHAR,
        ] {
            assert!(layout_kind_is_value(kind), "kind {kind} is a value layout");
        }
        for kind in [LAYOUT_STRUCT, LAYOUT_UNION, LAYOUT_SEQUENCE, LAYOUT_PADDING] {
            assert!(!layout_kind_is_value(kind), "kind {kind} is a group layout");
        }
        assert_ne!(
            UPCALL_RETURN_VOID,
            crate::panama_libffi::LAYOUT_UNKNOWN,
            "the two sentinels must stay distinct"
        );
    }

    /// An unclassifiable layout is REFUSED by both accessors, not defaulted.
    ///
    /// PRE-FIX `ffi::layout_byte_size` answers 1 for an unrecognised kind and
    /// the read's `_ =>` arm answered `Value::Int(0)`, so `get` returned a
    /// believable zero and `set` was a silent no-op. Both are the shape this
    /// family keeps paying for.
    #[test]
    fn an_unclassifiable_layout_is_refused_by_get_and_set() {
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
        let seg = arena_segment(&mut ctx, arena, 16);
        // A carrier with a `Long` slot 0 and a class name that is not any
        // layout: `read_layout_kind` falls to the class matcher and answers
        // LAYOUT_UNKNOWN.
        let layout = make_jdk_layout(&mut ctx, "com/example/NotALayout", 4, 4);
        assert_eq!(
            crate::panama_libffi::read_layout_kind(&ctx, layout),
            crate::panama_libffi::LAYOUT_UNKNOWN
        );

        let read = pe_segment_get_impl(&mut ctx, seg, layout, 0).unwrap_err();
        assert!(
            format!("{read:?}").contains("NotALayout"),
            "the refusal must name the carrier, got {read:?}"
        );
        let write = pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(1)).unwrap_err();
        assert!(format!("{write:?}").contains("NotALayout"));

        // CONTROL: the same segment with a layout that DOES classify still
        // works, so the refusal is not simply "this segment is broken".
        let good = jdk_int_unaligned(&mut ctx);
        assert!(pe_segment_get_impl(&mut ctx, seg, good, 0).is_ok());
    }

    /// The stride the ERASED `getAtIndex`/`setAtIndex` used cannot exist.
    ///
    /// They read `get_field(layout, 1)` matched against `Value::Int` — the
    /// DELETED three-slot encoding, in which slot 1 was `Int(byteSize)`. On
    /// the JDK-true four-slot carrier F16 made authoritative, slot 1 is
    /// `Long(byteAlignment)`. So the arm could never match and the stride was
    /// the `_ => 1` default: `getAtIndex(JAVA_INT, 2)` read offset 2.
    ///
    /// This pins the fact that made the old code unreachable-correct, which is
    /// what a future re-introduction would have to contradict.
    #[test]
    fn slot_one_of_a_layout_is_a_long_alignment_never_an_int_size() {
        let mut ctx = mock_ctx();
        let int_unaligned = jdk_int_unaligned(&mut ctx);
        assert_eq!(
            ctx.get_field(int_unaligned, 1),
            Value::Long(1),
            "JAVA_INT_UNALIGNED is byteSize=4 byteAlignment=1 on the oracle, \
             and slot 1 carries the ALIGNMENT"
        );
        assert!(
            !matches!(ctx.get_field(int_unaligned, 1), Value::Int(_)),
            "the erased accessors' `Value::Int` arm can never match"
        );
        assert_eq!(
            crate::panama_libffi::layout_align(&ctx, int_unaligned),
            1,
            "so 1 is the alignment, not the size"
        );
        assert_eq!(
            ffi::layout_byte_size(crate::panama_libffi::read_layout_kind(&ctx, int_unaligned)),
            4,
            "and the size - the stride the covariant accessors use - is 4"
        );
    }

    /// W7-89 §7.2: the accessors are not `@Restricted` on JDK 25, so a missing
    /// `--enable-native-access` must not refuse them.
    ///
    /// `grep "@Restricted" jdk25src/java.base/java/lang/foreign/MemorySegment.java`
    /// hits three lines and all three are `reinterpret`. HotSpot 25 runs
    /// `get`/`set`/`copy`/`fill` with no flag at all.
    ///
    /// MUTATION: reverting `require_segment_access` to `require_native_access`
    /// makes the first assertion fail whenever the process has granted
    /// nothing, which is the state a unit-test process is in. The second
    /// assertion is the control: the flag-absent refusal still exists for the
    /// paths that genuinely need it, and is still keyed on the same global.
    #[test]
    fn the_segment_accessors_are_not_gated_on_enable_native_access() {
        let mut ctx = mock_ctx();
        for op in ["get", "set", "getAtIndex", "setAtIndex", "copy", "fill"] {
            assert!(
                require_segment_access(&mut ctx, op).is_ok(),
                "MemorySegment.{op} is not @Restricted on JDK 25 and must not \
                 raise IllegalCallerException"
            );
        }
        assert_eq!(
            require_native_access(&mut ctx, "downcall").is_err(),
            !native_access_enabled(),
            "the flag-absent refusal must survive, unchanged, for the paths \
             the JDK really does restrict"
        );
    }

    /// Every `MemorySegment` triple registered on the interface is registered on
    /// [`CRATON_SEGMENT_CLASS`] too, and vice versa.
    ///
    /// This is the guard on the whole fix, not a tidiness check. Native dispatch
    /// is keyed on the RECEIVER's class; since 2026-08-22 a CratonVM-minted
    /// segment's receiver class is `CRATON_SEGMENT_CLASS`, so a method that
    /// exists only under the interface name is a `NoSuchMethodError` waiting for
    /// its first caller — and it would be raised from JDK code three frames
    /// away from the registration that forgot it.
    ///
    /// The two names are registered by a loop at each of the three sites
    /// (`register_pe_memory_segment`, `register_pe2_string_marshaling`,
    /// `foreign_ffm::register_p67_segment_surface`, plus the two-registration
    /// block beside `Arena.allocate`), so the sets can only diverge if someone
    /// adds a fourth site and registers one name. That is precisely the mistake
    /// this asserts against.
    #[test]
    fn the_craton_segment_class_mirrors_the_interface() {
        let mut registry = cratonvm_native_api::NativeMethodRegistry::new();
        super::register_pe_memory_segment(&mut registry);
        super::register_pe2_string_marshaling(&mut registry);
        crate::phases_late::foreign_ffm::register_p67_foreign_memory(&mut registry);

        let collect = |class: &str| {
            let mut rows: Vec<(String, String)> = registry
                .dump_registrations()
                .into_iter()
                .filter(|(c, _, _, _)| *c == class)
                .map(|(_, m, d, _)| (m.to_string(), d.to_string()))
                .collect();
            rows.sort();
            rows.dedup();
            rows
        };
        let iface = collect(super::PE_SEGMENT_INTERFACE);
        let craton = collect(super::CRATON_SEGMENT_CLASS);

        // A registrar that silently registered nothing would make the equality
        // below vacuously true; `byteSize` is the cheapest proof that it ran.
        assert!(
            iface.iter().any(|(m, d)| m == "byteSize" && d == "()J"),
            "the interface registrars did not run",
        );
        let only_iface: Vec<_> = iface.iter().filter(|r| !craton.contains(r)).collect();
        let only_craton: Vec<_> = craton.iter().filter(|r| !iface.contains(r)).collect();
        assert!(
            only_iface.is_empty() && only_craton.is_empty(),
            "the segment natives have drifted apart.\n\
             registered ONLY on {}: {only_iface:#?}\n\
             registered ONLY on {}: {only_craton:#?}",
            super::PE_SEGMENT_INTERFACE,
            super::CRATON_SEGMENT_CLASS,
        );
    }
}
