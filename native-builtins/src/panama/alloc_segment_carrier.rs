#[allow(unused_imports)]
use super::*;

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
/// under `--real-jdk` it is (a `cratonvm/internal/*` name is a
/// `CompatibilityStub` origin, not `VmInternal`). That mode reaches this
/// function only where there is no real bytecode to run instead — every FFM
/// native here is a `Bridge`, and a `Bridge` loses to real bytes under strict
/// policy — so the fallback is not a new fake, it is the pre-existing one, on a
/// path strict mode does not otherwise use.
pub(crate) fn alloc_segment_carrier(
    ctx: &mut dyn NativeContext,
    slots: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    alloc_segment_carrier_with(ctx, slots, false)
}

/// zs s2/nd2: [`alloc_segment_carrier`] for a site that holds no unpinned
/// reference, so that, when `collecting` ([`ffm_mint_collects`]), a refusal
/// runs the native reclaim ladder and retries before answering the
/// `OutOfMemoryError` -- never the one-attempt door, whose infallible arm
/// aborts the process on ZGC when a JIT helper made the call. `collecting ==
/// false` is [`alloc_segment_carrier`] exactly.
pub(crate) fn alloc_segment_carrier_with(
    ctx: &mut dyn NativeContext,
    slots: usize,
    collecting: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let Some(class_id) = craton_segment_class_id(ctx) else {
        if collecting {
            return crate::util_concurrent_ext::try_alloc_concurrent_synthetic_reclaiming(
                ctx,
                PE_SEGMENT_INTERFACE,
                slots,
            );
        }
        return try_alloc_concurrent_synthetic(ctx, PE_SEGMENT_INTERFACE, slots);
    };
    if collecting {
        let n = slots.max(ctx.class_num_total_fields(class_id));
        return ffm_alloc_object_collecting(ctx, class_id, n, CRATON_SEGMENT_CLASS);
    }
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

/// zs s2/nd2 (`zgc-zs1nd-jit-helper-native-doors-20261014.md`): may an FFM
/// segment mint that runs where the native holds no unpinned reference -- a
/// downcall's `ADDRESS` return after every release, an upcall's address
/// argument -- collect and retry on a refused allocation? `true` only on the
/// VM's ZGC heap ([`NativeContext::heap_is_zgc`]) with
/// `CRATONVM_ZGC_NATIVE_DOORS_COLLECT` on
/// ([`NativeContext::native_doors_collect_on_refusal`]) and
/// `CRATONVM_ZGC_FFM_MINT_COLLECT` (default ON, `=0` restores the one-attempt
/// door). Generational and G1 keep dev's one-attempt doors here: gci i48/zl2
/// made the second gate true on every collector, and zt t1/nd (`4d9001deab`)
/// added the first so that the zs/dev merge does not change them; see
/// `docs/feature-designs/zgc-architecture.md`
/// item 2 for the maintainer call. (zt t2/x1: this paragraph still said
/// Generational and G1 collect here, which `4d9001deab` reverted.) The memo
/// path's `ADDRESS` return is the one exception: it collects on every
/// collector but ZGC under its kill (`memo_address_return_collects`).
///
/// Why it matters: the mint after the C call cannot be redone (the C function
/// ran), so the native funnel's replay cannot cover it, and a JIT helper that
/// made the downcall runs with the unwind permission cleared, where ZGC's
/// infallible arm calls `abort()` without collecting. HotSpot's
/// `Utils.longToAddress` allocates in Java: collect, then throw.
pub(crate) fn ffm_mint_collects(ctx: &dyn NativeContext) -> bool {
    // zt t1: ZGC only. gci i48/zl2 made `native_doors_collect_on_refusal` true
    // on Generational and G1 too; these doors stay dev's one-attempt doors there
    // (the zt round leaves G1 / Gen behaviour unchanged).
    ctx.heap_is_zgc() && ctx.native_doors_collect_on_refusal() && ffm_mint_collect_switch()
}

/// zt t3/nd (`zgc-zs3rv2-wave-s2-review-residuals-CLOSED-20261019.md` §3):
/// `CRATONVM_ZGC_FFM_MINT_COLLECT` (default ON, `=0` kill) through a
/// [`cratonvm_types::flags::MemoSlot`]: one relaxed load per ADDRESS-returning
/// downcall instead of a declared-name lookup and an `OsString`; a test's
/// thread override resets the slot (the slot's own contract), so overrides
/// still reach it.
pub(in crate::panama) fn ffm_mint_collect_switch() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// The collecting object mint of [`ffm_mint_collects`]: the fallible door,
/// then the native reclaim ladder with a retry after each rung
/// ([`crate::util_concurrent_ext::retry_after_reclaim`]), then the catchable
/// `OutOfMemoryError` (HotSpot's text; the parenthesised `what` is stripped
/// before Java sees it). The caller holds no unpinned reference.
pub(in crate::panama) fn ffm_alloc_object_collecting(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    num_fields: usize,
    what: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(obj) = ctx.try_alloc_object(class_id, num_fields) {
        return Ok(obj);
    }
    crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
        ctx.try_alloc_object(class_id, num_fields)
    })
    .ok_or_else(|| crate::util_concurrent_ext::native_heap_oom(what))
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
/// `NATIVE_ACCESS_POLICY` were process-global for the same reason, and a second
/// VM in the same process inheriting the first one's `ClassId` would allocate
/// every segment against whatever class happened to hold that id.
///
/// `None` means the fabrication was REFUSED — see [`alloc_segment_carrier`].
pub(in crate::panama) fn craton_segment_class_id(ctx: &mut dyn NativeContext) -> Option<cratonvm_types::ClassId> {
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
    // mints stand-ins and which `--real-jdk` refuses by design.
    //
    // That refusal was the defect. Strict got `None` here and the caller fell
    // back to stamping the receiver with the `java/lang/foreign/MemorySegment`
    // INTERFACE -- an impossible class for an instance to have, and the exact
    // condition the carrier exists to cure. So the mode whose purpose is to
    // refuse fabrications landed back on the older wrong answer.
    //
    // The decision and its evidence:
    // `docs/feature-designs/native-dispatch-architecture.md`.
    // In short, five signals and none of them a matter of taste: no JDK class
    // has this name; the JDK relationships come from `synthetic_implements`
    // rather than from the name; the carrier deliberately does NOT share
    // `AbstractMemorySegmentImpl`'s layout (see that table's own comment on why
    // aliasing it would be wrong); it is one of a family of three CratonVM
    // carriers; and `ensure_generated_class`'s doc names "the VM's own internal
    // allocation shapes" as its fourth listed category.
    //
    // MEASURED by `FfmCarrierProbe.java`: `getClass().isInterface()`
    // is false for every FFM receiver on HotSpot, and was true for nine
    // `MemorySegment` doors under `--real-jdk`.
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
pub(in crate::panama) const MAX_COPY_SIZE: usize = 256 * 1024 * 1024; // 256 MiB

// ofArray segments do not have an arena. Reuse their arena/alive slots to
// retain the Java backing array and its primitive layout kind.
pub(in crate::panama) const SEG_BACKING_ARRAY_FIELD: usize = 2;
pub(in crate::panama) const SEG_BACKING_KIND_FIELD: usize = 4;

// A CratonVM-minted segment that aliases a Java array WITHOUT an off-heap
// mirror carries two extra slots. It is deliberately NOT a third tenant of
// slots 2/4/5: `segment_address` answers `[0] + [5]` for any carrier with six
// or more fields, so putting the array's byte offset in slot 5 would hand every
// raw-pointer consumer in the tree a small integer to dereference — which is
// exactly the defect F27 closed one level down (a heap segment's `length`
// answered as its address, `0x10`). Slot 0 stays 0 here so `segment_address`
// keeps answering 0, the value every consumer already refuses on.
pub(in crate::panama) const SEG_HEAP_BASE_FIELD: usize = 6;
pub(crate) const SEG_HEAP_START_FIELD: usize = 7;
pub(in crate::panama) const SEG_HEAP_FIELDS: usize = 8;

/// Where an `ofArray` MIRROR carrier used to keep its session.
///
/// RETIRED 2026-09-21: nothing mints this shape any more. `ofArray` unified
/// all seven primitive array types onto [`pe_of_array_alias`]'s carrier
/// (`SEG_HEAP_BASE_FIELD`/`SEG_HEAP_START_FIELD`), so int[]/long[]/float[]/
/// double[] no longer need a slot of their own for the session — they use
/// slot 2 the same way byte[]/short[]/char[] always did. Kept, with
/// [`SEG_MIRROR_FIELDS`] and [`pe_mirror_session`], purely as a defensive
/// recognizer: [`pe_mirror_session`] now always answers `None` (nothing is
/// ever `SEG_MIRROR_FIELDS` wide any more), and its two callers in
/// `phases_late/foreign_ffm.rs` fall through to their own alias-carrier
/// handling, unchanged.
pub(in crate::panama) const SEG_MIRROR_SESSION_FIELD: usize = 6;

/// How wide an `ofArray` MIRROR carrier used to be. See
/// [`SEG_MIRROR_SESSION_FIELD`] — retired, kept only so [`pe_mirror_session`]
/// still compiles as an inert recognizer.
pub(in crate::panama) const SEG_MIRROR_FIELDS: usize = 7;

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
pub(in crate::panama) const HEAP_ARRAY_BASE_OFFSET: i64 = cratonvm_native_io::ARRAY_BYTE_BASE_OFFSET;

/// `native-api`'s `ffi::UpcallEntry` `return_kind` for a **void** upcall (the
/// `register_upcall` table this file's tests still exercise; the stubs
/// `Linker.upcallStub` makes since round 12 wave 7 live in `panama_upcall.rs`
/// and spell a void return as `UpcallShape::ret == None`, so production code
/// no longer reads this constant).
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
pub(in crate::panama) const UPCALL_RETURN_VOID: i32 = -1;

/// Maximum length to scan when reading a C string from native memory.
pub(in crate::panama) const MAX_CSTR_LEN: usize = 4096;

/// Native-access policy for Panama downcalls and raw-address memory access.
///
/// Linking a downcall is a restricted method. JDK 25 applies the caller's
/// module grant and allow/warn/deny policy at that point. Invoking an accepted
/// handle validates its address and host/capability permissions, without a
/// second restricted-method warning or a new module-grant requirement.
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
/// ### Caller-module resolution
///
/// [`restricted_downcall_link_check`] resolves the Java caller through the
/// security stack walk and reads its actual Module mirror. The JDK's own
/// `Module.ensureNativeAccess` records warn-mode grants on that mirror. Other
/// legacy gates still use [`native_access_enabled`]'s VM-wide view of the
/// grant (per VM since zt t18/df7: [`NativeAccessState`]).
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
    /// VM-wide gate (see the type docs for why the caller module is
    /// not available at the gate point).
    pub(in crate::panama) fn any_granted(&self) -> bool {
        match self {
            NativeAccessPolicy::None => false,
            NativeAccessPolicy::All => true,
            NativeAccessPolicy::Modules(m) => !m.is_empty(),
        }
    }

    /// Whether the given module is granted native access. `None` denotes the
    /// unnamed module. This is the per-module query the JDK actually performs;
    /// it is exposed now so a future caller-module-aware gate can use it.
    pub(in crate::panama) fn module_granted(&self, module: Option<&str>) -> bool {
        match self {
            NativeAccessPolicy::None => false,
            NativeAccessPolicy::All => true,
            NativeAccessPolicy::Modules(m) => m.contains(module.unwrap_or("")),
        }
    }

    /// The grant of the bare / unscoped `--enable-native-access` (`true`:
    /// [`NativeAccessPolicy::All`]) or of none (`false`). Never a grant under
    /// `CRATONVM_UNTRUSTED_CODE`.
    pub fn from_enabled(enabled: bool) -> Self {
        if enabled && !cratonvm_types::flags::flags().io.untrusted_code {
            NativeAccessPolicy::All
        } else {
            NativeAccessPolicy::None
        }
    }

    /// The grant of `--enable-native-access=<module-list>` (the list split on
    /// `,`). The sentinels `ALL-UNNAMED`, `ALL-MODULES`, and an
    /// empty/whitespace-only list preserve the legacy bare-flag grant.
    /// `ALL-UNNAMED` records only the unnamed module; it must not grant an
    /// unrelated named caller native access. Each named module is recorded
    /// individually. Never a grant under `CRATONVM_UNTRUSTED_CODE`.
    pub fn from_module_list<I, S>(modules: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if cratonvm_types::flags::flags().io.untrusted_code {
            return NativeAccessPolicy::None;
        }
        let mut set = std::collections::BTreeSet::new();
        let mut grant_all = false;
        for m in modules {
            let name = m.as_ref().trim();
            if name.is_empty() {
                continue;
            }
            if name.eq_ignore_ascii_case("ALL-UNNAMED") {
                set.insert(String::new());
                continue;
            }
            if name.eq_ignore_ascii_case("ALL-MODULES") {
                grant_all = true;
                continue;
            }
            set.insert(name.to_string());
        }
        if grant_all || set.is_empty() {
            // A non-empty but all-blank argument is treated like the bare flag.
            NativeAccessPolicy::All
        } else {
            NativeAccessPolicy::Modules(set)
        }
    }
}

/// zt t18/df7 (`docs/feature-designs/native-dispatch-architecture.md`,
/// "What is left": the process globals): the launcher's native-access
/// settings for ONE VM -- `--enable-native-access` ([`NativeAccessPolicy`])
/// and `--illegal-native-access` ([`IllegalNativeAccess`]). Carried by the
/// VM's configuration (`VmConfig::native_access`) and installed into the
/// VM's own [`NativeAccessState`] when the VM is made. The default (no grant,
/// `warn`) is what every embedding entry point had before the move.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeAccessConfig {
    /// `--enable-native-access`.
    pub policy: NativeAccessPolicy,
    /// `--illegal-native-access`.
    pub illegal: IllegalNativeAccess,
}

/// zt t18/df7: one VM's native-access state, owned by its
/// `LoaderIdentityTables` (the per-VM cell the natives reach through
/// `NativeContext::loader_identity_tables`; a mock context gets the detached
/// instance of its VM identity) and read through [`native_access`]. Before,
/// these were four process globals (`NATIVE_ACCESS_POLICY`, its lock-free
/// mirror `NATIVE_ACCESS_ANY_GRANTED`, `ILLEGAL_NATIVE_ACCESS`,
/// `RESTRICTED_WARNED`), so a second VM in the process saw the first one's
/// launcher policy and its warn-once latch (AGENTS.md: no process globals for
/// compatibility state).
#[derive(Debug)]
pub struct NativeAccessState {
    /// The granted modules.
    pub(in crate::panama) policy: std::sync::RwLock<NativeAccessPolicy>,
    /// `policy.any_granted()`, mirrored by [`Self::store_policy`], its one
    /// writer (`r16-ffm-plain-downcall-costs-microseconds`, round 19: the lock
    /// read was 11 % of a direct downcall, which asks on every call through
    /// `jit_downcall_direct_admits`). A cache of the policy, not new state.
    pub(in crate::panama) any_granted: std::sync::atomic::AtomicBool,
    /// `--illegal-native-access`: 0 = Allow, 1 = Warn, 2 = Deny. Plain
    /// integer rather than a lock: read on every restricted call, written
    /// once, before any bytecode runs.
    pub(in crate::panama) illegal: std::sync::atomic::AtomicU8,
    /// The no-caller fallback's warn-once latch
    /// ([`restricted_method_check_owned`]): HotSpot prints its
    /// restricted-method warning ONCE per calling module, and the only module
    /// that fallback can attribute to is the unnamed one, so it fires once per
    /// VM.
    pub(in crate::panama) restricted_warned: std::sync::atomic::AtomicBool,
    /// The modules `--compatible`'s `Reflection.ensureNativeAccess` native
    /// (`deprecated_internal::native_reflection_ensure_native_access`) has
    /// warned about, once each (a process `OnceLock` before zt t18/df7).
    pub(in crate::panama) warned_modules: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

impl Default for NativeAccessState {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeAccessState {
    /// No grant, `warn`: the state of a VM whose launcher passed neither flag.
    pub fn new() -> Self {
        Self {
            policy: std::sync::RwLock::new(NativeAccessPolicy::None),
            any_granted: std::sync::atomic::AtomicBool::new(false),
            illegal: std::sync::atomic::AtomicU8::new(1),
            restricted_warned: std::sync::atomic::AtomicBool::new(false),
            // The guard only inserts an owned name; warning output follows its release.
            warned_modules: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        }
    }

    /// Install the VM's launcher settings (`VmConfig::native_access`), once,
    /// when the VM is made.
    pub fn install(&self, config: &NativeAccessConfig) {
        self.store_policy(config.policy.clone());
        self.set_illegal(config.illegal);
    }

    /// Replace the grant wholesale.
    pub fn store_policy(&self, policy: NativeAccessPolicy) {
        if let Ok(mut guard) = self.policy.write() {
            // Written under the policy's write lock, so the mirror never
            // answers for a policy other than the one stored ([`Self::enabled`]).
            self.any_granted
                .store(policy.any_granted(), std::sync::atomic::Ordering::Release);
            *guard = policy;
        }
    }

    /// The grant now (a copy).
    pub fn policy(&self) -> NativeAccessPolicy {
        self.policy
            .read()
            .map(|p| p.clone())
            .unwrap_or(NativeAccessPolicy::None)
    }

    /// [`NativeAccessPolicy::from_enabled`], stored.
    pub fn set_enabled(&self, enabled: bool) {
        self.store_policy(NativeAccessPolicy::from_enabled(enabled));
    }

    /// [`NativeAccessPolicy::from_module_list`], stored.
    pub fn set_modules<I, S>(&self, modules: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.store_policy(NativeAccessPolicy::from_module_list(modules));
    }

    /// Set the `--illegal-native-access` mode.
    pub fn set_illegal(&self, mode: IllegalNativeAccess) {
        let v = match mode {
            IllegalNativeAccess::Allow => 0,
            IllegalNativeAccess::Warn => 1,
            IllegalNativeAccess::Deny => 2,
        };
        self.illegal.store(v, std::sync::atomic::Ordering::Relaxed);
    }

    /// The `--illegal-native-access` mode.
    pub fn illegal(&self) -> IllegalNativeAccess {
        match self.illegal.load(std::sync::atomic::Ordering::Relaxed) {
            0 => IllegalNativeAccess::Allow,
            2 => IllegalNativeAccess::Deny,
            _ => IllegalNativeAccess::Warn,
        }
    }

    /// Whether *any* module is granted native access (see
    /// [`native_access_enabled`]): one load of the mirror.
    #[inline]
    pub fn enabled(&self) -> bool {
        if cratonvm_types::flags::flags().io.untrusted_code {
            return false;
        }
        self.any_granted.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether the named module (`None`: the unnamed module) is granted.
    pub fn module_granted(&self, module: Option<&str>) -> bool {
        self.policy
            .read()
            .map(|p| p.module_granted(module))
            .unwrap_or(false)
    }

    /// `true` the first time per VM that `module` (`ALL-UNNAMED` for the
    /// unnamed one) is warned about by `Reflection.ensureNativeAccess`'s
    /// native.
    pub fn first_warning_for_module(&self, module: &str) -> bool {
        self.warned_modules
            .lock()
            .map(|mut seen| seen.insert(module.to_string()))
            .unwrap_or(false)
    }

    /// `true` once per VM: the no-caller fallback's first warning.
    pub(in crate::panama) fn first_restricted_warning(&self) -> bool {
        !self
            .restricted_warned
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    }
}

/// zt t18/df7: THIS context's VM's [`NativeAccessState`] (its
/// `LoaderIdentityTables`'; a mock context's detached one).
#[inline]
pub fn native_access(ctx: &dyn NativeContext) -> &NativeAccessState {
    crate::classloader::loader_tables(ctx).native_access()
}

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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum IllegalNativeAccess {
    /// Permit silently.
    Allow,
    /// Permit, and warn once — the JDK 25 default, and ours.
    #[default]
    Warn,
    /// Throw `IllegalCallerException`, as `--illegal-native-access=deny` does.
    Deny,
}

/// This context's VM's `--illegal-native-access` mode
/// ([`NativeAccessState::illegal`]; the launcher's `VmConfig::native_access`).
pub fn illegal_native_access(ctx: &dyn NativeContext) -> IllegalNativeAccess {
    native_access(ctx).illegal()
}

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
pub(in crate::panama) fn restricted_method_check(
    ctx: &mut dyn NativeContext,
    method: &str,
) -> Result<(), MethodCallFailed> {
    restricted_method_check_owned(ctx, "java.lang.foreign.MemorySegment", method)
}

/// [`restricted_method_check`] for a restricted method of `owner` (dotted
/// binary name), which the warning text names -- HotSpot's
/// `Module.ensureNativeAccess` prints the method's own class, not always
/// `MemorySegment`.
pub(in crate::panama) fn restricted_method_check_owned(
    ctx: &mut dyn NativeContext,
    owner: &str,
    method: &str,
) -> Result<(), MethodCallFailed> {
    if !native_access_enabled(&*ctx) {
        // `untrusted_code` is NOT the JDK flag and must not be relaxed by it.
        // `native_access_enabled` already returns false in that mode, so
        // without this the new `Warn` default would let a restricted call
        // through where the old unconditional refusal stopped it — the one
        // place this change could have weakened something rather than aligned
        // it. Sandbox first, JDK policy second.
        if cratonvm_types::flags::flags().io.untrusted_code {
            return Err(RuntimeError::IllegalCallerException {
                message: "Native access is not enabled for this module (untrusted-code mode)"
                    .into(),
            }
            .into());
        }
        match illegal_native_access(&*ctx) {
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
                if native_access(&*ctx).first_restricted_warning() {
                    eprintln!("WARNING: A restricted method in {owner} has been called");
                    eprintln!(
                        "WARNING: {owner}::{method} has been called by code in an unnamed module"
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

/// Whether Panama native downcalls are currently permitted for *any* module
/// of this context's VM ([`NativeAccessState::enabled`]).
///
/// This is the coarse, VM-wide view the gate helpers use today because the
/// calling module is not reachable at the gate point (see
/// [`NativeAccessPolicy`]). It fails closed: it returns `false` whenever no
/// module has been granted access.
#[inline]
pub fn native_access_enabled(ctx: &dyn NativeContext) -> bool {
    native_access(ctx).enabled()
}

/// Whether the named module is granted native access in this context's VM.
/// `None` denotes the unnamed module. This is the per-module query the JDK
/// performs; it is the intended entry point for a future caller-module-aware
/// gate once the calling module is plumbed to the native dispatch boundary.
pub fn module_native_access_enabled(ctx: &dyn NativeContext, module: Option<&str>) -> bool {
    native_access(ctx).module_granted(module)
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
pub(in crate::panama) fn require_native_access(ctx: &mut dyn NativeContext, op: &str) -> Result<(), MethodCallFailed> {
    if !native_access_enabled(&*ctx) {
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
/// `MemorySegment.copy(Object, int, MemorySegment, ValueLayout, long, int)`'s
/// range refusal: `Objects.checkFromIndexSize(srcIndex, elementCount,
/// Array.getLength(srcArray))`'s IOOBE, "Range [i, i + c) out of bounds for
/// length n", from the raw `int` arguments (`args[1]`, `args[5]`). Interpreter
/// round i5 wave 100, lane L7.
pub(in crate::panama) fn panama_copy_range_error(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    args: &[Value],
) -> MethodCallFailed {
    let int_at = |i: usize| match args.get(i) {
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    };
    let length = ctx.array_length(src) as i64;
    RuntimeError::ioobe(
        cratonvm_types::error::out_of_bounds_message::check_from_index_size(
            int_at(1),
            int_at(5),
            length,
        ),
    )
    .into()
}

pub(in crate::panama) fn require_segment_access(ctx: &mut dyn NativeContext, op: &str) -> Result<(), MethodCallFailed> {
    crate::security_manager::check_host_native_access_or_throw(ctx, "foreign")?;
    crate::capability_gate::gate_raw_memory_named(&*ctx, op)?;
    Ok(())
}

/// A native Linker bridge has to perform the check its Java caller-sensitive
/// body would have performed. Let Module's real bytecode announce the warning
/// and record it in the caller's per-VM module mirror.
pub(crate) fn restricted_downcall_link_check(
    ctx: &mut dyn NativeContext,
) -> Result<(), MethodCallFailed> {
    restricted_caller_check(ctx, "java/lang/foreign/Linker", "downcallHandle")
}

/// zt t12/df4: `CRATONVM_FFM_UPCALL_STUB_NAMES_ITSELF` (default ON, `=0`
/// kill; read per stub creation, which is rare). The reviewed `Intrinsic` on
/// `AbstractLinker.upcallStub` (`ffm_fallback_linker::intrinsic_upcall_stub`)
/// ran the DOWNCALL check ([`restricted_downcall_link_check`]) first, so the
/// JDK's `Module.ensureNativeAccess` printed `Linker::downcallHandle has been
/// called by <caller>` for a program whose first restricted call is
/// `upcallStub` (row `pr_P1_*`, `Zs3Nd3UpcallMints`, 6 of 6 runs), where
/// HotSpot's `AbstractLinker.upcallStub` names `Linker::upcallStub`. The
/// warning is printed once per module, so the later `pe_upcall_stub` gate,
/// which already names `upcallStub`, never printed. With the switch on the
/// intrinsic runs [`restricted_upcall_stub_check`]; `=0` restores the
/// downcall check. Every collector (not a GC switch).
pub(crate) fn upcall_stub_names_itself() -> bool {
    true
}

/// zt t12/df4: the caller-module check `AbstractLinker.upcallStub` opens
/// with, `Reflection.ensureNativeAccess(caller, Linker.class, "upcallStub",
/// false)`: [`restricted_linker_check`] for `upcallStub`.
pub(crate) fn restricted_upcall_stub_check(
    ctx: &mut dyn NativeContext,
) -> Result<(), MethodCallFailed> {
    restricted_linker_check(ctx, "upcallStub")
}



/// The `<mod>` of HotSpot 25's `Module.ensureNativeAccess` (Module.java,
/// `ensureNativeAccess`): `"module " + getName()` for a named module, else
/// `"an unnamed module"`; then `" (" + url + ")"` when the calling class's
/// `ProtectionDomain` has a `CodeSource` with a location. Pure, so the shape
/// is unit-tested (`zt12df4_the_deny_text_is_hotspots`).
pub(crate) fn illegal_native_access_module_text(
    module_name: Option<&str>,
    location: Option<&str>,
) -> String {
    let mut text = match module_name {
        Some(name) => format!("module {name}"),
        None => "an unnamed module".to_string(),
    };
    if let Some(location) = location {
        text.push_str(" (");
        text.push_str(location);
        text.push(')');
    }
    text
}

/// The location `Module.ensureNativeAccess` appends for `caller_mirror`:
/// `getProtectionDomain().getCodeSource().getLocation().toString()`, or
/// `None` at the first null. Runs Java (a GC point): `caller_mirror` must be
/// current at entry, and nothing the caller holds is re-read here.
pub(in crate::panama) fn caller_code_source_location(
    ctx: &mut dyn NativeContext,
    caller_mirror: ObjectRef,
) -> Result<Option<String>, MethodCallFailed> {
    let Some(Value::Object(Some(domain))) = ctx.invoke_virtual(
        caller_mirror,
        "getProtectionDomain",
        "()Ljava/security/ProtectionDomain;",
        &[],
    )?
    else {
        return Ok(None);
    };
    let Some(Value::Object(Some(source))) =
        ctx.invoke_virtual(domain, "getCodeSource", "()Ljava/security/CodeSource;", &[])?
    else {
        return Ok(None);
    };
    let Some(Value::Object(Some(url))) =
        ctx.invoke_virtual(source, "getLocation", "()Ljava/net/URL;", &[])?
    else {
        return Ok(None);
    };
    match ctx.invoke_virtual(url, "toString", "()Ljava/lang/String;", &[])? {
        Some(Value::Object(Some(text))) => Ok(ctx.read_string(text)),
        _ => Ok(None),
    }
}

/// The same caller-module check for any restricted `Linker` method
/// (`downcallHandle`, `upcallStub`): HotSpot enables native access for the JDK's
/// own modules (the `java.desktop` `HBShaper` binds upcall stubs), and warns once
/// per other module (SBR5 lane awtjfr).
pub(in crate::panama) fn restricted_linker_check(
    ctx: &mut dyn NativeContext,
    method_name: &str,
) -> Result<(), MethodCallFailed> {
    restricted_caller_check(ctx, "java/lang/foreign/Linker", method_name)
}

/// The caller-sensitive restricted-method check a native bridge for
/// `owner.method` must perform: the JDK's own
/// `Reflection.ensureNativeAccess(caller, owner, method, false)`, i.e.
/// `Module.ensureNativeAccess` on the calling frame's module -- allow
/// silently, warn once per module (HotSpot 25's default
/// `--illegal-native-access=warn`), or throw `IllegalCallerException` under
/// `deny`. Generalised from [`restricted_downcall_link_check`] in round 18
/// wave 11 (lane backlogfix, R16B-174) so `SymbolLookup.libraryLookup`'s
/// bridge stops refusing outright where HotSpot only warns.
pub(in crate::panama) fn restricted_caller_check(
    ctx: &mut dyn NativeContext,
    owner_internal: &str,
    method_name: &str,
) -> Result<(), MethodCallFailed> {
    let owner_dotted = owner_internal.replace('/', ".");
    if cratonvm_types::flags::flags().io.untrusted_code {
        return restricted_method_check_owned(ctx, &owner_dotted, method_name);
    }
    let Some(caller) = crate::stack_walker::native_bridge_caller_class_id(ctx) else {
        return restricted_method_check_owned(ctx, &owner_dotted, method_name);
    };
    let caller_mirror = ctx.get_class_mirror(caller);
    let pin = ctx.pin_native_root(caller_mirror);
    let result = (|| {
        let caller_mirror = ctx.read_native_pin(pin, caller_mirror);
        let module =
            match ctx.invoke_virtual(caller_mirror, "getModule", "()Ljava/lang/Module;", &[])? {
                Some(Value::Object(Some(module))) => module,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "native caller has no module".into(),
                    }
                    .into())
                }
            };
        let module_pin = ctx.pin_native_root(module);
        let enabled = matches!(
            ctx.invoke_virtual(module, "isNativeAccessEnabled", "()Z", &[])?,
            Some(Value::Int(1))
        );
        if enabled || illegal_native_access(&*ctx) == IllegalNativeAccess::Allow {
            return crate::security_manager::check_host_native_access_or_throw(ctx, "foreign");
        }
        if illegal_native_access(&*ctx) == IllegalNativeAccess::Deny {
            let module = ctx.read_native_pin(module_pin, module);
            {
                // zt t14/df5: when the JDK sees `deny` too (the launcher's mode
                // reaches it as `jdk.module.illegal.native.access`,
                // `CRATONVM_FFM_ILLEGAL_NATIVE_ACCESS_PROPERTY`), the JDK's own
                // `Module.ensureNativeAccess` throws HotSpot's refusal, location
                // and all: the one text builder the JDK-body restricted methods
                // (`MemorySegment.reinterpret`, row `zt12df4_f4`) already use.
                // The VM-side builder below dropped the ` (<code source
                // location>)` suffix on the `Linker` bridges (t12 rows
                // `zt12df4_f3` / `_c`). It stays as the fallback for a JDK that
                // does not see `deny` (the property kill, or a user `-D`
                // naming another mode), which returns here after its own
                // warn / allow answer.
                if illegal_native_access_reaches_jdk() {
                    jdk_module_ensure_native_access(
                        ctx,
                        owner_internal,
                        method_name,
                        (module_pin, module),
                        (pin, caller_mirror),
                    )?;
                }
                let module = ctx.read_native_pin(module_pin, module);
                // zt t12/df4: HotSpot's `Module.ensureNativeAccess` text.
                let name =
                    match ctx.invoke_virtual(module, "getName", "()Ljava/lang/String;", &[])? {
                        Some(Value::Object(Some(text))) => ctx.read_string(text),
                        _ => None,
                    };
                let caller_mirror = ctx.read_native_pin(pin, caller_mirror);
                let location = caller_code_source_location(ctx, caller_mirror)?;
                let description =
                    illegal_native_access_module_text(name.as_deref(), location.as_deref());
                return Err(RuntimeError::IllegalCallerException {
                    message: format!("Illegal native access from {description}"),
                }
                .into());
            }
        }
        jdk_module_ensure_native_access(
            ctx,
            owner_internal,
            method_name,
            (module_pin, module),
            (pin, caller_mirror),
        )?;
        crate::security_manager::check_host_native_access_or_throw(ctx, "foreign")?;
        Ok(())
    })();
    ctx.unpin_native_roots(pin);
    result
}

/// zt t14/df5: whether the JDK's `ModuleBootstrap` was handed the launcher's
/// `--illegal-native-access` mode (`vm_init`'s
/// `illegal_native_access_property_value`, the same switch,
/// `CRATONVM_FFM_ILLEGAL_NATIVE_ACCESS_PROPERTY`, default ON). Under `deny`
/// with it on, the JDK's `Module.ensureNativeAccess` throws HotSpot's
/// refusal itself; with it off the JDK sees `warn` and would print the
/// warning lines before the VM's refusal.
pub(in crate::panama) fn illegal_native_access_reaches_jdk() -> bool {
    true
}

/// The JDK's own `module.ensureNativeAccess(owner, method_name, caller,
/// false)` -- `Reflection.ensureNativeAccess`'s body -- for the native bridge
/// of `owner_internal.method_name`: allow, warn once per module, or (when the
/// JDK sees `deny`) throw HotSpot's `IllegalCallerException`. `module` and
/// `caller` are `(pin, last value)` pairs of [`restricted_caller_check`]'s
/// pins and are re-read after the owner's initialisation (a GC point).
pub(in crate::panama) fn jdk_module_ensure_native_access(
    ctx: &mut dyn NativeContext,
    owner_internal: &str,
    method_name: &str,
    module: (usize, ObjectRef),
    caller: (usize, ObjectRef),
) -> Result<(), MethodCallFailed> {
    let owner = ctx.ensure_class_initialized(owner_internal)?;
    let owner_mirror = ctx.get_class_mirror(owner);
    let owner_pin = ctx.pin_native_root(owner_mirror);
    let result = (|| -> Result<(), MethodCallFailed> {
        let method = ctx.intern_string(method_name);
        let module = ctx.read_native_pin(module.0, module.1);
        let owner_mirror = ctx.read_native_pin(owner_pin, owner_mirror);
        let caller_mirror = ctx.read_native_pin(caller.0, caller.1);
        ctx.invoke_virtual(
            module,
            "ensureNativeAccess",
            "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;Z)V",
            &[
                Value::Object(Some(owner_mirror)),
                Value::Object(Some(method)),
                Value::Object(Some(caller_mirror)),
                Value::Int(0),
            ],
        )?;
        Ok(())
    })();
    ctx.unpin_native_roots(owner_pin);
    result
}

/// Validate a target of an already linked handle. Native-access policy is
/// checked at handle creation; invoking a granted handle is not restricted.
pub(in crate::panama) fn validated_fn_ptr<T>(fn_addr: i64) -> Result<T, MethodCallFailed>
where
    T: Copy,
{
    validated_fn_ptr_with(fn_addr, false)
}



/// `CRATONVM_FFM_LIBRARY_LOOKUP_RESTRICTED_WARN` (default ON, `=0` off; JIT
/// round 18 wave 11, lane backlogfix, R16B-174): `SymbolLookup.libraryLookup`
/// without `--enable-native-access` follows HotSpot 25's
/// `--illegal-native-access` policy -- warn once by default -- instead of
/// throwing `IllegalCallerException`.
///
/// JDK 25's `libraryLookup` is `@Restricted` and `@CallerSensitive`: its body
/// calls `Reflection.ensureNativeAccess(caller, SymbolLookup.class,
/// "libraryLookup", false)` (warn / allow / deny on the caller's module), then
/// `RawNativeLibraries.load`, whose `load0` native performs no access check of
/// its own. CratonVM's `load0` refused through [`require_native_access`] --
/// the raw-address accessor gate, all-or-nothing on the process grant -- so
/// `--real-jdk` printed the JDK's warning and THEN threw, and the
/// `--compatible` bridge threw without warning. On:
///
/// * `RawNativeLibraries.load0` keeps only the sandbox and host-policy rows
///   ([`raw_library_load_check`]); the policy decision was already made by
///   the Java caller, exactly once, as on HotSpot (and the JDK's own
///   `SystemLookup` / `defaultLookup()` path, which is not restricted, loads
///   through the same native);
/// * the `SymbolLookup.libraryLookup` bridge runs [`restricted_caller_check`]
///   (the caller module's `Module.ensureNativeAccess`, as the downcall link
///   bridge already does).
///
/// `=0` restores [`require_native_access`] at both sites.
pub(in crate::panama) fn ffm_library_lookup_restricted_warn() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        true
    })
}

/// `RawNativeLibraries.load0`'s gate under
/// [`ffm_library_lookup_restricted_warn`]: the untrusted-code sandbox still
/// refuses (it is not the JDK flag and must not be relaxed by it, see
/// [`restricted_method_check_owned`]), and the host-policy / capability rows
/// still run; the `--enable-native-access` question belongs to the Java
/// caller.
pub(in crate::panama) fn raw_library_load_check(ctx: &mut dyn NativeContext) -> Result<(), MethodCallFailed> {
    if cratonvm_types::flags::flags().io.untrusted_code {
        return Err(RuntimeError::IllegalCallerException {
            message: "Native access is not enabled for this module (untrusted-code mode)".into(),
        }
        .into());
    }
    crate::security_manager::check_host_native_access_or_throw(ctx, "foreign")?;
    crate::capability_gate::gate_raw_memory_named(&*ctx, "libraryLookup")?;
    Ok(())
}

/// [`validated_fn_ptr`] with the alignment refusal chosen by the caller (a
/// unit test runs both arms).
pub(in crate::panama) fn validated_fn_ptr_with<T>(fn_addr: i64, align_check: bool) -> Result<T, MethodCallFailed>
where
    T: Copy,
{
    let addr = fn_addr as usize;
    if addr == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Null function pointer in downcall".into(),
        }
        .into());
    }
    if align_check && addr % std::mem::align_of::<usize>() != 0 {
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

pub(in crate::panama) fn register_pe_value_layout(r: &mut NativeMethodRegistry) {
    let vl = "java/lang/foreign/ValueLayout";

    // NINE ROWS WERE DELETED HERE, AND THE SIX BELOW THEM WITH THEM
    // (F16, 2026-08-13). What stood here was:
    //
    //     r.register(vl, "JAVA_BYTE",  "()Ljava/lang/foreign/ValueLayout;", …, cratonvm_native_api::NativeKind::Bridge)
    //     r.register(vl, "JAVA_SHORT", "()Ljava/lang/foreign/ValueLayout;", …, cratonvm_native_api::NativeKind::Bridge)
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

/// Each VM's global-arena root handle; no row before its first `Arena.global()`.
///
/// `LockLevel::Scratch` (L0) is a LEAF: takeable while holding anything, and
/// nothing takeable while holding it. Every `ctx` call in the caller happens
/// outside this lock's scope, which is the property L0 states and the order
/// checker enforces -- a lock in this crate held across a VM call is a cycle
/// through the heap and class-manager locks.
///
/// gc-common w20-f: keyed by `vm_identity`. This was ONE process-wide
/// `Option<usize>`, but the handle indexes the publishing VM's own global-ref
/// table (`add_global_root`), so a second VM in the process resolved VM A's
/// index in ITS table -- an unrelated object handed out as `Arena.global()`,
/// or `None` and then a lost claim that answered its own fresh arena on every
/// call -- and the row outlived VM A's teardown. Dropped by
/// [`forget_vm_ffm_singletons`].
pub(in crate::panama) fn global_arena_cell(// addr-keyed: not a heap address -- VM identity -> global-root handle; dropped by forget_vm_ffm_singletons.
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashMap<usize, usize>> {
    use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
    // addr-keyed: the accessor's table (see above).
    static CELL: std::sync::OnceLock<OrderedPlMutex<std::collections::HashMap<usize, usize>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch))
}

/// `vm`'s published handle, if any. A function so the guard cannot outlive the
/// map read. `if let Some(h) = cell.lock() { ctx.something(h) }` keeps the
/// guard alive for the whole statement, which is a native-builtins lock held
/// across a re-entry into the VM -- the edge `lock_discipline_ratchet` exists
/// to keep out of this crate. `shared_secrets_bridge` carries the same note on
/// its own copy of this shape; two lanes wrote it the same day.
pub(crate) fn global_arena_handle(vm: usize) -> Option<usize> {
    global_arena_cell().lock().get(&vm).copied()
}

/// Publish `handle` as `vm`'s unless someone already did; returns the winner if
/// not ours.
pub(crate) fn claim_global_arena(vm: usize, handle: usize) -> Option<usize> {
    let mut cell = global_arena_cell().lock();
    match cell.get(&vm) {
        Some(existing) => Some(*existing),
        None => {
            cell.insert(vm, handle);
            None
        }
    }
}

/// Drop `vm`'s `Arena.global()` row and its other per-VM FFM tables (VM
/// teardown, from `forget_vm_native_root_stores`). The handles index `vm`'s own
/// global-ref table, which dies with it. Idempotent. gc-common w20-f.
pub fn forget_vm_ffm_singletons(vm: usize) {
    global_arena_cell().lock().remove(&vm);
    // Round 14 wave 3 (lane ffm, FFM7-1): the weak handles index `vm`'s own
    // table and the block ids its own native-memory table, both gone with it.
    auto_arena_cell().lock().remove(&vm);
    syslookup_cell().lock().remove(&vm);
}

// ---------------------------------------------------------------------------
// `Arena.ofAuto()` blocks freed once the session is collected (round 14 wave 3,
// lane ffm; FFM7-1 of `jit-r13-ffm7-proposals-RETIRED-20260929.md`)
// ---------------------------------------------------------------------------
//
// An implicit (`Arena.ofAuto()`) session is never closed, so its blocks were
// never freed: every `Arena.ofAuto().allocate(..)` leaked for the life of the
// process, where HotSpot's `ImplicitSession` frees them from its `Cleaner`.
// No JDK `Runnable` can reach `free_native_memory`, so this is the wave-9
// upcall-stub shape instead: each block id of an implicit session is kept in a
// per-VM row next to a WEAK global handle on the session, and a sweep at a
// later auto-arena allocation frees the ids of every row whose handle a
// collection cleared. Every segment of the arena holds the session (slot 2),
// a real `DirectByteBuffer` view holds its segment, and a downcall acquires
// its arguments' sessions, so a cleared handle means no path this VM models
// still reaches the block -- HotSpot's contract, where a raw `address()` kept
// in a `long` is the program's problem.
//
// One ordering HotSpot gives that a sweep would not: its cleaner runs the
// session's Java close actions (a `reinterpret(.., auto, cleanup)` cleanup
// reading the block by address) BEFORE it frees the blocks allocated earlier.
// Here those actions run on the `Common-Cleaner` thread, whenever it gets to
// them, so a row whose session has had a Java action registered is KEPT
// (never freed, the old behaviour) -- `auto_arena_blocks_keep`, called before
// every `Cleaner.register` of an implicit session. The synthetic-JDK
// `asByteBuffer` view, which holds no segment, keeps its session's rows too.
// Since round 14 wave 4 (FFM3W-1, below `auto_arena_blocks_keep`) such a row
// is freed after its last Java action ran instead of kept.
//
// Rows are looked up by the session's identity hash (stable across moves),
// then by resolving the handles of that bucket OUTSIDE the lock: the lock is
// `LockLevel::Scratch`, a leaf, and no `ctx` call happens under it.

/// One implicit session's recorded blocks.
pub(in crate::panama) struct AutoArenaBlocks {
    /// WEAK global handle on the session (never a root).
    pub(in crate::panama) handle: usize,
    pub(in crate::panama) ids: Vec<i64>,
    /// A Java close action was registered on the session: never free.
    pub(in crate::panama) kept: bool,
    /// Java close actions registered through a `CratonVM$FfmAutoSessionAction`
    /// wrapper that have not run yet (round 14 wave 4, lane ffm4, FFM3W-1).
    /// A row with any is never taken by a sweep; the last one to run frees it
    /// (`auto_arena_action_done`).
    pub(in crate::panama) pending: usize,
    /// The blocks of a CLOSED SHARED session, filed by its close walk
    /// ([`shared_close_defer_free`], round 15 wave 3, lane ffm2): never
    /// pending, freed by a sweep once the session's handle cleared -- unless
    /// an address-only view was ever made ([`AutoArenaTable::address_only_views`]).
    pub(in crate::panama) closed_shared: bool,
}

/// A VM's rows.
pub(in crate::panama) struct AutoArenaTable {
    /// Session identity hash -> rows (almost always one).
    pub(in crate::panama) rows: std::collections::HashMap<i32, Vec<AutoArenaBlocks>>,
    /// Rows in `rows`.
    pub(in crate::panama) len: usize,
    /// Rows added since the last sweep.
    pub(in crate::panama) added_since_sweep: usize,
    /// Rows that survived the last sweep.
    pub(in crate::panama) after_sweep: usize,
    /// A sweep of this VM is between [`auto_arena_claim_sweep`] and
    /// [`auto_arena_take_dead`] (round 14 wave 4, lane review4). One sweeper
    /// at a time: a weak handle is a boxed address the JNI table reuses once
    /// released, so a second sweeper's stale `dead` set could name a handle
    /// the first one released and a new live session's row re-filed -- and
    /// free that live row's blocks.
    pub(in crate::panama) sweeping: bool,
    /// `CRATONVM_FFM_AUTO_ARENA_TAKE_ONE` (round 15 wave 2, lane ffm; read
    /// when the table is made): a wrapped action's last run takes only its
    /// own row ([`auto_arena_take_row`]), not a walk of every row, and a
    /// sweep does not resolve rows with an action pending
    /// ([`auto_arena_snapshot`]).
    pub(in crate::panama) take_one: bool,
    /// `CRATONVM_FFM_AUTO_ARENA_GC_PACED` (round 15 wave 2, lane ffm, FFM3W-2;
    /// read when the table is made): the first auto allocation after a
    /// collection sweeps (see [`auto_arena_sweep_due`]).
    pub(in crate::panama) gc_paced: bool,
    /// The collection count the last sweep started at (`None`: no sweep yet;
    /// the first check adopts the count without sweeping).
    pub(in crate::panama) swept_at_gc: Option<u64>,
    /// `CRATONVM_FFM_AUTO_ARENA_GC_BUDGET_MB` in bytes (FFM3W-2; 0 = off, the
    /// default): auto-arena bytes allocated with no collection in between
    /// after which the next `Arena.allocate` of an auto arena asks for one
    /// ([`auto_arena_relieve_native_pressure`]).
    pub(in crate::panama) gc_budget: u64,
    /// Auto-arena bytes recorded since the collection count was `bytes_at_gc`.
    pub(in crate::panama) bytes_since_gc: u64,
    pub(in crate::panama) bytes_at_gc: u64,
    /// The collection count a pressure request was last raised at: one
    /// request per collection epoch, however much more is allocated.
    pub(in crate::panama) pressure_raised_at_gc: Option<u64>,
    /// A request raised and not yet served.
    pub(in crate::panama) pressure_due: bool,
    /// `CRATONVM_FFM_AUTO_ARENA_LAST_ACTION_FREE` (round 15 wave 3, lane ffm2,
    /// FFR2-2; read when the table is made): a wrapped action's last run
    /// takes its row in the same locked step that counts it off
    /// ([`auto_arena_finish_action`]), with no sweep claim and no weak-handle
    /// check.
    pub(in crate::panama) last_action_free: bool,
    /// Handles of rows [`auto_arena_finish_action`] took while a sweep was
    /// running: released by the sweep's own take, which ends its claim, so no
    /// handle is reused while a sweep's `dead` set may still name it.
    pub(in crate::panama) deferred_release: Vec<usize>,
    /// A synthetic-JDK `asByteBuffer` view (it holds the address, not the
    /// segment) was made over a non-implicit session's memory (round 15 wave
    /// 3, lane ffm2). Sticky: from then on a closed shared session's blocks
    /// are never freed (the pre-wave-3 leak), since such a view can outlive
    /// the session. Real-JDK views hold their segment and never set it. Since
    /// round 15 wave 5 (lane upcall1) a confined close and the synthetic
    /// four-slot arena's close free nothing either ([`address_only_view_made`]).
    pub(in crate::panama) address_only_views: bool,
    /// `CRATONVM_FFM_AUTO_ARENA_CLEANER_CACHE` (round 15 wave 3, lane ffm2,
    /// FFR2-3; read when the table is made) and the cached strong global
    /// handle on `CleanerFactory.cleaner()` (0: none yet).
    pub(in crate::panama) cleaner_cache: bool,
    pub(in crate::panama) cleaner_handle: usize,
    /// `CRATONVM_FFM_AUTO_ARENA_ACTION_CLASS_CACHE` (round 15 wave 5, lane
    /// upcall1, FF2W3-5; read when the table is made) and whether
    /// [`auto_arena_action_wrapper`] has ensured `CratonVM$FfmAutoSessionAction`
    /// in this VM. A `VmInternal` class is never unloaded, and the table goes
    /// with the VM ([`forget_vm_ffm_singletons`]), so the answer cannot go stale.
    pub(in crate::panama) action_class_cache: bool,
    pub(in crate::panama) action_class_ensured: bool,
}

impl Default for AutoArenaTable {
    /// Made once per VM, under the table lock: the flag reads take only the
    /// flag store's own leaf locks.
    fn default() -> Self {
        let gc_budget = cratonvm_types::flags::runtime_var("CRATONVM_FFM_AUTO_ARENA_GC_BUDGET_MB")
            .ok()
            .and_then(|mb| mb.trim().parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_mul(1 << 20);
        AutoArenaTable {
            rows: std::collections::HashMap::new(),
            len: 0,
            added_since_sweep: 0,
            after_sweep: 0,
            sweeping: false,
            take_one: true,
            gc_paced: true,
            swept_at_gc: None,
            gc_budget,
            bytes_since_gc: 0,
            bytes_at_gc: 0,
            pressure_raised_at_gc: None,
            pressure_due: false,
            last_action_free: true,
            deferred_release: Vec::new(),
            address_only_views: false,
            cleaner_cache: true,
            cleaner_handle: 0,
            action_class_cache: true,
            action_class_ensured: false,
        }
    }
}

/// A sweep runs once this many rows were added since the last one, or as many
/// as survived it if that is more: amortised O(1) resolves per new row.
pub(in crate::panama) const AUTO_ARENA_SWEEP_MIN: usize = 64;

// addr-keyed: not a heap address -- VM identity (rows hold global-root handles).
pub(in crate::panama) type AutoArenaRows = std::collections::HashMap<usize, AutoArenaTable>;

/// Per-VM rows, keyed by `vm_identity` for the reason [`global_arena_cell`] is.
pub(in crate::panama) fn auto_arena_cell() -> &'static cratonvm_types::lock_order::OrderedPlMutex<AutoArenaRows> {
    use cratonvm_types::lock_order::{LockLevel, OrderedPlMutex};
    static CELL: std::sync::OnceLock<OrderedPlMutex<AutoArenaRows>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| OrderedPlMutex::new(std::collections::HashMap::new(), LockLevel::Scratch))
}



/// The handles of `vm`'s rows filed under `hash`. Lock only.
pub(in crate::panama) fn auto_arena_bucket_handles(vm: usize, hash: i32) -> Vec<usize> {
    let cell = auto_arena_cell().lock();
    cell.get(&vm)
        .and_then(|table| table.rows.get(&hash))
        .map(|rows| rows.iter().map(|row| row.handle).collect())
        .unwrap_or_default()
}

/// The handles of the calling VM's rows that resolve to `session`. The
/// resolves run outside the lock.
pub(in crate::panama) fn auto_arena_session_handles(
    ctx: &dyn NativeContext,
    hash: i32,
    session: ObjectRef,
) -> Vec<usize> {
    auto_arena_bucket_handles(ctx.vm_identity(), hash)
        .into_iter()
        .filter(|&handle| ctx.resolve_global_root(handle) == Some(session))
        .collect()
}

/// Append `alloc_id` to the row with `handle`, or file a new row for it when
/// `new_row`. Answers whether the id was recorded. Lock only.
pub(in crate::panama) fn auto_arena_push(vm: usize, hash: i32, handle: usize, alloc_id: i64, new_row: bool) -> bool {
    let mut cell = auto_arena_cell().lock();
    let table = cell.entry(vm).or_default();
    let bucket = table.rows.entry(hash).or_default();
    if let Some(row) = bucket.iter_mut().find(|row| row.handle == handle) {
        row.ids.push(alloc_id);
        return true;
    }
    if !new_row {
        // The row matched a moment ago is gone: only a sweep removes one, and
        // only once its handle cleared, which a live session's cannot. Leak.
        if bucket.is_empty() {
            table.rows.remove(&hash);
        }
        return false;
    }
    bucket.push(AutoArenaBlocks {
        handle,
        ids: vec![alloc_id],
        kept: false,
        pending: 0,
        closed_shared: false,
    });
    table.len += 1;
    table.added_since_sweep += 1;
    true
}

/// Mark the rows with these handles kept. Lock only.
pub(in crate::panama) fn auto_arena_mark_kept(vm: usize, hash: i32, handles: &[usize]) {
    let mut cell = auto_arena_cell().lock();
    if let Some(rows) = cell
        .get_mut(&vm)
        .and_then(|table| table.rows.get_mut(&hash))
    {
        for row in rows.iter_mut().filter(|row| handles.contains(&row.handle)) {
            row.kept = true;
        }
    }
}

/// Whether `vm` is due a sweep, asked after an auto allocation of `bytes`
/// with the collection count `gc` (`None`: count-blind, the round-14 rule
/// only). Lock only.
///
/// Round 15 wave 2 (lane ffm, FFM3W-2): a weak handle clears only in a
/// collection, so a sweep with no collection since the last one finds nothing
/// new, and after one it finds everything that died. The round-14 rule --
/// `max(64, survivors)` new rows -- is kept; the collection rule adds a sweep
/// at the first allocation after each collection (`gc_paced`). That is one
/// resolve per row per collection at most, never more than the collection's
/// own pass over the same weak globals (`watch_weak_global_referents` and the
/// pause's sweep), and it frees a few large blocks after the collection that
/// killed them instead of after 64 more sessions.
///
/// The same call accounts `bytes` for the native-pressure request
/// ([`auto_arena_relieve_native_pressure`]).
pub(in crate::panama) fn auto_arena_sweep_due(vm: usize, gc: Option<u64>, bytes: u64) -> bool {
    let mut cell = auto_arena_cell().lock();
    let Some(table) = cell.get_mut(&vm) else {
        return false;
    };
    if let Some(gc) = gc {
        if table.gc_budget > 0 {
            if table.bytes_at_gc != gc {
                table.bytes_at_gc = gc;
                table.bytes_since_gc = 0;
            }
            table.bytes_since_gc = table.bytes_since_gc.saturating_add(bytes);
            if table.bytes_since_gc >= table.gc_budget && table.pressure_raised_at_gc != Some(gc) {
                table.pressure_raised_at_gc = Some(gc);
                table.pressure_due = true;
            }
        }
        if table.gc_paced {
            let swept_at = table.swept_at_gc;
            match swept_at {
                None => table.swept_at_gc = Some(gc),
                Some(swept) if swept != gc && !table.sweeping && table.len > 0 => return true,
                Some(_) => {}
            }
        }
    }
    table.added_since_sweep >= AUTO_ARENA_SWEEP_MIN.max(table.after_sweep)
}

/// Claim `vm`'s sweep: `false` while another thread's sweep is between its
/// claim and its [`auto_arena_take_dead`] (see [`AutoArenaTable::sweeping`]).
/// Lock only.
pub(in crate::panama) fn auto_arena_claim_sweep(vm: usize) -> bool {
    let mut cell = auto_arena_cell().lock();
    let Some(table) = cell.get_mut(&vm) else {
        return false;
    };
    if table.sweeping {
        return false;
    }
    table.sweeping = true;
    true
}

/// Every handle of `vm`'s rows; starts a new sweep period, at collection
/// count `gc` when known (FFM3W-2). Lock only.
pub(in crate::panama) fn auto_arena_snapshot(vm: usize, gc: Option<u64>) -> Vec<usize> {
    let mut cell = auto_arena_cell().lock();
    let Some(table) = cell.get_mut(&vm) else {
        return Vec::new();
    };
    table.added_since_sweep = 0;
    if gc.is_some() {
        table.swept_at_gc = gc;
    }
    // Round 15 wave 2 (lane ffm, with `take_one`): a row with an action
    // pending is never taken (`auto_arena_take_dead`), so resolving its
    // handle is wasted -- and while a program's cleaner lags, those are most
    // of the rows.
    let skip_pending = table.take_one;
    table
        .rows
        .values()
        .flatten()
        .filter(|row| !(skip_pending && row.pending > 0))
        .map(|row| row.handle)
        .collect()
}

/// Remove `vm`'s rows whose handle is in `dead`. Answers the handles to
/// release and the block ids to free (none from a kept row). Lock only.
pub(in crate::panama) fn auto_arena_take_dead(
    vm: usize,
    // addr-keyed: not an address -- global-root handles.
    dead: &std::collections::HashSet<usize>,
) -> (Vec<usize>, Vec<i64>) {
    let mut released = Vec::new();
    let mut ids = Vec::new();
    let mut cell = auto_arena_cell().lock();
    let Some(table) = cell.get_mut(&vm) else {
        return (released, ids);
    };
    table.sweeping = false;
    // Round 15 wave 3 (lane ffm2): see `AutoArenaTable::address_only_views`.
    let frees_closed_shared = !table.address_only_views;
    for bucket in table.rows.values_mut() {
        bucket.retain(|row| {
            // Round 14 wave 4 (lane ffm4, FFM3W-1): a row with Java actions
            // still to run stays, handle and all -- the last action frees it,
            // and its handle must not be released (and reused) before then.
            if !dead.contains(&row.handle) || row.pending > 0 {
                return true;
            }
            released.push(row.handle);
            if !row.kept && (frees_closed_shared || !row.closed_shared) {
                ids.extend_from_slice(&row.ids);
            }
            false
        });
    }
    table.rows.retain(|_, bucket| !bucket.is_empty());
    table.len = table.len.saturating_sub(released.len());
    table.after_sweep = table.len;
    // Round 15 wave 3 (lane ffm2, FFR2-2): the claim ends here, so the
    // handles of rows a last action took meanwhile may be reused from now on.
    released.append(&mut table.deferred_release);
    (released, ids)
}

/// Round 15 wave 2 (lane ffm): [`auto_arena_take_dead`] for the ONE row
/// `(hash, handle)`, for [`auto_arena_action_done`]: ends the claim and, when
/// `dead` and no action is pending on the row, removes it. O(bucket), where
/// `take_dead` walks every row of the VM (and hashes each handle into the
/// `dead` set) -- once per cleaner action, i.e. O(rows^2) cleaner-thread work
/// for a program that drops many sessions with actions at once:
/// `R13Ffm7AutoArenaCleanup` runs 20 000 actions over up to 20 000 rows,
/// ~2e8 row visits under this lock, which is what starves its cleanups on a
/// loaded host (`r15w1-orch-ffm-auto-arena-cleanup-regressed-on-dev`).
/// `None` when `CRATONVM_FFM_AUTO_ARENA_TAKE_ONE=0` (or no table): the claim
/// is still held and the caller ends it with `take_dead`. Lock only.
pub(in crate::panama) fn auto_arena_take_row(
    vm: usize,
    hash: i32,
    handle: usize,
    dead: bool,
) -> Option<(Vec<usize>, Vec<i64>)> {
    let mut cell = auto_arena_cell().lock();
    let table = cell.get_mut(&vm)?;
    if !table.take_one {
        return None;
    }
    table.sweeping = false;
    let mut released = Vec::new();
    let mut ids = Vec::new();
    if !dead {
        return Some((released, ids));
    }
    let mut emptied = false;
    if let Some(bucket) = table.rows.get_mut(&hash) {
        if let Some(at) = bucket
            .iter()
            .position(|row| row.handle == handle && row.pending == 0)
        {
            let row = bucket.remove(at);
            emptied = bucket.is_empty();
            released.push(row.handle);
            if !row.kept {
                ids = row.ids;
            }
        }
    }
    if emptied {
        table.rows.remove(&hash);
    }
    table.len = table.len.saturating_sub(released.len());
    table.after_sweep = table.after_sweep.min(table.len);
    Some((released, ids))
}

/// Record `alloc_id`, a block of `arena`'s implicit session, for the sweep.
/// Plain reads, the JNI weak table and the lock: no Java, no heap allocation.
/// Answers whether it was recorded; `false` leaves the block to leak, as every
/// such block did before.
pub(in crate::panama) fn auto_arena_record_block(ctx: &mut dyn NativeContext, session: ObjectRef, alloc_id: i64) -> bool {
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(session);
    if let Some(&handle) = auto_arena_session_handles(&*ctx, hash, session).first() {
        return auto_arena_push(vm, hash, handle, alloc_id, false);
    }
    let handle = ctx.add_weak_global_root(session);
    if handle == 0 {
        return false;
    }
    // Two threads' first allocations in one arena can each file a row; both
    // are freed once the session dies, and `auto_arena_blocks_keep` marks all.
    if auto_arena_push(vm, hash, handle, alloc_id, true) {
        return true;
    }
    ctx.remove_global_root(handle);
    false
}

/// A Java close action is about to be registered on the implicit `session`
/// (or a view that holds no segment is being made over its memory): never
/// free the blocks recorded for it so far (see the section comment). Plain
/// reads and the lock; no Java.
pub(crate) fn auto_arena_blocks_keep(ctx: &dyn NativeContext, session: ObjectRef) {
    let hash = ctx.identity_hash_code(session);
    let handles = auto_arena_session_handles(ctx, hash, session);
    if !handles.is_empty() {
        auto_arena_mark_kept(ctx.vm_identity(), hash, &handles);
    }
}

// ---------------------------------------------------------------------------
// FFM3W-1 (round 14 wave 4, lane ffm4): a kept session's blocks are freed after
// its Java actions ran
// ---------------------------------------------------------------------------
//
// `auto_arena_blocks_keep` leaks a session's blocks for good once a Java close
// action is registered on it, because that action runs on the `Common-Cleaner`
// thread at no fixed point relative to the sweep and may read a block. HotSpot's
// `ImplicitSession` runs its actions and frees its blocks in one cleaner run.
// Here each action is registered with the `Cleaner` wrapped in a VM-owned
// `CratonVM$FfmAutoSessionAction` whose native `run()` runs the action and then
// counts it off the session's row (`AutoArenaBlocks::pending`); the last one
// to run frees the row's blocks once the session's weak handle has cleared
// (else the next sweep does). A sweep never takes a row with an action
// pending, so its handle is never released -- and reused -- under a wrapper
// that still names it. The wrapper holds the action, the row's handle and the
// session's identity hash, never the session itself, so the session stays
// collectable. A wrapper that cannot be built, or a registration that fails,
// leaves its count behind: that row is never freed, i.e. the old "kept".
// `CRATONVM_FFM_AUTO_ARENA_ACTION_FREE=0` restores `auto_arena_blocks_keep`.
//
// Known window (unchanged from the kept model): a row a racing FIRST
// allocation files after the action's lookup is not tied to the action.

/// The wrapper class. `CratonVM$` is this VM's reserved namespace
/// (`classloading::is_vm_reserved_namespace_name`): minted as `VmInternal` in
/// every mode, never a stand-in for image bytes.
pub(crate) const AUTO_ARENA_ACTION_CLASS: &str = "CratonVM$FfmAutoSessionAction";
/// Wrapper slot: the program's `Runnable` (or the VM's carrier of its action).
pub(in crate::panama) const AUTO_ARENA_ACTION_RUNNABLE: usize = 0;
/// Wrapper slot: the row's weak handle as a `Long`; 0 once `run()` started.
pub(in crate::panama) const AUTO_ARENA_ACTION_HANDLE: usize = 1;
/// Wrapper slot: the session's identity hash (the row's bucket) as an `Int`.
pub(in crate::panama) const AUTO_ARENA_ACTION_HASH: usize = 2;
pub(in crate::panama) const AUTO_ARENA_ACTION_FIELDS: usize = 3;

/// `CRATONVM_FFM_AUTO_ARENA_ACTION_FREE` (default on; round 14 wave 4, lane
/// ffm4, FFM3W-1): see the section comment. Read per action registration.
pub(in crate::panama) fn auto_arena_action_free_enabled() -> bool {
    true
}

/// Count one more pending action on the row with `handle`. Answers whether
/// the row exists. Lock only.
pub(in crate::panama) fn auto_arena_add_pending(vm: usize, hash: i32, handle: usize) -> bool {
    let mut cell = auto_arena_cell().lock();
    let Some(row) = cell
        .get_mut(&vm)
        .and_then(|table| table.rows.get_mut(&hash))
        .and_then(|bucket| bucket.iter_mut().find(|row| row.handle == handle))
    else {
        return false;
    };
    row.pending += 1;
    true
}

/// File a block-less row for `handle` with one pending action. Lock only.
pub(in crate::panama) fn auto_arena_file_pending_row(vm: usize, hash: i32, handle: usize) {
    let mut cell = auto_arena_cell().lock();
    let table = cell.entry(vm).or_default();
    table.rows.entry(hash).or_default().push(AutoArenaBlocks {
        handle,
        ids: Vec::new(),
        kept: false,
        pending: 1,
        closed_shared: false,
    });
    table.len += 1;
    table.added_since_sweep += 1;
}

/// Count off one pending action on the row with `handle`. Answers whether the
/// row is now free to go (no action pending, not kept). Lock only.
pub(in crate::panama) fn auto_arena_sub_pending(vm: usize, hash: i32, handle: usize) -> bool {
    let mut cell = auto_arena_cell().lock();
    let Some(row) = cell
        .get_mut(&vm)
        .and_then(|table| table.rows.get_mut(&hash))
        .and_then(|bucket| bucket.iter_mut().find(|row| row.handle == handle))
    else {
        return false;
    };
    row.pending = row.pending.saturating_sub(1);
    row.pending == 0 && !row.kept
}

/// FFR2-2 (round 15 wave 3, lane ffm2): count off one pending action on the
/// row with `handle` and, when it was the row's last and the row is not kept,
/// take the row in the same locked step. Answers the handle to release now
/// (`None` while a sweep is running: that sweep's take releases it) and the
/// block ids to free. `None` when `CRATONVM_FFM_AUTO_ARENA_LAST_ACTION_FREE=0`
/// or `vm` has no table: nothing was counted, the caller takes the round-14
/// path. Lock only.
///
/// Why no weak-handle check is needed: a wrapper's `run()` is reached only
/// through the `Cleanable` `cleaner.register(session, wrapper)` returned,
/// which `foreign_ffm::p67_cleaner_invoke_register` drops -- so only the
/// cleaner, once the SESSION is phantom-reachable (its referent). From then
/// on no segment, buffer view or downcall of the session can be reached, and
/// no new action or block can be added to it. When the last counted action
/// has run, every Java action that may read the blocks by address has run
/// too. HotSpot's `ResourceList` frees at the same point.
///
/// Why no sweep claim is needed: while this action was pending the row could
/// be taken by nobody else (`take_dead` and `take_row` skip a row with an
/// action pending, under this lock), so the row found here by `handle` is this
/// session's, and it leaves the table in this same critical section. Only
/// the RELEASE of `handle` could meet a running sweep's stale `dead` set (a
/// released handle is reused by the next weak global), so it waits for that
/// sweep's take ([`AutoArenaTable::deferred_release`]).
pub(in crate::panama) fn auto_arena_finish_action(
    vm: usize,
    hash: i32,
    handle: usize,
) -> Option<(Option<usize>, Vec<i64>)> {
    let mut cell = auto_arena_cell().lock();
    let table = cell.get_mut(&vm)?;
    if !table.last_action_free {
        return None;
    }
    let Some(bucket) = table.rows.get_mut(&hash) else {
        return Some((None, Vec::new()));
    };
    let Some(at) = bucket.iter().position(|row| row.handle == handle) else {
        return Some((None, Vec::new()));
    };
    let row = &mut bucket[at];
    row.pending = row.pending.saturating_sub(1);
    if row.pending > 0 || row.kept {
        // A kept row stays for a sweep: its handle goes once it cleared, its
        // blocks never.
        return Some((None, Vec::new()));
    }
    let row = bucket.remove(at);
    if bucket.is_empty() {
        table.rows.remove(&hash);
    }
    table.len = table.len.saturating_sub(1);
    table.after_sweep = table.after_sweep.min(table.len);
    if table.sweeping {
        table.deferred_release.push(row.handle);
        return Some((None, row.ids));
    }
    Some((Some(row.handle), row.ids))
}

/// A Java close action is about to be registered on the implicit `session`:
/// count it on the session's row (filing one if the session has no block
/// yet) and answer `(hash, handle)` for its wrapper. `None` -- switch off, no
/// handle -- means the caller keeps the old behaviour
/// ([`auto_arena_blocks_keep`]). Plain reads, the JNI weak table and the lock:
/// no Java, no heap allocation, so every reference the caller holds stays
/// valid across it.
pub(crate) fn auto_arena_action_pending(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
) -> Option<(i32, usize)> {
    if !auto_arena_action_free_enabled() {
        return None;
    }
    let vm = ctx.vm_identity();
    let hash = ctx.identity_hash_code(session);
    let handles = auto_arena_session_handles(&*ctx, hash, session);
    if let Some((&handle, others)) = handles.split_first() {
        // Two racing first allocations filed two rows: the action is counted
        // on one, the others are kept (their blocks may be what it reads).
        if !others.is_empty() {
            auto_arena_mark_kept(vm, hash, others);
        }
        if auto_arena_add_pending(vm, hash, handle) {
            return Some((hash, handle));
        }
        // Only a sweep removes a row, and only a dead one: not this session's.
        return None;
    }
    let handle = ctx.add_weak_global_root(session);
    if handle == 0 {
        return None;
    }
    auto_arena_file_pending_row(vm, hash, handle);
    Some((hash, handle))
}

/// A fresh `CratonVM$FfmAutoSessionAction` for the row `(hash, handle)`, its
/// runnable slot still empty (the caller stores the action right after,
/// with no allocation in between). `None` when it cannot be allocated.
/// Allocates: every reference the caller holds must be pinned across it.
pub(crate) fn auto_arena_action_wrapper(
    ctx: &mut dyn NativeContext,
    hash: i32,
    handle: usize,
) -> Option<ObjectRef> {
    // Round 15 wave 5 (lane upcall1, FF2W3-5): once per VM, not per action.
    let vm = ctx.vm_identity();
    if !auto_arena_action_class_ensured(vm) {
        ctx.ensure_vm_internal_class(AUTO_ARENA_ACTION_CLASS, AUTO_ARENA_ACTION_FIELDS);
        auto_arena_note_action_class_ensured(vm);
    }
    let wrapper =
        try_alloc_concurrent_synthetic(ctx, AUTO_ARENA_ACTION_CLASS, AUTO_ARENA_ACTION_FIELDS)
            .ok()?;
    ctx.set_field(wrapper, AUTO_ARENA_ACTION_RUNNABLE, Value::Object(None));
    ctx.set_field(
        wrapper,
        AUTO_ARENA_ACTION_HANDLE,
        Value::Long(handle as i64),
    );
    ctx.set_field(wrapper, AUTO_ARENA_ACTION_HASH, Value::Int(hash));
    Some(wrapper)
}

/// Whether `vm` already ensured the wrapper class (FF2W3-5; see
/// [`AutoArenaTable::action_class_ensured`]). Lock only.
pub(in crate::panama) fn auto_arena_action_class_ensured(vm: usize) -> bool {
    let cell = auto_arena_cell().lock();
    cell.get(&vm)
        .is_some_and(|table| table.action_class_ensured)
}

/// `vm` has ensured the wrapper class; remembered only while
/// `CRATONVM_FFM_AUTO_ARENA_ACTION_CLASS_CACHE` is on. Lock only.
pub(in crate::panama) fn auto_arena_note_action_class_ensured(vm: usize) {
    let mut cell = auto_arena_cell().lock();
    let table = cell.entry(vm).or_default();
    if table.action_class_cache {
        table.action_class_ensured = true;
    }
}

/// Store the action a wrapper runs. Plain store.
pub(crate) fn auto_arena_action_set_runnable(
    ctx: &mut dyn NativeContext,
    wrapper: ObjectRef,
    runnable: ObjectRef,
) {
    ctx.set_field(
        wrapper,
        AUTO_ARENA_ACTION_RUNNABLE,
        Value::Object(Some(runnable)),
    );
}

/// One wrapped action has run: count it off, and if it was the row's last,
/// free the row now ([`auto_arena_finish_action`], FFR2-2). With
/// `CRATONVM_FFM_AUTO_ARENA_LAST_ACTION_FREE=0`, the round-14 rule: only if
/// the session is gone, under the sweep claim, re-checking the handle there
/// (for the reason `AutoArenaTable::sweeping` gives); otherwise the next sweep
/// frees it. Answers how many blocks were freed. No Java and no heap
/// allocation.
pub(in crate::panama) fn auto_arena_action_done(ctx: &mut dyn NativeContext, hash: i32, handle: usize) -> usize {
    let vm = ctx.vm_identity();
    // Round 15 wave 3 (lane ffm2, FFR2-2): the last action takes its row at
    // once, whether or not the weak handle has cleared yet and whether or not
    // a sweep is running (see `auto_arena_finish_action`). Before, a sweep
    // holding the claim -- whose snapshot skipped this row while its action
    // was pending -- left the row to the NEXT sweep, which runs only at an
    // auto-arena allocation: a program that stopped allocating them kept the
    // blocks for good.
    if let Some((release, ids)) = auto_arena_finish_action(vm, hash, handle) {
        if let Some(release) = release {
            ctx.remove_global_root(release);
        }
        for &id in &ids {
            ctx.free_native_memory(id);
        }
        return ids.len();
    }
    if !auto_arena_sub_pending(vm, hash, handle) {
        return 0;
    }
    if !auto_arena_claim_sweep(vm) {
        return 0;
    }
    let dead = ctx.resolve_global_root(handle).is_none();
    // Ends the claim either way. Round 15 wave 2 (lane ffm): only this row,
    // not a walk of every row (see `auto_arena_take_row`).
    let (released, ids) = match auto_arena_take_row(vm, hash, handle, dead) {
        Some(taken) => taken,
        None => {
            let mut dead_set = std::collections::HashSet::new();
            if dead {
                dead_set.insert(handle);
            }
            auto_arena_take_dead(vm, &dead_set)
        }
    };
    for handle in released {
        ctx.remove_global_root(handle);
    }
    for &id in &ids {
        ctx.free_native_memory(id);
    }
    ids.len()
}

/// `CratonVM$FfmAutoSessionAction.run()V`: the wrapped action, then
/// [`auto_arena_action_done`] -- once: the handle slot is cleared first, so a
/// second `run()` only runs the action. The action's own exception is
/// answered after the count (the `Cleaner` ignores it, as HotSpot's does).
pub(in crate::panama) fn auto_arena_action_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let runnable = ctx.get_field(this, AUTO_ARENA_ACTION_RUNNABLE);
    let handle = match ctx.get_field(this, AUTO_ARENA_ACTION_HANDLE) {
        Value::Long(handle) => handle as usize,
        _ => 0,
    };
    let hash = match ctx.get_field(this, AUTO_ARENA_ACTION_HASH) {
        Value::Int(hash) => hash,
        _ => 0,
    };
    ctx.set_field(this, AUTO_ARENA_ACTION_HANDLE, Value::Long(0));
    let result = match runnable {
        Value::Object(Some(runnable)) => ctx.invoke_virtual(runnable, "run", "()V", &[]),
        _ => Ok(None),
    };
    if handle != 0 {
        auto_arena_action_done(ctx, hash, handle);
    }
    result.map(|_| None)
}

/// Register [`auto_arena_action_run`]. Explicitly `Bridge`: the class is this
/// VM's own `VmInternal` carrier in the reserved `CratonVM$` namespace (no image
/// declares it, so no bytecode can stand behind it), not a compatibility stub
/// for a JDK name -- the `CratonVM$HttpServerLoop` / `CratonVM$StsForkRunner`
/// shape. It goes away only with FFM3W-1 itself.
pub(in crate::panama) fn register_auto_arena_action(r: &mut NativeMethodRegistry) {
    r.register(AUTO_ARENA_ACTION_CLASS, "run", "()V", auto_arena_action_run, cratonvm_native_api::NativeKind::Bridge);
}

/// Free the blocks of every row of the calling VM whose session a collection
/// found unreachable, and release their handles. `gc` is the collection count
/// the sweep starts at (FFM3W-2 paces the next one by it). Answers how many
/// blocks were freed. No Java and no heap allocation; the resolves run outside
/// the lock.
pub(in crate::panama) fn reclaim_auto_arena_blocks(ctx: &mut dyn NativeContext, gc: Option<u64>) -> usize {
    let vm = ctx.vm_identity();
    // Round 14 wave 4 (lane review4): one sweeper per VM; a thread that finds
    // one running leaves the rows to it (`AutoArenaTable::sweeping`).
    if !auto_arena_claim_sweep(vm) {
        return 0;
    }
    // addr-keyed: not an address -- global-root handles, local.
    let dead: std::collections::HashSet<usize> = auto_arena_snapshot(vm, gc)
        .into_iter()
        .filter(|&handle| ctx.resolve_global_root(handle).is_none())
        .collect();
    // Even with nothing dead: it records the survivors, which paces the next
    // sweep.
    let (released, ids) = auto_arena_take_dead(vm, &dead);
    for handle in released {
        ctx.remove_global_root(handle);
    }
    for &id in &ids {
        ctx.free_native_memory(id);
    }
    ids.len()
}

// ---------------------------------------------------------------------------
// A closed SHARED arena's blocks, freed once its session is unreachable (round
// 15 wave 3, lane ffm2; `r12w5-ffm2-arena-close-frees-shared-residuals`)
// ---------------------------------------------------------------------------
//
// A shared close may not free at close: a thread can be past its liveness
// check on one of the session's segments and not yet at the block -- the JDK
// bytecode `ScopedMemoryAccess` accessors hold no access window -- and the
// quiescence proof HotSpot's `closeScope0` handshake gives is VM work
// (`CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` stays off for it). So the blocks
// leaked for good. Every such thread HOLDS the session, though: a `@Scoped`
// accessor has it as a parameter and fences it (`Reference.reachabilityFence`
// in its `finally`; the optimizing tier keeps that fence as a no-code
// keep-alive call and never splices its empty body -- nio20 wave 2 L1, see
// `r20-shared-arena-close-does-not-wait-for-compiled-scoped-access-bodies`); every native path holds the
// segment (slot 2 = the session) as an argument; a real `DirectByteBuffer`
// view holds its segment; a downcall acquires the sessions of its arguments,
// so a closed session has none in flight. Once a collection finds the closed
// session unreachable no thread can be anywhere between a check and an access
// on its blocks -- the same argument FFM7-1 frees `Arena.ofAuto()` blocks by.
// So the close walk hands the ids here (`foreign_ffm::p67_session_run_close_actions`),
// they go into a row of the auto-arena table with a WEAK handle on the
// session, and the ordinary sweep frees them once that handle cleared. Every
// Java close action has already run (the walk frees last), so no cleanup can
// read a freed block either. Later than HotSpot (at the session's death, not
// at close), never earlier. Switch `CRATONVM_FFM_SHARED_CLOSE_DEFERRED_FREE`
// (default on; `0` restores the leak), read in `foreign_ffm`.
//
// The one view that holds only the address: the synthetic-JDK `asByteBuffer`
// buffer. Once one is made over a non-implicit session, closed shared blocks
// are never freed again in that VM ([`AutoArenaTable::address_only_views`]).

/// File `ids`, the blocks of the just-closed SHARED `session`, to be freed
/// once a collection finds it unreachable (see the section comment), then
/// sweep if one is due -- a program that only uses shared arenas would never
/// sweep otherwise. Answers whether the ids were filed; `false` leaves them to
/// leak, as every shared block did before. No Java and no heap allocation.
pub(crate) fn shared_close_defer_free(
    ctx: &mut dyn NativeContext,
    session: ObjectRef,
    ids: Vec<i64>,
) -> bool {
    if ids.is_empty() {
        return true;
    }
    let vm = ctx.vm_identity();
    if auto_arena_address_only_views(vm) {
        return false;
    }
    let hash = ctx.identity_hash_code(session);
    let handle = ctx.add_weak_global_root(session);
    if handle == 0 {
        return false;
    }
    auto_arena_file_closed_shared_row(vm, hash, handle, ids);
    let gc = Some(ctx.gc_collection_count());
    if auto_arena_sweep_due(vm, gc, 0) {
        reclaim_auto_arena_blocks(ctx, gc);
    }
    true
}

/// File a closed shared session's row (see [`shared_close_defer_free`]).
/// Lock only.
pub(in crate::panama) fn auto_arena_file_closed_shared_row(vm: usize, hash: i32, handle: usize, ids: Vec<i64>) {
    let mut cell = auto_arena_cell().lock();
    let table = cell.entry(vm).or_default();
    table.rows.entry(hash).or_default().push(AutoArenaBlocks {
        handle,
        ids,
        kept: false,
        pending: 0,
        closed_shared: true,
    });
    table.len += 1;
    table.added_since_sweep += 1;
}

/// Whether `vm` ever made an address-only view over a non-implicit session.
/// Lock only.
pub(in crate::panama) fn auto_arena_address_only_views(vm: usize) -> bool {
    let cell = auto_arena_cell().lock();
    cell.get(&vm).is_some_and(|table| table.address_only_views)
}

/// [`auto_arena_address_only_views`] for the calling VM: a close must not free
/// its blocks (round 15 wave 5, lane upcall1; the confined close walk and
/// [`pe_arena_close`] ask). Lock only.
pub(crate) fn address_only_view_made(ctx: &dyn NativeContext) -> bool {
    auto_arena_address_only_views(ctx.vm_identity())
}

/// A synthetic-JDK `asByteBuffer` view was made over a non-implicit session's
/// memory: see [`AutoArenaTable::address_only_views`]. Lock only.
pub(in crate::panama) fn auto_arena_note_address_only_view(vm: usize) {
    let mut cell = auto_arena_cell().lock();
    cell.entry(vm).or_default().address_only_views = true;
}

// ---------------------------------------------------------------------------
// FFR2-3 (round 15 wave 3, lane ffm2): the common cleaner, cached per VM
// ---------------------------------------------------------------------------
//
// `foreign_ffm::p67_mark_auto_arena` called `CleanerFactory.cleaner()` (a Java
// static call) for every `Arena.ofAuto()`. That method answers one static
// final `Cleaner` for the life of the VM, so the first answer is kept as a
// strong global handle in the VM's auto-arena table and resolved after that
// (no Java). `CRATONVM_FFM_AUTO_ARENA_CLEANER_CACHE=0` calls every time.

/// The cached common cleaner of the calling VM, if any. No Java, no
/// allocation.
pub(crate) fn auto_arena_cached_cleaner(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    let handle = {
        let cell = auto_arena_cell().lock();
        cell.get(&vm).map_or(0, |table| table.cleaner_handle)
    };
    if handle == 0 {
        return None;
    }
    ctx.resolve_global_root(handle)
}

/// Cache `cleaner`, what `CleanerFactory.cleaner()` just answered, for the
/// calling VM (first caller wins; nothing when the switch is off). No Java, no
/// heap allocation; `cleaner` stays valid.
pub(crate) fn auto_arena_cache_cleaner(ctx: &mut dyn NativeContext, cleaner: ObjectRef) {
    let vm = ctx.vm_identity();
    let wanted = {
        let mut cell = auto_arena_cell().lock();
        let table = cell.entry(vm).or_default();
        table.cleaner_cache && table.cleaner_handle == 0
    };
    if !wanted {
        return;
    }
    let handle = ctx.add_global_root(cleaner);
    if handle == 0 {
        return;
    }
    let stored = {
        let mut cell = auto_arena_cell().lock();
        let table = cell.entry(vm).or_default();
        if table.cleaner_handle == 0 {
            table.cleaner_handle = handle;
            true
        } else {
            false
        }
    };
    if !stored {
        ctx.remove_global_root(handle);
    }
}

// ---------------------------------------------------------------------------
// FFM3W-2 (round 15 wave 2, lane ffm): a collection under native pressure
// ---------------------------------------------------------------------------
//
// An auto arena's blocks go only once a collection finds its session dead, but
// the Java heap sees nothing of them -- a program allocating a few large auto
// blocks can grow native memory without end while the heap never fills. On
// HotSpot `Arena.ofAuto().allocate` reserves its bytes through
// `Bits.reserveMemory` (the `-XX:MaxDirectMemorySize` budget), which runs
// `System.gc()` when the budget is spent. Here: once
// `CRATONVM_FFM_AUTO_ARENA_GC_BUDGET_MB` of auto-arena bytes were allocated
// with no collection in between (`auto_arena_sweep_due` counts them), the
// next `Arena.allocate` on an auto arena asks for ONE VM collection
// (`force_gc_for_vm`, not an explicit GC) at its entry, and its own record
// step then sweeps (the count moved). At most one request per collection
// epoch. A rate, not HotSpot's live-byte budget: a program holding more live
// auto memory than the budget pays one collection per budget of NEW
// allocation, never a collection per allocation. Default off (budget 0).
//
// Served only at the entry of the registered native `pe_arena_allocate`,
// where the receiver is the only reference held (pinned across): never
// inside `pe_arena_allocate_impl`, whose in-crate callers (`allocateFrom`,
// the upcall's by-value groups) hold references across it; and only for an
// auto arena, so the upcall's own confined arena never collects there.

/// A pressure request waits for `vm`. Lock only.
pub(in crate::panama) fn auto_arena_pressure_pending(vm: usize) -> bool {
    let cell = auto_arena_cell().lock();
    cell.get(&vm).is_some_and(|table| table.pressure_due)
}

/// Take `vm`'s pressure request: `true` for exactly one taker. Lock only.
pub(in crate::panama) fn auto_arena_take_pressure(vm: usize) -> bool {
    let mut cell = auto_arena_cell().lock();
    cell.get_mut(&vm)
        .is_some_and(|table| std::mem::take(&mut table.pressure_due))
}

/// Serve a pending pressure request (see the section comment) before
/// `arena.allocate`: one VM collection, `arena` pinned across it. Answers
/// `arena`, forwarded. Nothing when no request waits or `arena` is not an
/// auto arena.
pub(in crate::panama) fn auto_arena_relieve_native_pressure(ctx: &mut dyn NativeContext, arena: ObjectRef) -> ObjectRef {
    let vm = ctx.vm_identity();
    if !auto_arena_pressure_pending(vm) {
        return arena;
    }
    let implicit = pe_arena_session(&*ctx, arena).is_some_and(|session| {
        crate::phases_late::foreign_ffm::p67_session_is_implicit(&*ctx, session)
    });
    if !implicit || !auto_arena_take_pressure(vm) {
        return arena;
    }
    let arena_pin = ctx.pin_native_root(arena);
    ctx.force_gc_for_vm();
    let arena = ctx.read_native_pin(arena_pin, arena);
    ctx.unpin_native_roots(arena_pin);
    arena
}

// (Round 14 wave 4, lane ffm4: the per-VM `MemorySegment.NULL` singleton cell
// that stood here went with the field-shaped `MemorySegment.NULL` registration
// it served -- `getstatic` has no registry door, so that row never ran; see
// `r13w13-shadow5-field-shaped-rows-held-by-tests-patch`, item 4.)

/// Mint `register_pe_arena`'s four-slot arena carrier: [0] kind, [1] alloc-id
/// array, [2] closed flag, [3] count.
///
/// gc-common w20-f: the four factories each allocated the arena and then its
/// id array, and wrote all four slots through the arena's address from before
/// the array's allocation. The arena is now pinned across it and re-read.
pub(in crate::panama) fn pe_new_arena_carrier(
    ctx: &mut dyn NativeContext,
    kind: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
    let a_pin = ctx.pin_native_root(a);
    let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
    let a: ObjectRef = ctx.read_native_pin(a_pin, a);
    ctx.unpin_native_roots(a_pin);
    ctx.set_field(a, 0, Value::Int(kind));
    ctx.set_field(a, 1, Value::Object(Some(ids)));
    ctx.set_field(a, 2, Value::Int(0));
    ctx.set_field(a, 3, Value::Int(0));
    Ok(a)
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
    // MEASURED by `FfmCarrierProbe.java`, the one row that differs
    // in compatible mode as well as strict.
    // The singleton memo is NOT here: `--dump-native-registry` says
    // `phases_late/foreign_ffm.rs` owns all four `Arena` factories and this
    // registrar owns none of them, so a memo added here is inert. It lives at
    // the owning site, and `global_arena_handle`/`claim_global_arena` below are
    // shared with it rather than duplicated -- two copies of one rule is how
    // two registrars come to disagree.
    r.register(arena, "global", "()Ljava/lang/foreign/Arena;", |ctx, _| {
        let vm = ctx.vm_identity();
        if let Some(existing) = global_arena_handle(vm).and_then(|h| ctx.resolve_global_root(h)) {
            return Ok(Some(Value::Object(Some(existing))));
        }
        let a: ObjectRef = pe_new_arena_carrier(ctx, ffi::ARENA_GLOBAL)?;
        // Published under a global root: the global arena outlives every native
        // call and must survive a moving collection, which a raw `ObjectRef` in
        // a process-global cell would not.
        let handle = ctx.add_global_root(a);
        match claim_global_arena(vm, handle) {
            None => Ok(Some(Value::Object(Some(a)))),
            // Lost the race: drop ours and answer with theirs, so two callers
            // cannot hold two different global arenas.
            Some(published) => {
                ctx.remove_global_root(handle);
                let winner = ctx.resolve_global_root(published).unwrap_or(a);
                Ok(Some(Value::Object(Some(winner))))
            }
        }
    }, NativeKind::SyntheticStub);
    r.register(arena, "ofAuto", "()Ljava/lang/foreign/Arena;", |ctx, _| {
        let a: ObjectRef = pe_new_arena_carrier(ctx, ffi::ARENA_AUTO)?;
        Ok(Some(Value::Object(Some(a))))
    }, NativeKind::SyntheticStub);
    r.register(
        arena,
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _| {
            let a: ObjectRef = pe_new_arena_carrier(ctx, ffi::ARENA_CONFINED)?;
            Ok(Some(Value::Object(Some(a))))
        }, NativeKind::SyntheticStub,
    );
    // ofShared() — multi-thread-safe arena (same as confined but with ARENA_SHARED kind)
    r.register(
        arena,
        "ofShared",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _| {
            let a: ObjectRef = pe_new_arena_carrier(ctx, ffi::ARENA_SHARED)?;
            Ok(Some(Value::Object(Some(a))))
        }, NativeKind::SyntheticStub,
    );

    // allocate(byteSize, byteAlignment) → MemorySegment
    r.register(
        arena,
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        pe_arena_allocate, NativeKind::SyntheticStub,
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
        }, NativeKind::SyntheticStub,
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
        }, NativeKind::SyntheticStub,
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
            // That was already true for every `--real-jdk` run, because
            // `register_pe_panama` never executes there and the layout in hand
            // has always been `foreign_ffm.rs`'s. It only looked correct under
            // synthetic-JDK, where the kind-tagged carrier happened to answer.
            // Reading the layout's own recorded size and alignment is right in
            // both modes and does not depend on a name list staying in sync.
            let size = crate::phases_late::foreign_ffm::p67_layout_size_of(ctx, layout);
            let align = crate::phases_late::foreign_ffm::p67_layout_align_of(ctx, layout);
            pe_arena_allocate_impl(ctx, this, size, align)
        }, NativeKind::SyntheticStub,
    );

    // close() — free all allocations in this arena
    r.register(arena, "close", "()V", pe_arena_close, NativeKind::SyntheticStub);
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
    // Round 15 wave 2 (lane ffm, FFM3W-2): the one place a pressure request is
    // served -- see `auto_arena_relieve_native_pressure`.
    let this = auto_arena_relieve_native_pressure(ctx, this);
    pe_arena_allocate_impl(ctx, this, size, align)
}

/// `Arena.allocate`'s two argument guards, which this body had neither of.
///
/// MEASURED against HotSpot 25.0.4+7, both modes
/// (`FfmSegmentSweep.java`, messages from
/// `FfmMsgProbe.java`):
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
pub(in crate::panama) fn pe_arena_check_allocation(size: i64, align: i64) -> Result<(), MethodCallFailed> {
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

pub(in crate::panama) fn pe_arena_allocate_impl(
    ctx: &mut dyn NativeContext,
    arena_obj: ObjectRef,
    size: i64,
    align: i64,
) -> MethodCallResult {
    pe_arena_check_allocation(size, align)?;
    // Round 12 wave 6 (lane ffm3): an access window from the liveness check to
    // the block's record. A shared close racing this call then either makes
    // the check fail or waits until the block is on the session's close list
    // -- without it the close could detach the list first, and the block was
    // recorded on a dead session and never freed.
    let _window = crate::ffm_fast::AccessWindow::open();
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
    // Round 12 wave 5 (lane ffm2): the default model's arena records its
    // blocks so that `close()` frees them; see
    // `foreign_ffm::p67_arena_frees_on_close` for which arenas do.
    let record_block = !four_slot_layout
        && pe_is_modelled_arena(ctx, arena_obj)
        && crate::phases_late::foreign_ffm::p67_arena_frees_on_close(ctx, arena_obj);

    // Allocate off-heap memory via SharedVm's native_memory table
    let (alloc_id, ptr) = ctx
        .allocate_native_memory(size.max(1) as usize, align.max(1) as usize)
        // SBR5 ffmlink: HotSpot's `Unsafe.allocateMemory` failure inside
        // `NativeMemorySegmentImpl.makeNativeSegment` is
        // `OutOfMemoryError: Unable to allocate N bytes` (`Sbr5FfmlinkAcct acct.huge`).
        .ok_or_else(|| RuntimeError::OutOfMemoryError {
            // `Unsafe.alignToHeapWordSize`: the message names the size rounded up to 8.
            message: format!("Unable to allocate {} bytes", size.wrapping_add(7) & !7),
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
    //
    // gc-common w20-f: the arena was stored into the segment at its address
    // from before the carrier's allocation. Pinned across it and re-read; a
    // failed allocation releases the pin before the `?` exit.
    let arena_pin = ctx.pin_native_root(arena_obj);
    let seg_alloc = alloc_segment_carrier(ctx, 6);
    let arena_obj: ObjectRef = ctx.read_native_pin(arena_pin, arena_obj);
    ctx.unpin_native_roots(arena_pin);
    let seg: ObjectRef = match seg_alloc {
        Ok(seg) => seg,
        Err(err) => {
            // Round 13 wave 9 (lane ffm5): the block is on no close list yet
            // (the four-slot synthetic-JDK arena listed it above and frees it
            // at its close), so an `OutOfMemoryError` for the carrier leaked
            // it for good.
            if !four_slot_layout {
                ctx.free_native_memory(alloc_id);
            }
            return Err(err);
        }
    };
    let scope = pe_carrier_scope(ctx, arena_obj);
    ctx.set_field(seg, 0, Value::Long(ptr as i64));
    ctx.set_field(seg, 1, Value::Long(size));
    ctx.set_field(seg, 2, Value::Object(Some(scope)));
    ctx.set_field(seg, 3, Value::Int(0)); // read-write
    ctx.set_field(seg, 4, Value::Int(1)); // alive
    ctx.set_field(seg, 5, Value::Long(0)); // no offset

    if record_block {
        // The record can grow (an allocation), so the segment is pinned across
        // it; the session is re-derived from the segment's own arena slot
        // rather than held across the carrier allocation above.
        let seg_pin = ctx.pin_native_root(seg);
        if let Some(session) = pe_segment_session(ctx, seg) {
            crate::phases_late::foreign_ffm::p67_session_record_block(ctx, session, alloc_id);
        }
        let seg = ctx.read_native_pin(seg_pin, seg);
        ctx.unpin_native_roots(seg_pin);
        return Ok(Some(Value::Object(Some(seg))));
    }

    // Round 14 wave 3 (lane ffm, FFM7-1): an `Arena.ofAuto()` block is freed
    // once a collection finds its session unreachable
    // (`CRATONVM_FFM_AUTO_ARENA_BLOCK_FREE`; see `auto_arena_record_block`).
    // Neither the record nor the sweep runs Java or allocates on the heap; the
    // segment is pinned across them anyway, as the record above does.
    if !four_slot_layout {
        if let Some(session) = pe_segment_session(ctx, seg) {
            if crate::phases_late::foreign_ffm::p67_session_is_implicit(ctx, session) {
                let seg_pin = ctx.pin_native_root(seg);
                auto_arena_record_block(ctx, session, alloc_id);
                // Round 15 wave 2 (lane ffm, FFM3W-2): paced by collections
                // too, and the bytes feed the pressure request -- which is
                // served at the NEXT `Arena.allocate`'s entry, never here:
                // this body's in-crate callers hold references across it.
                let gc = Some(ctx.gc_collection_count());
                if auto_arena_sweep_due(ctx.vm_identity(), gc, size.max(1) as u64) {
                    reclaim_auto_arena_blocks(ctx, gc);
                }
                let seg = ctx.read_native_pin(seg_pin, seg);
                ctx.unpin_native_roots(seg_pin);
                return Ok(Some(Value::Object(Some(seg))));
            }
        }
    }

    Ok(Some(Value::Object(Some(seg))))
}

pub(in crate::panama) fn pe_arena_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

    // Marked closed before anything is freed, so no reader sees an open arena
    // whose blocks are already gone.
    ctx.set_field(this, 2, Value::Int(1)); // closed

    // The ids are read out BEFORE the handshake: its wait is a blocking region
    // in which a collection can move `this` and its id array.
    let mut alloc_ids: Vec<i64> = Vec::new();
    if let Value::Object(Some(ids_arr)) = ctx.get_field(this, 1) {
        let count = match ctx.get_field(this, 3) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        for i in 0..count {
            if let Value::Long(alloc_id) = ctx.get_array_element(ids_arr, i) {
                alloc_ids.push(alloc_id);
            }
        }
    }

    // Round 12 wave 3 (lane ffm): an element access already past its check on
    // another thread completes before the blocks go (see
    // `ffm_fast::AccessWindow`); every later one reads the flag above and
    // throws (`pe_segment_in_closed_synthetic_arena`).
    pe_close_handshake(ctx, true, &mut []);

    // Round 15 wave 5 (lane upcall1): a synthetic `asByteBuffer` view holds
    // only an address, so once one exists nothing is freed here (a leak, the
    // pre-round-12 behaviour). Read after the closed flag above: see the
    // re-check in `pe_segment_as_byte_buffer`.
    if address_only_view_made(&*ctx) {
        return Ok(None);
    }

    // Free all allocations tracked by this arena
    for alloc_id in alloc_ids {
        ctx.free_native_memory(alloc_id);
    }

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
pub(in crate::panama) fn register_craton_segment_impl_surface(r: &mut NativeMethodRegistry) {
    // Registered on the interface name as well as the craton class: a receiver
    // minted by the `--real-jdk` fallback in `alloc_segment_carrier` still
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge);
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        // `checkReadOnly(boolean)` — throws only when a WRITE is attempted on a
        // read-only segment. The argument is the caller's own read-only-ness,
        // so `checkReadOnly(true)` never throws.
        r.register(owner, "checkReadOnly", "(Z)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let caller_read_only = matches!(args.get(1), Some(Value::Int(n)) if *n != 0);
            craton_segment_check_read_only(ctx, this, caller_read_only)?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge);
        // `checkBounds(long, long)`. The JDK's body is a bounds test over
        // `length`; the message shape is the JDK's own
        // (`AbstractMemorySegmentImpl.outOfBoundException`) so a caller matching
        // on it is not surprised.
        r.register(owner, "checkBounds", "(JJ)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = seg_long_arg(args, 1);
            let length = seg_long_arg(args, 2);
            craton_segment_check_bounds(ctx, this, offset, length)
        }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge);
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
        }, cratonvm_native_api::NativeKind::Bridge);
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
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        // `toString()` — the JDK's text (`segment_to_string`; j27 wave 4, lane
        // FFMMSG: it was `MemorySegment{ heapBase: <array>, address: .. }`,
        // with no `kind:` and address 0 for a heap slice).
        // Not decoration: an unregistered `toString` on this carrier reaches
        // `java/lang/Object`'s and prints the identity hash, which is what every
        // FFM diagnostic in the tree would then say.
        r.register(owner, "toString", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let text = segment_to_string(ctx, this);
            let s = ctx.create_string(&text);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge);
        // `allocate(long, long)` -- `AbstractMemorySegmentImpl` IS a
        // `SegmentAllocator`, and `SegmentAllocator.prefixAllocator(segment)`
        // is `return (AbstractMemorySegmentImpl) segment;`. The JDK's body is
        // `checkAllocationSizeAndAlign` then `asSlice(0, size, align)`; this
        // carrier does not inherit it, so a prefix allocator over one of its
        // segments was `AbstractMethodError` (round 13 wave 5, lane ffm3:
        // `R13Ffm3StructReturn`'s `prefix` row).
        r.register(
            owner,
            "allocate",
            "(JJ)Ljava/lang/foreign/MemorySegment;",
            pe_segment_allocate_prefix, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    }

/// `AbstractMemorySegmentImpl.allocate(long, long)` on this VM's carrier: the
/// JDK's two argument checks (`pe_arena_check_allocation` transcribes
/// `Utils.checkAllocationSizeAndAlign`), then `asSlice(0, size, align)` with the
/// alignment-checked slice's own checks, in its order.
pub(in crate::panama) fn pe_segment_allocate_prefix(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = pe_long_arg(args, 1);
    let align = pe_long_arg(args, 2);
    pe_arena_check_allocation(size, align)?;
    pe_slice_bounds_check(ctx, this, 0, size)?;
    pe_slice_alignment_check(ctx, this, 0, align)?;
    pe_segment_slice(ctx, this, 0, size, None)
}

/// A `long` argument at `index`, or 0.
///
/// The `Value::Int` arm is not defensive padding: a `J` slot can arrive as an
/// `Int` from a caller that pushed a literal through the operand stack, and
/// reading that as 0 would be a silent wrong bound rather than a refusal.
pub(in crate::panama) fn seg_long_arg(args: &[Value], index: usize) -> i64 {
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
pub(in crate::panama) fn craton_segment_check_read_only(
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
pub(in crate::panama) fn craton_segment_check_bounds(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    length: i64,
) -> MethodCallResult {
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);
    let end = offset.checked_add(length);
    let in_range = offset >= 0 && length >= 0 && matches!(end, Some(end) if end <= size);
    if !in_range {
        // j27 wave 4 (lane FFMMSG): the segment's `toString()`, as HotSpot's
        // `outOfBoundException`; it printed `MemorySegment{ byteSize: N }`.
        return Err(segment_out_of_bounds(ctx, seg, offset, length));
    }
    Ok(None)
}

/// `AbstractMemorySegmentImpl.isAlignedForElement`, both overloads.
pub(in crate::panama) fn craton_segment_is_aligned(
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
    // Once, not inside the per-`ms` loop above: a second registration of the
    // same triple is a shadowed row for `duplicate_registration_gate`.
    register_arena_impl_allocate_no_init(r);
    register_auto_arena_action(r);
}

/// `ArenaImpl.allocateNoInit(long, long)`'s descriptor (JDK 25 declares it
/// returning the concrete `NativeMemorySegmentImpl`; there is no bridge twin).
pub(in crate::panama) const ARENA_ALLOCATE_NO_INIT_DESC: &str = "(JJ)Ljdk/internal/foreign/NativeMemorySegmentImpl;";

/// Round 13 wave 5 (lane ffm3): `jdk.internal.foreign.ArenaImpl.allocateNoInit`.
///
/// Every `SegmentAllocator` default that allocates before it writes --
/// `allocateFrom(ValueLayout.OfByte|OfChar|OfShort|OfInt|OfFloat|OfLong|OfDouble,
/// value)`, `allocateFrom(AddressLayout, MemorySegment)`,
/// `allocateFrom(String, Charset)` and
/// `allocateFrom(ValueLayout, MemorySegment, ValueLayout, long, long)` -- goes
/// through the private `SegmentAllocator.allocateNoInit`, which for an
/// `ArenaImpl` receiver (every arena this VM mints) calls THIS method, not
/// `allocate(JJ)`. Only `allocate` is routed to this VM's carrier
/// (`foreign_ffm`'s `ArenaImpl.allocate` rows), so these shapes ran the JDK's
/// `SegmentFactories.allocateNativeSegment` and handed back a REAL
/// `NativeMemorySegmentImpl`, whose memory the JDK frees through a
/// `ResourceCleanup` registered on the session -- and the close walk
/// (`foreign_ffm::p67_session_run_close_actions`) runs that `cleanup()` for a
/// SHARED arena too, whatever `CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES` says. A
/// real segment's own accessors are JDK bytecode (`ScopedMemoryAccess`) that
/// hold no access window, so a shared close racing one was a use-after-free
/// the switch exists to rule out (`r12w5-ffm2-arena-close-frees-shared-residuals`).
/// Routed here, those blocks are this VM's carrier blocks: recorded and freed
/// for a confined arena, left alone for a shared one, like `allocate`'s.
///
/// Any receiver this model did not build (no modelled session), and
/// `CRATONVM_FFM_ARENA_NOINIT_CARRIER=0`, run the JDK's own body.
pub(in crate::panama) fn register_arena_impl_allocate_no_init(r: &mut NativeMethodRegistry) {
    r.register(
        "jdk/internal/foreign/ArenaImpl",
        "allocateNoInit",
        "(JJ)Ljdk/internal/foreign/NativeMemorySegmentImpl;",
        pe_arena_allocate_no_init, cratonvm_native_api::NativeKind::Bridge,
    );
    // `allocate(JJ)`'s COVARIANT declaration. `foreign_ffm` routes the
    // `MemorySegment`-returning bridge every interface call site links to; a
    // call site typed `ArenaImpl` links to this one and reached
    // `SegmentFactories` the same way.
    r.register(
        "jdk/internal/foreign/ArenaImpl",
        "allocate",
        "(JJ)Ljdk/internal/foreign/NativeMemorySegmentImpl;",
        pe_arena_allocate_covariant, cratonvm_native_api::NativeKind::Bridge,
    );
}

/// `ArenaImpl.allocate(long, long)`'s covariant descriptor.
pub(in crate::panama) const ARENA_ALLOCATE_COVARIANT_DESC: &str = "(JJ)Ljdk/internal/foreign/NativeMemorySegmentImpl;";

pub(in crate::panama) fn pe_arena_allocate_covariant(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    pe_arena_allocate_modelled_or_jdk(ctx, args, "allocate", ARENA_ALLOCATE_COVARIANT_DESC)
}



pub(in crate::panama) fn pe_arena_allocate_no_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    pe_arena_allocate_modelled_or_jdk(ctx, args, "allocateNoInit", ARENA_ALLOCATE_NO_INIT_DESC)
}

/// This VM's carrier for an arena it modelled; the JDK's `name`/`desc` body
/// otherwise, or with the switch off.
pub(in crate::panama) fn pe_arena_allocate_modelled_or_jdk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    desc: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if pe_arena_session(ctx, this).is_none() {
        return ctx.invoke_virtual_bytecode_only(this, name, desc, args.get(1..).unwrap_or(&[]));
    }
    // "No init" is a permission, not a promise: the carrier's block comes back
    // zeroed or not as `allocate_native_memory` leaves it, and every caller
    // writes the whole block before handing it out.
    pe_arena_allocate(ctx, args)
}

