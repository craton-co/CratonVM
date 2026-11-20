#[allow(unused_imports)]
use super::*;


/// The StringConcatFactory bootstrap method class name.
pub(in crate::runtime::invokedynamic) const STRING_CONCAT_FACTORY: &str = "java/lang/invoke/StringConcatFactory";

/// The StringConcatFactory bootstrap method name (with recipe).
pub(in crate::runtime::invokedynamic) const MAKE_CONCAT_WITH_CONSTANTS: &str = "makeConcatWithConstants";

/// The StringConcatFactory.makeConcat method name (no recipe, all args concatenated).
pub(in crate::runtime::invokedynamic) const MAKE_CONCAT: &str = "makeConcat";

/// The LambdaMetafactory bootstrap method class name.
pub(in crate::runtime::invokedynamic) const LAMBDA_METAFACTORY: &str = "java/lang/invoke/LambdaMetafactory";

/// The LambdaMetafactory.metafactory bootstrap method name.
pub(in crate::runtime::invokedynamic) const METAFACTORY: &str = "metafactory";

/// The LambdaMetafactory.altMetafactory bootstrap method name (advanced flags variant).
pub(in crate::runtime::invokedynamic) const ALT_METAFACTORY: &str = "altMetafactory";

/// `LambdaMetafactory.FLAG_SERIALIZABLE` — the spun proxy implements
/// `java.io.Serializable` and carries a `writeReplace()`.
pub(in crate::runtime::invokedynamic) const FLAG_SERIALIZABLE: i32 = 1;

/// `LambdaMetafactory.FLAG_MARKERS` — the flags word is followed by
/// `markerCount` and then that many `Class` bootstrap arguments, each an
/// ADDITIONAL interface the spun proxy class implements. Emitted by javac for
/// an intersection target type (`(Adder & Cloneable) (a, b) -> a + b`) and by
/// any hand-written `altMetafactory` call.
///
/// The block that follows the markers is [`FLAG_BRIDGES`]'s. Markers come
/// FIRST, so the marker block's extent is computable without looking at the
/// bridge bit.
pub(in crate::runtime::invokedynamic) const FLAG_MARKERS: i32 = 2;

/// `LambdaMetafactory.FLAG_BRIDGES` — after the marker block (if any) come
/// `bridgeCount` and then that many `MethodType`s: ADDITIONAL descriptors of the
/// SAM name the spun class implements, each forwarding to the SAM. javac emits
/// it when no interface can hold the bridge, e.g. an intersection target
/// `(ObjM & StrM)` whose `Object m()` / `String m()` are unrelated. Lambda
/// dispatch enters the body only for a descriptor the proxy implements
/// (`lambda_accepts_descriptor`), so the list is recorded beside the proxy —
/// see `record_lambda_proxy_bridges`.
pub(in crate::runtime::invokedynamic) const FLAG_BRIDGES: i32 = 4;

/// The SwitchBootstraps bootstrap method class name (JEP 441, Java 21).
pub(in crate::runtime::invokedynamic) const SWITCH_BOOTSTRAPS: &str = "java/lang/runtime/SwitchBootstraps";

/// SwitchBootstraps.typeSwitch bootstrap method name.
pub(in crate::runtime::invokedynamic) const TYPE_SWITCH: &str = "typeSwitch";

/// SwitchBootstraps.enumSwitch bootstrap method name.
pub(in crate::runtime::invokedynamic) const ENUM_SWITCH: &str = "enumSwitch";

/// ObjectMethods bootstrap class (JEP 395, Java 16+) — records.
pub(in crate::runtime::invokedynamic) const OBJECT_METHODS: &str = "java/lang/runtime/ObjectMethods";

/// ObjectMethods.bootstrap method name.
pub(in crate::runtime::invokedynamic) const BOOTSTRAP: &str = "bootstrap";

/// Apache Groovy's invokedynamic bootstrap class.
pub(in crate::runtime::invokedynamic) const GROOVY_INDY_INTERFACE: &str = "org/codehaus/groovy/vmplugin/v8/IndyInterface";
pub(in crate::runtime::invokedynamic) fn dbg_indy_all() -> bool { false }
pub(in crate::runtime::invokedynamic) fn dbg_indy_generic() -> bool { false }
pub(in crate::runtime::invokedynamic) fn dbg_lambda_dispatch() -> bool { false }

/// `CRATONVM_DBG_INDY_GENERIC`, re-exported for the compiled bridge's own
/// tracing. Public because `jit::helpers::jit_indy_bridge` is the other half of
/// that trace and lives in a different module: a wrong result from a bridged
/// site raises exactly one question — which SIDE lost the value — and only a
/// line from each can answer it.
pub fn dbg_indy_generic_enabled() -> bool {
    dbg_indy_generic()
}

/// `CRATONVM_DBG_INDY_ALL`, for the `ldc` method-handle access refusal in
/// `interpreter::constants` (wave 40, lane L4), whose line belongs to the same
/// trace as the `ObjectMethods` getter refusal here.
pub(crate) fn dbg_indy_all_enabled() -> bool {
    dbg_indy_all()
}

/// Groovy call-site name for a coercion (`cast:(Object)Z`, `cast:(Object)I`, …).
pub(in crate::runtime::invokedynamic) const GROOVY_CAST: &str = "cast";

/// Groovy's runtime type-coercion helper (implements "Groovy truth").
pub(in crate::runtime::invokedynamic) const GROOVY_DTT: &str = "org/codehaus/groovy/runtime/typehandling/DefaultTypeTransformation";

/// `StringConcatFactory` recipe tags, as UTF-16 code units.
///
/// The recipe is walked unit-by-unit rather than `char`-by-`char` so a lone
/// surrogate in a folded literal survives; these are the two markers it looks
/// for. Values per `java.lang.invoke.StringConcatFactory`.
pub(in crate::runtime::invokedynamic) const TAG_ARG_UNIT: u16 = 0x0001;
pub(in crate::runtime::invokedynamic) const TAG_CONST_UNIT: u16 = 0x0002;

/// Data extracted from the constant pool under a read lock, owned so we can
/// drop the lock before proceeding with string creation (which needs a write lock).
pub(in crate::runtime::invokedynamic) struct IndyInfo {
    pub(in crate::runtime::invokedynamic) bsm_class: String,
    pub(in crate::runtime::invokedynamic) bsm_method: String,
    pub(in crate::runtime::invokedynamic) target_name: String,
    pub(in crate::runtime::invokedynamic) target_descriptor: String,
    /// The recipe (first bootstrap argument) as UTF-16 code units, for a
    /// `StringConcatFactory.makeConcatWithConstants` site only; empty for every
    /// other bootstrap. Units rather than `String` because a recipe may embed a
    /// lone surrogate from a folded string literal, which a Rust `str` cannot
    /// hold -- see `resolve_concat_constant_units`.
    pub(in crate::runtime::invokedynamic) recipe: Vec<u16>,
    /// Additional constants from bootstrap arguments (TAG_CONST placeholders),
    /// as UTF-16 code units, for the same reason as `recipe`. Same scope.
    pub(in crate::runtime::invokedynamic) constant_args: Vec<Vec<u16>>,
    /// Raw CP indices for bootstrap arguments (needed for LambdaMetafactory).
    pub(in crate::runtime::invokedynamic) bootstrap_arg_indices: Vec<u16>,
    /// The bootstrap handle names `LambdaMetafactory.metafactory` or
    /// `altMetafactory` with its real descriptor: only then does
    /// `lambda_site_refusal` model `BootstrapMethodInvoker`'s call.
    pub(in crate::runtime::invokedynamic) bsm_descriptor_is_lmf: bool,
    /// `ResolutionCache::fill_snapshot` taken before the constant pool was
    /// read for this record: every `put_call_site_as_of` of this bootstrap
    /// publishes under it (interpreter round i1 wave 23, lane L5).
    pub(in crate::runtime::invokedynamic) fill_as_of: u64,
}

/// A compiled `invokedynamic` bridge site is a BRIDGE of one of two kinds, and
/// this is the tag that says which.
///
/// The codegen carries exactly one `usize` per indy site (the fifth element of
/// `indy_info`) and calls exactly one entry (`cratonvm_jit::DirectHelperTable::indy_bridge`),
/// so the discriminator has to live in the pointed-to metadata rather than in
/// the call sequence. Both site structs are `#[repr(C)]` with this `u32` as
/// their first field, and [`jit_indy_site_kind`] is the only reader.
///
/// A tag rather than a second helper cell and a sixth `indy_info` element: the
/// tuple appears in fifteen places across two crates and every one of them
/// would have had to grow a field it does not use, for a fact the metadata
/// already knows.
pub const JIT_INDY_SITE_CONCAT: u32 = 1;
/// A site whose bootstrap is `LambdaMetafactory` — see [`JitIndyGenericSite`].
pub const JIT_INDY_SITE_GENERIC: u32 = 2;
/// JIT round 21 wave 1 (lane indy, INDY21-1): an [`IndyMutableGuardCell`]
/// handed to compiled code as the bridge site of a bimorphic guard's second
/// arm ([`jit_indy_guarded_leaf`]).
pub const JIT_INDY_SITE_GUARDED_LEAF: u32 = 3;

/// nio21 wave 10, lane L3 (W9L2-1): byte offset of the CONSTANT WORD in both
/// `#[repr(C)]` site structs (the `usize` after the `u32` kind). The compiled
/// `invokedynamic` of a non-capturing lambda reads `[site + 8]` and, when it
/// is non-zero, `[[site + 8]]`: a non-zero result is the instruction's
/// zero-capture singleton and replaces the bridge call (single-pass
/// `x64/op_invoke.rs` `walk_invokedynamic`, optimizing `ir_lower.rs`
/// `Op::IndyBridge`; both spell the offset `8`, pinned here and by
/// `nio21w10_l3_constant_lambda_tests`).
pub const JIT_INDY_SITE_CONSTANT_WORD_OFFSET: usize = 8;

const _: () = {
    assert!(
        std::mem::offset_of!(JitIndyGenericSite, constant_word)
            == JIT_INDY_SITE_CONSTANT_WORD_OFFSET
    );
    assert!(
        std::mem::offset_of!(JitStringConcatSite, constant_word)
            == JIT_INDY_SITE_CONSTANT_WORD_OFFSET
    );
    assert!(std::mem::offset_of!(JitIndyGenericSite, kind) == 0);
    assert!(std::mem::offset_of!(JitStringConcatSite, kind) == 0);
    assert!(std::mem::offset_of!(IndyMutableGuardCell, kind) == 0);
    assert!(
        std::mem::offset_of!(IndyMutableGuardCell, constant_word)
            == JIT_INDY_SITE_CONSTANT_WORD_OFFSET
    );
};

/// `CRATONVM_JIT_INDY_CONSTANT_LAMBDA` (nio21 wave 10, lane L3; perf,
/// **default ON**): may a compiled non-capturing `LambdaMetafactory` site
/// answer its singleton from the slot word ([`JIT_INDY_SITE_CONSTANT_WORD_OFFSET`])
/// instead of calling `jit_indy_bridge`? Read at site construction here (the
/// site never publishes its word with it off) and per compile by both
/// emitters (no probe is emitted with it off), so `=0` is byte-for-byte the
/// bridge call of wave 9.
pub fn jit_indy_constant_lambda_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_INDY_CAPTURE_FAST` (nio21 wave 14, lane L1, W11L3-2; perf,
/// **default ON**; `0` is the wave-13 bridge path): may a compiled CAPTURING
/// lambda site answer from its learnt proxy memo and allocate its proxy in the
/// TLAB without capture pins? Read at site construction only (no per-call
/// read, no static).
pub fn jit_indy_capture_fast_enabled() -> bool {
    true
}

/// The `(proxy class, capture count)` a [`JitIndyGenericSite::capture_memo`]
/// word holds, or `None` for `0`.
#[inline]
pub(in crate::runtime::invokedynamic) fn decode_capture_memo(word: u64) -> Option<(ClassId, usize)> {
    (word != 0).then(|| {
        (
            ClassId::new((word >> 32) as u32),
            ((word as u32) - 1) as usize,
        )
    })
}

/// The memo word for `(proxy class, capture count)`, or `0` for a count that
/// does not fit (never memoised).
#[inline]
pub(in crate::runtime::invokedynamic) fn encode_capture_memo(proxy_class_id: ClassId, num_captures: usize) -> u64 {
    match u32::try_from(num_captures)
        .ok()
        .and_then(|n| n.checked_add(1))
    {
        Some(n) => (u64::from(proxy_class_id.as_u32()) << 32) | u64::from(n),
        None => 0,
    }
}

/// Immutable metadata owned for the lifetime of generated code which directly
/// invokes a `StringConcatFactory` call site.
#[repr(C)]
pub struct JitStringConcatSite {
    /// [`JIT_INDY_SITE_CONCAT`]. FIRST FIELD, and `#[repr(C)]`, so
    /// [`jit_indy_site_kind`] can read it through either site type's pointer.
    pub(in crate::runtime::invokedynamic) kind: u32,
    /// [`JIT_INDY_SITE_CONSTANT_WORD_OFFSET`]: always `0` for a concat site
    /// (a concatenation is never a constant object). Present so compiled code
    /// that reads the word through any site pointer reads a defined `0`.
    /// Read only by generated code (by address), hence the `allow`.
    #[allow(dead_code)]
    pub(in crate::runtime::invokedynamic) constant_word: std::sync::atomic::AtomicUsize,
    pub(in crate::runtime::invokedynamic) recipe: Arc<[u16]>,
    pub(in crate::runtime::invokedynamic) constant_args: Vec<Arc<[u16]>>,
    pub(in crate::runtime::invokedynamic) target_descriptor: Arc<str>,
    /// The site's argument tags, parsed once at site construction. The bridge
    /// used to re-parse the descriptor into a fresh `Vec` on every call.
    pub(in crate::runtime::invokedynamic) arg_types: Box<[char]>,
    pub(in crate::runtime::invokedynamic) deopt_on_overflow: bool,
    /// The synthetic frame's parts, pre-built for [`Frame::new_from_arcs`] for
    /// the reason [`JitIndyGenericSite`] states: `Frame::new` takes the global
    /// `padded_bytecode_for_method` mutex and allocates three `String`s, on
    /// every compiled string concatenation. Read only by the kill-switch arm
    /// (`execute_jit_string_concat_on_frame`) since the bridge went frame-free
    /// in round i1 wave 20; drop them with that arm.
    pub(in crate::runtime::invokedynamic) frame_class_name: Arc<str>,
    pub(in crate::runtime::invokedynamic) frame_method_name: Arc<str>,
    pub(in crate::runtime::invokedynamic) frame_method_descriptor: Arc<str>,
    pub(in crate::runtime::invokedynamic) frame_code: Arc<[u8]>,
    pub(in crate::runtime::invokedynamic) frame_exception_table: Arc<[cratonvm_reader::attribute::ExceptionTableEntry]>,
    /// JIT round 19, lane strgap (STRGAP19-1; `CRATONVM_JIT_CONCAT_LATIN1_FAST`,
    /// default ON, `=0` off): the bridge first tries the exact-size Latin-1 fast path
    /// (`concat_latin1`). Decided at site construction from the switch and the
    /// recipe (Latin-1 literals, tags matching the operands); no per-call flag
    /// read.
    pub(in crate::runtime::invokedynamic) latin1_fast: bool,
    /// JIT round 19 proposal wave 6, lane str6 (CONCAT5-4;
    /// `CRATONVM_JIT_CONCAT_TLAB_RESULT`, default OFF): the general renderer
    /// builds an all-Latin-1 result from the thread's TLAB (refill allowed)
    /// instead of the shared heap, so a decline from an inline concat (whose
    /// TLAB bump found the buffer full) refills the TLAB even when the
    /// Latin-1 fast path is off or declines at run time. Decided at site
    /// construction; no per-call flag read.
    pub(in crate::runtime::invokedynamic) tlab_result: bool,
    /// JIT round 21 wave 5, lane concat (CONCAT21-2;
    /// `CRATONVM_JIT_CONCAT_UTF16_FAST`, default ON since the r22 A/B, `=0` off): after the
    /// Latin-1 fast path declines, a result whose operands need no
    /// `toString()` is rendered as units and built from the TLAB (UTF16, or
    /// LATIN1 when every unit fits) instead of the general renderer. Decided at
    /// site construction; no per-call flag read.
    pub(in crate::runtime::invokedynamic) utf16_fast: bool,
    /// JIT round 19 proposal wave 4, lane strgap4 (STRGAP19-3,
    /// `CRATONVM_JIT_CONCAT_INLINE`, default ON, `=0` off): the single-pass backend's
    /// inline plan of this site, built at the first compile that asks
    /// ([`jit_concat_site_inline_plan`]); `None` inside = the plan does not
    /// admit the site. Pure data: the switch is read by the compiler.
    pub(in crate::runtime::invokedynamic) inline_plan: std::sync::OnceLock<Option<cratonvm_jit::concat_inline_plan::JitConcatInlinePlan>>,
}

// The inline plan reads the recipe with its own copy of the two tags.
const _: () = {
    assert!(TAG_ARG_UNIT == cratonvm_jit::concat_inline_plan::CONCAT_TAG_ARG);
    assert!(TAG_CONST_UNIT == cratonvm_jit::concat_inline_plan::CONCAT_TAG_CONST);
};

/// The inline plan of a concat bridge site (STRGAP19-3), or `None`: a null
/// or non-concat site, or one the plan does not admit. Built once per site,
/// from the site's own recipe, constants and descriptor.
///
/// SAFETY: `site_ptr` is 0 or a pointer returned by
/// [`make_jit_indy_bridge_site_from_parts`], whose allocations are
/// process-lived; the answer borrows that allocation for the process.
pub unsafe fn jit_concat_site_inline_plan(
    site_ptr: usize,
) -> Option<&'static cratonvm_jit::concat_inline_plan::JitConcatInlinePlan> {
    if jit_indy_site_kind(site_ptr) != JIT_INDY_SITE_CONCAT {
        return None;
    }
    let site: &'static JitStringConcatSite = &*(site_ptr as *const JitStringConcatSite);
    site.inline_plan
        .get_or_init(|| {
            let constants: Vec<&[u16]> = site.constant_args.iter().map(|c| &c[..]).collect();
            cratonvm_jit::concat_inline_plan::JitConcatInlinePlan::from_recipe(
                &site.recipe,
                &constants,
                &site.target_descriptor,
            )
        })
        .as_ref()
}

/// Immutable metadata for a compiled `invokedynamic` site whose bootstrap is
/// `LambdaMetafactory` — the shape that makes reactive code slow.
///
/// # Why this exists
///
/// Before it, a method containing ANY non-concat `invokedynamic` could not stay
/// compiled: the codegen lowered the site to an unconditional reason-8 uncommon
/// trap, the first execution took it, and `DeoptimizationController` retired the
/// method with `MakeNotCompilable`. OSR was refused outright for the same
/// reason. So every method that CREATES a lambda ran interpreted for the life
/// of the process, and every compiled caller of one paid the
/// compiled-to-interpreted transition on top.
///
/// MEASURED 2026-08-23, `probes/IndyScopeProbe.java`, one binary:
///
/// | arm | HotSpot | CratonVM |
/// |---|---:|---:|
/// | loop whose method creates the lambda | 1.6 ns | **1055.8 ns** |
/// | identical loop, lambda hoisted out | 4.0 ns | 42.2 ns |
///
/// 25x, for moving one `->` across a method boundary. Reactor and WebFlux
/// assembly is nothing but methods that create lambdas, which is why
/// `internal/performance/webclient-integration-tests-reactive-exchange-gap-RETIRED-20260823.md`
/// reads as a flat profile with no single lever: the lever is that none of it
/// is compiled.
///
/// # Why `LambdaMetafactory` and not every bootstrap
///
/// The bridge below hands the site to `execute_invokedynamic`, i.e. to the
/// interpreter's own implementation, so nothing about the bootstrap is
/// reimplemented and any bootstrap would in principle work. The admission is
/// narrow anyway because the RETURN VALUE has to fit the bridge's `i64` ABI as
/// an object reference, and because a bootstrap that can throw at a point the
/// compiled caller cannot resume is a different question from this one. Every
/// other bootstrap keeps the trap it has today.
#[repr(C)]
pub struct JitIndyGenericSite {
    /// [`JIT_INDY_SITE_GENERIC`]. FIRST FIELD — see [`JIT_INDY_SITE_CONCAT`].
    pub(in crate::runtime::invokedynamic) kind: u32,
    /// nio21 wave 10, lane L3 (W9L2-1, `CRATONVM_JIT_INDY_CONSTANT_LAMBDA`):
    /// the address of this site's zero-capture singleton slot (the
    /// `LambdaSingletonSlot` word `singleton_slot` holds an `Arc` of), or `0`.
    /// SECOND FIELD, at [`JIT_INDY_SITE_CONSTANT_WORD_OFFSET`]: both tiers'
    /// compiled code read it before the bridge call, and a non-zero word
    /// whose slot holds a non-zero reference IS the instruction's answer --
    /// the very instance the interpreter hands out for the instruction (one
    /// `LAMBDA_SINGLETON_CACHE` row per instruction). Published once, by
    /// [`Self::remember_singleton_slot`], only for a non-capturing `L` site
    /// built with the switch on; the slot's memory lives as long as this
    /// process-lived site (the `OnceLock` holds the `Arc`), and a retired row
    /// reads `0`, which takes the bridge.
    pub(in crate::runtime::invokedynamic) constant_word: std::sync::atomic::AtomicUsize,
    /// May [`Self::remember_singleton_slot`] publish `constant_word`? Decided
    /// at site construction (no per-call flag read, no static).
    pub(in crate::runtime::invokedynamic) constant_lambda: bool,
    /// nio21 wave 14, lane L1 (W11L3-2, `CRATONVM_JIT_INDY_CAPTURE_FAST`): a
    /// CAPTURING `L` site of the bridge's own admission whose warm evaluation
    /// may (1) take its `(proxy class, capture count)` from
    /// [`Self::capture_memo`] instead of this thread's site cache and the
    /// resolution cache, and (2) allocate the proxy in the thread's TLAB with
    /// no capture pins ([`allocate_lambda_proxy_tlab_fast`]). Decided at site
    /// construction: also off under `CRATONVM_RESOLUTION_LINKED_PERMANENT=0`
    /// (a linked row may then be evicted and relinked to another proxy) and
    /// under the `CRATONVM_DBG_WATCH_ALLOC_CID` diagnostic the full path owns.
    pub(in crate::runtime::invokedynamic) capture_fast: bool,
    /// `(proxy class id << 32) | (capture count + 1)` the bridge's fast path
    /// resolved for this instruction, or `0` (not learnt). The instruction's
    /// linkage is permanent (wave 11 L4: linked rows are never evicted, and a
    /// redefinition interns a NEW site), so the pair never changes once learnt.
    pub(in crate::runtime::invokedynamic) capture_memo: std::sync::atomic::AtomicU64,
    /// The class whose constant pool holds `cp_index`. The bridge pushes a
    /// synthetic frame carrying it, because `execute_invokedynamic` reads the
    /// caller class from `thread.frames[frame_idx].class_id` and keys the
    /// resolved-call-site cache on `(that class, cp_index)` — so the compiled
    /// site and the interpreted one share ONE cache entry and one bootstrap.
    pub(in crate::runtime::invokedynamic) class_id: ClassId,
    pub(in crate::runtime::invokedynamic) cp_index: u16,
    pub(in crate::runtime::invokedynamic) target_descriptor: Arc<str>,
    /// The site's argument type tags, parsed ONCE.
    ///
    /// `parse_descriptor_args` walks a `String` and allocates a `Vec` — per
    /// call, on a path whose whole reason to exist is that the interpreter's
    /// version re-derives constants. Everything below is here for the same
    /// reason: the synthetic frame is built with [`Frame::new_from_arcs`],
    /// which takes pre-built `Arc`s, rather than with `Frame::new`, whose
    /// `padded_bytecode_for_method` takes a GLOBAL MUTEX and verifies its memo
    /// with a body compare — on every single bridged call.
    pub(in crate::runtime::invokedynamic) arg_types: Arc<[u8]>,
    pub(in crate::runtime::invokedynamic) frame_class_name: Arc<str>,
    pub(in crate::runtime::invokedynamic) frame_method_name: Arc<str>,
    /// Padded empty bytecode. The frame decodes no instruction, but the
    /// interpreter's dispatch loop reads two bytes past the last opcode
    /// unconditionally, so the two-byte tail is a precondition rather than a
    /// courtesy — see `Frame::new_from_arcs`.
    pub(in crate::runtime::invokedynamic) frame_code: Arc<[u8]>,
    pub(in crate::runtime::invokedynamic) frame_exception_table: Arc<[cratonvm_reader::attribute::ExceptionTableEntry]>,
    pub(in crate::runtime::invokedynamic) frame_max_stack: u16,
    /// First byte of the site's RETURN descriptor. The bridge hands compiled
    /// code one `i64`, and this is what says how the `Value` was encoded into
    /// it — the same encoding `jit_invoke_dispatch` uses for an ordinary
    /// invoke's return, so the call site pushes it with the same three-way
    /// (`xmm0` / plain / oop-marked) choice.
    pub(in crate::runtime::invokedynamic) return_type: u8,
    /// The zero-capture singleton key of the INSTRUCTION this site compiles:
    /// [`lambda_singleton_site_for`] of the method containing it and its bci,
    /// i.e. exactly the key the interpreter derives from the real frame. Both
    /// bridge paths (the frame-free fast path and the bootstrapping frame
    /// path, through [`execute_invokedynamic_at`]) key on it, so a
    /// non-capturing lambda keeps its identity at tier-up. The same key finds
    /// the per-instruction linkage of an entry several instructions share
    /// (`lambda_entry_is_shared`), so such an instruction keeps its proxy
    /// CLASS at tier-up too.
    pub(in crate::runtime::invokedynamic) singleton_site: (u64, usize),
    /// `(vm_identity, proxy class, slot)` of this site's zero-capture singleton
    /// row, learnt at the first fast-path evaluation that went through the
    /// table (round i1 wave 20, lane L4). A warm evaluation reads the instance
    /// from the slot: no lock on `LAMBDA_SINGLETON_CACHE`. Answers only while
    /// the VM and the proxy class the fast path resolved are the ones recorded
    /// and the slot is live; otherwise the table answers, as before.
    pub(in crate::runtime::invokedynamic) singleton_slot: std::sync::OnceLock<(usize, ClassId, Arc<LambdaSingletonSlot>)>,
    /// JIT round 16 (lane indy): a `SwitchBootstraps` site whose linked
    /// `TypeSwitch` / `EnumSwitch` the bridge answers WITHOUT the synthetic
    /// frame (`CRATONVM_JIT_INDY_SWITCH_FAST`, default ON since round 16 (type switch 335->142 ns), read at site
    /// construction). See the frame-free switch arm of
    /// [`execute_jit_indy_generic_raw`].
    pub(in crate::runtime::invokedynamic) switch_fast: bool,
    /// JIT round 16 wave 7 (lane indy): an `ObjectMethods` site whose linked
    /// `RecordObjectMethod` the bridge answers WITHOUT the synthetic frame
    /// (`CRATONVM_JIT_INDY_RECORD_FAST`, default ON, `=0` off, read at
    /// site construction). See the frame-free record arm of
    /// [`execute_jit_indy_generic_raw`].
    pub(in crate::runtime::invokedynamic) record_fast: bool,
    /// JIT round 16 wave 7 (lane indy): a site the bridge admits only because
    /// its INSTRUCTION was already linked through a generic bootstrap when the
    /// site was built (`CRATONVM_JIT_INDY_LINKED_CALL_SITE`, default ON; see
    /// [`make_jit_indy_linked_site_from_parts`]). A `Linked` row's `CallSite`
    /// target is invoked without the synthetic frame; every other row shape
    /// takes the frame path, which finds the row and never bootstraps.
    pub(in crate::runtime::invokedynamic) generic_linked: bool,
    /// JIT round 16 wave 9 (lane indy, IN16-7): a `generic_linked` site whose
    /// `CallSite` target, when it is a direct handle of exactly the site's type,
    /// is CALLED as its target method (`CRATONVM_JIT_INDY_LINKED_DIRECT`,
    /// default ON, `=0` off, read at site construction). See
    /// [`jit_indy_linked_direct_leaf`].
    pub(in crate::runtime::invokedynamic) linked_direct: bool,
    /// The bound target of a `linked_direct` site ([`IndyDirectMemo`]).
    pub(in crate::runtime::invokedynamic) direct: IndyDirectState,
    /// JIT round 16 wave 10 (lane indyir, IN16-8): `(name, descriptor)` of the
    /// method whose instruction this site compiles, noted by the intern table
    /// ([`note_jit_indy_site_method`]); `None` for a site built without one.
    pub(in crate::runtime::invokedynamic) owner_method: std::sync::OnceLock<(Arc<str>, Arc<str>)>,
    /// JIT round 16 wave 10 (lane indyir, IN16-8): this site already asked for
    /// its method's optimizing recompile ([`Self::request_constant_recompile`]).
    pub(in crate::runtime::invokedynamic) constant_recompile_requested: std::sync::atomic::AtomicBool,
}

/// JIT round 16 wave 9 (lane indy, IN16-7): what a `linked_direct` site
/// learnt about its `CallSite`'s target, re-validated on every call by
/// [`jit_indy_linked_direct_leaf`].
///
/// Addresses are kept only for one address-memo epoch (`HeapRealm::address_epoch`,
/// since gci i4/wr; it was `collection_count`, which a G1 cleanup or a
/// Generational concurrent sweep slice does not move): an
/// equal epoch is the same object at the same address, the MH lane's
/// constant-bind rule (`vm/src/jit/helpers.rs`, `mh_constant_bind_redirect`).
/// The `CallSite` stays reachable for as long as the row that published it
/// (a `GENERIC_INDY_SITES` root), and its target for as long as it is the
/// target: a retarget changes `guard[0]`.
pub(in crate::runtime::invokedynamic) struct IndyDirectMemo {
    pub(in crate::runtime::invokedynamic) vm: usize,
    pub(in crate::runtime::invokedynamic) epoch: u64,
    /// The `CallSite`'s address in `epoch`.
    pub(in crate::runtime::invokedynamic) call_site: usize,
    /// `CallSite.target`'s field slot.
    pub(in crate::runtime::invokedynamic) target_slot: usize,
    /// The target handle's address, then its
    /// `MH_COMPILED_BIND_GUARD_SLOTS` values ([`indy_direct_slot_key`]).
    pub(in crate::runtime::invokedynamic) guard: [u64; 7],
    /// The target's dispatch info, or `None`: the admission refused the
    /// handle (the generic invoke serves the site while the memo is current).
    pub(in crate::runtime::invokedynamic) leaf: Option<&'static cratonvm_jit::JitInvokeInfo>,
    /// The caller class's redefinition gate of the row the memo was taken from.
    pub(in crate::runtime::invokedynamic) gate: crate::classloading::resolution::RedefineGate,
    /// JIT round 16 wave 10 (lane indyir, IN16-8): the `CallSite` is a
    /// `ConstantCallSite` (or a subclass), whose target never changes once
    /// linked. With `leaf`, the site is for the rest of the caller's life
    /// exactly a call of the leaf's method, which is what
    /// [`jit_indy_constant_direct_target`] hands the optimizing tier.
    pub(in crate::runtime::invokedynamic) constant: bool,
    /// JIT round 21 lane reflect: the [`IndyMutableGuardCell`] published for
    /// this memo's target (its leaked address), or `0`. A retarget BACK to a
    /// recent memo makes its cell the site's newest again
    /// ([`JitIndyGenericSite::fill_direct`]), so the next optimizing compile
    /// guards the target the site holds now, not the one it held last.
    pub(in crate::runtime::invokedynamic) cell: usize,
}

/// Fills of one site within one epoch after which the site keeps the generic
/// invoke for good (a `MutableCallSite` retargeted call after call).
pub(in crate::runtime::invokedynamic) const INDY_DIRECT_MAX_FILLS: u32 = 8;

#[derive(Default)]
pub(in crate::runtime::invokedynamic) struct IndyDirectState {
    pub(in crate::runtime::invokedynamic) memo: parking_lot::Mutex<Option<IndyDirectMemo>>,
    /// Fills counted as in the MH lane: a re-fill in the epoch of the last
    /// one is churn; one after a collection re-validates a moved object.
    pub(in crate::runtime::invokedynamic) fills: std::sync::atomic::AtomicU32,
    /// Leaf infos this site minted (each is leaked; see
    /// [`indy_direct_leaf_info`]). Past [`INDY_DIRECT_MAX_FILLS`] a site
    /// whose target keeps changing ACROSS collections keeps the generic invoke.
    pub(in crate::runtime::invokedynamic) minted: std::sync::atomic::AtomicU32,
    /// JIT round 20 wave 11 (lane pIndy, SPLICE19-1;
    /// [`jit_indy_mutable_guard_enabled`]): the address of this site's newest
    /// [`IndyMutableGuardCell`] (a `MutableCallSite` / `VolatileCallSite`
    /// whose bound target the optimizing tier may call behind a guard), or
    /// `0`. Cells are leaked and immutable, so a compile thread may read one.
    pub(in crate::runtime::invokedynamic) mutable_cell: std::sync::atomic::AtomicUsize,
    /// Cells minted for this site, capped at [`INDY_DIRECT_MAX_FILLS`] (each
    /// holds two never-released JNI global references).
    pub(in crate::runtime::invokedynamic) mutable_cells: std::sync::atomic::AtomicU32,
    /// JIT round 20 wave 17 (lane pStr3; [`jit_indy_direct_recent_targets_enabled`]):
    /// memos the site's memo displaced on a retarget, newest first, at most
    /// [`INDY_DIRECT_RECENT`]. A retarget BACK to one of them (same VM, epoch,
    /// call site, target and shim slots) swaps it in without counting a fill,
    /// so a `MutableCallSite` flipped between a few targets keeps the direct
    /// dispatch instead of falling to the generic `MethodHandle.invoke` for
    /// good after [`INDY_DIRECT_MAX_FILLS`] flips. Stale entries stay until
    /// evicted: their leaves are still reused by name (no new mint).
    /// Lock order: `memo`, then `recent` (only [`JitIndyGenericSite::fill_direct`]
    /// takes it, by `try_lock`).
    pub(in crate::runtime::invokedynamic) recent: parking_lot::Mutex<Vec<IndyDirectMemo>>,
    /// The switch, read once per site: 0 unread, 1 off, 2 on.
    pub(in crate::runtime::invokedynamic) recent_gate: std::sync::atomic::AtomicU8,
    /// JIT round 21 wave 1 (lane indy; [`jit_indy_mutable_retarget_recompile_enabled`]):
    /// every [`IndyMutableGuardCell`] this site minted, so a retarget back to
    /// a target that already has one re-points [`Self::mutable_cell`] to it
    /// instead of minting (or keeping the newest cell for a target the site no
    /// longer holds). At most [`INDY_DIRECT_MAX_FILLS`] words.
    pub(in crate::runtime::invokedynamic) cells: parking_lot::Mutex<Vec<usize>>,
    /// The cell word the newest compile of the owner method planned its
    /// guarded call against (`jit_indy_mutable_note_planned`, recorded by the
    /// compile door once the caller-side admission accepted the plan), or `0`.
    pub(in crate::runtime::invokedynamic) planned_cell: std::sync::atomic::AtomicUsize,
    /// Set by a retarget whose current target's cell is not
    /// [`Self::planned_cell`]: the compiled guard misses on every call now,
    /// and the bridge counts those misses ([`Self::retarget_misses`]).
    pub(in crate::runtime::invokedynamic) retarget_armed: std::sync::atomic::AtomicBool,
    /// Bridge calls since the site was armed. At
    /// [`INDY_MUTABLE_RETARGET_MISSES`] the target has proved stable enough
    /// for one recompile of the owner method.
    pub(in crate::runtime::invokedynamic) retarget_misses: std::sync::atomic::AtomicU32,
    /// Retarget recompiles this site asked for, capped at
    /// [`INDY_MUTABLE_RETARGET_RECOMPILES`]: past it the site keeps its guard
    /// and the bridge for good.
    pub(in crate::runtime::invokedynamic) retarget_recompiles: std::sync::atomic::AtomicU32,
    /// The switch, read once per site: 0 unread, 1 off, 2 on.
    pub(in crate::runtime::invokedynamic) retarget_gate: std::sync::atomic::AtomicU8,
    /// JIT round 21 wave 1 (lane indy, INDY21-1): `CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC`,
    /// read once per site ([`jit_indy_mutable_bimorphic_enabled`]): 0 unread,
    /// 1 off, 2 on.
    pub(in crate::runtime::invokedynamic) bimorphic_gate: std::sync::atomic::AtomicU8,
    /// JIT round 21 lane reflect ([`jit_indy_mutable_guard_bridge_recompile_enabled`]):
    /// bridge calls the direct leaf served while the site's newest guard cell
    /// named the very target the call site held -- calls an optimizing body
    /// planned from that cell would have served on its guard hit.
    pub(in crate::runtime::invokedynamic) guard_bridge_hits: std::sync::atomic::AtomicU32,
    /// Optimizing recompiles those bridge calls had queued (capped at
    /// [`INDY_MUTABLE_BRIDGE_RECOMPILES`]).
    pub(in crate::runtime::invokedynamic) guard_bridge_recompiles: std::sync::atomic::AtomicU32,
    /// `CRATONVM_JIT_INDY_MUTABLE_GUARD_BRIDGE_RECOMPILE`, read once per site:
    /// 0 unread, 1 off, 2 on.
    pub(in crate::runtime::invokedynamic) guard_bridge_gate: std::sync::atomic::AtomicU8,
    /// JIT round 21 lane indyconst ([`jit_indy_constant_bridge_recompile_enabled`]):
    /// bridge calls the direct leaf served while the memo held a
    /// `ConstantCallSite`'s target in the shape
    /// [`jit_indy_constant_direct_target`] plans as a plain call -- calls an
    /// optimizing body planned now would not have made through the bridge.
    pub(in crate::runtime::invokedynamic) constant_bridge_hits: std::sync::atomic::AtomicU32,
    /// Optimizing recompiles those bridge calls had queued (capped at
    /// [`INDY_MUTABLE_BRIDGE_RECOMPILES`], the mutable twin's bound).
    pub(in crate::runtime::invokedynamic) constant_bridge_recompiles: std::sync::atomic::AtomicU32,
    /// `CRATONVM_JIT_INDY_CONSTANT_BRIDGE_RECOMPILE`, read once per site:
    /// 0 unread, 1 off, 2 on.
    pub(in crate::runtime::invokedynamic) constant_bridge_gate: std::sync::atomic::AtomicU8,
    /// JIT round 21 lane mcs: bridge calls of a NON-constant site the direct
    /// leaf served, counted only under `CRATONVM_DBG_JITC` for the
    /// `[indy-mutable-miss]` line ([`JitIndyGenericSite::note_guard_miss_dbg`]).
    pub(in crate::runtime::invokedynamic) guard_miss_dbg: std::sync::atomic::AtomicU32,
    /// indy21c (INDYLATE21-3): compile-time reads of [`Self::memo`]
    /// ([`Self::memo_for_compile`]) that found it busy for the whole bounded
    /// retry and so read "no plan". Census only (`CRATONVM_DBG_JITC`).
    pub(in crate::runtime::invokedynamic) compile_memo_contended: std::sync::atomic::AtomicU32,
}

/// `try_lock` attempts a COMPILE-time read of a site's direct memo makes
/// before it reads "no plan" ([`IndyDirectState::memo_for_compile`]). The
/// bridge holds the memo for a few loads per call, so a handful of spins
/// outlasts it; a `fill_direct` in the middle of an admission may not be
/// outlasted, and that read stays a refusal, as before.
pub(in crate::runtime::invokedynamic) const INDY_COMPILE_MEMO_TRIES: u32 = 64;

impl IndyDirectState {
    /// indy21c (INDYLATE21-3, `indy21c-late-and-const-residuals-20261102.md`):
    /// the memo for a COMPILE-time read ([`jit_indy_constant_direct_target`]).
    /// A compile that raced a hot bridge used to read the contended memo as
    /// "no plan" on its first `try_lock` and build the bridge again, spending
    /// one of the site's four bridge re-asks. It now retries a bounded number
    /// of times (no blocking: `fill_direct` may hold the memo across the
    /// admission, and a blocked compile thread is not worth a deadlock with a
    /// parked mutator), and counts the reads that still found it busy.
    pub(in crate::runtime::invokedynamic) fn memo_for_compile(&self) -> Option<parking_lot::MutexGuard<'_, Option<IndyDirectMemo>>> {
        for _ in 0..INDY_COMPILE_MEMO_TRIES {
            if let Some(memo) = self.memo.try_lock() {
                return Some(memo);
            }
            std::hint::spin_loop();
        }
        self.compile_memo_contended
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    }
}

/// `CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC` (JIT round 21 wave 1, lane indy,
/// INDY21-1; **default OFF**, `=1` enables; effective only with
/// `CRATONVM_JIT_INDY_MUTABLE_GUARD`): may the optimizing tier's guarded call
/// of a `MutableCallSite` / `VolatileCallSite` test a SECOND target -- the
/// newest other target this site bound ([`IndyDirectState::cells`]) -- before
/// its bridge? The second arm is an `Op::IndyBridge` of that target's
/// [`IndyMutableGuardCell`] (a bridge site of kind
/// [`JIT_INDY_SITE_GUARDED_LEAF`]), which `jit_indy_bridge` dispatches to the
/// target method directly ([`jit_indy_guarded_leaf`]): the instruction's own
/// bridge, minus the call-site re-validation the compiled guard already did.
/// With it on, a retarget back to a target that already has a cell re-points
/// the site's newest cell to it (as the retarget switch does), so the two
/// cells a compile reads are the two targets the site flips between. Read
/// once per site.
pub(in crate::runtime::invokedynamic) fn jit_indy_mutable_bimorphic_enabled(state: &IndyDirectState) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    match state.bimorphic_gate.load(Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on =
                true;
            state.bimorphic_gate.store(if on { 2 } else { 1 }, Relaxed);
            on
        }
    }
}

/// JIT round 21 wave 1 (lane indy, INDY21-1): the bridge's answer for a site
/// pointer that may be a bimorphic guard's second arm (an
/// [`IndyMutableGuardCell`], kind [`JIT_INDY_SITE_GUARDED_LEAF`]).
///
/// * Not such a cell: `(None, site_ptr)` -- the bridge carries on unchanged.
/// * A cell of this VM whose caller was not redefined since, whose leaf takes
///   `arg_count` words and (a virtual leaf) has a non-null receiver:
///   `(Some(leaf), site_ptr)` -- the bridge dispatches the leaf with the same
///   argument words, exactly as its own direct path does for the same leaf
///   (`jit_indy_linked_direct_leaf`). The compiled guard has just compared
///   the call site's current target with the cell's handle, so the call
///   site's re-validation is done.
/// * A cell whose terms fail: `(None, owner site)` -- the instruction's own
///   bridge serves the call (it re-reads the call site's target).
///
/// Reads only.
///
/// # Safety
///
/// `vm_ptr` is 0 or a live `SharedVm`; `site_ptr` is a bridge site pointer as
/// compiled code passed it (any kind, or null); `args_ptr` holds `arg_count`
/// words.
pub unsafe fn jit_indy_guarded_leaf(
    vm_ptr: usize,
    site_ptr: usize,
    args_ptr: *const i64,
    arg_count: i64,
) -> (Option<&'static cratonvm_jit::JitInvokeInfo>, usize) {
    if jit_indy_site_kind(site_ptr) != JIT_INDY_SITE_GUARDED_LEAF {
        return (None, site_ptr);
    }
    // SAFETY: the kind word says it is a leaked, immutable cell.
    let cell = &*(site_ptr as *const IndyMutableGuardCell);
    let leaf = cell.leaf;
    let ok = vm_ptr != 0
        && arg_count >= 0
        && cell.vm == (*(vm_ptr as *const SharedVm)).vm_identity
        && !cell.gate.is_stale()
        && leaf.num_jit_args == arg_count as usize
        && (leaf.invoke_kind == 3 || (arg_count > 0 && !args_ptr.is_null() && *args_ptr != 0));
    if ok {
        (Some(leaf), site_ptr)
    } else {
        (None, cell.owner_site)
    }
}

/// Bridge misses of an armed retargeted site ([`IndyDirectState::retarget_misses`])
/// before its owner method is recompiled against the current target. A site
/// flipped more often than this (a flip storm) never recompiles: its guard
/// keeps whatever target it planned, and the bridge serves the rest.
pub(in crate::runtime::invokedynamic) const INDY_MUTABLE_RETARGET_MISSES: u32 = 1024;

/// Doublings [`retarget_miss_threshold`] applies at most (1024 << 12 = 4 Mi
/// bridge calls, well inside `u32`).
pub(in crate::runtime::invokedynamic) const INDY_MUTABLE_RETARGET_BACKOFF_MAX_SHIFT: u32 = 12;

/// Retarget recompiles one site may ask for ([`IndyDirectState::retarget_recompiles`]):
/// a backstop, not the storm gate ([`INDY_MUTABLE_RETARGET_MISSES`] is). HotSpot
/// recompiles a `setTarget` dependent on every retarget, and its trap-driven
/// cutoffs (`PerBytecodeRecompilationCutoff` 200, `PerMethodRecompilationCutoff`
/// 400) do not count dependency invalidations; past this many the site keeps
/// its guard and the bridge, as HotSpot's site would after it stopped inlining.
pub(in crate::runtime::invokedynamic) const INDY_MUTABLE_RETARGET_RECOMPILES: u32 = 1000;

/// `CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE` (JIT round 21 wave 1, lane
/// indy; **default on**, `=1` enables; effective only with
/// `CRATONVM_JIT_INDY_MUTABLE_GUARD`): HotSpot's `setTarget` dependency, made
/// lazy. A `MutableCallSite` / `VolatileCallSite` retargeted away from the
/// target its owner method's guarded call planned
/// ([`IndyDirectState::planned_cell`]) arms the site; once the bridge has
/// served [`INDY_MUTABLE_RETARGET_MISSES`] calls of the new target, the owner
/// method is recompiled through the late-bind door (its publication retires
/// the displaced body, so baked callers forward), and the recompile plans the
/// current target. A retarget back to a target that already has a guard cell
/// re-points the site's newest cell to it (no mint). At most
/// [`INDY_MUTABLE_RETARGET_RECOMPILES`] per site. The guard itself is
/// unchanged and is what keeps every body exact: this only decides which
/// target the compiled call is planned against. Read once per site.
pub(in crate::runtime::invokedynamic) fn jit_indy_mutable_retarget_recompile_enabled(state: &IndyDirectState) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    match state.retarget_gate.load(Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            state.retarget_gate.store(if on { 2 } else { 1 }, Relaxed);
            on
        }
    }
}

/// The bridge-miss count at which an armed site asks its next retarget
/// recompile, given the `recompiles` it already asked ([`IndyDirectState::retarget_recompiles`]):
/// [`INDY_MUTABLE_RETARGET_MISSES`] for the first, doubled for each one asked
/// since, up to [`INDY_MUTABLE_RETARGET_BACKOFF_MAX_SHIFT`] doublings.
///
/// INDY21 closure (2026-11-02): the r22 A/B measured the retarget switch ALONE
/// at 1.16-1.18x on a row it cannot help, and the pair with
/// `CRATONVM_JIT_INDY_MUTABLE_BIMORPHIC` at 0.86x. A site flipped between two
/// targets every batch longer than [`INDY_MUTABLE_RETARGET_MISSES`] calls
/// (`R21IndyMcsFlip` `mcs-flip-loop`) recompiled once per batch against the
/// target it had just left the running activation of, so each new body missed
/// the whole next batch (100% bridge calls against the planned-once body's
/// 50%). The doubling lets a one-way retarget recompile as before and makes a
/// ping-pong site settle on a body after a few asks instead of replacing it
/// every batch.
pub(in crate::runtime::invokedynamic) fn retarget_miss_threshold(recompiles: u32) -> u32 {
    INDY_MUTABLE_RETARGET_MISSES << recompiles.min(INDY_MUTABLE_RETARGET_BACKOFF_MAX_SHIFT)
}

/// `CRATONVM_JIT_INDY_CONSTANT_BRIDGE_RECOMPILE` (JIT round 21 lane
/// indyconst, REFLECT21-5; **default ON**, `=0` off; page
/// `jr21-indy-constant-recompile-request-spent-on-already-queued-FIXED-20261015.md`):
/// may a `ConstantCallSite` bridge that keeps serving the direct leaf the
/// optimizing tier would compile as a plain call
/// ([`jit_indy_constant_direct_target`]) ask, a bounded number of times, for
/// one more optimizing compile of its method? The one request the first bind
/// makes ([`JitIndyGenericSite::request_constant_recompile`]) is spent even
/// when the tier answers "already queued" (the C1 -> C2 upgrade the C1 publish
/// queued, whose plan may read the site before the bind) or when the bind came
/// after the optimizing body was built; either way that body bridges the site
/// for good. The constant twin of
/// [`jit_indy_mutable_guard_bridge_recompile_enabled`]. Read once per site, on
/// its first served call (no static).
pub(in crate::runtime::invokedynamic) fn jit_indy_constant_bridge_recompile_enabled(state: &IndyDirectState) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    match state.constant_bridge_gate.load(Relaxed) {
        1 => false,
        2 => true,
        _ => {
            // Without the constant invoke nothing plans the call, so asking
            // would only recompile the same bridge.
            let on = jit_indy_constant_invoke_enabled();
            state
                .constant_bridge_gate
                .store(if on { 2 } else { 1 }, Relaxed);
            on
        }
    }
}

/// Optimizing recompiles one site's bridge may queue for a guard it planned
/// but no published body carries ([`IndyDirectState::guard_bridge_recompiles`]).
pub(in crate::runtime::invokedynamic) const INDY_MUTABLE_BRIDGE_RECOMPILES: u32 = 4;

/// The first bridge-served call count at which a site asks for that
/// recompile; later asks at every power of two above it.
pub(in crate::runtime::invokedynamic) const INDY_MUTABLE_BRIDGE_FIRST_ASK: u32 = 1024;

/// `CRATONVM_JIT_INDY_MUTABLE_GUARD_BRIDGE_RECOMPILE` (JIT round 21 lane
/// reflect; **default ON**, `=0` off; page
/// `jr21-reflective-invoke-and-mcs-flip-handoff-rows-FIXED-20261014.md`): may a
/// `MutableCallSite` / `VolatileCallSite` bridge that keeps serving the direct
/// leaf of the target its newest guard cell plans for ask, a bounded number of
/// times, for one more optimizing compile of its method? The guard is planned
/// only at compile time, and the one recompile request the first cell makes
/// (`request_constant_recompile`) is spent even when the tier answers
/// "already queued" (the C1 -> C2 upgrade the C1 publish queued, which may
/// have read the site before any cell existed) or when a retarget came after
/// the optimizing body was built -- either way the body has no guard for the
/// target the site now holds and every call pays the bridge. Read once per
/// site, on its first served call (no static).
pub(in crate::runtime::invokedynamic) fn jit_indy_mutable_guard_bridge_recompile_enabled(state: &IndyDirectState) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    match state.guard_bridge_gate.load(Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            state
                .guard_bridge_gate
                .store(if on { 2 } else { 1 }, Relaxed);
            on
        }
    }
}

/// Displaced memos a site keeps for a retarget back ([`IndyDirectState::recent`]).
pub(in crate::runtime::invokedynamic) const INDY_DIRECT_RECENT: usize = 3;

/// `CRATONVM_JIT_INDY_DIRECT_RECENT_TARGETS` (JIT round 20 wave 17, lane
/// pStr3; **default ON**, `=0` restores the one-memo site): may a linked
/// direct `invokedynamic` site keep the few memos its retargets displaced
/// ([`IndyDirectState::recent`]) and swap one back in on a retarget back to
/// its target? Read once per site, on its first retarget.
pub(in crate::runtime::invokedynamic) fn jit_indy_direct_recent_targets_enabled(state: &IndyDirectState) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    match state.recent_gate.load(Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = true;
            state.recent_gate.store(if on { 2 } else { 1 }, Relaxed);
            on
        }
    }
}

/// JIT round 20 wave 11 (lane pIndy, SPLICE19-1 of
/// `jit-r19-splice19-proposals.md`): one planned target of a NON-constant call
/// site, for the optimizing tier's guarded call
/// (`IrBuilder::try_indy_mutable_guarded_call`): "while the call site's
/// `target` field holds the handle in `target_slot`, this instruction is a
/// call of `leaf`".
///
/// Both slots are the untagged boxes of JNI global references of this VM,
/// never released (the `compiled_ldc_slot_for` slot kind): stable addresses,
/// GC roots (so the planned handle stays alive and no other object can come
/// to sit at its address), rewritten in place while every mutator is parked,
/// which is what lets compiled code compare their CURRENT addresses.
///
/// LEAK(intentional, bounded): compiled code bakes the two slot addresses, so
/// neither the cell nor its references are ever freed. At most
/// [`INDY_DIRECT_MAX_FILLS`] cells per site (`IndyDirectState::mutable_cells`).
///
/// JIT round 21 wave 1 (lane indy, INDY21-1, [`jit_indy_mutable_bimorphic_enabled`]):
/// `#[repr(C)]` with a bridge-site header (`kind` = [`JIT_INDY_SITE_GUARDED_LEAF`]
/// at offset 0, a constant word that is always `0` at
/// [`JIT_INDY_SITE_CONSTANT_WORD_OFFSET`]), so the cell's own address is a
/// bridge-site pointer: a bimorphic guard's SECOND arm is an ordinary
/// `Op::IndyBridge` of the cell, and `jit_indy_bridge` dispatches the cell's
/// leaf directly ([`jit_indy_guarded_leaf`]) or, when the leaf's terms fail,
/// falls back to the instruction's own site (`owner_site`).
#[repr(C)]
pub(in crate::runtime::invokedynamic) struct IndyMutableGuardCell {
    pub(in crate::runtime::invokedynamic) kind: u32,
    /// Always `0`: a cell has no lambda singleton (the constant-lambda probe
    /// of `Op::IndyBridge` reads this word for a zero-argument reference site).
    pub(in crate::runtime::invokedynamic) constant_word: usize,
    /// The instruction's [`JitIndyGenericSite`] (process-lived).
    pub(in crate::runtime::invokedynamic) owner_site: usize,
    pub(in crate::runtime::invokedynamic) vm: usize,
    pub(in crate::runtime::invokedynamic) call_site_slot: usize,
    pub(in crate::runtime::invokedynamic) target_slot: usize,
    /// `CallSite.target`'s field slot in the call site's class.
    pub(in crate::runtime::invokedynamic) field_index: u32,
    /// mcs21 (MCS21-2, `CRATONVM_JIT_INDY_MUTABLE_GUARD_ROW`): `1 + the
    /// compact displacement` of that field in the call site's exact class
    /// ([`guard_target_field_row`]), or `0` (a legacy-layout class, compact
    /// reference fields off, a field that is not a reference). Read once at the
    /// mint: the call site is the one object the cell's root slot names.
    pub(in crate::runtime::invokedynamic) field_row: u32,
    /// The bound target's dispatch info ([`indy_direct_leaf_info`]).
    pub(in crate::runtime::invokedynamic) leaf: &'static cratonvm_jit::JitInvokeInfo,
    /// The caller class's redefinition gate of the row the leaf was taken from.
    pub(in crate::runtime::invokedynamic) gate: crate::classloading::resolution::RedefineGate,
}

/// mcs21 (MCS21-2): `1 + the compact displacement` of the `CallSite.target`
/// field (slot `field_index`) in `class_id`, the exact class of the guarded
/// site's call site, or `0` when there is none to name: compact reference
/// fields are off, the class has no compact layout (a legacy-layout or
/// synthetic class), the slot is out of range, or the field is not stored as a
/// reference. The same `compact_field_disp_storage(class, index)` answer the
/// compile door's ordinary field rows are made of
/// (`vm/src/jit/helpers/table.rs`), so the lowering reads the field at the
/// displacement the allocator laid it out at.
pub(in crate::runtime::invokedynamic) fn guard_target_field_row(class_id: ClassId, field_index: u32) -> u32 {
    if !cratonvm_types::compact_ref_fields_enabled() {
        return 0;
    }
    let Ok(index) = usize::try_from(field_index) else {
        return 0;
    };
    match cratonvm_types::compact_field_disp_storage(class_id.as_u32(), index) {
        Some((offset, storage)) if storage.is_reference() => u32::try_from(offset)
            .ok()
            .and_then(|o| o.checked_add(1))
            .unwrap_or(0),
        _ => 0,
    }
}

/// mcs21 review: claim one of a site's [`INDY_DIRECT_MAX_FILLS`] guard cells
/// ([`IndyDirectState::mutable_cells`]). The count stops at the cap. It used to
/// be a bare `fetch_add` that kept counting past it, so a site that kept asking
/// (every flip back to a target whose cell the cap had refused) wrapped the
/// `u32` after 2^32 asks and minted more never-released cells, each holding two
/// JNI global references.
pub(in crate::runtime::invokedynamic) fn claim_guard_cell_budget(minted: &std::sync::atomic::AtomicU32) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let mut curr = minted.load(Relaxed);
    while curr < INDY_DIRECT_MAX_FILLS {
        match minted.compare_exchange_weak(curr, curr + 1, Relaxed, Relaxed) {
            Ok(_) => return true,
            Err(actual) => curr = actual,
        }
    }
    false
}

impl JitIndyGenericSite {
    /// The `GENERIC_INDY_SITES` key of the instruction this site compiles:
    /// `singleton_site` is `(method key, pc after the instruction)`, the same
    /// two halves [`indy_instruction_key`] takes from an interpreter frame.
    #[inline]
    pub(in crate::runtime::invokedynamic) fn instruction_key(&self, shared: &SharedVm) -> GenericIndyKey {
        let (method_key, pc) = self.singleton_site;
        (
            shared.vm_identity,
            self.class_id,
            self.cp_index,
            method_key,
            pc,
        )
    }

    /// The singleton this site's slot memo holds for `proxy_class_id` on this
    /// VM, or `None` (no memo yet, another VM or proxy class, retired slot).
    #[inline]
    pub(in crate::runtime::invokedynamic) fn singleton_memo(&self, shared: &SharedVm, proxy_class_id: ClassId) -> Option<ObjectRef> {
        let (vm_identity, proxy, slot) = self.singleton_slot.get()?;
        if *vm_identity != shared.vm_identity || *proxy != proxy_class_id {
            return None;
        }
        slot.get()
    }

    /// Learn the slot of the row the table just answered from (or minted).
    /// First writer wins; a site whose proxy later changes keeps asking the
    /// table.
    pub(in crate::runtime::invokedynamic) fn remember_singleton_slot(&self, shared: &SharedVm, proxy_class_id: ClassId) {
        if self.singleton_slot.get().is_some() {
            return;
        }
        if let Some(slot) = lambda_singleton_slot(shared, proxy_class_id, self.singleton_site) {
            let word = Arc::as_ptr(&slot) as usize;
            let learnt = self
                .singleton_slot
                .set((shared.vm_identity, proxy_class_id, slot))
                .is_ok();
            // nio21 wave 10, lane L3 (W9L2-1): the compiled site may now read
            // the instance itself. Only after `set` succeeded: the `Arc` the
            // `OnceLock` holds is what keeps `word` valid for the site's life.
            if learnt && self.constant_lambda {
                self.constant_word
                    .store(word, std::sync::atomic::Ordering::Release);
            }
        }
    }

    /// The published constant word ([`JIT_INDY_SITE_CONSTANT_WORD_OFFSET`]),
    /// for tests and diagnostics.
    #[cfg(test)]
    pub(in crate::runtime::invokedynamic) fn constant_word(&self) -> usize {
        self.constant_word
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

/// The bridge kind of a site pointer handed to compiled code, or `0` for null.
///
/// SAFETY: `site_ptr` is either 0 or a pointer returned by
/// [`make_jit_indy_bridge_site_from_parts`], whose allocations are
/// process-lived.
pub unsafe fn jit_indy_site_kind(site_ptr: usize) -> u32 {
    if site_ptr == 0 {
        return 0;
    }
    *(site_ptr as *const u32)
}

/// Return a stable metadata pointer for a StringConcatFactory site, or `None`
/// for every other bootstrap.  The allocation is intentionally process-lived:
/// native code embeds the pointer and no individual compiled artifact owns it.
pub fn make_jit_string_concat_site_from_parts(
    pool: &ConstantPool,
    bootstraps: &[BootstrapMethod],
    cp_index: u16,
) -> Option<usize> {
    let (bsm_index, nat_index) = match pool.get(cp_index)? {
        ConstantPoolEntry::InvokeDynamic {
            bootstrap_method_attr_index,
            name_and_type_index,
        } => (*bootstrap_method_attr_index, *name_and_type_index),
        _ => return None,
    };
    let (_, descriptor) = pool.get_name_and_type(nat_index)?;
    let bsm = bootstraps.get(bsm_index as usize)?;
    let handle = resolve_method_handle_full(pool, bsm.bootstrap_method_ref).ok()?;
    if handle.class_name.as_ref() != STRING_CONCAT_FACTORY {
        return None;
    }
    if !jit_concat_site_is_pool_decided(pool, bsm, descriptor, handle.member_name.as_ref()) {
        if dbg_indy_all() {
            eprintln!("[indy-all] jit concat bridge declined cp#{cp_index} {descriptor}: trap");
        }
        return None;
    }
    let (recipe, constant_args) = if handle.member_name.as_ref() == MAKE_CONCAT_WITH_CONSTANTS {
        let recipe: Vec<u16> = bsm
            .bootstrap_arguments
            .first()
            .and_then(|idx| resolve_concat_constant_units(pool, *idx))?;
        let constants: Vec<Arc<[u16]>> = bsm
            .bootstrap_arguments
            .iter()
            .skip(1)
            .map(|idx| {
                Arc::from(
                    resolve_concat_constant_units(pool, *idx)
                        .unwrap_or_default()
                        .as_slice(),
                )
            })
            .collect();
        (Arc::from(recipe.as_slice()), constants)
    } else if handle.member_name.as_ref() == MAKE_CONCAT {
        let recipe: Vec<u16> = vec![TAG_ARG_UNIT; parse_descriptor_args(descriptor).len()];
        (Arc::from(recipe.as_slice()), Vec::new())
    } else {
        return None;
    };
    let arg_types = parse_descriptor_args(descriptor).into_boxed_slice();
    let latin1_fast = concat_latin1::site_admits(&recipe, &constant_args, &arg_types);
    let tlab_result = concat_latin1::tlab_result_enabled();
    let utf16_fast = concat_latin1::site_admits_utf16(&recipe, &constant_args, &arg_types);
    Some(Box::into_raw(Box::new(JitStringConcatSite {
        kind: JIT_INDY_SITE_CONCAT,
        constant_word: std::sync::atomic::AtomicUsize::new(0),
        recipe,
        constant_args,
        target_descriptor: Arc::from(descriptor),
        arg_types,
        latin1_fast,
        tlab_result,
        utf16_fast,
        inline_plan: std::sync::OnceLock::new(),
        deopt_on_overflow: false,
        // Same name, method and descriptor the per-call `Frame::new` used, so
        // a stack walk that meets this frame sees exactly what it saw before.
        frame_class_name: Arc::from(crate::runtime::stackwalker::JIT_INDY_BRIDGE_FRAME_CLASS),
        frame_method_name: Arc::from("concat"),
        frame_method_descriptor: Arc::from("()Ljava/lang/String;"),
        frame_code: crate::runtime::frame::padded_bytecode(&[]),
        frame_exception_table: Arc::from(
            Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
        ),
    })) as usize)
}

/// JIT round 21 wave 20 (lane sbfuse, STRINTR21-4;
/// `CRATONVM_JIT_IR_SB_CHAIN_FUSE`): a concat bridge site for the recipe of an
/// explicit `new StringBuilder().append(..)...toString()` chain the optimizing
/// tier fuses (`cratonvm_jit::sb_chain_fuse::SbChain::recipe`), or `None`.
///
/// The recipe holds tags only (`\1` an operand, `\2` a constant) and every
/// literal text is a constant, so nothing in it reads as a tag by accident;
/// the operands are `String` / `char` / `int` / `long`, the result a `String`
/// -- exactly the `StringBuilder.append` renderings the general renderer
/// ([`concat_values_to_string`]) and the inline plan share. Built like
/// [`make_jit_string_concat_site_from_parts`]'s site (same switches decided at
/// construction) and process-lived like it; the caller interns it per VM
/// (`interned_jit_sb_chain_concat_site`).
pub fn make_jit_string_concat_site_from_recipe(
    recipe: &[u16],
    constants: &[Vec<u16>],
    descriptor: &str,
    deopt_on_overflow: bool,
) -> Option<usize> {
    let arg_types = parse_descriptor_args(descriptor);
    let tags_args = recipe.iter().filter(|&&u| u == TAG_ARG_UNIT).count();
    let tags_consts = recipe.iter().filter(|&&u| u == TAG_CONST_UNIT).count();
    if tags_args != arg_types.len()
        || tags_consts != constants.len()
        || tags_args + tags_consts != recipe.len()
        || !arg_types.iter().all(|t| matches!(t, 'L' | 'C' | 'I' | 'J' | 'Z' | 'F' | 'D'))
        || !descriptor.ends_with(")Ljava/lang/String;")
    {
        return None;
    }
    let recipe: Arc<[u16]> = Arc::from(recipe);
    let constant_args: Vec<Arc<[u16]>> =
        constants.iter().map(|c| Arc::from(c.as_slice())).collect();
    let arg_types = arg_types.into_boxed_slice();
    let latin1_fast = concat_latin1::site_admits(&recipe, &constant_args, &arg_types);
    let tlab_result = concat_latin1::tlab_result_enabled();
    let utf16_fast = concat_latin1::site_admits_utf16(&recipe, &constant_args, &arg_types);
    Some(Box::into_raw(Box::new(JitStringConcatSite {
        kind: JIT_INDY_SITE_CONCAT,
        constant_word: std::sync::atomic::AtomicUsize::new(0),
        recipe,
        constant_args,
        target_descriptor: Arc::from(descriptor),
        arg_types,
        latin1_fast,
        tlab_result,
        utf16_fast,
        inline_plan: std::sync::OnceLock::new(),
        deopt_on_overflow,
        // The same synthetic frame as an `invokedynamic` concat site's (read
        // only by the bridge's kill-switch arm).
        frame_class_name: Arc::from(crate::runtime::stackwalker::JIT_INDY_BRIDGE_FRAME_CLASS),
        frame_method_name: Arc::from("concat"),
        frame_method_descriptor: Arc::from("()Ljava/lang/String;"),
        frame_code: crate::runtime::frame::padded_bytecode(&[]),
        frame_exception_table: Arc::from(
            Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
        ),
    })) as usize)
}

/// Can the compiled concat bridge render this `StringConcatFactory` site from
/// the constant pool alone, exactly as the interpreted instruction does?
/// `false` keeps the site's uncommon trap, so the interpreter answers it
/// (interpreter round i1 wave 38, lane L4;
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// The bridge is built from the pool with no class manager and no mode, so it
/// cannot follow the interpreter where the interpreter needs either:
///
/// * a `Class` constant, which the interpreter renders `interface X` for a
///   loaded interface (wave 35) and resolves through the caller's loader
///   under `--real-jdk` (wave 38); the bridge rendered `class X` for every
///   class, so a hot method's output changed once it was compiled;
/// * a constant the native renderer does not model (a `MethodType`, a
///   `MethodHandle`, a dynamic constant: the bridge rendered it as `""`), and
///   the shapes `makeConcatWithConstants` refuses (the recipe's tags against
///   the call type and the constants, a return type that cannot hold a
///   `String`, more than 200 argument slots), which `--real-jdk` links
///   through the JDK's own factory (`concat_needs_the_jdk_factory`, wave 35).
///
/// javac emits none of these (its constants are strings folded into the
/// recipe), so ordinary concatenations keep the bridge.
pub(in crate::runtime::invokedynamic) fn jit_concat_site_is_pool_decided(
    pool: &ConstantPool,
    bsm: &BootstrapMethod,
    descriptor: &str,
    bsm_method: &str,
) -> bool {
    let params = parse_descriptor_args(descriptor);
    let slots: usize = params
        .iter()
        .map(|c| if matches!(c, 'J' | 'D') { 2 } else { 1 })
        .sum();
    let ret = descriptor.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    if slots > 200
        || !matches!(
            ret,
            "Ljava/lang/String;"
                | "Ljava/lang/Object;"
                | "Ljava/lang/CharSequence;"
                | "Ljava/lang/Comparable;"
                | "Ljava/io/Serializable;"
                | "Ljava/lang/constant/Constable;"
                | "Ljava/lang/constant/ConstantDesc;"
        )
    {
        return false;
    }
    if bsm_method != MAKE_CONCAT_WITH_CONSTANTS {
        return true;
    }
    let Some(recipe) = bsm
        .bootstrap_arguments
        .first()
        .and_then(|&idx| resolve_concat_constant_units(pool, idx))
    else {
        return false;
    };
    let constants = &bsm.bootstrap_arguments[1..];
    let arg_tags = recipe.iter().filter(|&&u| u == TAG_ARG_UNIT).count();
    let const_tags = recipe.iter().filter(|&&u| u == TAG_CONST_UNIT).count();
    arg_tags == params.len()
        && const_tags == constants.len()
        && constants.iter().all(|&idx| {
            matches!(
                pool.get(idx),
                Some(
                    ConstantPoolEntry::StringReference { .. }
                        | ConstantPoolEntry::Integer(_)
                        | ConstantPoolEntry::Float(_)
                        | ConstantPoolEntry::Long(_)
                        | ConstantPoolEntry::Double(_)
                )
            )
        })
}

/// Return a stable metadata pointer for ANY `invokedynamic` site the compiled
/// bridge can serve, or `None` for one it cannot.
///
/// This is the resolver both compile doors call. It answers
/// [`make_jit_string_concat_site_from_parts`] first, so a `StringConcatFactory`
/// site keeps the direct concat bridge it has had since that fix; otherwise it
/// admits a `LambdaMetafactory` site (see [`JitIndyGenericSite`] for what that
/// is worth and why the list stops there).
///
/// The allocation is intentionally process-lived: generated code embeds the
/// pointer and no individual compiled artifact owns it — same contract as the
/// concat site.
///
/// A `None` here is not a compile failure. It means "this site keeps its
/// uncommon trap", which is the behaviour every indy site had before any bridge
/// existed.
///
/// `singleton_site` names the INSTRUCTION ([`lambda_singleton_site_for`] of
/// the containing method and the bci). `None` means the caller cannot say
/// which instruction it compiles — a compile door whose resolver sees only the
/// CP index, for an entry that more than one instruction of the method
/// shares. A `LambdaMetafactory` site is then refused (it keeps its trap and
/// runs interpreted): any key the bridge picked would give one of those
/// instructions a different instance (zero-capture), or a different proxy
/// class (any shared entry, linked per instruction), from the one the
/// interpreter hands out — the tier-up identity split of
/// `interpreter-L6-lambda-singleton-identity-splits-between-interpreter-and-jit-FIXED`.
/// Every other shape ignores the key.
pub fn make_jit_indy_bridge_site_from_parts(
    pool: &ConstantPool,
    bootstraps: &[BootstrapMethod],
    cp_index: u16,
    class_id: ClassId,
    singleton_site: Option<(u64, usize)>,
) -> Option<usize> {
    if let Some(concat) = make_jit_string_concat_site_from_parts(pool, bootstraps, cp_index) {
        return Some(concat);
    }
    let (bsm_index, nat_index) = match pool.get(cp_index)? {
        ConstantPoolEntry::InvokeDynamic {
            bootstrap_method_attr_index,
            name_and_type_index,
        } => (*bootstrap_method_attr_index, *name_and_type_index),
        _ => return None,
    };
    // The generic half's kill switch. It gates SITE CONSTRUCTION, not each
    // call, so the "off" arm is the VM as it was before this bridge existed:
    // with no site the codegen lowers the indy to its uncommon trap, and
    // `has_dispatch` / `needs_heap` never see it either. A switch that only
    // silenced the bridge while still claiming the site would be measuring a
    // third thing that ships nowhere.
    //
    // The `StringConcatFactory` half above is deliberately NOT gated: it
    // predates this switch and is not what a bisection here is asking about.
    if !crate::runtime::env_cache::jit_indy_bridge() {
        return None;
    }
    let (_, descriptor) = pool.get_name_and_type(nat_index)?;
    // A `void` site has no stack effect for the codegen's typed push to model
    // and does not occur in practice, so it keeps the trap rather than being
    // guessed at. Every other return kind is served: the bridge hands back one
    // `i64` and the call site pushes it by the descriptor, exactly as an
    // ordinary invoke's return is pushed.
    let ret = descriptor.rsplit(')').next()?.as_bytes().first().copied()?;
    if ret == b'V' {
        return None;
    }
    let bsm = bootstraps.get(bsm_index as usize)?;
    let handle = resolve_method_handle_full(pool, bsm.bootstrap_method_ref).ok()?;
    // The admitted set is exactly the bootstraps whose implementation in
    // `execute_invokedynamic` touches the frame ONLY through its operand stack
    // and its `class_id` — which is all the synthetic frame this bridge builds
    // can offer. Between them they cover every shape that makes a whole method
    // permanently uncompilable in ordinary Java:
    //
    //   `LambdaMetafactory`  every lambda and method reference;
    //   `SwitchBootstraps`   a pattern-matching `switch`;
    //   `ObjectMethods`      a record's `equals`/`hashCode`/`toString`.
    //
    // Deliberately NOT admitted: Groovy's `IndyInterface`, and the generic
    // MethodHandle fallback below it. Those build adapter chains whose
    // behaviour this tree already documents as caller-sensitive (see the
    // `groovy_cast_to_boolean` arm), and a bridge is the wrong place to
    // discover that. They keep the trap they have today.
    let admitted = match handle.class_name.as_ref() {
        // javac's shapes only (wave 48): any other lambda site may link
        // through the JDK's own `LambdaMetafactory` (`bootstrap_generic_jdk`),
        // the generic route this bridge does not admit.
        LAMBDA_METAFACTORY => {
            matches!(handle.member_name.as_ref(), METAFACTORY | ALT_METAFACTORY)
                && lambda_site_is_javac_shape(
                    pool,
                    &handle.member_name,
                    &handle.descriptor,
                    &bsm.bootstrap_arguments,
                )
        }
        SWITCH_BOOTSTRAPS => matches!(handle.member_name.as_ref(), TYPE_SWITCH | ENUM_SWITCH),
        // Only a site whose getters are all `REF_getField`s of the record
        // class (wave 39): any other getter may link through the JDK's own
        // `ObjectMethods` (`bootstrap_generic`, `--real-jdk`), the generic
        // route this bridge does not admit.
        OBJECT_METHODS => {
            handle.member_name.as_ref() == BOOTSTRAP
                && object_methods_getters_are_record_fields(pool, &bsm.bootstrap_arguments)
        }
        _ => false,
    };
    if !admitted {
        return None;
    }
    let arg_types: Vec<u8> = parse_descriptor_args(descriptor)
        .into_iter()
        .map(|c| c as u8)
        .collect();
    // See the doc above. Every `LambdaMetafactory` site consults the key: a
    // zero-capture one for its singleton, and ANY one whose entry other
    // instructions share for its per-instruction linkage (its proxy class,
    // `lambda_entry_is_shared`). A door passes `None` only for an entry shared
    // within the method, so a capturing site is refused too. `(0, 0)` for
    // every other shape is never read.
    let singleton_site = match singleton_site {
        Some(site) => site,
        None if handle.class_name.as_ref() == LAMBDA_METAFACTORY => {
            return None;
        }
        None => (0, 0),
    };
    Some(new_jit_indy_generic_site(
        class_id,
        cp_index,
        descriptor,
        arg_types,
        ret,
        singleton_site,
        // Round 16 (lane indy): decided ONCE, here, so the bridge reads no flag
        // per call and no process global holds it.
        handle.class_name.as_ref() == SWITCH_BOOTSTRAPS,
        // Round 16 wave 7 (lane indy): the same, for a record site.
        handle.class_name.as_ref() == OBJECT_METHODS,
        false,
    ))
}



/// `CRATONVM_JIT_INDY_LINKED_CALL_SITE` (JIT round 16 wave 7, lane indy;
/// **default ON**, `=0` off; it was opt-in until measured): may the compile doors bridge an
/// `invokedynamic` whose bootstrap the bridge does not implement, provided the
/// INSTRUCTION is already linked (a `GENERIC_INDY_SITES` row) when its site is
/// built? See [`make_jit_indy_linked_site_from_parts`]. Read per site build.
pub fn jit_indy_linked_call_site_enabled() -> bool {
    true
}

/// Allocate one process-lived [`JitIndyGenericSite`] (both makers' common tail).
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::invokedynamic) fn new_jit_indy_generic_site(
    class_id: ClassId,
    cp_index: u16,
    descriptor: &str,
    arg_types: Vec<u8>,
    ret: u8,
    singleton_site: (u64, usize),
    switch_fast: bool,
    record_fast: bool,
    generic_linked: bool,
) -> usize {
    let frame_max_stack = u16::try_from(arg_types.len())
        .unwrap_or(u16::MAX)
        .saturating_add(1);
    // nio21 wave 10, lane L3 (W9L2-1): only a non-capturing site of a
    // reference type, built by the bridge's own `LambdaMetafactory` admission
    // (a `generic_linked` site's answer is a `CallSite` target's, never a
    // singleton row), may publish its constant word.
    let constant_lambda = arg_types.is_empty()
        && ret == b'L'
        && !generic_linked
        && jit_indy_constant_lambda_enabled();
    // nio21 wave 14, lane L1 (W11L3-2): see `JitIndyGenericSite::capture_fast`.
    let capture_fast = !arg_types.is_empty()
        && ret == b'L'
        && !generic_linked
        && jit_indy_capture_fast_enabled()
        && true
        && Err::<String, std::env::VarError>(std::env::VarError::NotPresent).is_err();
    Box::into_raw(Box::new(JitIndyGenericSite {
        kind: JIT_INDY_SITE_GENERIC,
        constant_word: std::sync::atomic::AtomicUsize::new(0),
        constant_lambda,
        capture_fast,
        capture_memo: std::sync::atomic::AtomicU64::new(0),
        class_id,
        cp_index,
        target_descriptor: Arc::from(descriptor),
        arg_types: Arc::from(arg_types.as_slice()),
        frame_class_name: Arc::from(crate::runtime::stackwalker::JIT_INDY_BRIDGE_FRAME_CLASS),
        frame_method_name: Arc::from(JIT_INDY_FRAME_METHOD),
        frame_code: crate::runtime::frame::padded_bytecode(&[]),
        frame_exception_table: Arc::from(
            Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
        ),
        frame_max_stack,
        return_type: ret,
        singleton_site,
        singleton_slot: std::sync::OnceLock::new(),
        switch_fast,
        record_fast,
        generic_linked,
        linked_direct: generic_linked,
        direct: IndyDirectState::default(),
        owner_method: std::sync::OnceLock::new(),
        constant_recompile_requested: std::sync::atomic::AtomicBool::new(false),
    })) as usize
}

/// nio21 wave 11, lane L3 (W10L3-1): does `site_ptr` name a generic site whose
/// constant word is published AND whose slot holds an instance right now --
/// i.e. would its compiled probe answer the singleton? Reads only the site.
pub fn jit_indy_constant_word_published(site_ptr: usize) -> bool {
    // SAFETY: `site_ptr` is 0 or a site pointer the intern table handed out;
    // every such allocation is process-lived.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return false;
    }
    // SAFETY: as above, and the kind word says it is a generic site.
    let Some(site) = (unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() }) else {
        return false;
    };
    site.constant_word
        .load(std::sync::atomic::Ordering::Acquire)
        != 0
        && site
            .singleton_slot
            .get()
            .is_some_and(|(_, _, slot)| slot.get().is_some())
}

/// JIT round 20 wave 11 (lane pIndy, W10L3-3 / W11L3-3): the address of a
/// generic site's singleton slot -- its published constant word
/// ([`JIT_INDY_SITE_CONSTANT_WORD_OFFSET`]) -- when the word is published and
/// the slot holds an instance now ([`jit_indy_constant_word_published`]),
/// else `0`; `0` too with `CRATONVM_JIT_INDY_CONSTANT_LAMBDA` off. The word is
/// published once (`remember_singleton_slot` stores it only after the
/// site's `OnceLock` took the slot's `Arc`), so the optimizing tier may bake
/// the slot address and read the instance from it (`Op::IndyRootRef`): a GC
/// root rewritten in place while mutators are parked, `0` once retired.
/// Reads only the site: safe on a background compile thread.
pub fn jit_indy_constant_word_slot(site_ptr: usize) -> usize {
    if !jit_indy_constant_lambda_enabled() || !jit_indy_constant_word_published(site_ptr) {
        return 0;
    }
    // SAFETY: `jit_indy_constant_word_published` answered `true`, so this is a
    // process-lived generic site.
    match unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() } {
        Some(site) => site
            .constant_word
            .load(std::sync::atomic::Ordering::Acquire),
        None => 0,
    }
}

/// t1 wave 11, lane JLAM (LAMBDATRAP21-4): is `site_ptr` a generic bridge
/// site built by the bridge's own `LambdaMetafactory` admission for a
/// NON-CAPTURING reference site (`JitIndyGenericSite::constant_lambda`),
/// whether or not its word is published yet? Answers the question
/// [`jit_indy_constant_word_slot`]'s `0` cannot: "not a lambda" versus "a
/// lambda nobody evaluated yet". Such a site's bridge links the instruction
/// on its first evaluation through the interpreter's tables (the
/// interpreter's singleton, `singleton_site`) and publishes the word, so the
/// optimizing tier may call the bridge there instead of trapping
/// (`CompileRequest::indy_constant_lambda_site_resolver`). Reads only the
/// site: safe on a background compile thread.
pub fn jit_indy_site_is_constant_lambda(site_ptr: usize) -> bool {
    // SAFETY: `site_ptr` is 0 or a site pointer the intern table handed out;
    // every such allocation is process-lived.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return false;
    }
    // SAFETY: as above, and the kind word says it is a generic site.
    match unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() } {
        Some(site) => site.constant_lambda,
        None => false,
    }
}

/// `CRATONVM_JIT_INDY_CONSTANT_PRIME` (nio21 wave 11, lane L3, W10L3-4; perf,
/// **default ON**): may a compile door learn a non-capturing site's singleton
/// slot at compile time ([`prime_jit_indy_constant_word`]) instead of at the
/// site's first bridged evaluation? Read per prime (no static).
pub fn jit_indy_constant_prime_enabled() -> bool {
    true
}

/// nio21 wave 11, lane L3 (W10L3-4): learn a constant-lambda site's singleton
/// slot NOW, from the tables the bridge's warm path would read, and answer
/// [`jit_indy_constant_word_published`].
///
/// The proxy class comes from exactly where the bridge's frame-free fast path
/// takes it when this thread's site cache misses: the VM-wide resolution
/// cache's per-entry row, else the per-instruction row of a shared entry
/// (`probe_lambda_instruction_site`). The slot is then the
/// `LAMBDA_SINGLETON_CACHE` row `(vm, proxy, the site's instruction key)`,
/// which exists only once the interpreter (or the bridge) minted that
/// instruction's singleton, so the word published is the interpreter's
/// instance. Nothing here reads the heap or allocates: the background compile
/// worker may call it. Writes only the site's own first-writer-wins memo
/// (`remember_singleton_slot`); it does not touch the resolution cache.
///
/// Lock order: the resolution cache's read lock under the caller's class
/// manager read lock (the established order), then the singleton table's.
pub fn prime_jit_indy_constant_word(shared: &SharedVm, site_ptr: usize) -> bool {
    // SAFETY: as in `jit_indy_constant_word_published`.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return false;
    }
    // SAFETY: as above.
    let Some(site) = (unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() }) else {
        return false;
    };
    if !site.constant_lambda {
        return false;
    }
    if site
        .constant_word
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
        && site.singleton_slot.get().is_none()
        && jit_indy_constant_prime_enabled()
        && jit_indy_lambda_fast_enabled()
    {
        let per_entry = {
            let cache = shared.classes.resolution_cache.read();
            match cache.get_call_site(site.class_id, site.cp_index) {
                Some(ResolvedCallSite::Lambda(lcs)) => {
                    Some((lcs.proxy_class_id, lcs.capture_types.len()))
                }
                _ => None,
            }
        };
        let linked = per_entry.or_else(|| {
            let (method_key, pc) = site.singleton_site;
            probe_lambda_instruction_site(&(
                shared.vm_identity,
                site.class_id,
                site.cp_index,
                method_key,
                pc,
            ))
        });
        if let Some((proxy_class_id, 0)) = linked {
            site.remember_singleton_slot(shared, proxy_class_id);
        }
    }
    jit_indy_constant_word_published(site_ptr)
}

/// JIT round 20 wave 12 (lane qLamNew, W10L3-2 / W11L3-2 second half / W9L2-1
/// part 2; asked only with `CRATONVM_JIT_IR_INDY_CAPTURE_NEW` on): the
/// `(proxy class id, capture count)` a linked CAPTURING lambda site allocates on
/// every evaluation, for the optimizing tier to build the allocation itself
/// (`IrBuilder::try_indy_capture_new`), or `None`.
///
/// The fact is the bridge's own warm-path memo
/// ([`JitIndyGenericSite::capture_memo`]), learnt here from the same tables the
/// bridge reads when the memo is empty: the resolution cache's per-entry row,
/// else the per-instruction row of a shared entry
/// (`probe_lambda_instruction_site`). It is offered only where the memo is
/// sound -- a `capture_fast` site (the bridge's own `LambdaMetafactory`
/// admission, linked rows permanent, no allocation-watch diagnostic) with a
/// reference result and the frame-free fast path on -- and only for a proxy the
/// VM spun itself (`ClassRealm::is_lambda_proxy_class`): such a class has no
/// `<clinit>`, no finalizer and no compact layout, and capture `i` lives in its
/// field `i` (`allocate_lambda_proxy_tlab_fast`). Refused under the
/// `CRATONVM_DBG_DEADREF_STORE` diagnostic, which only the bridge's full path
/// serves.
///
/// Nothing here reads the heap or allocates: a background compile worker may
/// call it. Writes only the site's first-writer-wins memo, with the value the
/// bridge would write. Lock order: the resolution cache's read lock, released,
/// then the proxy table's read lock.
pub fn jit_indy_capture_alloc_class(shared: &SharedVm, site_ptr: usize) -> Option<(u32, usize)> {
    // SAFETY: `site_ptr` is 0 or a site pointer the intern table handed out;
    // every such allocation is process-lived.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return None;
    }
    // SAFETY: as above, and the kind word says it is a generic site.
    let site = unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() }?;
    if !site.capture_fast
        || site.return_type != b'L'
        || site.arg_types.is_empty()
        || !jit_indy_lambda_fast_enabled()
        || cratonvm_types::flags().gc.dbg_deadref_store
    {
        return None;
    }
    let (proxy_class_id, num_captures) =
        match decode_capture_memo(site.capture_memo.load(std::sync::atomic::Ordering::Acquire)) {
            Some(facts) => facts,
            None => {
                let per_entry = {
                    let cache = shared.classes.resolution_cache.read();
                    match cache.get_call_site(site.class_id, site.cp_index) {
                        Some(ResolvedCallSite::Lambda(lcs)) => {
                            Some((lcs.proxy_class_id, lcs.capture_types.len()))
                        }
                        _ => None,
                    }
                };
                let (proxy_class_id, num_captures) = per_entry.or_else(|| {
                    let (method_key, pc) = site.singleton_site;
                    probe_lambda_instruction_site(&(
                        shared.vm_identity,
                        site.class_id,
                        site.cp_index,
                        method_key,
                        pc,
                    ))
                })?;
                if num_captures != site.arg_types.len() {
                    return None;
                }
                let word = encode_capture_memo(proxy_class_id, num_captures);
                if word != 0 {
                    // First writer wins; a concurrent bridge writes the same
                    // pair (the linkage is permanent).
                    let _ = site.capture_memo.compare_exchange(
                        0,
                        word,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    );
                }
                (proxy_class_id, num_captures)
            }
        };
    if num_captures == 0
        || num_captures != site.arg_types.len()
        || !shared.classes.is_lambda_proxy_class(proxy_class_id)
    {
        return None;
    }
    Some((proxy_class_id.as_u32(), num_captures))
}

/// JIT round 21 (lane lambda2, TLOCAL21-1 / LAMBDATRAP21-3): the proxy class
/// of every value a linked lambda site evaluates to, for the optimizing tier's
/// impl-call plan, or `None`: a capturing site's
/// [`jit_indy_capture_alloc_class`] fact, or a non-capturing site's published
/// singleton's class (the proxy `remember_singleton_slot` recorded with the
/// slot, in this VM). The singleton answer needs the linked rows permanent
/// (`CRATONVM_RESOLUTION_LINKED_PERMANENT`, as `capture_fast` does): a row
/// that may be relinked could hand out another proxy. Nothing here reads the
/// heap or allocates: a background compile worker may call it.
pub fn jit_indy_lambda_proxy_class(shared: &SharedVm, site_ptr: usize) -> Option<u32> {
    // SAFETY: `site_ptr` is 0 or a site pointer the intern table handed out;
    // every such allocation is process-lived.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return None;
    }
    // SAFETY: as above, and the kind word says it is a generic site.
    let site = unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() }?;
    if !site.arg_types.is_empty() {
        return jit_indy_capture_alloc_class(shared, site_ptr).map(|(proxy, _)| proxy);
    }
    if site.return_type != b'L'
        || !jit_indy_constant_word_published(site_ptr)
        || !true
    {
        return None;
    }
    let (vm, proxy) = site
        .singleton_slot
        .get()
        .map(|(vm, proxy, _)| (*vm, *proxy))?;
    (vm == shared.vm_identity && shared.classes.is_lambda_proxy_class(proxy))
        .then(|| proxy.as_u32())
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): record on a generic bridge site
/// the method (`name`, `descriptor`) whose `invokedynamic` it compiles. First
/// writer wins; any other site kind (or `0`) is left alone.
pub fn note_jit_indy_site_method(site_ptr: usize, name: &str, descriptor: &str) {
    // SAFETY: `site_ptr` is 0 or a site pointer the intern table handed out;
    // every such allocation is process-lived.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return;
    }
    // SAFETY: as above, and the kind word says it is a generic site.
    if let Some(site) = unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() } {
        if site.owner_method.get().is_none() {
            let _ = site
                .owner_method
                .set((Arc::from(name), Arc::from(descriptor)));
        }
    }
}



/// A handle slot as a guard word (the MH lane's `mh_bind_slot_key`): an
/// object's address, an `Int` tagged so it can never equal an address.
pub(in crate::runtime::invokedynamic) fn indy_direct_slot_key(v: Value) -> u64 {
    match v {
        Value::Object(Some(o)) => o.as_ptr() as u64,
        Value::Object(None) => 0,
        Value::Int(i) => (1u64 << 63) | u64::from(i as u32),
        _ => u64::MAX,
    }
}

/// The guard words of `target`'s CratonVM-shim slots
/// (`MH_COMPILED_BIND_GUARD_SLOTS`), or all zero for a handle that has none
/// of them: a real JDK `DirectMethodHandle` (8 slots under `--real-jdk`,
/// which `mh_compiled_bind_target` binds too) is immutable, so its address
/// is its whole guard, and reading the shim indices out of its bounds logged
/// six `zgc real: field index OOB` warnings per bridged call (found by the
/// i41 page's `I41HandWrittenLoopShapes`).
pub(in crate::runtime::invokedynamic) fn indy_direct_guard_slots(shared: &SharedVm, target: ObjectRef) -> [u64; 6] {
    let slots = cratonvm_native_builtins::lang_invoke::MH_COMPILED_BIND_GUARD_SLOTS;
    let mut words = [0u64; 6];
    let needed = slots.iter().copied().max().unwrap_or(0);
    if (shared.mem.heap.get_header(target).num_slots() as usize) <= needed {
        return words;
    }
    for (word, slot) in words.iter_mut().zip(slots) {
        *word = indy_direct_slot_key(shared.mem.heap.get_field(target, slot));
    }
    words
}

/// JIT round 16 wave 9 (lane indy, IN16-7; `CRATONVM_JIT_INDY_LINKED_DIRECT`):
/// the dispatch info of the method a compiled `invokedynamic` may CALL in
/// place of the bridge, or `None` for the bridge.
///
/// `Some` only for a `linked_direct` site whose memo ([`IndyDirectMemo`],
/// filled by the bridge's generic linked arm) is current: the same VM and
/// address-memo epoch, the caller not redefined, the `CallSite`'s `target` still
/// the handle the admission measured with its decided-on slots unchanged, the
/// admission's answer a leaf, the call's argument count the leaf's, and for a
/// virtual leaf a non-null receiver (a null one is `MethodHandle.invoke`'s
/// NPE, so the bridge keeps it). The caller (`jit_indy_bridge`) then hands the
/// SAME argument words to `jit_invoke_dispatch_body`: the bridge's argument
/// block is the dispatch ABI's (one word per value, source order), and so is
/// its return and exception protocol.
///
/// Reads only: no allocation, no Java, not a safepoint. A contended memo is a
/// miss (the bridge). (JIT round 21 lane reflect: except, on a guarded
/// mutable site, a rare optimizing-recompile request after the memo is
/// released -- [`JitIndyGenericSite::note_guard_bridge_hit`]; a tier-manager
/// lock and a queue push, still no Java and no safepoint. JIT round 21 lane
/// indyconst: the same on a constant site,
/// [`JitIndyGenericSite::note_constant_bridge_hit`].)
///
/// # Safety
///
/// `site_ptr` is a bridge site pointer as compiled code passed it (any kind,
/// or null); `args_ptr` holds `arg_count` words.
pub unsafe fn jit_indy_linked_direct_leaf(
    shared: &SharedVm,
    site_ptr: usize,
    args_ptr: *const i64,
    arg_count: usize,
) -> Option<&'static cratonvm_jit::JitInvokeInfo> {
    if jit_indy_site_kind(site_ptr) != JIT_INDY_SITE_GENERIC {
        return None;
    }
    let site = (site_ptr as *const JitIndyGenericSite).as_ref()?;
    if !site.linked_direct {
        return None;
    }
    // JIT round 21 wave 1 (lane indy, `CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`):
    // an armed retargeted site counts this bridge call. One relaxed load
    // otherwise (the flag is only ever set with the switch on).
    if site
        .direct
        .retarget_armed
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        site.note_retarget_miss(shared);
    }
    let memo = site.direct.memo.try_lock()?;
    let m = Option::as_ref(&*memo)?;
    let leaf = m.leaf?;
    // gci i4/wr (`gci-i3wr-weak-tables-and-epoch-review-findings` item 2): the
    // memo holds raw addresses (`call_site`, `guard[0]`), so it keys on the
    // address-memo epoch, which also moves at a G1 cleanup and at every
    // Generational concurrent sweep slice (both free blocks and book no
    // collection). `fill_direct` stamps the same epoch.
    if m.vm != shared.vm_identity
        || m.epoch != shared.mem.address_epoch()
        || m.gate.is_stale()
        || leaf.num_jit_args != arg_count
    {
        return None;
    }
    let call_site = ObjectRef::from_raw(m.call_site as *mut u8);
    let target = match shared.mem.heap.get_field(call_site, m.target_slot) {
        Value::Object(Some(t)) if t.as_ptr() as u64 == m.guard[0] => t,
        _ => return None,
    };
    if indy_direct_guard_slots(shared, target)[..] != m.guard[1..] {
        return None;
    }
    if leaf.invoke_kind != 3 && (arg_count == 0 || args_ptr.is_null() || *args_ptr == 0) {
        return None;
    }
    let target_word = m.guard[0];
    let constant = m.constant;
    drop(memo);
    if constant {
        // JIT round 21 lane indyconst (`CRATONVM_JIT_INDY_CONSTANT_BRIDGE_RECOMPILE`):
        // the constant twin of the ask below -- the memo is exactly what the
        // optimizing tier compiles as a plain call, so a call reaching the
        // bridge means no published body has that call.
        site.note_constant_bridge_hit(shared, leaf);
    } else {
        // JIT round 21 lane reflect (`CRATONVM_JIT_INDY_MUTABLE_GUARD_BRIDGE_RECOMPILE`):
        // a guarded site whose call reached the bridge anyway asks, rarely and
        // a bounded number of times, for the optimizing body that guards it.
        site.note_guard_bridge_hit(shared, leaf, target_word);
    }
    Some(leaf)
}

impl JitIndyGenericSite {
    /// Learn (or re-learn) the target of `call_site`, the `Linked` row the
    /// bridge's generic arm is about to invoke, for
    /// [`jit_indy_linked_direct_leaf`]. Pure reads (the admission's included):
    /// no safepoint, so `call_site` needs no pin. A memo already current for
    /// this target, a contended memo, and a site that churned past
    /// [`INDY_DIRECT_MAX_FILLS`] are left alone.
    pub(in crate::runtime::invokedynamic) fn fill_direct(&self, shared: &SharedVm, thread: &mut JvmThread, call_site: ObjectRef) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(mut memo) = self.direct.memo.try_lock() else {
            return;
        };
        // gci i4/wr: the address-memo epoch, as `jit_indy_linked_direct_leaf`
        // checks it (both or neither).
        let epoch = shared.mem.address_epoch();
        // indy21c: a memo of this very object (the same epoch, the same
        // address) already knows its slot -- the common per-call case, with no
        // lookup; otherwise `CallSite`'s own `target` slot, not a look-alike a
        // subclass declares ([`call_site_target_slot`]).
        let known_slot = Option::as_ref(&*memo)
            .filter(|m| {
                m.vm == shared.vm_identity
                    && m.epoch == epoch
                    && m.call_site == call_site.as_ptr() as usize
            })
            .map(|m| m.target_slot);
        let Some(target_slot) = known_slot
            .or_else(|| call_site_target_slot(shared, shared.mem.heap.class_id_of(call_site)))
        else {
            return;
        };
        let target = match shared.mem.heap.get_field(call_site, target_slot) {
            Value::Object(Some(t)) => t,
            _ => return,
        };
        let raw = target.as_ptr() as u64;
        let same_epoch =
            Option::as_ref(&*memo).is_some_and(|m| m.vm == shared.vm_identity && m.epoch == epoch);
        if let Some(m) = Option::as_ref(&*memo) {
            if same_epoch
                && m.call_site == call_site.as_ptr() as usize
                && m.guard[0] == raw
                && !m.gate.is_stale()
            {
                // Current: a refused handle stays refused for this epoch.
                return;
            }
        }
        // JIT round 20 wave 17 (lane pStr3): a retarget back to a target a
        // displaced memo still describes swaps that memo in -- no fill, no
        // mint, no admission (`R20PIndyMutableSite` mcs-flip paid the generic
        // invoke on every call after the eighth flip).
        let recent_on = jit_indy_direct_recent_targets_enabled(&self.direct);
        let mut recent = if recent_on {
            self.direct.recent.try_lock()
        } else {
            None
        };
        if let Some(recent) = recent.as_mut() {
            let site_addr = call_site.as_ptr() as usize;
            let hit = recent.iter().position(|m| {
                m.vm == shared.vm_identity
                    && m.epoch == epoch
                    && m.call_site == site_addr
                    && m.guard[0] == raw
                    && m.target_slot == target_slot
                    && !m.gate.is_stale()
            });
            if let Some(i) = hit {
                if recent[i].guard[1..] == indy_direct_guard_slots(shared, target)[..] {
                    let back = recent.remove(i);
                    // JIT round 21 wave 1 (lane indy): the swapped-in memo's
                    // bound leaf of a MUTABLE site, for the guard cell below.
                    let regained = back
                        .leaf
                        .filter(|_| !back.constant && jit_indy_mutable_guard_enabled())
                        .map(|leaf| (leaf, back.gate.clone()));

                    // JIT round 21 lane reflect (routing fix, unconditional):
                    // the swapped-in target's guard cell is the site's newest
                    // again. Without this the newest cell kept naming the
                    // target the site had just LEFT, so an optimizing compile
                    // after a flip back (`R20PIndyMutableSite`: B then A
                    // before the stable rows) planned its guard on the wrong
                    // handle and every call missed into the bridge. The cell
                    // is leaked and immutable, and was minted for this very
                    // memo's call site and target (same epoch: same objects).
                    if back.cell != 0 && back.leaf.is_some() && jit_indy_mutable_guard_enabled() {
                        self.direct
                            .mutable_cell
                            .store(back.cell, std::sync::atomic::Ordering::Release);
                    }
                    if let Some(old) = memo.replace(back) {
                        recent.insert(0, old);
                        recent.truncate(INDY_DIRECT_RECENT);
                    }
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[indy-direct] {} at cp#{}: retarget back to a recent target (no fill)",
                            self.target_descriptor, self.cp_index
                        );
                    }
                    // The memo is released before the cell work (the bridge
                    // `try_lock`s it per call); `recent` is released on return.
                    drop(memo);
                    // JIT round 21 wave 1 (lane indy,
                    // `CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`): the site's
                    // newest guard cell follows the target back (an existing
                    // cell, no mint), and a target the compiled guard did not
                    // plan arms the retarget recompile. Off: unchanged (the
                    // cell keeps naming the last FILLED target).
                    if let Some((leaf, gate)) = regained {
                        if jit_indy_mutable_retarget_recompile_enabled(&self.direct)
                            || jit_indy_mutable_bimorphic_enabled(&self.direct)
                        {
                            let cell = self.publish_mutable_guard(
                                shared,
                                call_site,
                                target,
                                target_slot,
                                leaf,
                                gate,
                            );
                            if cell == 0 {
                                self.disarm_retarget();
                            }
                        }
                    } else {
                        // INDY21 closure: no leaf (or no guard) for the target
                        // swapped back in: nothing a recompile could plan.
                        self.disarm_retarget();
                    }
                    return;
                }
            }
        }
        let fills = self.direct.fills.load(Relaxed);
        if fills >= INDY_DIRECT_MAX_FILLS {
            // INDY21 closure: the site stopped following its target; a
            // recompile would plan a target it no longer holds.
            self.disarm_retarget();
            return;
        }
        if same_epoch {
            self.direct.fills.store(fills + 1, Relaxed);
        }
        let Some((_, _, gate)) = probe_generic_indy_site_gated(&self.instruction_key(shared))
        else {
            return;
        };
        let bind = {
            let mut ctx = NativeContextImpl {
                shared,
                thread: &mut *thread,
            };
            cratonvm_native_builtins::lang_invoke::mh_compiled_bind_target(
                &mut ctx,
                target,
                &self.target_descriptor,
            )
        };
        // A `bindTo` handle's receiver is no call-site argument: the bridge.
        let bind = bind.filter(|b| !b.bound);
        let previous = Option::as_ref(&*memo).and_then(|m| m.leaf);
        let was_constant = Option::as_ref(&*memo).is_some_and(|m| m.constant && m.leaf.is_some());
        let leaf = bind.and_then(|b| {
            // JIT round 20 wave 17 (lane pStr3): a displaced memo's leaf naming
            // the same call is reused too, so a flipping site mints no more.
            let known = previous
                .filter(|p| indy_direct_leaf_names(p, &b))
                .or_else(|| {
                    recent.as_ref().and_then(|r| {
                        r.iter()
                            .filter_map(|m| m.leaf)
                            .find(|p| indy_direct_leaf_names(p, &b))
                    })
                });
            if known.is_none() && self.direct.minted.fetch_add(1, Relaxed) >= INDY_DIRECT_MAX_FILLS
            {
                return None;
            }
            Some(indy_direct_leaf_info(known.or(previous), &b))
        });
        let mut guard = [0u64; 7];
        guard[0] = raw;
        guard[1..].copy_from_slice(&indy_direct_guard_slots(shared, target));
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-direct] {} at cp#{}: {}",
                self.target_descriptor,
                self.cp_index,
                match leaf {
                    Some(l) => format!(
                        "bound to {}.{}{} kind={}",
                        l.class_name, l.method_name, l.descriptor, l.invoke_kind
                    ),
                    None => "refused (generic invoke)".to_owned(),
                }
            );
        }
        // Asked only for a bound leaf: a refused handle's site is never a
        // constant-invoke candidate (IN16-8), so it costs no class-manager read.
        let constant = leaf.is_some() && is_constant_call_site(shared, call_site);
        // JIT round 20 wave 11 (lane pIndy, SPLICE19-1): a bound target of a
        // MUTABLE call site may become a guarded call (below, after the memo).
        let mutable_gate =
            (leaf.is_some() && !constant && jit_indy_mutable_guard_enabled()).then(|| gate.clone());
        let displaced = memo.replace(IndyDirectMemo {
            vm: shared.vm_identity,
            epoch,
            call_site: call_site.as_ptr() as usize,
            target_slot,
            guard,
            leaf,
            gate,
            constant,
            cell: 0,
        });
        // JIT round 20 wave 17 (lane pStr3): keep the displaced memo for a
        // retarget back (see `IndyDirectState::recent`).
        if let (Some(old), Some(recent)) = (displaced, recent.as_mut()) {
            recent.insert(0, old);
            recent.truncate(INDY_DIRECT_RECENT);
        }
        drop(recent);
        drop(memo);
        if constant && !was_constant {
            self.request_constant_recompile(shared);
        }
        if let (Some(gate), Some(leaf)) = (mutable_gate, leaf) {
            let cell =
                self.publish_mutable_guard(shared, call_site, target, target_slot, leaf, gate);
            // JIT round 21 lane reflect: the memo remembers its cell, for a
            // retarget back (the recent-memo swap above). Only the memo this
            // fill wrote: another mutator may have refilled in between.
            if cell == 0 {
                // INDY21 closure: the cell cap was reached; no cell names this
                // target (see `disarm_retarget`).
                self.disarm_retarget();
            } else if let Some(mut memo) = self.direct.memo.try_lock() {
                if let Some(m) = Option::as_mut(&mut *memo) {
                    if m.vm == shared.vm_identity
                        && m.epoch == epoch
                        && m.call_site == call_site.as_ptr() as usize
                        && m.guard[0] == raw
                    {
                        m.cell = cell;
                    }
                }
            }
        } else {
            // INDY21 closure: the new target is refused (no bound leaf) or the
            // site is constant / unguarded: no cell to plan (see
            // `disarm_retarget`).
            self.disarm_retarget();
        }
    }

    /// JIT round 20 wave 11 (lane pIndy, SPLICE19-1;
    /// [`jit_indy_mutable_guard_enabled`]): publish an [`IndyMutableGuardCell`]
    /// for `target`, the current target of this site's NON-constant
    /// `call_site`, just bound to `leaf` by [`Self::fill_direct`] -- unless the
    /// newest cell already names this call site, this target and this leaf,
    /// or the site minted its [`INDY_DIRECT_MAX_FILLS`] cells. The first cell
    /// asks for the method's optimizing recompile through the constant-site
    /// path's one-per-site request (a site's call site never changes kind),
    /// for the reason that path gives: the C2 upgrade is queued before the
    /// first bridge call has bound anything.
    ///
    /// On the mutator that ran `fill_direct`, before the call site is pinned
    /// and invoked: adding a JNI global reference neither allocates on the
    /// Java heap nor reaches a safepoint, so `call_site` and `target` are
    /// still the objects `fill_direct` read.
    ///
    /// Returns the address of the cell that now names this target (the one
    /// just minted, or the newest one already naming it), `0` when none does
    /// (JIT round 21 lane reflect: the memo keeps it for a retarget back).
    pub(in crate::runtime::invokedynamic) fn publish_mutable_guard(
        &self,
        shared: &SharedVm,
        call_site: ObjectRef,
        target: ObjectRef,
        target_slot: usize,
        leaf: &'static cratonvm_jit::JitInvokeInfo,
        gate: crate::classloading::resolution::RedefineGate,
    ) -> usize {
        use std::sync::atomic::Ordering;
        let Ok(field_index) = u32::try_from(target_slot) else {
            return 0;
        };
        // Does the cell at `word` name this call site, this target and this
        // leaf now?
        let names_this = |word: usize| {
            if word == 0 {
                return false;
            }
            // SAFETY: a non-zero word is a cell leaked below, never freed.
            let cell = unsafe { &*(word as *const IndyMutableGuardCell) };
            // SAFETY: both slots are live JNI global-reference boxes of this
            // VM, never released; read on a mutator that is not parked, so no
            // collection rewrites them under the read.
            let (site_now, target_now) = unsafe {
                (
                    *(cell.call_site_slot as *const usize),
                    *(cell.target_slot as *const usize),
                )
            };
            cell.vm == shared.vm_identity
                && site_now == call_site.as_ptr() as usize
                && target_now == target.as_ptr() as usize
                && std::ptr::eq(cell.leaf, leaf)
        };
        let current = self.direct.mutable_cell.load(Ordering::Acquire);
        // JIT round 21 wave 1 (lane indy,
        // `CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`): any cell this site
        // minted for the target, not only the newest, is re-pointed to.
        let retarget = jit_indy_mutable_retarget_recompile_enabled(&self.direct);
        // INDY21-1: the bimorphic guard needs the same per-target cells.
        let reuse = retarget || jit_indy_mutable_bimorphic_enabled(&self.direct);
        if reuse {
            let known = if names_this(current) {
                Some(current)
            } else {
                self.direct
                    .cells
                    .lock()
                    .iter()
                    .copied()
                    .find(|&word| names_this(word))
            };
            if let Some(word) = known {
                if word != current {
                    self.direct.mutable_cell.store(word, Ordering::Release);
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[indy-mutable] {} at cp#{}: guard cell re-pointed to {}.{}{} (no mint)",
                            self.target_descriptor,
                            self.cp_index,
                            leaf.class_name,
                            leaf.method_name,
                            leaf.descriptor,
                        );
                    }
                }
                if retarget {
                    self.note_mutable_retarget(word);
                }
                return word;
            }
        } else if names_this(current) {
            return current;
        }
        if !claim_guard_cell_budget(&self.direct.mutable_cells) {
            return 0;
        }
        // Two separate lock scopes: `add` never re-enters the table.
        let site_handle = shared.natives.jni_global_refs.lock().add(call_site) as usize;
        let target_handle = shared.natives.jni_global_refs.lock().add(target) as usize;
        // LEAK(intentional, bounded): compiled code bakes the cell's slots;
        // at most `INDY_DIRECT_MAX_FILLS` cells per site (see the struct).
        let cell: &'static IndyMutableGuardCell = Box::leak(Box::new(IndyMutableGuardCell {
            kind: JIT_INDY_SITE_GUARDED_LEAF,
            constant_word: 0,
            owner_site: self as *const JitIndyGenericSite as usize,
            vm: shared.vm_identity,
            call_site_slot: site_handle & !1,
            target_slot: target_handle & !1,
            field_index,
            // mcs21 (MCS21-2): the exact class of the call site this cell is
            // minted for (the same read `fill_direct` made the slot with).
            field_row: guard_target_field_row(shared.mem.heap.class_id_of(call_site), field_index),
            leaf,
            gate,
        }));
        let word = cell as *const IndyMutableGuardCell as usize;
        let previous = self.direct.mutable_cell.swap(word, Ordering::AcqRel);
        if reuse {
            self.direct.cells.lock().push(word);
        }
        if retarget {
            self.note_mutable_retarget(word);
        }
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-mutable] {} at cp#{}: guard cell for {}.{}{} kind={} ({})",
                self.target_descriptor,
                self.cp_index,
                leaf.class_name,
                leaf.method_name,
                leaf.descriptor,
                leaf.invoke_kind,
                if previous == 0 { "first" } else { "retargeted" },
            );
        }
        if previous == 0 {
            self.request_constant_recompile(shared);
        }
        cell as *const IndyMutableGuardCell as usize
    }

    /// JIT round 21 wave 1 (lane indy;
    /// [`jit_indy_mutable_retarget_recompile_enabled`]): the site's newest
    /// guard cell is now `cell_word`, the cell of the call site's CURRENT
    /// target. When the owner method's newest compile planned another cell
    /// ([`IndyDirectState::planned_cell`]), its guard misses on every call
    /// from now on: arm the miss count ([`Self::note_retarget_miss`]). When it
    /// planned this one, disarm. Nothing before the first compile planned a
    /// cell (the first cell's own recompile request covers that).
    pub(in crate::runtime::invokedynamic) fn note_mutable_retarget(&self, cell_word: usize) {
        use std::sync::atomic::Ordering::{Acquire, Relaxed};
        let planned = self.direct.planned_cell.load(Acquire);
        if planned == 0 {
            return;
        }
        if planned == cell_word {
            self.direct.retarget_armed.store(false, Relaxed);
            return;
        }
        if self.direct.retarget_recompiles.load(Relaxed) >= INDY_MUTABLE_RETARGET_RECOMPILES {
            return;
        }
        self.direct.retarget_misses.store(0, Relaxed);
        self.direct.retarget_armed.store(true, Relaxed);
    }

    /// INDY21 closure: the site's current target has NO guard cell to plan
    /// (the admission refused the handle, the memo holds no leaf, or the site
    /// minted its [`INDY_DIRECT_MAX_FILLS`] cells), so a recompile of the owner
    /// method would plan the cell of a target the site no longer holds. Without
    /// this, a site armed by an earlier retarget kept counting bridge calls of
    /// the unbindable target and asked that useless recompile at the threshold.
    pub(in crate::runtime::invokedynamic) fn disarm_retarget(&self) {
        self.direct
            .retarget_armed
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// JIT round 21 wave 1 (lane indy): one bridge call of an armed site
    /// ([`jit_indy_linked_direct_leaf`]). The
    /// [`INDY_MUTABLE_RETARGET_MISSES`]-th disarms the site and asks one
    /// recompile of the owner method (at most
    /// [`INDY_MUTABLE_RETARGET_RECOMPILES`] per site). Reads and atomics only
    /// until then; the request itself takes the class-manager read lock and
    /// the tiered manager's lock, as the dispatch helper's late-bind fills do.
    pub(in crate::runtime::invokedynamic) fn note_retarget_miss(&self, shared: &SharedVm) {
        use std::sync::atomic::Ordering::{AcqRel, Relaxed};
        let n = self.direct.retarget_misses.fetch_add(1, Relaxed);
        // INDY21 closure: the threshold doubles per recompile already asked
        // ([`retarget_miss_threshold`]); the first is the plain constant.
        let threshold = retarget_miss_threshold(self.direct.retarget_recompiles.load(Relaxed));
        if n.wrapping_add(1) != threshold {
            return;
        }
        // Exactly one caller per arming gets past the swap.
        if !self.direct.retarget_armed.swap(false, AcqRel) {
            return;
        }
        if self.direct.retarget_recompiles.fetch_add(1, Relaxed) >= INDY_MUTABLE_RETARGET_RECOMPILES
        {
            return;
        }
        let Some((name, descriptor)) = self.owner_method.get() else {
            return;
        };
        let class_name: Option<Arc<str>> = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(self.class_id).map(|c| Arc::<str>::from(&*c.name))
        };
        let Some(class_name) = class_name else {
            return;
        };
        let asked = crate::runtime::interpreter::jit_bridge::indy_mutable_retarget_recompile(
            shared,
            self.class_id.as_u32(),
            Arc::clone(&class_name),
            Arc::clone(name),
            Arc::clone(descriptor),
        );
        // JIT round 22 wave 1, lane runtime (INDY21-5): counted per VM for the
        // exit census, whatever `CRATONVM_DBG_JITC` says.
        crate::runtime::interpreter::jit_bridge::note_indy_retarget_recompile(shared, asked);
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-mutable] {} at cp#{}: retarget recompile {} {class_name}.{name}{descriptor} \
                 ({} calls on a target the compiled guard did not plan)",
                self.target_descriptor,
                self.cp_index,
                if asked { "requested" } else { "REFUSED" },
                INDY_MUTABLE_RETARGET_MISSES,
            );
        }
    }

    /// JIT round 16 wave 10 (lane indyir, IN16-8;
    /// `CRATONVM_JIT_INDY_CONSTANT_INVOKE`): this site's memo just bound a
    /// constant call site's target for the first time, so the optimizing tier
    /// can now compile the instruction as a plain call
    /// ([`jit_indy_constant_direct_target`]). The tiered manager queues the
    /// method's C1 -> C2 upgrade the moment its C1 body publishes -- before
    /// that body has run the bridge once, i.e. before any memo exists -- so
    /// without this the optimizing body never sees the target. One request
    /// per site, ever; the manager's own gates (already queued, `c2_bailout`,
    /// the failure budget, the hotness gate) still apply.
    pub(in crate::runtime::invokedynamic) fn request_constant_recompile(&self, shared: &SharedVm) {
        use std::sync::atomic::Ordering;
        if !jit_indy_constant_invoke_enabled() || !crate::runtime::env_cache::c2_supersede() {
            return;
        }
        let Some((name, descriptor)) = self.owner_method.get() else {
            return;
        };
        if self
            .constant_recompile_requested
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let class_name = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(self.class_id).map(|c| c.name.to_string())
        };
        let Some(class_name) = class_name else {
            return;
        };
        self.reoffer_optimizing_build(shared, &class_name, name, descriptor);
        let answer = shared.jit.tiered_manager.request_indy_constant_recompile(
            &crate::jit::tiered::MethodKey::with_class_id(
                self.class_id,
                class_name.as_str(),
                Arc::clone(name),
                Arc::clone(descriptor),
            ),
        );
        // JIT round 21 lane indyconst: the answer is said. `AlreadyQueued`
        // spends this one-shot request on a compile whose plan may have read
        // the site before the bind; the constant bridge's own asks
        // ([`Self::note_constant_bridge_hit`]) are what recover it.
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-constant] recompile requested {class_name}.{name}{descriptor}: \
                 cp#{} bound a constant call site's target, {answer:?}",
                self.cp_index
            );
        }
    }

    /// JIT round 21 lane reflect
    /// ([`jit_indy_mutable_guard_bridge_recompile_enabled`]): the bridge just
    /// served `leaf`, the direct leaf of the target `target_word` the call
    /// site holds. When the site's newest guard cell plans exactly that call
    /// (same VM, same leaf, its target slot holding that very handle), an
    /// optimizing body planned now would have served the call on its guard
    /// hit -- so no published body carries that guard. Ask for one, at the
    /// [`INDY_MUTABLE_BRIDGE_FIRST_ASK`]-th such call and at every power of two
    /// after it, until [`INDY_MUTABLE_BRIDGE_RECOMPILES`] requests were
    /// queued. A tier that answers "already queued" is asked again at the next
    /// power of two (that compile may have read the site before the cell).
    ///
    /// A single-pass body always bridges, so its calls count too: they ask
    /// for the optimizing compile its upgrade would queue anyway, and the
    /// tier manager's own gates (`c2_bailout`, the budget, the hotness gate)
    /// still decide. Cost per bridged call: one atomic load, and on a guarded
    /// site a slot read, a compare and one relaxed add.
    pub(in crate::runtime::invokedynamic) fn note_guard_bridge_hit(
        &self,
        shared: &SharedVm,
        leaf: &'static cratonvm_jit::JitInvokeInfo,
        target_word: u64,
    ) {
        use std::sync::atomic::Ordering;
        let word = self.direct.mutable_cell.load(Ordering::Acquire);
        if word == 0 {
            self.note_guard_miss_dbg("no guard cell (nothing planned a guarded call yet)");
            return;
        }
        // SAFETY: a non-zero word is a leaked, immutable `IndyMutableGuardCell`.
        let cell = unsafe { &*(word as *const IndyMutableGuardCell) };
        if cell.vm != shared.vm_identity || !std::ptr::eq(cell.leaf, leaf) || cell.target_slot == 0
        {
            self.note_guard_miss_dbg("the newest guard cell plans another call");
            return;
        }
        // SAFETY: a live JNI global-reference box of this VM, never released,
        // read on a mutator that is not parked (as `publish_mutable_guard`).
        let planned = unsafe { *(cell.target_slot as *const usize) };
        if planned as u64 != target_word {
            self.note_guard_miss_dbg(
                "the call site's target is not the planned handle (retargeted; a guard miss)",
            );
            return;
        }
        // JIT round 21 lane mcs: the target IS the planned handle, so a body
        // carrying the guard would have hit -- the calling body has none.
        self.note_guard_miss_dbg(
            "the target is the planned handle: the calling body carries no guard for it \
             (single-pass / C1 body, or an optimizing body built before the cell)",
        );
        if !jit_indy_mutable_guard_bridge_recompile_enabled(&self.direct) {
            return;
        }
        let n = self
            .direct
            .guard_bridge_hits
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if n < INDY_MUTABLE_BRIDGE_FIRST_ASK
            || !n.is_power_of_two()
            || self.direct.guard_bridge_recompiles.load(Ordering::Relaxed)
                >= INDY_MUTABLE_BRIDGE_RECOMPILES
        {
            return;
        }
        if self.request_bridge_recompile(shared, n, false) {
            self.direct
                .guard_bridge_recompiles
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// JIT round 21 lane mcs (`CRATONVM_DBG_JITC` only; page
    /// `jr21-mcs-guarded-indy-call-lost-its-direct-row-20261014.md`): name why
    /// a call of this `MutableCallSite` / `VolatileCallSite` site reached the
    /// bridge although the direct leaf serves it -- the miss edge of a guarded
    /// body, or a body with no guard. One line at the first such call and at
    /// every power of two after it, with the reason of that call:
    /// `[indy-mutable-miss] <descriptor> at cp#N: <n> bridged calls, this one: <why>`.
    /// Off the debug switch: one cached flag read.
    pub(in crate::runtime::invokedynamic) fn note_guard_miss_dbg(&self, why: &str) {
        if !crate::runtime::env_cache::dbg_jitc() {
            return;
        }
        let n = self
            .direct
            .guard_miss_dbg
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        if n.is_power_of_two() {
            eprintln!(
                "[indy-mutable-miss] {} at cp#{}: {n} bridged calls, this one: {why}",
                self.target_descriptor, self.cp_index
            );
        }
    }

    /// JIT round 21 lane indyconst
    /// ([`jit_indy_constant_bridge_recompile_enabled`]): the bridge just
    /// served `leaf` for a `ConstantCallSite` whose memo is current -- the very
    /// memo [`jit_indy_constant_direct_target`] hands the optimizing tier as a
    /// plain call (`leaf` names a static or unbound virtual method with a class
    /// context: [`indy_constant_direct_of`]'s terms). So no published body of
    /// the calling method has that call. Ask for one, as the mutable twin
    /// ([`Self::note_guard_bridge_hit`]) does: at the
    /// [`INDY_MUTABLE_BRIDGE_FIRST_ASK`]-th such call and at every power of two
    /// after it, until [`INDY_MUTABLE_BRIDGE_RECOMPILES`] requests were queued;
    /// an "already queued" answer is asked again at the next power of two.
    ///
    /// Cost per bridged call: one relaxed load past the cap; below it, the
    /// per-site gate, two field compares and one relaxed add.
    pub(in crate::runtime::invokedynamic) fn note_constant_bridge_hit(
        &self,
        shared: &SharedVm,
        leaf: &'static cratonvm_jit::JitInvokeInfo,
    ) {
        use std::sync::atomic::Ordering;
        if self
            .direct
            .constant_bridge_recompiles
            .load(Ordering::Relaxed)
            >= INDY_MUTABLE_BRIDGE_RECOMPILES
            || leaf.declaring_class_id == 0
            || !matches!(leaf.invoke_kind, 0 | 3)
            || !jit_indy_constant_bridge_recompile_enabled(&self.direct)
        {
            return;
        }
        let n = self
            .direct
            .constant_bridge_hits
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if n < INDY_MUTABLE_BRIDGE_FIRST_ASK || !n.is_power_of_two() {
            return;
        }
        if self.request_bridge_recompile(shared, n, true) {
            self.direct
                .constant_bridge_recompiles
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// One more optimizing compile of this site's method, for
    /// [`Self::note_guard_bridge_hit`] (`constant == false`) and, JIT round 21
    /// lane indyconst, [`Self::note_constant_bridge_hit`] (`constant ==
    /// true`); `true` when the tier queued it.
    pub(in crate::runtime::invokedynamic) fn request_bridge_recompile(&self, shared: &SharedVm, served: u32, constant: bool) -> bool {
        if !crate::runtime::env_cache::c2_supersede() {
            return false;
        }
        let Some((name, descriptor)) = self.owner_method.get() else {
            return false;
        };
        let class_name = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(self.class_id).map(|c| c.name.to_string())
        };
        let Some(class_name) = class_name else {
            return false;
        };
        let key = crate::jit::tiered::MethodKey::with_class_id(
            self.class_id,
            class_name.as_str(),
            Arc::clone(name),
            Arc::clone(descriptor),
        );
        self.reoffer_optimizing_build(shared, &class_name, name, descriptor);
        let tier = &shared.jit.tiered_manager;
        let answer = if constant {
            tier.request_indy_constant_recompile(&key)
        } else {
            tier.request_indy_mutable_guard_recompile(&key)
        };
        if crate::runtime::env_cache::dbg_jitc() {
            let (tag, what) = if constant {
                ("indy-constant", "constant")
            } else {
                ("indy-mutable", "guarded")
            };
            eprintln!(
                "[{tag}] {class_name}.{name}{descriptor} cp#{}: {served} bridged calls \
                 of the {what} target, recompile {answer:?}",
                self.cp_index
            );
        }
        answer == crate::jit::tiered::C2RetryAnswer::Enqueued
    }

    /// JIT round 21 lane indylate (a lost request, so unconditional; page
    /// `jr21-indylate-indy-recompile-requests-skipped-by-the-acceptance-memo-20261014.md`):
    /// before this site asks for its method's optimizing recompile, drop the
    /// acceptance gate's input-stamped refusal of that method
    /// (`JitVerdictRegistry::forget_ir_input_refusal`; a sticky site-trap ban
    /// is kept).
    ///
    /// The optimizing body built before this site bound its target had only
    /// the bridge call there, so it carried no evidence and the gate refused
    /// it, stamping the refusal with the inputs generation -- which moves on a
    /// class initialization or the String layout, never on an indy bind. The
    /// recompile the bind asks for was then answered `Enqueued` and SKIPPED
    /// by the compile door's memo (`[ir] acceptance ..: SKIPPED`), so the
    /// method kept its single-pass body's bridge for good: `R21IndyConstLateBind`
    /// const-fresh at the bridge cost under every arm, `R20PIndyMutableSite`
    /// stable rows likewise. The bound target is a new build input, which is
    /// exactly what the stamp exists to re-judge. Bounded by the asks
    /// themselves (one per constant bind or first guard cell, and the bridge's
    /// powers-of-two re-asks).
    pub(in crate::runtime::invokedynamic) fn reoffer_optimizing_build(
        &self,
        shared: &SharedVm,
        class_name: &str,
        name: &str,
        descriptor: &str,
    ) {
        let forgot = crate::runtime::interpreter::jit_bridge::jit_verdicts(shared)
            .forget_ir_input_refusal(class_name, name, descriptor, self.class_id);
        if forgot && crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-late] {class_name}.{name}{descriptor} cp#{}: the acceptance gate's \
                 earlier refusal forgotten, the optimizing build is offered again",
                self.cp_index
            );
        }
    }
}

/// `CRATONVM_JIT_INDY_CONSTANT_INVOKE` (JIT round 16 wave 10, lane indyir,
/// IN16-8; **default ON**, `=0` off; it was opt-in until measured): may the
/// optimizing tier compile a
/// constant call site's bound direct target as a plain `invokestatic` /
/// `invokevirtual` of that method (so its ordinary call machinery, direct
/// binds and IR inlining included, serves it)? Read at compile time only, by
/// [`jit_indy_constant_direct_target`] (no per-call read, no static).
pub fn jit_indy_constant_invoke_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_INDY_SITE_BY_PC` (JIT round 16 wave 10, lane indyir;
/// **default ON**, `=0` off; it was opt-in until measured): may a compile door that knows WHICH
/// `invokedynamic` instruction it compiles name it to the bridge-site intern
/// table even when other instructions of the method share its constant-pool
/// entry? The interpreter links such instructions separately (its
/// per-instruction key), so naming the instruction gives the compiled site the
/// same linkage; leaving it unnamed refuses a lambda or linked bridge and the
/// site traps in both tiers. Read per compile (no static).
pub fn jit_indy_site_by_pc_enabled() -> bool {
    true
}

/// JIT round 16 wave 10 (lane indyir, IN16-8): the method a compiled
/// `invokedynamic` may be compiled AS, by the optimizing tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndyConstantDirect {
    /// The target's declaring class (internal name).
    pub class: String,
    pub name: String,
    pub desc: String,
    /// `3` a static target, `0` an unbound virtual one (receiver first).
    pub invoke_kind: u8,
    /// The target class's id: the class context of the compiled call's
    /// `JitInvokeInfo` (`declaring_class_id`), so every by-name resolution
    /// on that path -- the dispatch, a deopting target's callee resume --
    /// asks the target class's own loader, never the bootstrap one.
    pub class_id: u32,
}

/// JIT round 16 wave 10 (lane indyir, IN16-8; `CRATONVM_JIT_INDY_CONSTANT_INVOKE`):
/// the call a bridge site may be compiled as, or `None` to keep the bridge.
///
/// `Some` only for a `linked_direct` site whose memo ([`IndyDirectMemo`]) holds
/// a bound leaf for a `ConstantCallSite`, on this VM, with the caller not
/// redefined since. Such a site never changes: the row that published the
/// `CallSite` is first-writer-wins and lives until the caller is redefined or
/// unloaded (both retire the compiled code), a `ConstantCallSite`'s target is
/// fixed at construction, and the admission (`mh_compiled_bind_target`) was
/// taken of that very handle. So no guard is compiled: the leaf IS the
/// call's target for the rest of the caller's life. A `MutableCallSite` /
/// `VolatileCallSite` is refused -- its target is re-read per call by the
/// bridge (and `jit_indy_linked_direct_leaf`), which is how a retarget is
/// seen; compiling it as a plain call would need a dependency on
/// `setTarget` that does not exist yet.
///
/// Pure reads of the process-lived site and its memo: no heap read, so it is
/// safe on a background compile thread. A contended memo is a refusal.
pub fn jit_indy_constant_direct_target(
    shared: &SharedVm,
    site_ptr: usize,
) -> Option<IndyConstantDirect> {
    if !jit_indy_constant_invoke_enabled() {
        return None;
    }
    // SAFETY: `site_ptr` is 0 or a site pointer the intern table handed out;
    // every such allocation is process-lived.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return None;
    }
    // SAFETY: as above, and the kind word says it is a generic site.
    let site = unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() }?;
    if !site.linked_direct {
        return None;
    }
    // indy21c (INDYLATE21-3): a bounded retry, not the single `try_lock`.
    let Some(memo) = site.direct.memo_for_compile() else {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[indy-constant] {} at cp#{}: the memo was busy for a compile-time read \
                 ({} such reads); read as no plan",
                site.target_descriptor,
                site.cp_index,
                site.direct
                    .compile_memo_contended
                    .load(std::sync::atomic::Ordering::Relaxed)
            );
        }
        return None;
    };
    indy_constant_direct_of(Option::as_ref(&*memo)?, shared.vm_identity)
}

/// `CRATONVM_JIT_INDY_MUTABLE_GUARD` (JIT round 20 wave 11, lane pIndy,
/// SPLICE19-1 of `jit-r19-splice19-proposals.md`; **default ON**, `=0`
/// restores the bridge): may a `MutableCallSite` / `VolatileCallSite`
/// instruction whose current target the bridge bound to a direct method
/// ([`JitIndyGenericSite::fill_direct`]) be compiled by the optimizing tier
/// as a plain call of that method behind a guard on the call site's target,
/// with the bridge as the miss edge? Read when the bridge binds a target and
/// per compile (no per-call read, no static).
pub fn jit_indy_mutable_guard_enabled() -> bool {
    true
}

/// JIT round 20 wave 11 (lane pIndy, SPLICE19-1): the guarded call a bridge
/// site of a NON-constant call site may be compiled as -- the call, and the
/// guard's `(call-site slot, target slot, CallSite.target field slot)` --
/// from the site's newest [`IndyMutableGuardCell`], or `None` to keep the
/// bridge. `Some` only for a `linked_direct` site with a cell minted on this
/// VM, a caller not redefined since, and a leaf naming a static or unbound
/// virtual method with a class context (the [`indy_constant_direct_of`]
/// terms).
///
/// The compiled guard does the rest: whatever the site's target is when the
/// compiled code runs, only the cell's handle reaches the planned call, so
/// unlike [`jit_indy_constant_direct_target`] nothing here needs the target
/// to be fixed. Pure reads of process-lived memory (the site, a leaked cell):
/// safe on a background compile thread.
pub fn jit_indy_mutable_guard_target(
    shared: &SharedVm,
    site_ptr: usize,
) -> Option<(IndyConstantDirect, usize, usize, u32, usize, u32)> {
    if !jit_indy_mutable_guard_enabled() {
        return None;
    }
    // SAFETY: `site_ptr` is 0 or a site pointer the intern table handed out;
    // every such allocation is process-lived.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return None;
    }
    // SAFETY: as above, and the kind word says it is a generic site.
    let site = unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() }?;
    if !site.linked_direct {
        return None;
    }
    let word = site
        .direct
        .mutable_cell
        .load(std::sync::atomic::Ordering::Acquire);
    if word == 0 {
        return None;
    }
    // SAFETY: a non-zero word is a leaked, immutable `IndyMutableGuardCell`.
    let cell = unsafe { &*(word as *const IndyMutableGuardCell) };
    let leaf = cell.leaf;
    if cell.vm != shared.vm_identity
        || cell.gate.is_stale()
        || cell.call_site_slot == 0
        || cell.target_slot == 0
        || leaf.declaring_class_id == 0
        || !matches!(leaf.invoke_kind, 0 | 3)
    {
        return None;
    }
    // The cell word is the fifth element: the caller records it as planned
    // ([`jit_indy_mutable_note_planned`]) only once the caller-side admission
    // accepted the target (INDY21 closure: this door used to record it before
    // that test, so a refused admission still moved the planned cell).
    Some((
        IndyConstantDirect {
            class: leaf.class_name.to_owned(),
            name: leaf.method_name.to_owned(),
            desc: leaf.descriptor.to_owned(),
            invoke_kind: leaf.invoke_kind,
            class_id: leaf.declaring_class_id,
        },
        cell.call_site_slot,
        cell.target_slot,
        cell.field_index,
        word,
        // mcs21 (MCS21-2): the sixth element, `CallSite.target`'s compact row.
        cell.field_row,
    ))
}

/// JIT round 21 wave 1 (lane indy, `CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`)
/// / INDY21 closure: the guard cell `cell_word` (the fifth element of
/// [`jit_indy_mutable_guard_target`]'s answer) is the one the compile now
/// planning at `site_ptr` builds its guarded call against, so a retarget away
/// from it arms the site ([`JitIndyGenericSite::note_mutable_retarget`]). Called
/// by the compile door only for a plan the caller-side admission accepted.
/// Written whatever the switch: only the switch's own paths read it. A no-op
/// for a null or non-generic site pointer. Atomics only.
pub fn jit_indy_mutable_note_planned(site_ptr: usize, cell_word: usize) {
    // SAFETY: `site_ptr` is 0 or a site pointer the intern table handed out;
    // every such allocation is process-lived.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return;
    }
    // SAFETY: as above, and the kind word says it is a generic site.
    if let Some(site) = unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() } {
        site.direct
            .planned_cell
            .store(cell_word, std::sync::atomic::Ordering::Release);
    }
}

/// JIT round 21 wave 1 (lane indy, INDY21-1; [`jit_indy_mutable_bimorphic_enabled`]):
/// the SECOND target a bimorphic guard of the site at `site_ptr` may test --
/// `(target root slot, cell word)` of the newest other cell this site minted,
/// whose cell word is also the bridge site of the second arm
/// ([`JIT_INDY_SITE_GUARDED_LEAF`]) -- or `(0, 0)`. Asked by the compile door
/// right after [`jit_indy_mutable_note_planned`] recorded the planned cell;
/// `(0, 0)` without a planned cell, with the switch off, for a
/// contended cell list, or when no other cell of this VM names a static or
/// unbound virtual leaf of the planned leaf's kind (so one receiver rule
/// serves both arms) for a caller not redefined since. Every cell of one site
/// names the same `CallSite` (the instruction's row is first-writer-wins), so
/// the guard compares both handles with the one target it read. Pure reads of
/// process-lived memory: safe on a background compile thread.
pub fn jit_indy_mutable_second_target(shared: &SharedVm, site_ptr: usize) -> (usize, usize) {
    use std::sync::atomic::Ordering::Acquire;
    // SAFETY: `site_ptr` is 0 or a site pointer the intern table handed out;
    // every such allocation is process-lived.
    if unsafe { jit_indy_site_kind(site_ptr) } != JIT_INDY_SITE_GENERIC {
        return (0, 0);
    }
    // SAFETY: as above, and the kind word says it is a generic site.
    let Some(site) = (unsafe { (site_ptr as *const JitIndyGenericSite).as_ref() }) else {
        return (0, 0);
    };
    if !site.linked_direct
        || !jit_indy_mutable_guard_enabled()
        || !jit_indy_mutable_bimorphic_enabled(&site.direct)
    {
        return (0, 0);
    }
    let planned = site.direct.planned_cell.load(Acquire);
    if planned == 0 {
        return (0, 0);
    }
    // SAFETY: a non-zero planned word is a leaked, immutable cell.
    let planned_kind = unsafe { &*(planned as *const IndyMutableGuardCell) }
        .leaf
        .invoke_kind;
    let Some(cells) = site.direct.cells.try_lock() else {
        return (0, 0);
    };
    for &word in cells.iter().rev() {
        if word == 0 || word == planned {
            continue;
        }
        // SAFETY: every word in the list is a leaked, immutable cell.
        let cell = unsafe { &*(word as *const IndyMutableGuardCell) };
        let leaf = cell.leaf;
        if cell.vm == shared.vm_identity
            && !cell.gate.is_stale()
            && cell.call_site_slot != 0
            && cell.target_slot != 0
            && leaf.invoke_kind == planned_kind
            && matches!(leaf.invoke_kind, 0 | 3)
        {
            return (cell.target_slot, word);
        }
    }
    (0, 0)
}

/// [`jit_indy_constant_direct_target`]'s verdict on one memo: a bound leaf of
/// a `ConstantCallSite`, filled on VM `vm_identity`, for a caller that was not
/// redefined since, naming a static or unbound virtual method with a class
/// context.
pub(in crate::runtime::invokedynamic) fn indy_constant_direct_of(m: &IndyDirectMemo, vm_identity: usize) -> Option<IndyConstantDirect> {
    let leaf = m.leaf?;
    if m.vm != vm_identity
        || !m.constant
        || m.gate.is_stale()
        || leaf.declaring_class_id == 0
        || !matches!(leaf.invoke_kind, 0 | 3)
    {
        return None;
    }
    Some(IndyConstantDirect {
        class: leaf.class_name.to_owned(),
        name: leaf.method_name.to_owned(),
        desc: leaf.descriptor.to_owned(),
        invoke_kind: leaf.invoke_kind,
        class_id: leaf.declaring_class_id,
    })
}

/// Does `info` name the call `bind` admits?
pub(in crate::runtime::invokedynamic) fn indy_direct_leaf_names(
    info: &cratonvm_jit::JitInvokeInfo,
    bind: &cratonvm_native_builtins::lang_invoke::MhCompiledBind,
) -> bool {
    info.declaring_class_id == bind.class_id
        && info.class_name == bind.class
        && info.method_name == bind.name
        && info.descriptor == bind.desc
        && info.invoke_kind == bind.invoke_kind
}

/// The dispatch info for a bound target: `previous` when it names the same
/// call (so the dispatch memos keyed by an info's address stay warm across a
/// re-fill after a collection), else a new one.
///
/// LEAK(intentional, bounded): compiled dispatch caches key on an info's
/// address, so an info handed to compiled dispatch is never freed. A re-fill
/// of the same target reuses `previous`; a site mints at most
/// [`INDY_DIRECT_MAX_FILLS`] (`IndyDirectState::minted`).
pub(in crate::runtime::invokedynamic) fn indy_direct_leaf_info(
    previous: Option<&'static cratonvm_jit::JitInvokeInfo>,
    bind: &cratonvm_native_builtins::lang_invoke::MhCompiledBind,
) -> &'static cratonvm_jit::JitInvokeInfo {
    if let Some(p) = previous.filter(|p| indy_direct_leaf_names(p, bind)) {
        return p;
    }
    let class: &'static str = Box::leak(Box::from(bind.class.as_str()));
    let name: &'static str = Box::leak(Box::from(bind.name.as_str()));
    let desc: &'static str = Box::leak(Box::from(bind.desc.as_str()));
    Box::leak(Box::new(cratonvm_jit::JitInvokeInfo {
        class_name: class,
        method_name: name,
        descriptor: desc,
        // One word per value plus a virtual leaf's receiver: for a handle of
        // exactly the site's type, the site's own argument count.
        num_jit_args: cratonvm_jit_api::count_param_slots(desc)
            + usize::from(bind.invoke_kind != 3),
        return_type: cratonvm_jit::return_type(desc),
        invoke_kind: bind.invoke_kind,
        // The target's own class as the call's class context, as the MH lane's
        // `mh_bind_leaf_info` (a deopting target's callee resume asks this
        // class's loader for it).
        declaring_class_id: bind.class_id,
        owner_class_id: 0,
    }))
}

/// JIT round 16 wave 7 (lane indy; `CRATONVM_JIT_INDY_LINKED_CALL_SITE`,
/// default OFF): a bridge site for an `invokedynamic` whose bootstrap
/// [`make_jit_indy_bridge_site_from_parts`] does not admit -- a JDK-linked
/// lambda or concat shape, an `ObjectMethods` site with method getters,
/// Groovy's `IndyInterface`, any user bootstrap -- built ONLY when the
/// instruction already has a settled per-instruction linkage in
/// `GENERIC_INDY_SITES` (`vm_identity`'s row for `(class_id, cp_index,
/// singleton_site)`).
///
/// # Why "already linked" and nothing weaker
///
/// The bridge must never BOOTSTRAP such a site: a bootstrap runs arbitrary
/// Java with the caller's `Lookup`, and the bridge's synthetic frame is not the
/// caller's frame (the reason the generic bootstraps were refused when the
/// bridge was introduced). A site built only after the interpreter linked the
/// instruction is never bootstrapped by compiled code: its row is published
/// first-writer-wins and stays until the caller is redefined or unloaded, both
/// of which retire the compiled code that baked the site. Until the row
/// exists the answer is `None`, i.e. exactly today's trap; the intern table
/// asks again at the next compile (`interned_jit_indy_bridge_site`).
///
/// # What the bridge then runs
///
/// A `Linked` row: the `CallSite`'s CURRENT target, read per call (a
/// `MutableCallSite` retarget is seen at the next call, as in the
/// interpreter), invoked frame-free (`invoke_generic_call_site_with`). A
/// `Failed`, `Lambda` or Groovy-cast row: the frame path, whose
/// `execute_invokedynamic_at` finds the row under the site's own instruction
/// key and runs it without bootstrapping.
///
/// `void` sites keep the trap (the bridge returns one value), as everywhere
/// else; a missing instruction key (`singleton_site` `None`: an entry shared
/// within the method) refuses.
pub fn make_jit_indy_linked_site_from_parts(
    vm_identity: usize,
    pool: &ConstantPool,
    cp_index: u16,
    class_id: ClassId,
    singleton_site: Option<(u64, usize)>,
) -> Option<usize> {
    if !jit_indy_linked_call_site_enabled() || !crate::runtime::env_cache::jit_indy_bridge() {
        return None;
    }
    let (method_key, pc) = singleton_site?;
    let nat_index = match pool.get(cp_index)? {
        ConstantPoolEntry::InvokeDynamic {
            name_and_type_index,
            ..        } => *name_and_type_index,
        _ => return None,
    };
    let (_, descriptor) = pool.get_name_and_type(nat_index)?;
    let ret = descriptor.rsplit(')').next()?.as_bytes().first().copied()?;
    if ret == b'V' {
        return None;
    }
    if !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    let (_, shape) = probe_generic_indy_site(&(vm_identity, class_id, cp_index, method_key, pc))?;
    // The row was published for THIS pool's entry (a redefined caller's row is
    // stale and the probe misses), so the shapes agree; a disagreement is a
    // row this site must not drive.
    if &*shape.target_descriptor != descriptor {
        return None;
    }
    let arg_types: Vec<u8> = parse_descriptor_args(descriptor)
        .into_iter()
        .map(|c| c as u8)
        .collect();
    Some(new_jit_indy_generic_site(
        class_id,
        cp_index,
        descriptor,
        arg_types,
        ret,
        (method_key, pc),
        false,
        false,
        true,
    ))
}

/// JIT round 21 lane indylate (`CRATONVM_JIT_INDY_LATE_LINK_TRAP_RECOMPILE`):
/// may the linked bridge ([`make_jit_indy_linked_site_from_parts`]) ever serve
/// the `invokedynamic` at `cp_index` of the method `(name, descriptor, bci)`,
/// and is it linked yet? `None` when it never may (not an `InvokeDynamic`
/// entry, a `void` site); otherwise whether the instruction has its live
/// per-instruction generic linkage on VM `vm_identity` -- the row whose
/// existence makes the linked bridge admit the site at the next compile.
/// One read lock on the row table.
pub fn jit_indy_instruction_linked(
    vm_identity: usize,
    pool: &ConstantPool,
    class_id: ClassId,
    cp_index: u16,
    instruction: (&str, &str, usize),
) -> Option<bool> {
    let nat_index = match pool.get(cp_index)? {
        ConstantPoolEntry::InvokeDynamic {
            name_and_type_index,
            ..        } => *name_and_type_index,
        _ => return None,
    };
    let (_, descriptor) = pool.get_name_and_type(nat_index)?;
    let ret = descriptor.rsplit(')').next()?.as_bytes().first().copied()?;
    if ret == b'V' {
        return None;
    }
    let (name, method_descriptor, bci) = instruction;
    let (method_key, pc) = lambda_singleton_site_for(name, method_descriptor, bci);
    Some(probe_generic_indy_site(&(vm_identity, class_id, cp_index, method_key, pc)).is_some())
}

/// Execute the narrow compiled-code concat bridge. Raw slots are descriptor
/// typed before they enter the normal interpreter concat implementation, so a
/// category-2 value never gets reclassified from its bits alone.
///
/// `Ok(None)` is the historical "no answer" (a null site, a descriptor/arity
/// disagreement, a result that is not an object) and the caller turns it into
/// a null reference, as it always has. `Err` is a Java throwable raised while
/// rendering an operand -- a `toString()` that threw, or the `OutOfMemoryError`
/// for the result -- and must reach the compiled caller's exception check
/// instead of being flattened into that null.
pub unsafe fn execute_jit_string_concat_raw(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site_ptr: usize,
    args_ptr: *const i64,
    arg_count: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let Some(site) = (site_ptr as *const JitStringConcatSite).as_ref() else {
        return Ok(None);
    };
    let arg_types = &site.arg_types;
    if arg_types.len() != arg_count || (arg_count != 0 && args_ptr.is_null()) {
        return Ok(None);
    }
    // NOT `from_raw_parts(args_ptr, 0)` when there are no arguments:
    // `from_raw_parts` requires a non-null, aligned pointer even for a length
    // of zero, and the guard directly above deliberately admits a NULL
    // `args_ptr` in exactly that case (a zero-arg call sequence pushes
    // nothing, so the compiled code has no argument block to point at). A
    // debug build's `unsafe precondition` check aborts the process on it. The
    // twin in `execute_jit_indy_generic_raw` below is where that was observed;
    // this one is the same shape and is corrected with it rather than left as
    // the copy that still aborts. (A zero-argument concat is `"" + ""` folded
    // to a site with no operands — rare, not impossible.)
    let raw_args = if arg_count == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(args_ptr, arg_count)
    };
    if site.latin1_fast
        && shared.classes.latin1_coder_allowed()
        && site.constant_args.is_empty()
        && site.recipe.as_ref() == [TAG_ARG_UNIT, TAG_ARG_UNIT]
        && &arg_types[..] == ['L', 'L']
        && raw_args.len() == 2
        && raw_args[0] != 0
        && raw_args[1] != 0
    {
        let str_class_id = crate::vm::java_string_class_id(shared);
        let a = ObjectRef::from_raw(raw_args[0] as usize as *mut u8);
        let b = ObjectRef::from_raw(raw_args[1] as usize as *mut u8);
        if let (Some((a_ptr, a_len)), Some((b_ptr, b_len))) = (
            shared
                .mem
                .heap
                .try_read_compact_string_latin1_raw(str_class_id, a),
            shared
                .mem
                .heap
                .try_read_compact_string_latin1_raw(str_class_id, b),
        ) {
            if a_len == 0 {
                return Ok(Some(shared.mem.heap.load_and_forward(b)));
            }
            if b_len == 0 {
                return Ok(Some(shared.mem.heap.load_and_forward(a)));
            }
            if let Some(total) = a_len.checked_add(b_len) {
                if total <= CONCAT_MAX_LATIN1_UNITS {
                    let a_slice = std::slice::from_raw_parts(a_ptr, a_len);
                    let b_slice = std::slice::from_raw_parts(b_ptr, b_len);
                    if let Some(obj) = crate::vm::try_create_java_string_latin1_two_parts(
                        shared, thread, a_slice, b_slice,
                    ) {
                        return Ok(Some(obj));
                    }
                }
            }
        }
    }
    // Inline storage (wave 20): a heap `Vec` here was one allocation per
    // compiled string concatenation.
    let mut values: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::with_capacity(arg_count);
    for (&raw, ty) in raw_args.iter().zip(arg_types.iter()) {
        let value = match ty {
            'J' => Value::Long(raw),
            'D' => Value::Double(f64::from_bits(raw as u64)),
            'F' => Value::Float(f32::from_bits(raw as u32)),
            'L' | '[' => {
                if raw == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(raw as usize as *mut u8)))
                }
            }
            _ => Value::Int(raw as i32),
        };
        values.push(value);
    }
    // STRGAP19-1 (`CRATONVM_JIT_CONCAT_LATIN1_FAST`, default ON, `=0` off): an
    // all-Latin-1 concat whose operands need no `toString()` is one exact-size
    // TLAB String; anything else declines before running any Java code and
    // takes the general renderer below, unchanged.
    if site.latin1_fast {
        if let Some(obj) = concat_latin1::try_concat(
            shared,
            thread,
            &site.recipe,
            site.constant_args.as_slice(),
            &site.arg_types,
            &values,
        ) {
            return Ok(Some(obj));
        }
    }
    // CONCAT21-2 (`CRATONVM_JIT_CONCAT_UTF16_FAST`, default ON): the UTF16 (or
    // Latin-1 with a UTF16 operand) result of operands that need no
    // `toString()`, from the TLAB; a decline is the general renderer below.
    if site.utf16_fast {
        if let Some(obj) = concat_latin1::try_concat_utf16(
            shared,
            thread,
            &site.recipe,
            site.constant_args.as_slice(),
            &site.arg_types,
            &values,
        ) {
            return Ok(Some(obj));
        }
    }
    if !JIT_CONCAT_BRIDGE_FRAME_FREE {
        return execute_jit_string_concat_on_frame(shared, thread, site, values);
    }
    // Frame-free (round i1 wave 20, lane L4;
    // `i16-L1-proposal-frame-free-compiled-concat-bridge`): the operands go
    // straight to the frame-free half of the interpreter's concat, which is
    // what the interpreter itself calls after popping them. GC safety is the
    // interpreter's: nothing between the decode above and
    // `concat_values_to_string`'s first statement allocates or polls, and that
    // function pins every reference operand before its first safepoint (an
    // operand's `toString()`). A throwing `toString()` therefore builds its
    // stack trace over the same frames the interpreted concat of the same
    // instruction would, with no `<jit-indy>.concat` frame on top.
    match concat_values_to_string(
        shared,
        thread,
        &site.recipe,
        site.constant_args.as_slice(),
        &site.arg_types,
        &values,
        site.tlab_result,
    ) {
        Ok(obj) => Ok(Some(obj)),
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { ref message },
        ))) if site.deopt_on_overflow && message == CONCAT_LENGTH_OVERFLOW_MESSAGE => Ok(None),
        Err(e) => Err(e),
    }
}

/// Kill switch of the frame-free compiled concat bridge: `false` restores the
/// synthetic `<jit-indy>.concat` frame ([`execute_jit_string_concat_on_frame`])
/// so the two arms can be priced against each other. A `const`, not a flag.
pub(in crate::runtime::invokedynamic) const JIT_CONCAT_BRIDGE_FRAME_FREE: bool = true;

/// The pre-wave-20 arm of [`execute_jit_string_concat_raw`]: push a synthetic
/// frame, push the operands onto it, and run the interpreter's frame-based
/// concat. Kept behind [`JIT_CONCAT_BRIDGE_FRAME_FREE`] and for the unit test
/// that pins the two arms to one answer.
pub(in crate::runtime::invokedynamic) fn execute_jit_string_concat_on_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &JitStringConcatSite,
    values: smallvec::SmallVec<[Value; 8]>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let arg_count = values.len();
    let frame_idx = thread.frames.len();
    // `new_from_arcs` over the site's pre-built parts; see the fields' doc.
    let caller_class_id = thread
        .frames
        .last()
        .expect("concat bridge has a caller frame")
        .class_id;
    thread.frames.push(Frame::new_from_arcs(
        caller_class_id,
        Arc::clone(&site.frame_class_name),
        Arc::clone(&site.frame_method_name),
        Arc::clone(&site.frame_method_descriptor),
        None,
        Arc::clone(&site.frame_code),
        Arc::clone(&site.frame_exception_table),
        u16::try_from(arg_count)
            .unwrap_or(u16::MAX)
            .saturating_add(1),
        0,
        &[],
    ));
    thread.frames.last_mut().expect("bridge frame was just pushed").mark_vm_synthetic();
    for value in values {
        if thread.frames[frame_idx].stack.push(value).is_err() {
            thread.frames.pop();
            return Ok(None);
        }
    }
    let executed = execute_string_concat(
        shared,
        thread,
        frame_idx,
        &site.recipe,
        site.constant_args.as_slice(),
        &site.target_descriptor,
        // The compiled site's own record decides its fast path; this frame
        // arm asks the per-call scan as before.
        None,
    );
    let result = match executed {
        Ok(()) => match thread.frames[frame_idx].stack.pop() {
            Ok(Value::Object(Some(obj))) => Ok(Some(obj)),
            _ => Ok(None),
        },
        Err(error) => Err(error),
    };
    // The synthetic frame comes off on every path, the error one included.
    thread.frames.pop();
    result
}

/// `CRATONVM_JIT_INDY_LAMBDA_FAST` — default-ON, `=0` opts out. See the fast
/// path in [`execute_jit_indy_generic_raw`].
pub(in crate::runtime::invokedynamic) fn jit_indy_lambda_fast_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static GATE: AtomicU8 = AtomicU8::new(0);
    match GATE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
                Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
                Err(_) => true,
            };
            GATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Execute a compiled `LambdaMetafactory` site — the generic half of the
/// bridge.
///
/// # How little it reimplements
///
/// Nothing. It builds the same synthetic frame the concat bridge builds, pushes
/// the descriptor-typed arguments onto it, and calls [`execute_invokedynamic`]
/// — the interpreter's own implementation, including its resolved-call-site
/// cache. The frame carries the SITE's `class_id`, which is what makes the
/// compiled site and the interpreted one share a cache entry rather than
/// bootstrap twice.
///
/// # The return value
///
/// `Ok((bits, Some(obj)))` for a reference result — the second half is the
/// object the caller must root through `native_pending_return` before it hands
/// the pointer to compiled code. `Ok((bits, None))` for a primitive (or a null
/// reference), where `bits` is encoded exactly as `jit_invoke_dispatch` encodes
/// an ordinary invoke's return: sign-extended for the integral kinds, raw
/// `to_bits()` for `F`/`D`.
///
/// # Errors
///
/// `Err` on anything the bootstrap or the call site raises. The caller
/// (`jit::helpers::jit_indy_bridge`) routes it through the same
/// stash-and-return-sentinel path a compiled dispatch failure takes, so an
/// exception thrown out of a bridged site reaches the compiled caller's
/// post-call check rather than being swallowed into a `0`.
///
/// SAFETY: `site_ptr` is a pointer from [`make_jit_indy_bridge_site_from_parts`]
/// whose `kind` is [`JIT_INDY_SITE_GENERIC`]; `args_ptr` is the JIT caller's own
/// spill buffer, live and unmoved for the duration of this call.
pub unsafe fn execute_jit_indy_generic_raw(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site_ptr: usize,
    args_ptr: *const i64,
    arg_count: usize,
) -> Result<(i64, Option<ObjectRef>), MethodCallFailed> {
    let Some(site) = (site_ptr as *const JitIndyGenericSite).as_ref() else {
        return Err(VmError::Internal {
            message: "jit indy bridge: null site".to_owned(),
        }
        .into());
    };
    let arg_types = &site.arg_types;
    if arg_types.len() != arg_count || (arg_count != 0 && args_ptr.is_null()) {
        // A disagreement between the descriptor and what the call sequence
        // pushed is a miscompile, not a value to guess at.
        return Err(VmError::Internal {
            message: format!(
                "jit indy bridge: {} args for descriptor {}",
                arg_count, site.target_descriptor
            ),
        }
        .into());
    }
    // NOT `from_raw_parts(args_ptr, 0)` when there are no arguments:
    // `from_raw_parts` requires a non-null, aligned pointer even for a length
    // of zero, and the guard directly above deliberately admits a NULL
    // `args_ptr` in exactly that case (a zero-arg call sequence pushes
    // nothing, so the compiled code has no argument block to point at). A
    // debug build's `unsafe precondition` check aborts the process on it —
    // `cratonvm-vm --test class_loader_unload_regression
    // custom_loader_metadata_is_reclaimed_with_jit`, whose lambda call sites
    // take exactly this door.
    let raw_args = if arg_count == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(args_ptr, arg_count)
    };
    // Inline storage (wave 16): one heap allocation per bridged lambda
    // evaluation with captures, before.
    let mut values: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::with_capacity(arg_count);
    for (&raw, ty) in raw_args.iter().zip(arg_types.iter()) {
        // Descriptor-typed, never bits-typed: a category-2 value must not be
        // reclassified from its payload (the same rule the concat bridge
        // states).
        values.push(match *ty {
            b'J' => Value::Long(raw),
            b'D' => Value::Double(f64::from_bits(raw as u64)),
            b'F' => Value::Float(f32::from_bits(raw as u32)),
            b'L' | b'[' => {
                if raw == 0 {
                    Value::Object(None)
                } else {
                    Value::Object(Some(ObjectRef::from_raw(raw as usize as *mut u8)))
                }
            }
            _ => Value::Int(raw as i32),
        });
    }
    // Frame-free fast path: an already-bootstrapped `LambdaMetafactory` site
    // is nothing but an allocation, and the frame below exists only to carry
    // the captures to it on an operand stack. Skipping it removes the largest
    // single population in the `CRATONVM_DBG_INTERP_FRAMES` census of
    // `HibfixComposeProbe2` — 63.5 % of every interpreted frame push on that
    // workload was this bridge.
    //
    // Deliberately narrow. It engages only when the site is ALREADY in the
    // resolution cache (so bootstrapping still runs its normal course through
    // `execute_invokedynamic`), only for `ResolvedCallSite::Lambda`, and only
    // when the site's capture count agrees with what the call sequence pushed.
    // Anything else falls through to the frame path unchanged.
    //
    // The zero-capture singleton key is `site.singleton_site`: the real
    // instruction's key, which the frame path below hands to
    // `execute_invokedynamic_at` as well, so the first, bootstrapping
    // execution -- which always takes the frame path -- every later fast-path
    // one, and the interpreter running the same instruction all agree on the
    // instance.
    // `CRATONVM_JIT_INDY_LAMBDA_FAST=0` restores the frame path so the two arms
    // can be priced in one binary.
    if jit_indy_lambda_fast_enabled() && site.return_type == b'L' {
        // nio21 wave 14, lane L1 (W11L3-2): a capturing site's learnt memo
        // answers first -- no site-cache probe, no resolution-cache lock.
        let memo = if site.capture_fast {
            decode_capture_memo(site.capture_memo.load(std::sync::atomic::Ordering::Acquire))
        } else {
            None
        };
        // Only the two `Copy` facts the allocation needs are taken out of the
        // guard; cloning the whole `LambdaCallSite` cost seven refcount
        // round-trips and a `Vec` per bridged lambda evaluation. This thread's
        // `IndySiteCache` (shared with the interpreter's fast path, same
        // (class, cp index) key) answers first, without the lock.
        let local = match memo {
            Some(facts) => Some(facts),
            None => match thread.indy_sites.get(site.class_id, site.cp_index) {
                Some(CachedIndySite::Lambda {
                    proxy_class_id,
                    num_captures,
                    ..
                }) => Some((*proxy_class_id, *num_captures)),
                _ => None,
            },
        };
        let cached = match local {
            Some(facts) => Some(facts),
            None => {
                let epochs_at_entry = IndySiteCache::epochs_for(shared);
                let facts = {
                    let cache = shared.classes.resolution_cache.read();
                    match cache.get_call_site(site.class_id, site.cp_index) {
                        Some(ResolvedCallSite::Lambda(lcs)) => {
                            Some((lcs.proxy_class_id, lcs.capture_types.len()))
                        }
                        _ => None,
                    }
                };
                if let Some((proxy_class_id, num_captures)) = facts {
                    // No singleton-key memo: the interpreter's hit derives it
                    // from its own frame, never from a compiled site's key.
                    thread.indy_sites.put(
                        site.class_id,
                        site.cp_index,
                        epochs_at_entry,
                        CachedIndySite::Lambda {
                            proxy_class_id,
                            num_captures,
                            singleton_key: None,
                            singleton: None,
                        },
                    );
                }
                // An entry several instructions share is linked per
                // instruction and has no per-entry row; the site's own
                // instruction key finds it (see `lambda_entry_is_shared`).
                facts.or_else(|| {
                    let (method_key, pc) = site.singleton_site;
                    probe_lambda_instruction_site(&(
                        shared.vm_identity,
                        site.class_id,
                        site.cp_index,
                        method_key,
                        pc,
                    ))
                })
            }
        };
        if let Some((proxy_class_id, num_captures)) = cached {
            if num_captures == arg_count {
                // Zero-capture: the site's own slot memo answers without the
                // singleton table's lock (wave 20, see `LAMBDA_SINGLETON_CACHE`).
                if num_captures == 0 {
                    if let Some(obj) = site.singleton_memo(shared, proxy_class_id) {
                        return Ok((obj.as_ptr() as i64, Some(obj)));
                    }
                }
                // nio21 wave 14, lane L1 (W11L3-2): learn the pair once, then
                // try the pin-free TLAB allocation; a miss takes the full
                // ladder below exactly as before.
                if num_captures > 0 && site.capture_fast {
                    if memo.is_none() {
                        site.capture_memo.store(
                            encode_capture_memo(proxy_class_id, num_captures),
                            std::sync::atomic::Ordering::Release,
                        );
                    }
                    if let Some(proxy) =
                        allocate_lambda_proxy_tlab_fast(shared, thread, proxy_class_id, &values)
                    {
                        return Ok((proxy.as_ptr() as i64, Some(proxy)));
                    }
                }
                let proxy = allocate_lambda_proxy_from_values(
                    shared,
                    thread,
                    proxy_class_id,
                    &mut values,
                    site.singleton_site,
                )?;
                if num_captures == 0 {
                    site.remember_singleton_slot(shared, proxy_class_id);
                }
                return Ok((proxy.as_ptr() as i64, Some(proxy)));
            }
            // Capture-count disagreement: an internal error rather than a
            // guess.
            return Err(VmError::Internal {
                message: format!(
                    "jit indy bridge: lambda site expects {} captures, call sequence pushed {}",
                    num_captures, arg_count
                ),
            }
            .into());
        }
    }
    // JIT round 16 (lane indy; `CRATONVM_JIT_INDY_SWITCH_FAST`, decided at
    // site construction): a linked `SwitchBootstraps` site is a pure function
    // of its two operands, so the synthetic frame below -- a `Frame` built,
    // pushed, two operand pushes, the cache probe, a pop and the frame's drop,
    // on every compiled `switch (o) { case X x -> … }` -- buys nothing. Served
    // from this thread's `IndySiteCache` entry (the same `(class, cp index)`
    // row the frame path fills on its first, bootstrapping, execution) through
    // the very functions the interpreter's executor calls
    // (`type_switch_index` / `enum_switch_index`). A miss -- not linked yet,
    // lent to an enclosing execution, another site kind -- takes the frame
    // path unchanged.
    if site.switch_fast && arg_count == 2 {
        let linked = match thread.indy_sites.get_mut(site.class_id, site.cp_index) {
            Some(CachedIndySite::Other(lent)) => lent.take(),
            _ => None,
        };
        if let Some(linked) = linked {
            let answer = match &*linked {
                ResolvedCallSite::TypeSwitch { labels } => Some(type_switch_index(
                    shared, thread, values[0], values[1], labels,
                )),
                ResolvedCallSite::EnumSwitch { labels } => {
                    Some(enum_switch_index(shared, values[0], values[1], labels))
                }
                _ => None,
            };
            return_lent_indy_site(thread, site.class_id, site.cp_index, linked);
            if let Some(answer) = answer {
                return answer.map(|index| (i64::from(index), None));
            }
        }
    }
    // JIT round 16 wave 7 (lane indy; `CRATONVM_JIT_INDY_RECORD_FAST`, decided
    // at site construction, default ON, `=0` off): a linked record `ObjectMethods` site
    // is answered by the interpreter's own record code
    // ([`record_object_method_value`]) given the operands as values, with no
    // synthetic frame, no frame push / operand pushes / pop, and no second
    // trip through `execute_invokedynamic_at`'s probes. The same lent-`Arc`
    // protocol as the switch arm above: a miss -- not linked on this thread
    // yet, lent to an enclosing execution of this site (a record whose
    // component's `toString` re-enters it), another row kind, an arity the row
    // does not take -- takes the frame path unchanged.
    if site.record_fast {
        let linked = match thread.indy_sites.get_mut(site.class_id, site.cp_index) {
            Some(CachedIndySite::Other(lent)) => lent.take(),
            _ => None,
        };
        if let Some(linked) = linked {
            let answer = match &*linked {
                ResolvedCallSite::RecordObjectMethod {
                    method,
                    component_names,
                    field_indices,
                    field_descriptors,
                    getter_driven,
                    accessor_getters,
                } if values.len() == record_object_method_arity(*method) => {
                    Some(record_object_method_value(
                        shared,
                        thread,
                        site.class_id,
                        *method,
                        &values,
                        component_names,
                        field_indices,
                        field_descriptors,
                        *getter_driven,
                        accessor_getters,
                    ))
                }
                _ => None,
            };
            return_lent_indy_site(thread, site.class_id, site.cp_index, linked);
            if let Some(answer) = answer {
                return encode_jit_indy_result(site, Some(answer?));
            }
        }
    }
    // JIT round 16 wave 7 (lane indy; `CRATONVM_JIT_INDY_LINKED_CALL_SITE`,
    // default ON, `=0` off): a site admitted because its instruction was already linked
    // through a generic bootstrap. A `Linked` row's `CallSite` target is
    // invoked here, frame-free, exactly as the interpreter invokes it
    // ([`invoke_generic_call_site_with`]: the target read per call, the same
    // `Z` boxing, the same `MethodHandle.invoke` with the site's descriptor,
    // the same return coercion). Any other row -- and a row that is gone --
    // takes the frame path, which runs the row it finds under the site's
    // instruction key. The probe's `CallSite` is held unpinned until
    // `invoke_generic_call_site_with` pins it, with no safepoint between.
    if site.generic_linked {
        if let Some((GenericIndyLink::Linked(call_site), shape)) =
            probe_generic_indy_site(&site.instruction_key(shared))
        {
            // JIT round 16 wave 9 (lane indy, IN16-7): learn the target for
            // the next call's direct dispatch (`jit_indy_linked_direct_leaf`).
            // Reads only, before the pin below: no safepoint in between.
            if site.linked_direct {
                site.fill_direct(shared, thread, call_site);
            }
            let answer = invoke_generic_call_site_with(shared, thread, call_site, &shape, &values)?;
            return encode_jit_indy_result(site, answer);
        }
    }
    let frame_idx = thread.frames.len();
    // `new_from_arcs`, not `new`: every part is precomputed on the site, so
    // this costs refcount bumps instead of a global-mutex memo probe with a
    // body compare (`padded_bytecode_for_method`) plus three `String`
    // allocations, on every bridged call.
    thread.frames.push(Frame::new_from_arcs(
        site.class_id,
        Arc::clone(&site.frame_class_name),
        Arc::clone(&site.frame_method_name),
        Arc::clone(&site.target_descriptor),
        None,
        Arc::clone(&site.frame_code),
        Arc::clone(&site.frame_exception_table),
        site.frame_max_stack,
        0,
        &[],
    ));
    thread.frames.last_mut().expect("bridge frame was just pushed").mark_vm_synthetic();
    for value in values {
        if thread.frames[frame_idx].stack.push(value).is_err() {
            thread.frames.pop();
            return Err(VmError::Internal {
                message: "jit indy bridge: operand stack overflow".to_owned(),
            }
            .into());
        }
    }
    // The synthetic frame is `bridge` at pc 0 and names no instruction; the
    // site's own key stands in for it (see `singleton_site`).
    let executed = execute_invokedynamic_at(
        shared,
        thread,
        frame_idx,
        site.cp_index,
        Some(site.singleton_site),
    );
    let popped = match executed {
        Ok(()) => thread.frames[frame_idx].stack.pop().ok(),
        Err(error) => {
            // The frame this bridge owns must come off the stack whatever
            // happened on it — an abandoned synthetic frame would be visible to
            // every stack walk and to the collector's root scan from here on.
            thread.frames.pop();
            return Err(error);
        }
    };
    thread.frames.pop();
    encode_jit_indy_result(site, popped)
}

/// The bridge's answer for `popped`, the value the site produced (`None`: no
/// value at all), as [`execute_jit_indy_generic_raw`] hands it back.
///
/// Encoded by the SITE's descriptor, not by the `Value`'s own shape: a
/// bootstrap that answered with a differently-tagged value than its
/// descriptor promises is a defect to surface, not one to re-interpret. The
/// arms mirror `jit_invoke_dispatch`'s return encoding one for one.
pub(in crate::runtime::invokedynamic) fn encode_jit_indy_result(
    site: &JitIndyGenericSite,
    popped: Option<Value>,
) -> Result<(i64, Option<ObjectRef>), MethodCallFailed> {
    Ok(match (site.return_type, popped) {
        (b'L' | b'[', Some(Value::Object(Some(obj)))) => (obj.as_ptr() as i64, Some(obj)),
        // A genuine null. NOT folded together with an EMPTY stack below: those
        // two look identical from the call site and mean opposite things — one
        // is the answer, the other is the answer having gone missing.
        (b'L' | b'[', Some(Value::Object(None))) => (0, None),
        (b'J', Some(Value::Long(v))) => (v, None),
        (b'F', Some(Value::Float(f))) => (f.to_bits() as i64, None),
        (b'D', Some(Value::Double(d))) => (d.to_bits() as i64, None),
        (_, Some(Value::Int(v))) => (v as i64, None),
        (_, other) => {
            return Err(VmError::Internal {
                message: format!(
                    "jit indy bridge: {} returned {:?} for descriptor {}",
                    site.cp_index, other, site.target_descriptor
                ),
            }
            .into());
        }
    })
}

/// What [`execute_invokedynamic`]'s fast path takes out of the resolution
/// cache before dropping the guard. See the comment there for why a lambda
/// site is reduced to two `Copy` facts instead of being cloned.
pub enum CachedIndySite {
    Lambda {
        proxy_class_id: ClassId,
        num_captures: usize,
        /// The zero-capture singleton key of the entry's ONE instruction,
        /// `(method key, pc after it)` (`lambda_site_method_key`), memoised
        /// when an interpreter frame filled the row so a hit does not hash the
        /// method's name and descriptor again. `None` for a capturing site and
        /// for a row the compiled bridge filled. Sound because a per-entry
        /// lambda row exists only for an entry no other instruction of the
        /// class names (`lambda_entry_is_shared`); the hit still checks the
        /// frame's pc against it.
        singleton_key: Option<(u64, usize)>,
        /// The `LAMBDA_SINGLETON_CACHE` slot of the row `singleton_key` names,
        /// learnt at this thread's first warm evaluation (round i1 wave 20,
        /// lane L4): a later hit at the memo's pc reads the instance from it
        /// without the table's lock or hash. `None` until then, and for every
        /// row without a `singleton_key`. Only a pointer to the scanned slot —
        /// this thread holds no copy of the reference.
        singleton: Option<Arc<LambdaSingletonSlot>>,
    },
    /// The map's own `Arc`, shared with it. `None` while a hit has LENT it
    /// out: the hit moves the `Arc` out of the entry for the duration of the
    /// call and puts it back after (`execute_invokedynamic_at`), so a warm
    /// site bumps no refcount — a refcount on the map's `Arc` is one cache
    /// line every thread executing the site writes. A `None` entry is a miss
    /// (a re-entrant execution of the same site on this thread, from a
    /// `toString()` the site runs, takes the shared path).
    Other(Option<Arc<ResolvedCallSite>>),
}

/// Per-thread copy of the JDK-factory sites linked in `resolution_cache`
/// (`JvmThread::indy_sites`), probed before that cache so a warm
/// `invokedynamic` takes neither its VM-wide `RwLock` (two atomic RMWs on a
/// cache line every thread shares) nor its hash probe.
///
/// # Validity
///
/// The table is a [`SiteCache`](crate::runtime::interpreter::site_cache::SiteCache),
/// so an entry is served only while the class-NAME generation, the
/// resolution epoch and the no-redefinition latch are all as they were when
/// the `resolution_cache` row was READ. Every path that drops or changes a
/// `call_sites` row moves one of them: class unloading
/// (`memory::gc::unload_dead_class_metadata` runs `forget_classes`, removes
/// the classes from `loaded_classes` — a name removal, which moves the name
/// generation — and calls `bump_resolution_epoch`), redefinition and
/// in-place layout changes (`resolution_invalidate_adapter` bumps the
/// resolution epoch before `invalidate_class`; redefinition also latches the
/// table off). The FIFO cap evicts a row without a signal, but an evicted
/// row's value stays a correct linkage of the entry: every shape stored here
/// is a pure function of the constant pool and the resolved classes, and
/// carries no heap reference (so the copy needs no GC root).
///
/// Tagged with [`NameEpochs`](crate::runtime::interpreter::site_cache::NameEpochs)
/// (wave 16): no stored shape is a subtype verdict — a `TypeSwitch` label
/// holds a resolved class id and the match is decided per execution against
/// the live hierarchy; a lambda row holds a synthetic proxy id; a record
/// row's field indices move only with the resolution epoch. The
/// class-definition epoch the default tag used moves on EVERY class
/// definition, so every warm site of every thread missed and refilled under
/// the `resolution_cache` lock through each class-loading burst.
///
/// Filled only from a `resolution_cache` HIT, never from a bootstrap's own
/// result, so threads converge on the row the map settled on. An `Other`
/// entry shares the map's `Arc`, which a hit lends out of the entry instead
/// of cloning (see [`CachedIndySite::Other`]).
pub type IndySiteCache = crate::runtime::interpreter::site_cache::SiteCache<
    CachedIndySite,
    crate::runtime::interpreter::site_cache::NameEpochs,
>;

/// Put back the `Arc` a hit lent out of this thread's entry for
/// `(class_id, cp_index)`: only into that entry, only while it is live (same
/// key, epochs unmoved) and still empty. An entry a re-entrant execution
/// refilled, an epoch move and a slot collision all leave the table as it is
/// and drop the loan — every one of those outcomes is a valid table state.
#[inline]
pub(in crate::runtime::invokedynamic) fn return_lent_indy_site(
    thread: &mut JvmThread,
    class_id: ClassId,
    cp_index: u16,
    site: Arc<ResolvedCallSite>,
) {
    if let Some(CachedIndySite::Other(slot)) = thread.indy_sites.get_mut(class_id, cp_index) {
        if slot.is_none() {
            *slot = Some(site);
        }
    }
}

/// Execute an invokedynamic instruction.
///
/// Supports two bootstrap methods:
/// - `StringConcatFactory.makeConcatWithConstants` — string concatenation
/// - `LambdaMetafactory.metafactory` — lambda / method reference creation
///
/// Call sites are cached after first bootstrap: subsequent executions reuse the
/// cached result without re-resolving the constant pool.
pub fn execute_invokedynamic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
) -> Result<(), MethodCallFailed> {
    execute_invokedynamic_at(shared, thread, frame_idx, cp_index, None)
}

/// [`execute_invokedynamic`] with the zero-capture singleton key given
/// explicitly. `None` derives it from the executing frame (method and pc: the
/// interpreter). `Some` is for a caller whose frame is synthetic and names no
/// instruction -- the compiled bridge, which passes the key of the instruction
/// it compiles ([`JitIndyGenericSite::singleton_site`]).
pub(in crate::runtime::invokedynamic) fn execute_invokedynamic_at(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    singleton_site: Option<(u64, usize)>,
) -> Result<(), MethodCallFailed> {
    // Interpreter round i4 wave 94, lane L4 (`CRATONVM_DBG=mh-finder-census`;
    // one flag read otherwise): every execution timed by the path it takes,
    // so the linked site's own cost is read apart from the call around it
    // (`L4W87IndyLinkCostBench` `lambda-second` / `capture-second`).
    if !cratonvm_native_builtins::mh_finder_census::indy_census_on() {
        return execute_invokedynamic_at_uncensused(
            shared,
            thread,
            frame_idx,
            cp_index,
            singleton_site,
        );
    }
    let path = indy_census_path(shared, thread, frame_idx, cp_index, singleton_site);
    // Started after the path probe, so its lock is not in the row.
    let start = std::time::Instant::now();
    let result =
        execute_invokedynamic_at_uncensused(shared, thread, frame_idx, cp_index, singleton_site);
    cratonvm_native_builtins::mh_finder_census::note_indy(path, start);
    result
}

/// The path an `invokedynamic` execution is about to take, for the census
/// (wave 94): which caller (`interp`, or `bridge` for the compiled bridge's
/// synthetic frame), then this thread's own entry (`thread-site`, the third
/// execution on), the VM-wide per-entry row (`shared-hit`, a linked site's
/// first execution on this thread: the second execution of a fresh site),
/// or neither (`other`: a per-instruction linkage, or the link itself).
/// Census only: takes the `resolution_cache` read lock. (`get` takes `&mut`:
/// a stale entry is dropped by the probe, as the execution's own probe
/// would drop it.)
#[cold]
pub(in crate::runtime::invokedynamic) fn indy_census_path(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    singleton_site: Option<(u64, usize)>,
) -> &'static str {
    let class_id = thread.frames[frame_idx].class_id;
    let thread_site = thread.indy_sites.get(class_id, cp_index).is_some();
    let shared_hit = !thread_site
        && shared
            .classes
            .resolution_cache
            .read()
            .get_call_site_arc(class_id, cp_index)
            .is_some();
    match (singleton_site.is_some(), thread_site, shared_hit) {
        (false, true, _) => "interp thread-site",
        (false, false, true) => "interp shared-hit",
        (false, false, false) => "interp other",
        (true, true, _) => "bridge thread-site",
        (true, false, true) => "bridge shared-hit",
        (true, false, false) => "bridge other",
    }
}



/// What the cached-site probe of [`execute_invokedynamic_at_uncensused`] puts
/// in this thread's entry on a site's next execution, put right after the
/// link (`CRATONVM_INDY_LINK_FILLS_THREAD_SITE`). Nothing when the link left
/// no per-entry row (a per-instruction linkage of a shared entry, a refused
/// fill under a redefinition).
pub(in crate::runtime::invokedynamic) fn fill_thread_indy_site_after_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    (class_id, cp_index): (ClassId, u16),
    epochs_at_entry: (u64, u64),
    singleton_site: Option<(u64, usize)>,
) {
    // Wave 103 (lane L4): `SiteCache::put` drops a fill whose entry epochs
    // moved during the link; ask first, so such a link does not also pay the
    // `resolution_cache` read lock, the row's `Arc` and the method-key hash
    // for nothing.
    if epochs_at_entry != IndySiteCache::epochs_for(shared) {
        return;
    }
    let row = {
        let cache = shared.classes.resolution_cache.read();
        match cache.get_call_site_arc(class_id, cp_index) {
            Some(site) => match &**site {
                ResolvedCallSite::Lambda(lcs) => {
                    Some((Some((lcs.proxy_class_id, lcs.capture_types.len())), None))
                }
                _ => Some((None, Some(Arc::clone(site)))),
            },
            None => None,
        }
    };
    let entry = match row {
        Some((Some((proxy_class_id, num_captures)), _)) => {
            // The zero-capture singleton key of an interpreter frame, as the
            // cached-site probe computes it (the frame's pc is still the one
            // after this instruction).
            let singleton_key = match singleton_site {
                None if num_captures == 0 => {
                    let frame = &thread.frames[frame_idx];
                    Some((lambda_site_method_key(frame), frame.pc))
                }
                _ => None,
            };
            // Wave 103 (lane L4; the w102 triage measured this switch 0.82-2.20x
            // on `L4W101IndyFirstLinkBench`): without the slot, the site's SECOND
            // execution became this thread's first warm hit, which asks
            // `LAMBDA_SINGLETON_CACHE` (its lock and hash) to learn the slot --
            // work the switch-off arm pays only at the THIRD execution, which no
            // row times. The link has just minted the singleton under exactly
            // this key (`allocate_lambda_proxy` with the frame's key), so learn
            // the slot here and the second execution reads it lock-free. `None`
            // (no row: a capturing site, a refused mint) keeps the old learning.
            let singleton = singleton_key
                .and_then(|key| lambda_singleton_slot(shared, proxy_class_id, key));
            CachedIndySite::Lambda {
                proxy_class_id,
                num_captures,
                singleton_key,
                singleton,
            }
        }
        Some((None, Some(site))) => CachedIndySite::Other(Some(site)),
        _ => return,
    };
    thread
        .indy_sites
        .put(class_id, cp_index, epochs_at_entry, entry);
}

/// Publish a concat link's `site` in `resolution_cache` (as of `fill_as_of`)
/// and, when `keep` (`CRATONVM_INDY_LINK_FILLS_THREAD_SITE`), answer the row's
/// own `Arc`, read under the same write guard. Interpreter round i5 wave 104,
/// lane L4: the w103b `--nojit` A/B read the concat link 1.34x slower with
/// the switch on; [`fill_thread_indy_site_after_link`] took the cache's read
/// lock and hashed the key AGAIN right after this put, so the moved fill cost
/// a second lock round on top of the second execution's saving. `None` when
/// the put was refused (a redefinition since `fill_as_of`) or not kept.
pub(in crate::runtime::invokedynamic) fn put_concat_site_keeping_row(
    shared: &SharedVm,
    (class_id, cp_index): (ClassId, u16),
    site: ResolvedCallSite,
    fill_as_of: u64,
    keep: bool,
) -> Option<Arc<ResolvedCallSite>> {
    let mut cache = shared.classes.resolution_cache.write();
    let put = cache.put_call_site_as_of(class_id, cp_index, site, fill_as_of);
    if !(put && keep) {
        return None;
    }
    cache.get_call_site_arc(class_id, cp_index).cloned()
}

/// [`fill_thread_indy_site_after_link`] for a non-lambda row the link already
/// holds ([`put_concat_site_keeping_row`]): the same epochs rule, no
/// `resolution_cache` read. Interpreter round i5 wave 104, lane L4.
pub(in crate::runtime::invokedynamic) fn fill_thread_other_site_after_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    (class_id, cp_index): (ClassId, u16),
    epochs_at_entry: (u64, u64),
    row: Arc<ResolvedCallSite>,
) {
    if epochs_at_entry != IndySiteCache::epochs_for(shared) {
        return;
    }
    thread.indy_sites.put(
        class_id,
        cp_index,
        epochs_at_entry,
        CachedIndySite::Other(Some(row)),
    );
}

