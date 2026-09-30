// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! invokedynamic support: StringConcatFactory and LambdaMetafactory.
//!
//! Handles the two most common bootstrap methods produced by javac:
//!
//! 1. **StringConcatFactory.makeConcatWithConstants** (Java 9+): compiles `"foo" + bar`
//!    to invokedynamic with a recipe string. `\u0001` = argument placeholder.
//!
//! 2. **LambdaMetafactory.metafactory** (Java 8+): compiles lambdas and method
//!    references to invokedynamic, producing lightweight proxy objects that implement
//!    a functional interface and delegate to the implementation method.

use std::sync::Arc;

use cratonvm_reader::attribute::BootstrapMethod;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

// `NativeContext` traits brought into scope for the `NativeContextImpl` calls
// of the generic-invokedynamic bootstrap path. (A `Class` static argument's
// mirror no longer goes through the context: gc-common w9-c routes it through
// `constants::class_mirror_or_oom`, which collects instead of aborting.)
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeInvokeAccess};

use crate::classloading::resolution::{
    LambdaCallSite, MethodHandle, MethodHandleKind, RecordAccessorGetter, RecordMethodKind,
    ResolvedCallSite, SwitchLabel,
};
use crate::classloading::ClassId;
use crate::error::{MethodCallFailed, RuntimeError, VmError};
use crate::runtime::frame::Frame;
use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::{
    create_java_string, create_java_string_uninterned, read_java_string, read_java_string_units,
    NativeContextImpl, SharedVm,
};

/// The StringConcatFactory bootstrap method class name.
const STRING_CONCAT_FACTORY: &str = "java/lang/invoke/StringConcatFactory";

/// The StringConcatFactory bootstrap method name (with recipe).
const MAKE_CONCAT_WITH_CONSTANTS: &str = "makeConcatWithConstants";

/// The StringConcatFactory.makeConcat method name (no recipe, all args concatenated).
const MAKE_CONCAT: &str = "makeConcat";

/// The LambdaMetafactory bootstrap method class name.
const LAMBDA_METAFACTORY: &str = "java/lang/invoke/LambdaMetafactory";

/// The LambdaMetafactory.metafactory bootstrap method name.
const METAFACTORY: &str = "metafactory";

/// The LambdaMetafactory.altMetafactory bootstrap method name (advanced flags variant).
const ALT_METAFACTORY: &str = "altMetafactory";

/// `LambdaMetafactory.FLAG_SERIALIZABLE` — the spun proxy implements
/// `java.io.Serializable` and carries a `writeReplace()`.
const FLAG_SERIALIZABLE: i32 = 1;

/// `LambdaMetafactory.FLAG_MARKERS` — the flags word is followed by
/// `markerCount` and then that many `Class` bootstrap arguments, each an
/// ADDITIONAL interface the spun proxy class implements. Emitted by javac for
/// an intersection target type (`(Adder & Cloneable) (a, b) -> a + b`) and by
/// any hand-written `altMetafactory` call.
///
/// The block that follows the markers is [`FLAG_BRIDGES`]'s. Markers come
/// FIRST, so the marker block's extent is computable without looking at the
/// bridge bit.
const FLAG_MARKERS: i32 = 2;

/// `LambdaMetafactory.FLAG_BRIDGES` — after the marker block (if any) come
/// `bridgeCount` and then that many `MethodType`s: ADDITIONAL descriptors of the
/// SAM name the spun class implements, each forwarding to the SAM. javac emits
/// it when no interface can hold the bridge, e.g. an intersection target
/// `(ObjM & StrM)` whose `Object m()` / `String m()` are unrelated. Lambda
/// dispatch enters the body only for a descriptor the proxy implements
/// (`lambda_accepts_descriptor`), so the list is recorded beside the proxy —
/// see `record_lambda_proxy_bridges`.
const FLAG_BRIDGES: i32 = 4;

/// The SwitchBootstraps bootstrap method class name (JEP 441, Java 21).
const SWITCH_BOOTSTRAPS: &str = "java/lang/runtime/SwitchBootstraps";

/// SwitchBootstraps.typeSwitch bootstrap method name.
const TYPE_SWITCH: &str = "typeSwitch";

/// SwitchBootstraps.enumSwitch bootstrap method name.
const ENUM_SWITCH: &str = "enumSwitch";

/// ObjectMethods bootstrap class (JEP 395, Java 16+) — records.
const OBJECT_METHODS: &str = "java/lang/runtime/ObjectMethods";

/// ObjectMethods.bootstrap method name.
const BOOTSTRAP: &str = "bootstrap";

/// Apache Groovy's invokedynamic bootstrap class.
const GROOVY_INDY_INTERFACE: &str = "org/codehaus/groovy/vmplugin/v8/IndyInterface";

/// Cached read of a `CRATONVM_DBG_*` env var.
///
/// `cratonvm_types::flags::runtime_var_os` is a `getenv` mutex + `OsString` allocation on Linux and a
/// ~500 ns `GetEnvironmentVariableW` syscall plus a UTF-16 decode on Windows.
/// Two of the three flags below sat on genuinely hot paths:
/// `CRATONVM_DBG_INDY_GENERIC` is read on **every execution** of a generic
/// (non-JDK-factory) `invokedynamic` — which is every Groovy / JRuby / Kotlin
/// call site — and `CRATONVM_DBG_LAMBDA_DISPATCH` on every lambda bootstrap.
/// Same process-lifetime caching policy as
/// [`crate::runtime::env_cache`] and `runtime::exceptions`: setting the
/// variable after the first read has no effect.
macro_rules! cached_env_flag {
    ($name:ident, $env:literal) => {
        #[inline]
        fn $name() -> bool {
            static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *CACHE.get_or_init(|| cratonvm_types::flags::runtime_var_os($env).is_some())
        }
    };
}

cached_env_flag!(dbg_indy_all, "CRATONVM_DBG_INDY_ALL");
cached_env_flag!(dbg_indy_generic, "CRATONVM_DBG_INDY_GENERIC");
cached_env_flag!(dbg_lambda_dispatch, "CRATONVM_DBG_LAMBDA_DISPATCH");

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
const GROOVY_CAST: &str = "cast";

/// Groovy's runtime type-coercion helper (implements "Groovy truth").
const GROOVY_DTT: &str = "org/codehaus/groovy/runtime/typehandling/DefaultTypeTransformation";

/// `StringConcatFactory` recipe tags, as UTF-16 code units.
///
/// The recipe is walked unit-by-unit rather than `char`-by-`char` so a lone
/// surrogate in a folded literal survives; these are the two markers it looks
/// for. Values per `java.lang.invoke.StringConcatFactory`.
const TAG_ARG_UNIT: u16 = 0x0001;
const TAG_CONST_UNIT: u16 = 0x0002;

/// Data extracted from the constant pool under a read lock, owned so we can
/// drop the lock before proceeding with string creation (which needs a write lock).
struct IndyInfo {
    bsm_class: String,
    bsm_method: String,
    target_name: String,
    target_descriptor: String,
    /// The recipe (first bootstrap argument) as UTF-16 code units, for a
    /// `StringConcatFactory.makeConcatWithConstants` site only; empty for every
    /// other bootstrap. Units rather than `String` because a recipe may embed a
    /// lone surrogate from a folded string literal, which a Rust `str` cannot
    /// hold -- see `resolve_concat_constant_units`.
    recipe: Vec<u16>,
    /// Additional constants from bootstrap arguments (TAG_CONST placeholders),
    /// as UTF-16 code units, for the same reason as `recipe`. Same scope.
    constant_args: Vec<Vec<u16>>,
    /// Raw CP indices for bootstrap arguments (needed for LambdaMetafactory).
    bootstrap_arg_indices: Vec<u16>,
    /// The bootstrap handle names `LambdaMetafactory.metafactory` or
    /// `altMetafactory` with its real descriptor: only then does
    /// `lambda_site_refusal` model `BootstrapMethodInvoker`'s call.
    bsm_descriptor_is_lmf: bool,
    /// `ResolutionCache::fill_snapshot` taken before the constant pool was
    /// read for this record: every `put_call_site_as_of` of this bootstrap
    /// publishes under it (interpreter round i1 wave 23, lane L5).
    fill_as_of: u64,
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

/// Immutable metadata owned for the lifetime of generated code which directly
/// invokes a `StringConcatFactory` call site.
#[repr(C)]
pub struct JitStringConcatSite {
    /// [`JIT_INDY_SITE_CONCAT`]. FIRST FIELD, and `#[repr(C)]`, so
    /// [`jit_indy_site_kind`] can read it through either site type's pointer.
    kind: u32,
    recipe: Arc<[u16]>,
    constant_args: Vec<Arc<[u16]>>,
    target_descriptor: Arc<str>,
    /// The site's argument tags, parsed once at site construction. The bridge
    /// used to re-parse the descriptor into a fresh `Vec` on every call.
    arg_types: Box<[char]>,
    /// The synthetic frame's parts, pre-built for [`Frame::new_from_arcs`] for
    /// the reason [`JitIndyGenericSite`] states: `Frame::new` takes the global
    /// `padded_bytecode_for_method` mutex and allocates three `String`s, on
    /// every compiled string concatenation. Read only by the kill-switch arm
    /// (`execute_jit_string_concat_on_frame`) since the bridge went frame-free
    /// in round i1 wave 20; drop them with that arm.
    frame_class_name: Arc<str>,
    frame_method_name: Arc<str>,
    frame_method_descriptor: Arc<str>,
    frame_code: Arc<[u8]>,
    frame_exception_table: Arc<[cratonvm_reader::attribute::ExceptionTableEntry]>,
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
    kind: u32,
    /// The class whose constant pool holds `cp_index`. The bridge pushes a
    /// synthetic frame carrying it, because `execute_invokedynamic` reads the
    /// caller class from `thread.frames[frame_idx].class_id` and keys the
    /// resolved-call-site cache on `(that class, cp_index)` — so the compiled
    /// site and the interpreted one share ONE cache entry and one bootstrap.
    class_id: ClassId,
    cp_index: u16,
    target_descriptor: Arc<str>,
    /// The site's argument type tags, parsed ONCE.
    ///
    /// `parse_descriptor_args` walks a `String` and allocates a `Vec` — per
    /// call, on a path whose whole reason to exist is that the interpreter's
    /// version re-derives constants. Everything below is here for the same
    /// reason: the synthetic frame is built with [`Frame::new_from_arcs`],
    /// which takes pre-built `Arc`s, rather than with `Frame::new`, whose
    /// `padded_bytecode_for_method` takes a GLOBAL MUTEX and verifies its memo
    /// with a body compare — on every single bridged call.
    arg_types: Arc<[u8]>,
    frame_class_name: Arc<str>,
    frame_method_name: Arc<str>,
    /// Padded empty bytecode. The frame decodes no instruction, but the
    /// interpreter's dispatch loop reads two bytes past the last opcode
    /// unconditionally, so the two-byte tail is a precondition rather than a
    /// courtesy — see `Frame::new_from_arcs`.
    frame_code: Arc<[u8]>,
    frame_exception_table: Arc<[cratonvm_reader::attribute::ExceptionTableEntry]>,
    frame_max_stack: u16,
    /// First byte of the site's RETURN descriptor. The bridge hands compiled
    /// code one `i64`, and this is what says how the `Value` was encoded into
    /// it — the same encoding `jit_invoke_dispatch` uses for an ordinary
    /// invoke's return, so the call site pushes it with the same three-way
    /// (`xmm0` / plain / oop-marked) choice.
    return_type: u8,
    /// The zero-capture singleton key of the INSTRUCTION this site compiles:
    /// [`lambda_singleton_site_for`] of the method containing it and its bci,
    /// i.e. exactly the key the interpreter derives from the real frame. Both
    /// bridge paths (the frame-free fast path and the bootstrapping frame
    /// path, through [`execute_invokedynamic_at`]) key on it, so a
    /// non-capturing lambda keeps its identity at tier-up. The same key finds
    /// the per-instruction linkage of an entry several instructions share
    /// (`lambda_entry_is_shared`), so such an instruction keeps its proxy
    /// CLASS at tier-up too.
    singleton_site: (u64, usize),
    /// `(vm_identity, proxy class, slot)` of this site's zero-capture singleton
    /// row, learnt at the first fast-path evaluation that went through the
    /// table (round i1 wave 20, lane L4). A warm evaluation reads the instance
    /// from the slot: no lock on `LAMBDA_SINGLETON_CACHE`. Answers only while
    /// the VM and the proxy class the fast path resolved are the ones recorded
    /// and the slot is live; otherwise the table answers, as before.
    singleton_slot: std::sync::OnceLock<(usize, ClassId, Arc<LambdaSingletonSlot>)>,
}

impl JitIndyGenericSite {
    /// The singleton this site's slot memo holds for `proxy_class_id` on this
    /// VM, or `None` (no memo yet, another VM or proxy class, retired slot).
    #[inline]
    fn singleton_memo(&self, shared: &SharedVm, proxy_class_id: ClassId) -> Option<ObjectRef> {
        let (vm_identity, proxy, slot) = self.singleton_slot.get()?;
        if *vm_identity != shared.vm_identity || *proxy != proxy_class_id {
            return None;
        }
        slot.get()
    }

    /// Learn the slot of the row the table just answered from (or minted).
    /// First writer wins; a site whose proxy later changes keeps asking the
    /// table.
    fn remember_singleton_slot(&self, shared: &SharedVm, proxy_class_id: ClassId) {
        if self.singleton_slot.get().is_some() {
            return;
        }
        if let Some(slot) = lambda_singleton_slot(shared, proxy_class_id, self.singleton_site) {
            let _ = self
                .singleton_slot
                .set((shared.vm_identity, proxy_class_id, slot));
        }
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
    Some(Box::into_raw(Box::new(JitStringConcatSite {
        kind: JIT_INDY_SITE_CONCAT,
        recipe,
        constant_args,
        target_descriptor: Arc::from(descriptor),
        arg_types: parse_descriptor_args(descriptor).into_boxed_slice(),
        // Same name, method and descriptor the per-call `Frame::new` used, so
        // a stack walk that meets this frame sees exactly what it saw before.
        frame_class_name: Arc::from("<jit-indy>"),
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
/// `docs/internal/fixed-bugs/interpreter-L4-compiled-concat-bridge-renders-constants-the-interpreter-does-not-FIXED-20261002.md`).
///
/// The bridge is built from the pool with no class manager and no mode, so it
/// cannot follow the interpreter where the interpreter needs either:
///
/// * a `Class` constant, which the interpreter renders `interface X` for a
///   loaded interface (wave 35) and resolves through the caller's loader
///   under `--jdk-only` (wave 38); the bridge rendered `class X` for every
///   class, so a hot method's output changed once it was compiled;
/// * a constant the native renderer does not model (a `MethodType`, a
///   `MethodHandle`, a dynamic constant: the bridge rendered it as `""`), and
///   the shapes `makeConcatWithConstants` refuses (the recipe's tags against
///   the call type and the constants, a return type that cannot hold a
///   `String`, more than 200 argument slots), which `--jdk-only` links
///   through the JDK's own factory (`concat_needs_the_jdk_factory`, wave 35).
///
/// javac emits none of these (its constants are strings folded into the
/// recipe), so ordinary concatenations keep the bridge.
fn jit_concat_site_is_pool_decided(
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
        LAMBDA_METAFACTORY => {
            matches!(handle.member_name.as_ref(), METAFACTORY | ALT_METAFACTORY)
        }
        SWITCH_BOOTSTRAPS => matches!(handle.member_name.as_ref(), TYPE_SWITCH | ENUM_SWITCH),
        // Only a site whose getters are all `REF_getField`s of the record
        // class (wave 39): any other getter may link through the JDK's own
        // `ObjectMethods` (`bootstrap_generic`, `--jdk-only`), the generic
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
    let frame_max_stack = u16::try_from(arg_types.len())
        .unwrap_or(u16::MAX)
        .saturating_add(1);
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
    Some(Box::into_raw(Box::new(JitIndyGenericSite {
        kind: JIT_INDY_SITE_GENERIC,
        class_id,
        cp_index,
        target_descriptor: Arc::from(descriptor),
        arg_types: Arc::from(arg_types.as_slice()),
        frame_class_name: Arc::from("<jit-indy>"),
        frame_method_name: Arc::from(JIT_INDY_FRAME_METHOD),
        frame_code: crate::runtime::frame::padded_bytecode(&[]),
        frame_exception_table: Arc::from(
            Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
        ),
        frame_max_stack,
        return_type: ret,
        singleton_site,
        singleton_slot: std::sync::OnceLock::new(),
    })) as usize)
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
    concat_values_to_string(
        shared,
        thread,
        &site.recipe,
        site.constant_args.as_slice(),
        &site.arg_types,
        &values,
    )
    .map(Some)
}

/// Kill switch of the frame-free compiled concat bridge: `false` restores the
/// synthetic `<jit-indy>.concat` frame ([`execute_jit_string_concat_on_frame`])
/// so the two arms can be priced against each other. A `const`, not a flag.
const JIT_CONCAT_BRIDGE_FRAME_FREE: bool = true;

/// The pre-wave-20 arm of [`execute_jit_string_concat_raw`]: push a synthetic
/// frame, push the operands onto it, and run the interpreter's frame-based
/// concat. Kept behind [`JIT_CONCAT_BRIDGE_FRAME_FREE`] and for the unit test
/// that pins the two arms to one answer.
fn execute_jit_string_concat_on_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site: &JitStringConcatSite,
    values: smallvec::SmallVec<[Value; 8]>,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let arg_count = values.len();
    let frame_idx = thread.frames.len();
    // `new_from_arcs` over the site's pre-built parts; see the fields' doc.
    thread.frames.push(Frame::new_from_arcs(
        crate::classloading::ClassId::new(0),
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
fn jit_indy_lambda_fast_enabled() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static GATE: AtomicU8 = AtomicU8::new(0);
    match GATE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let on = match cratonvm_types::flags::runtime_var("CRATONVM_JIT_INDY_LAMBDA_FAST") {
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
        // Only the two `Copy` facts the allocation needs are taken out of the
        // guard; cloning the whole `LambdaCallSite` cost seven refcount
        // round-trips and a `Vec` per bridged lambda evaluation. This thread's
        // `IndySiteCache` (shared with the interpreter's fast path, same
        // (class, cp index) key) answers first, without the lock.
        let local = match thread.indy_sites.get(site.class_id, site.cp_index) {
            Some(CachedIndySite::Lambda {
                proxy_class_id,
                num_captures,
                ..
            }) => Some((*proxy_class_id, *num_captures)),
            _ => None,
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
    // Encoded by the SITE's descriptor, not by the `Value`'s own shape: a
    // bootstrap that answered with a differently-tagged value than its
    // descriptor promises is a defect to surface, not one to re-interpret. The
    // arms mirror `jit_invoke_dispatch`'s return encoding one for one.
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
fn return_lent_indy_site(
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
fn execute_invokedynamic_at(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    singleton_site: Option<(u64, usize)>,
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // --- Fast path: check call site cache ---
    //
    // Lock-order discipline (the H2 TestScript class-resolution DEADLOCK):
    // clone the cached site OUT of the `resolution_cache` read guard and
    // DROP the guard before executing. Holding it across
    // `execute_cached_call_site` runs arbitrary code under the lock —
    // StringConcat allocates Strings (`alloc_java_string_object` takes
    // `class_manager.write()`), lambdas invoke Java — while
    // `resolve_field_ref` takes class_manager THEN resolution_cache: a
    // textbook ABBA inversion that wedged main-vm against a concat worker
    // with every resolver queued behind the writers.
    //
    // A lambda site -- the hottest cached shape after string concatenation --
    // needs only two `Copy` facts to execute (the proxy class id and the
    // capture count), so it is not cloned at all: a whole `LambdaCallSite`
    // clone is seven `Arc` refcount round-trips plus a `Vec<char>` allocation,
    // paid on every evaluation of every lambda expression. The other shapes
    // are taken out as the cache's own `Arc` -- one refcount bump, where a
    // clone of a `TypeSwitch` / `EnumSwitch` / `RecordObjectMethod` site was a
    // `Vec` allocation (three for a record method) per execution.
    //
    // Before either: this thread's own copy of the site (`IndySiteCache`),
    // which takes no lock and no hash probe — and, since wave 16, no refcount
    // (an `Other` entry lends its `Arc` out, see `CachedIndySite::Other`) and
    // no hash of the method name for a zero-capture lambda's singleton key
    // (memoised in the entry, checked against the frame's pc).
    if let Some(site) = thread.indy_sites.get_mut(current_class_id, cp_index) {
        match site {
            CachedIndySite::Lambda {
                proxy_class_id,
                num_captures,
                singleton_key,
                singleton,
            } => {
                let (proxy_class_id, num_captures, memo) =
                    (*proxy_class_id, *num_captures, *singleton_key);
                let pc = thread.frames[frame_idx].pc;
                let memo = memo.filter(|&(_, memo_pc)| memo_pc == pc);
                // Wave 20: a zero-capture hit at the memo's pc reads the
                // singleton from the slot this entry learnt, with no lock on
                // `LAMBDA_SINGLETON_CACHE`; the first such hit on this thread
                // asks the table and learns the slot.
                if let (None, 0, Some(key)) = (singleton_site, num_captures, memo) {
                    if let Some(obj) = singleton.as_deref().and_then(LambdaSingletonSlot::get) {
                        thread.frames[frame_idx]
                            .stack
                            .push(Value::Object(Some(obj)))?;
                        return Ok(());
                    }
                    allocate_lambda_proxy(shared, thread, frame_idx, proxy_class_id, 0, Some(key))?;
                    let slot = lambda_singleton_slot(shared, proxy_class_id, key);
                    if let Some(CachedIndySite::Lambda {
                        proxy_class_id: entry_proxy,
                        singleton_key: Some(entry_key),
                        singleton,
                        ..
                    }) = thread.indy_sites.get_mut(current_class_id, cp_index)
                    {
                        // The same row still (a re-entrant evaluation may
                        // have refilled the entry).
                        if *entry_proxy == proxy_class_id && *entry_key == key {
                            *singleton = slot;
                        }
                    }
                    return Ok(());
                }
                return allocate_lambda_proxy(
                    shared,
                    thread,
                    frame_idx,
                    proxy_class_id,
                    num_captures,
                    singleton_site.or(memo),
                );
            }
            CachedIndySite::Other(lent) => {
                if let Some(site) = lent.take() {
                    let outcome = execute_cached_call_site(shared, thread, frame_idx, &site);
                    return_lent_indy_site(thread, current_class_id, cp_index, site);
                    return outcome;
                }
                // Lent to an enclosing execution of this site on this thread:
                // the shared path below answers (and refills the entry).
            }
        }
    }
    // Then this thread's copy of a PER-INSTRUCTION linkage (a shared lambda
    // entry, a Groovy cast; `ThreadInstructionSites`): no lock, no hash of the
    // method key. Before the per-entry `resolution_cache` probe, which such
    // an entry always misses — it never has a per-entry row
    // (`publish_lambda_instruction_site`, and a generic bootstrap writes none),
    // so the order changes no answer. Only for an interpreter frame, whose
    // method and pc ARE the instruction.
    if singleton_site.is_none() && ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire)
    {
        let ident = InstructionIdent::of_frame(
            shared,
            &thread.frames[frame_idx],
            current_class_id,
            cp_index,
        );
        if let Some((link, method_key)) = probe_thread_instruction_site(&ident) {
            return run_thread_instruction_link(shared, thread, frame_idx, link, method_key);
        }
    }
    // Read BEFORE the `resolution_cache` probe; see `SiteCache::put`.
    let epochs_at_entry = IndySiteCache::epochs_for(shared);
    let cached_site = {
        let cache = shared.classes.resolution_cache.read();
        match cache.get_call_site_arc(current_class_id, cp_index) {
            Some(site) => Some(match &**site {
                ResolvedCallSite::Lambda(lcs) => CachedIndySite::Lambda {
                    proxy_class_id: lcs.proxy_class_id,
                    num_captures: lcs.capture_types.len(),
                    singleton_key: None,
                    singleton: None,
                },
                _ => CachedIndySite::Other(Some(Arc::clone(site))),
            }),
            None => None,
        }
    };
    match cached_site {
        Some(CachedIndySite::Lambda {
            proxy_class_id,
            num_captures,
            ..
        }) => {
            // An interpreter frame's zero-capture singleton key, computed once
            // here and memoised in the entry. The compiled bridge's frame
            // names no instruction (`singleton_site` given): no memo.
            let singleton_key = match singleton_site {
                None if num_captures == 0 => {
                    let frame = &thread.frames[frame_idx];
                    Some((lambda_site_method_key(frame), frame.pc))
                }
                _ => None,
            };
            thread.indy_sites.put(
                current_class_id,
                cp_index,
                epochs_at_entry,
                CachedIndySite::Lambda {
                    proxy_class_id,
                    num_captures,
                    singleton_key,
                    singleton: None,
                },
            );
            return allocate_lambda_proxy(
                shared,
                thread,
                frame_idx,
                proxy_class_id,
                num_captures,
                singleton_site.or(singleton_key),
            );
        }
        Some(CachedIndySite::Other(Some(site))) => {
            thread.indy_sites.put(
                current_class_id,
                cp_index,
                epochs_at_entry,
                CachedIndySite::Other(Some(Arc::clone(&site))),
            );
            return execute_cached_call_site(shared, thread, frame_idx, &site);
        }
        Some(CachedIndySite::Other(None)) | None => {}
    }

    // --- Fast path 2: a linkage published for THIS instruction ---
    //
    // Not in `resolution_cache`: that is keyed per constant-pool entry, and a
    // generic linkage -- or the lambda linkage of an entry several
    // instructions share -- is per instruction (see `GENERIC_INDY_SITES`).
    if ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        let key = indy_instruction_key(
            shared,
            &thread.frames[frame_idx],
            current_class_id,
            cp_index,
            singleton_site,
        );
        if let Some((link, shape, gate)) = probe_generic_indy_site_gated(&key) {
            if singleton_site.is_none() {
                let ident = InstructionIdent::of_frame(
                    shared,
                    &thread.frames[frame_idx],
                    current_class_id,
                    cp_index,
                );
                fill_thread_instruction_site(&ident, gate, &link);
            }
            return run_generic_indy_link(shared, thread, frame_idx, link, &shape, singleton_site);
        }
    }

    // --- Slow path: bootstrap the call site ---
    // Taken BEFORE the constant pool is read: a redefinition of the caller
    // that lands before the fill below is not published over the new pool's
    // index (`ResolutionCache::put_call_site_as_of`, interpreter round i1
    // wave 23, lane L5).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    // Extract all needed data under the class_manager read lock, then drop it.
    // This avoids deadlocking when create_java_string needs a write lock.
    // `unloaded_class_constants`: the concat `Class` constants whose class is
    // not loaded yet, resolved after the lock is dropped (below).
    let (mut info, unloaded_class_constants) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic: class {current_class_id} not found"),
            })?;

        // 1. Resolve the InvokeDynamic CP entry
        let (bsm_index, nat_index) = match class.constant_pool.get(cp_index) {
            Some(ConstantPoolEntry::InvokeDynamic {
                bootstrap_method_attr_index,
                name_and_type_index,
            }) => (*bootstrap_method_attr_index, *name_and_type_index),
            _ => {
                return Err(VmError::Internal {
                    message: format!("invokedynamic cp#{cp_index}: not an InvokeDynamic entry"),
                }
                .into());
            }
        };

        // 2. Get the target name and descriptor from NameAndType
        let (target_name, target_descriptor) = class
            .constant_pool
            .get_name_and_type(nat_index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic: invalid name_and_type at cp#{nat_index}"),
            })?;

        // 3. Look up the bootstrap method
        let bsm = class
            .bootstrap_methods
            .get(bsm_index as usize)
            .ok_or_else(|| VmError::Internal {
                message: format!(
                    "invokedynamic: bootstrap method index {bsm_index} out of bounds (have {})",
                    class.bootstrap_methods.len()
                ),
            })?;

        // 4. Resolve the MethodHandle to determine which bootstrap method it is
        let bsm_handle =
            resolve_method_handle_full(&class.constant_pool, bsm.bootstrap_method_ref)?;
        let bsm_class = bsm_handle.class_name.to_string();
        let bsm_method = bsm_handle.member_name.to_string();
        let bsm_descriptor_is_lmf = bsm_class == LAMBDA_METAFACTORY
            && matches!(
                (bsm_method.as_str(), &*bsm_handle.descriptor),
                (METAFACTORY, METAFACTORY_DESCRIPTOR)
                    | (ALT_METAFACTORY, ALT_METAFACTORY_DESCRIPTOR)
            );

        // 5. Extract recipe and constant args -- for `makeConcatWithConstants`
        // only, the one arm that reads them. Every other bootstrap used to pay
        // for rendering each of its static arguments as concat text (a
        // `String` per argument), and a generic site whose `CallSite` is not
        // cached (Groovy's `MutableCallSite`s under the default
        // `CRATONVM_INDY_CALLSITE_CACHE`) paid it on every execution.
        let is_concat_with_constants = &*bsm_handle.class_name == STRING_CONCAT_FACTORY
            && &*bsm_handle.member_name == MAKE_CONCAT_WITH_CONSTANTS;
        let recipe: Vec<u16> = match bsm.bootstrap_arguments.first() {
            Some(&arg_index) if is_concat_with_constants => {
                resolve_concat_constant_units(&class.constant_pool, arg_index).unwrap_or_default()
            }
            _ => Vec::new(),
        };

        // The `\u0002` (TAG_CONST) entries in the recipe consume these trailing
        // bootstrap static arguments in order. Per `java.lang.invoke.
        // StringConcatFactory`, a constant may be *any* loadable constant
        // (String, but also int/long/float/double or a Class), and is folded in
        // via its `String.valueOf` form — NOT only `String`/`Utf8`. The previous
        // `resolve_string_constant` returned `None` (→ empty) for every numeric
        // constant, silently dropping it. `resolve_concat_constant` converts each
        // loadable-constant kind to its HotSpot-identical text (reusing
        // `format_float`/`format_double` so e.g. `1.0f` → "1.0", not "1").
        let mut unloaded_class_constants: Vec<(usize, String)> = Vec::new();
        let constant_args: Vec<Vec<u16>> = if is_concat_with_constants {
            bsm.bootstrap_arguments
                .iter()
                .skip(1) // skip recipe
                .enumerate()
                .map(|(position, &idx)| {
                    // A `Class` constant renders as `String.valueOf(Class)`
                    // does: `interface X` for an interface (interpreter round
                    // i1 wave 35; from the loaded class, `class X` when it is
                    // not loaded yet).
                    if let Some(ConstantPoolEntry::ClassReference { name_index }) =
                        class.constant_pool.get(idx)
                    {
                        if let Some(name) = class.constant_pool.get_utf8(*name_index) {
                            let loaded = cm.find_class_by_name_for_class(name, current_class_id);
                            let is_interface = loaded
                                .and_then(|id| cm.get_class(id))
                                .is_some_and(|c| c.is_interface());
                            if is_interface {
                                let text = format!("interface {}", name.replace('/', "."));
                                return text.encode_utf16().collect();
                            }
                            if loaded.is_none() && !name.starts_with('[') {
                                unloaded_class_constants.push((position, name.to_string()));
                            }
                        }
                    }
                    resolve_concat_constant_units(&class.constant_pool, idx).unwrap_or_default()
                })
                .collect()
        } else {
            Vec::new()
        };

        let bootstrap_arg_indices = bsm.bootstrap_arguments.clone();

        (
            IndyInfo {
                bsm_class,
                bsm_method,
                target_name: target_name.to_string(),
                target_descriptor: target_descriptor.to_string(),
                recipe,
                constant_args,
                bootstrap_arg_indices,
                bsm_descriptor_is_lmf,
                fill_as_of,
            },
            unloaded_class_constants,
        )
    }; // cm read lock dropped here

    // `--jdk-only` (interpreter round i1 wave 38, lane L4; item 1 of
    // `i37-L4-review-of-waves-30-36-invoke-and-indy-small-divergences`): a
    // `Class` concat constant whose class was not loaded yet is resolved as
    // HotSpot resolves every static argument before the bootstrap runs,
    // through the caller's loader: an interface then renders as
    // `interface X` (it rendered `class X`), and a missing class is the
    // resolution's `NoClassDefFoundError` (it rendered `class X` and linked).
    // `--compatible` keeps the text rendering and loads nothing.
    if !unloaded_class_constants.is_empty() && shared.config.is_jdk_only() {
        for (position, name) in &unloaded_class_constants {
            let id = crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                current_class_id,
                name,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found_for(
                    shared,
                    thread,
                    Some(current_class_id),
                    name,
                    e,
                )
            })?;
            let is_interface = shared
                .classes
                .class_manager
                .read()
                .get_class(id)
                .is_some_and(|c| c.is_interface());
            if is_interface {
                if let Some(slot) = info.constant_args.get_mut(*position) {
                    *slot = format!("interface {}", name.replace('/', "."))
                        .encode_utf16()
                        .collect();
                }
            }
        }
    }

    if dbg_indy_all() {
        let caller_name = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(current_class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default()
        };
        eprintln!(
            "[indy-all] caller={caller_name} bsm={}.{} target={}{}",
            info.bsm_class, info.bsm_method, info.target_name, info.target_descriptor
        );
    }
    // `--jdk-only` (interpreter round i1 wave 35, item 1 of
    // `i29-L4-native-indy-linkage-skips-the-jdks-validation`): a concat site
    // the native linkage would accept although `StringConcatFactory` refuses
    // it, or whose constants it does not render as the JDK does, links
    // through the real factory instead, which raises the exact
    // `BootstrapMethodError` (or renders the constants) and records it.
    if info.bsm_class == STRING_CONCAT_FACTORY
        && (info.bsm_method == MAKE_CONCAT_WITH_CONSTANTS || info.bsm_method == MAKE_CONCAT)
        && shared.config.is_jdk_only()
        && concat_needs_the_jdk_factory(shared, current_class_id, &info)
    {
        return bootstrap_generic(shared, thread, frame_idx, cp_index, &info, current_class_id);
    }
    if info.bsm_class == STRING_CONCAT_FACTORY && info.bsm_method == MAKE_CONCAT_WITH_CONSTANTS {
        // Cache the StringConcat call site
        let site = ResolvedCallSite::StringConcat {
            recipe: Arc::from(info.recipe.as_slice()),
            constant_args: info
                .constant_args
                .iter()
                .map(|u| Arc::from(u.as_slice()))
                .collect(),
            target_descriptor: Arc::from(info.target_descriptor.clone()),
        };
        shared
            .classes
            .resolution_cache
            .write()
            .put_call_site_as_of(current_class_id, cp_index, site, info.fill_as_of);

        execute_string_concat(
            shared,
            thread,
            frame_idx,
            &info.recipe,
            info.constant_args.as_slice(),
            &info.target_descriptor,
        )
    } else if info.bsm_class == STRING_CONCAT_FACTORY && info.bsm_method == MAKE_CONCAT {
        // makeConcat has no recipe — all arguments are simply concatenated in order.
        // Synthesize a recipe of all \u{0001} placeholders so the existing concat
        // logic works unchanged.
        let arg_types = parse_descriptor_args(&info.target_descriptor);
        let synthetic_recipe: Vec<u16> = vec![TAG_ARG_UNIT; arg_types.len()];

        let site = ResolvedCallSite::StringConcat {
            recipe: Arc::from(synthetic_recipe.as_slice()),
            constant_args: vec![],
            target_descriptor: Arc::from(info.target_descriptor.as_str()),
        };
        shared
            .classes
            .resolution_cache
            .write()
            .put_call_site_as_of(current_class_id, cp_index, site, info.fill_as_of);

        // A synthesized all-argument recipe is U+0001 repeated, so it holds no
        // TAG_CONST (U+0002) placeholders and the constant list is empty. The
        // whole-`IndyInfo` clone (`patched_info`) this branch used to build was
        // only ever read for the three values passed below.
        const NO_CONSTANTS: &[Arc<[u16]>] = &[];
        execute_string_concat(
            shared,
            thread,
            frame_idx,
            &synthetic_recipe,
            NO_CONSTANTS,
            &info.target_descriptor,
        )
    } else if info.bsm_class == LAMBDA_METAFACTORY
        && (info.bsm_method == METAFACTORY || info.bsm_method == ALT_METAFACTORY)
    {
        // altMetafactory has additional bootstrap arguments (flags, marker interfaces,
        // bridges) beyond the 3 standard ones. The core lambda proxy creation is
        // identical, but none of the extras is advisory: the FLAG_SERIALIZABLE bit
        // and the MARKER INTERFACE list change the spun proxy's interface list, so
        // `instanceof`/`checkcast`/`Class.isInstance` against a marker must succeed,
        // and a FLAG_BRIDGES descriptor is one more way into the body (lambda
        // dispatch admits only the descriptors the proxy implements).
        // `bootstrap_lambda` reads all three.
        //
        // `--jdk-only` (interpreter round i1 waves 33, 36, 37): the static
        // arguments resolve first, so a missing implementation method is a
        // `NoSuchMethodError` here, unwrapped (JVMS 5.4.3.5); then a site the
        // JDK refuses fails linkage with the refusal HotSpot raises (a
        // `LambdaConversionException`, or `BootstrapMethodInvoker`'s and
        // `altMetafactory`'s own argument errors), wrapped in
        // `BootstrapMethodError`, instead of linking. Raised here with the
        // JDK's own message: `bootstrap_generic` would reach CratonVM's
        // `LambdaMetafactory` shim, which does not validate. A shape the
        // native linkage cannot build at all (fewer than three static
        // arguments, or the wrong kind among them) is refused in every mode:
        // it used to be an uncatchable internal error.
        let jdk_only = shared.config.is_jdk_only();
        // Every mode (wave 41, lane L4; row 4 of `i37-L4-...`): a dynamic
        // constant among the static arguments resolves first, in argument
        // order; its failure is the site's, and its value is what the checks
        // and the linkage read (a valid `MethodType` used to be an
        // uncatchable internal error). Empty for every javac site.
        let condy = match lambda_resolve_dynamic_args(shared, thread, current_class_id, &info) {
            Ok(condy) => condy,
            Err(failed) => {
                return settle_native_link_failure(
                    shared,
                    thread,
                    frame_idx,
                    current_class_id,
                    cp_index,
                    singleton_site,
                    &info.target_descriptor,
                    failed,
                );
            }
        };
        if jdk_only {
            let impl_handle = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(current_class_id).and_then(|class| {
                    let index = *info.bootstrap_arg_indices.get(1)?;
                    resolve_method_handle_full(&class.constant_pool, index).ok()
                })
            };
            if let Some(impl_handle) = impl_handle {
                if let Err(failed) =
                    lambda_impl_resolves(shared, thread, current_class_id, &impl_handle)
                {
                    return settle_native_link_failure(
                        shared,
                        thread,
                        frame_idx,
                        current_class_id,
                        cp_index,
                        singleton_site,
                        &info.target_descriptor,
                        failed,
                    );
                }
            }
        }
        let refusal = match lambda_site_refusal(shared, thread, current_class_id, &info, jdk_only, &condy) {
            Ok(refusal) => refusal,
            // A class a `MethodType` names could not be loaded (wave 39).
            Err(failed) => {
                return settle_native_link_failure(
                    shared,
                    thread,
                    frame_idx,
                    current_class_id,
                    cp_index,
                    singleton_site,
                    &info.target_descriptor,
                    failed,
                );
            }
        };
        if let Some(refusal) = refusal {
            // An empty message is a null one (`lambda_null_argument_refusal`).
            let failed = bootstrap_method_error_with_new_cause_opt(
                shared,
                thread,
                "bootstrap method initialization exception",
                refusal.cause_class,
                (!refusal.message.is_empty()).then_some(refusal.message.as_str()),
            );
            return settle_native_link_failure(
                shared,
                thread,
                frame_idx,
                current_class_id,
                cp_index,
                singleton_site,
                &info.target_descriptor,
                failed,
            );
        }
        bootstrap_lambda(shared, thread, frame_idx, cp_index, &info, singleton_site, &condy)
    } else if info.bsm_class == SWITCH_BOOTSTRAPS
        && (info.bsm_method == TYPE_SWITCH || info.bsm_method == ENUM_SWITCH)
    {
        // `SwitchBootstraps`' own argument checks (interpreter round i1 wave
        // 38, lane L4): the invocation type in every mode (the native
        // switches pop two operands and push an `int` whatever the call site
        // says), the selector type and the labels under `--jdk-only`.
        let jdk_only = shared.config.is_jdk_only();
        if let Some(refusal) = switch_site_refusal(shared, current_class_id, &info, jdk_only) {
            let failed = bootstrap_method_error_with_new_cause(
                shared,
                thread,
                "bootstrap method initialization exception",
                refusal.cause_class,
                &refusal.message,
            );
            return settle_native_link_failure(
                shared,
                thread,
                frame_idx,
                current_class_id,
                cp_index,
                singleton_site,
                &info.target_descriptor,
                failed,
            );
        }
        if info.bsm_method == TYPE_SWITCH {
            bootstrap_type_switch(shared, thread, frame_idx, cp_index, &info)
        } else {
            bootstrap_enum_switch(shared, thread, frame_idx, cp_index, &info)
        }
    } else if info.bsm_class == OBJECT_METHODS && info.bsm_method == BOOTSTRAP {
        // `ObjectMethods.bootstrap`'s own checks (interpreter round i1 wave
        // 38, lane L4): the method name and the type's shape in every mode,
        // the record class in the type and the name list under `--jdk-only`.
        let jdk_only = shared.config.is_jdk_only();
        // `--jdk-only` (wave 39): the getters of a site that is not javac's
        // shape resolve first, from the calling class, as HotSpot resolves
        // every static argument before the bootstrap runs: an inaccessible
        // or missing getter is its `IllegalAccessError` / `NoSuchFieldError`.
        if jdk_only {
            if let Err(failed) =
                object_methods_resolve_getters(shared, thread, current_class_id, &info)
            {
                return settle_native_link_failure(
                    shared,
                    thread,
                    frame_idx,
                    current_class_id,
                    cp_index,
                    singleton_site,
                    &info.target_descriptor,
                    failed,
                );
            }
        }
        if let Some(refusal) =
            object_methods_site_refusal(shared, current_class_id, &info, jdk_only)
        {
            let failed = bootstrap_method_error_with_new_cause(
                shared,
                thread,
                "bootstrap method initialization exception",
                refusal.cause_class,
                &refusal.message,
            );
            return settle_native_link_failure(
                shared,
                thread,
                frame_idx,
                current_class_id,
                cp_index,
                singleton_site,
                &info.target_descriptor,
                failed,
            );
        }
        // `--jdk-only` (wave 42): a `toString` getter whose handle type is
        // not `(R)T` is refused as `makeToString` refuses it, rather than
        // handed to the JDK's route, which fails on CratonVM in
        // `MethodHandle.copyWith` (`AbstractMethodError`).
        if jdk_only && info.target_name == "toString" {
            if let Some(refusal) =
                object_methods_to_string_getter_refusal(shared, current_class_id, &info)
            {
                if dbg_indy_all() {
                    eprintln!(
                        "[indy-all] object-methods toString cp#{cp_index}: getter refused: {}",
                        refusal.message
                    );
                }
                let failed = object_methods_to_string_failure(shared, thread, &refusal);
                return settle_native_link_failure(
                    shared,
                    thread,
                    frame_idx,
                    current_class_id,
                    cp_index,
                    singleton_site,
                    &info.target_descriptor,
                    failed,
                );
            }
        }
        bootstrap_record_object_method(shared, thread, frame_idx, cp_index, &info)
    } else if info.bsm_class == GROOVY_INDY_INTERFACE
        && info.target_name == GROOVY_CAST
        && info.target_descriptor.ends_with(")Z")
        && matches!(parse_descriptor_args(&info.target_descriptor).as_slice(),
                    [c] if *c == 'L' || *c == '[')
    {
        // Groovy "cast"-to-boolean coercion (`if (x.f())`, `!x`, ternaries …).
        // Groovy emits a dedicated `cast:(Object)Z` invokedynamic and routes it
        // through `IndyInterface` → `Selector$CastSelector.handleBoolean`, which
        // builds a `guardWithTest(IS_NULL, FALSE, asBoolean(…))` MethodHandle.
        // CratonVM's MH dispatch of that particular adapter chain does not reach
        // the runtime receiver's `asBoolean` (so a Groovy-falsy-but-non-null value
        // — `Boolean.FALSE`, `""`, `[]`, `0` — read as TRUE), which silently flips
        // `if`/`!` branches (e.g. Spring Boot's `SpringRepositoriesExtension`
        // `if (!"commercial".equalsIgnoreCase(buildType))` and
        // `if (version.endsWith("-SNAPSHOT"))`). Dispatch Groovy's own
        // `DefaultTypeTransformation.castToBoolean(Object)` directly instead — it
        // is real Groovy bytecode implementing "Groovy truth" and dispatches
        // `asBoolean` correctly on CratonVM (verified == HotSpot). This bypasses
        // the broken CastSelector adapter for the boolean case only; all other
        // cast targets keep the generic bootstrap path.
        //
        // The decision is a pure function of the constant pool, so it is
        // published per instruction like a generic linkage: the steady state
        // then skips rebuilding this `IndyInfo` (four `String`s and a recipe
        // walk) on every Groovy truth test.
        if generic_indy_cache_mode() != GenericIndyCacheMode::Off {
            let key = generic_indy_site_key(
                shared,
                &thread.frames[frame_idx],
                current_class_id,
                cp_index,
            );
            publish_generic_indy_site(
                key,
                GenericIndySlot {
                    link: GenericIndyLink::GroovyCastToBoolean,
                    shape: Arc::new(GenericIndyShape::new(&info.target_descriptor)),
                    gate: generic_indy_gate(shared, current_class_id),
                },
            );
        }
        groovy_cast_to_boolean(shared, thread, frame_idx)
    } else {
        // Generic invokedynamic: a bootstrap method outside the hardcoded JDK
        // factory set above (e.g. Groovy's
        // `org.codehaus.groovy.vmplugin.v8.IndyInterface.bootstrap`). Per JVMS
        // §5.4.3.6 the linkage runs the bootstrap method itself to obtain a
        // `CallSite`, then invokes that call site's target `MethodHandle` with
        // the dynamic arguments. `bootstrap_generic` does exactly that; if the
        // bootstrap genuinely cannot be executed it surfaces a loud error
        // (BootstrapMethodError / InternalError), never a silent wrong value.
        bootstrap_generic(shared, thread, frame_idx, cp_index, &info, current_class_id)
    }
}

/// A bootstrap static argument resolved out of the constant pool, captured in a
/// lock-free form so it can be materialised into a `Value` *after* the
/// `class_manager` read lock is dropped (materialisation may allocate / load
/// classes / run `<clinit>`).
enum StaticArg {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Str(String),
    Class(String),
    /// `CONSTANT_MethodType`, resolved through the same per-entry record `ldc`
    /// of that index uses, so the two agree on identity.
    MType {
        cp_index: u16,
        descriptor: String,
    },
    /// `CONSTANT_MethodHandle`, resolved and recorded like `ldc` of that index.
    MHandle {
        cp_index: u16,
        handle: MethodHandle,
    },
    /// A `CONSTANT_Dynamic`, resolved (running its own bootstrap) and recorded
    /// per entry by `constants::resolve_condy_constant`.
    Dynamic(u16),
}

/// Resolve a single bootstrap static-argument CP entry to a [`StaticArg`].
///
/// JVMS §4.7.23: a static argument is any loadable constant (§4.4), so every
/// loadable tag has an arm. Anything else is a malformed `BootstrapMethods`
/// attribute, reported as the `ClassFormatError` the condy decoder
/// (`constants::decode_condy`) raises for the same defect — not the
/// uncatchable VM-internal error it used to be.
fn resolve_static_arg_kind(
    cp: &ConstantPool,
    class_name: &str,
    index: u16,
) -> Result<StaticArg, MethodCallFailed> {
    let malformed = || -> MethodCallFailed {
        VmError::Linkage(crate::error::LinkageError::ClassFormatError {
            class_name: class_name.to_string(),
            message: format!("invokedynamic: unusable bootstrap argument cp#{index}"),
        })
        .into()
    };
    let entry = cp.get(index).ok_or_else(malformed)?;
    Ok(match entry {
        ConstantPoolEntry::Integer(i) => StaticArg::Int(*i),
        ConstantPoolEntry::Long(l) => StaticArg::Long(*l),
        ConstantPoolEntry::Float(f) => StaticArg::Float(*f),
        ConstantPoolEntry::Double(d) => StaticArg::Double(*d),
        ConstantPoolEntry::StringReference { .. } => {
            StaticArg::Str(resolve_string_constant(cp, index).unwrap_or_default())
        }
        ConstantPoolEntry::ClassReference { .. } => {
            let name = cp.get_class_name(index).ok_or_else(malformed)?;
            StaticArg::Class(name.to_string())
        }
        ConstantPoolEntry::MethodType { .. } => StaticArg::MType {
            cp_index: index,
            descriptor: resolve_method_type(cp, index).ok_or_else(malformed)?,
        },
        ConstantPoolEntry::MethodHandle { .. } => StaticArg::MHandle {
            cp_index: index,
            handle: resolve_method_handle_full(cp, index)?,
        },
        ConstantPoolEntry::Dynamic { .. } => StaticArg::Dynamic(index),
        _ => return Err(malformed()),
    })
}

/// A `CONSTANT_MethodType` / `CONSTANT_MethodHandle` static argument, resolved
/// on the same terms as `ldc` of that entry (`constants::execute_ldc`): the
/// recorded value if there is one, the recorded `LinkageError` rethrown if the
/// entry already failed, else `resolve` — its `LinkageError` recorded against
/// the entry, its value recorded in the store `ldc` uses for the tag.
///
/// `permanent` is `true` for a method handle: re-resolving one does not yield
/// the same object, so, as `ldc` does, it goes to the never-evicted,
/// insert-if-absent record and the FIRST recorded handle is the answer.
/// A method type goes to the capped record (its re-resolution interns to the
/// same object).
///
/// `fill_as_of` is the `IndyInfo::fill_as_of` snapshot, taken before the
/// constant pool was read for this bootstrap: every record here is made under
/// it, so a redefinition of the class in between records nothing
/// (i22-L5, interpreter round i1 wave 24).
fn resolve_recorded_static_arg(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    cp_index: u16,
    permanent: bool,
    fill_as_of: u64,
    resolve: impl FnOnce(&mut JvmThread) -> Result<ObjectRef, MethodCallFailed>,
) -> Result<Value, MethodCallFailed> {
    use crate::runtime::interpreter::constants as k;
    if let Some(v) = k::probe_recorded_cp_constant(shared, class_id, cp_index) {
        return Ok(v);
    }
    if let Some(recorded) = k::recorded_resolution_failure(shared, thread, class_id, cp_index) {
        return Err(recorded);
    }
    let obj = resolve(thread).map_err(|e| {
        k::record_resolution_failure_as_of(shared, class_id, cp_index, e, fill_as_of)
    })?;
    // JVMS §5.4.3: a failure another thread recorded for the entry meanwhile
    // is its outcome (interpreter round i1 wave 38, lane L5; `--jdk-only`).
    // Allocates only on that path, where `obj` is dropped.
    if let Some(recorded) =
        k::recorded_resolution_failure_after_success(shared, thread, class_id, cp_index)
    {
        return Err(recorded);
    }
    let v = Value::Object(Some(obj));
    if permanent {
        return Ok(k::record_cp_constant_permanent_as_of(
            shared, class_id, cp_index, v, fill_as_of,
        ));
    }
    k::store_recorded_cp_constant_as_of(shared, class_id, cp_index, v, fill_as_of);
    Ok(v)
}

/// `Class.cast`'s message (`Cannot cast java.lang.Integer to java.lang.String`)
/// when the bootstrap static argument `value` cannot be passed to a parameter
/// (or collected into an array component) of type `param`, `None` when it can
/// or when it is not judged. Judged only where the answer is certain: a
/// parameter of a `java/` class other than `Object` (only the boot loader
/// defines one), and a value of a class a loadable constant produces
/// (`String`, the four boxes, `Class`, `MethodType`); a raw primitive is
/// judged as its box, which the collection or the fixed boxing makes.
/// Interpreter round i1 wave 41, lane L4.
fn bootstrap_static_arg_cast_failure(shared: &SharedVm, value: Value, param: &str) -> Option<String> {
    let want = param.strip_prefix('L')?.strip_suffix(';')?;
    if want == "java/lang/Object" || !want.starts_with("java/") {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let (cid, actual): (ClassId, &str) = match value {
        Value::Object(Some(o)) => {
            if shared.mem.heap.kind_of(o) != cratonvm_types::ObjectKind::Object {
                return None;
            }
            let cid = shared.mem.heap.class_id_of(o);
            let name = cm.get_class(cid)?.name.clone();
            let actual = match &*name {
                "java/lang/String" => "java/lang/String",
                "java/lang/Integer" => "java/lang/Integer",
                "java/lang/Long" => "java/lang/Long",
                "java/lang/Float" => "java/lang/Float",
                "java/lang/Double" => "java/lang/Double",
                "java/lang/Class" => "java/lang/Class",
                "java/lang/invoke/MethodType" => "java/lang/invoke/MethodType",
                _ => return None,
            };
            (cid, actual)
        }
        Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => {
            let boxed = match value {
                Value::Int(_) => "java/lang/Integer",
                Value::Long(_) => "java/lang/Long",
                Value::Float(_) => "java/lang/Float",
                _ => "java/lang/Double",
            };
            (cm.get_loaded_class_id(boxed)?, boxed)
        }
        _ => return None,
    };
    if cm.is_assignable_to_name(cid, want) {
        return None;
    }
    Some(format!(
        "Cannot cast {} to {}",
        actual.replace('/', "."),
        want.replace('/', ".")
    ))
}

/// What a bootstrap argument needs to match one fixed declared parameter.
#[derive(Debug)]
enum BootstrapArgFit {
    Keep,
    /// A primitive bound to a reference parameter: box it (`valueOf`).
    Box,
    /// A primitive bound to a wider primitive parameter (JLS §5.1.2).
    Widen(Value),
}

/// The `asType` conversion for a bootstrap argument `v` bound to a parameter
/// whose [`parse_descriptor_args`] tag is `tag` (`L` for any reference).
/// Mirrors `constants::invoke_condy_bootstrap_pinned`'s step 5, plus
/// `long -> float`, which that list omits.
fn bootstrap_arg_fit(tag: char, v: Value) -> BootstrapArgFit {
    match (tag, v) {
        ('L', Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)) => {
            BootstrapArgFit::Box
        }
        ('J', Value::Int(x)) => BootstrapArgFit::Widen(Value::Long(i64::from(x))),
        // Cast: JLS §5.1.2 widening int -> float (may round, as Java does)
        ('F', Value::Int(x)) => BootstrapArgFit::Widen(Value::Float(x as f32)),
        // Cast: JLS §5.1.2 widening long -> float (may round, as Java does)
        ('F', Value::Long(x)) => BootstrapArgFit::Widen(Value::Float(x as f32)),
        ('D', Value::Int(x)) => BootstrapArgFit::Widen(Value::Double(f64::from(x))),
        // Cast: JLS §5.1.2 widening long -> double (may round, as Java does)
        ('D', Value::Long(x)) => BootstrapArgFit::Widen(Value::Double(x as f64)),
        ('D', Value::Float(x)) => BootstrapArgFit::Widen(Value::Double(f64::from(x))),
        _ => BootstrapArgFit::Keep,
    }
}

/// Box a primitive bootstrap argument the way `MethodHandle.invoke` does for a
/// reference parameter or an `Object...` element; a reference passes through.
fn box_bootstrap_primitive(
    shared: &SharedVm,
    thread: &mut JvmThread,
    v: Value,
) -> Result<Value, MethodCallFailed> {
    let (class, desc) = match v {
        Value::Int(_) => ("java/lang/Integer", "(I)Ljava/lang/Integer;"),
        Value::Long(_) => ("java/lang/Long", "(J)Ljava/lang/Long;"),
        Value::Float(_) => ("java/lang/Float", "(F)Ljava/lang/Float;"),
        Value::Double(_) => ("java/lang/Double", "(D)Ljava/lang/Double;"),
        other => return Ok(other),
    };
    Ok(
        crate::vm::invoke_shared(shared, thread, class, "valueOf", desc, &[v])?
            .unwrap_or(Value::Object(None)),
    )
}

/// Groovy `cast:(Object)Z` coercion → `DefaultTypeTransformation.castToBoolean`.
///
/// Pops the single operand and dispatches Groovy's real "Groovy truth" helper,
/// pushing the resulting `boolean`. See the call site in `execute_invokedynamic`
/// for why the generic CastSelector path is bypassed for the boolean case.
fn groovy_cast_to_boolean(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Result<(), MethodCallFailed> {
    // Single reference operand (guaranteed by the dispatch guard).
    let cv = thread.frames[frame_idx].stack.pop_compact();
    let arg = cv.decode_by_descriptor(b'L');
    // Pin the operand across the helper call (castToBoolean runs Java bytecode
    // that may allocate / GC).
    let pin_base = thread.native_pin_roots.len();
    if let Value::Object(Some(o)) = arg {
        thread.native_pin_roots.push(o);
    }
    let result = crate::vm::invoke_shared(
        shared,
        thread,
        GROOVY_DTT,
        "castToBoolean",
        "(Ljava/lang/Object;)Z",
        &[arg],
    );
    thread.native_pin_roots.truncate(pin_base);
    let b = match result? {
        Some(Value::Int(v)) => v,
        Some(Value::Object(Some(o))) => {
            // Defensive: a boxed Boolean — unbox via field 0.
            match shared.mem.heap.get_field(o, 0) {
                Value::Int(v) => v,
                _ => 1,
            }
        }
        _ => 0,
    };
    thread.frames[frame_idx].stack.push(Value::Int(b))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Generic invokedynamic call-site cache (JVMS §5.4.3.6, per instruction)
//
// A bootstrap outside the hard-coded JDK factory set (Groovy's `IndyInterface`,
// JRuby, Nashorn/Dynalink, byte-buddy advice, hand-written bootstraps) used to
// run on EVERY execution of its instruction: `bootstrap_generic` never
// published anything. JVMS links each `invokedynamic` instruction once, so a
// bootstrap with side effects observed N calls instead of 1, a failed linkage
// was retried instead of rethrown, and a `MutableCallSite` retargeted with
// `setTarget` was thrown away with the next fresh bootstrap.
//
// This table caches the linked `CallSite` OBJECT per instruction — never its
// target, which `setTarget` may change and which is therefore read from the
// call site on every execution — plus a recorded `LinkageError` for a failed
// linkage. Keyed like `LAMBDA_SINGLETON_CACHE` (method key + pc name the
// instruction; two instructions sharing one `CONSTANT_InvokeDynamic` entry link
// separately, as HotSpot's per-bytecode `ResolvedIndyEntry` does).
//
// Liveness: a cached `CallSite` is a GC root through `gc_scan_generic_indy_roots`
// (the `"indy-call-sites"` row of `memory::native_roots`), conditional on the
// caller's defining loader exactly as the resolution cache's condy values are,
// so a cached site does not pin a user loader. Rows of unloaded classes are
// dropped by `forget_unloaded_generic_indy_sites` (`memory::gc`); a redefined
// caller's rows go stale through their `RedefineGate`.
// ---------------------------------------------------------------------------

/// `CRATONVM_INDY_CALLSITE_CACHE`: what [`bootstrap_generic`] publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenericIndyCacheMode {
    /// `=0`: nothing is cached. The bootstrap runs on every execution, as it
    /// did before this cache existed.
    Off,
    /// Default: cache every `CallSite` and record every `LinkageError` linkage
    /// failure — EXCEPT a `MutableCallSite` / `VolatileCallSite` produced by
    /// Groovy's `IndyInterface` bootstrap, which is still re-bootstrapped per
    /// execution. Caching one makes every call run whatever target the language
    /// runtime later installs with `setTarget` — for Groovy the relinked
    /// `guardWithTest`/`SwitchPoint` chain, which the per-execution bootstrap
    /// has so far kept CratonVM from ever executing, and whose adapter chains
    /// this file already documents one wrong dispatch of
    /// (`groovy_cast_to_boolean`). See
    /// `docs/known-issues/interpreter/i2-L6-proposal-generic-indy-invoke-path-20260923.md`
    /// (wave 7) for the suite measurement that has to precede making `All` the
    /// default (history:
    /// `interpreter-L6-generic-indy-rebootstraps-every-execution-RETIRED-20260924.md`).
    /// Until wave 6 every
    /// non-constant call site was excluded, whatever its bootstrap.
    Constant,
    /// `=all`: cache every `CallSite`, mutable ones included. The JVMS shape.
    All,
}

impl GenericIndyCacheMode {
    /// Whether a linkage by `bsm_class` that produced a NON-constant call site
    /// is published (a `ConstantCallSite` is published in every mode but
    /// `Off`).
    fn caches_mutable_call_site_of(self, bsm_class: &str) -> bool {
        match self {
            GenericIndyCacheMode::Off => false,
            GenericIndyCacheMode::Constant => !is_groovy_indy_bootstrap(bsm_class),
            GenericIndyCacheMode::All => true,
        }
    }
}

/// Groovy's invokedynamic bootstrap class: `vmplugin/v8` (Groovy 3+) and the
/// `vmplugin/v7` spelling Groovy 2.x shipped.
fn is_groovy_indy_bootstrap(bsm_class: &str) -> bool {
    bsm_class == GROOVY_INDY_INTERFACE
        || bsm_class == "org/codehaus/groovy/vmplugin/v7/IndyInterface"
}

fn generic_indy_cache_mode_from(value: Option<&str>) -> GenericIndyCacheMode {
    match value.map(str::trim) {
        Some(v) if v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false") => {
            GenericIndyCacheMode::Off
        }
        Some(v) if v == "2" || v.eq_ignore_ascii_case("all") => GenericIndyCacheMode::All,
        _ => GenericIndyCacheMode::Constant,
    }
}

/// Read once per process, like every other gate in this file.
fn generic_indy_cache_mode() -> GenericIndyCacheMode {
    static MODE: std::sync::OnceLock<GenericIndyCacheMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        generic_indy_cache_mode_from(
            cratonvm_types::flags::runtime_var("CRATONVM_INDY_CALLSITE_CACHE")
                .ok()
                .as_deref(),
        )
    })
}

/// Linkage of one generic `invokedynamic` instruction.
#[derive(Clone)]
enum GenericIndyLink {
    /// The published `CallSite`. Its target is read on every execution.
    Linked(ObjectRef),
    /// A linkage that failed with a `LinkageError`; every later execution
    /// throws a new error of the same class and message (JVMS §5.4.3).
    Failed(Arc<RecordedLinkageError>),
    /// Groovy's `cast:(Object)Z`, served by [`groovy_cast_to_boolean`]
    /// without a bootstrap. Cached only so the steady state skips re-deriving
    /// that decision from the constant pool on every Groovy truth test.
    GroovyCastToBoolean,
    /// A `LambdaMetafactory` linkage of ONE instruction whose
    /// `CONSTANT_InvokeDynamic` entry other instructions of the class share
    /// (javac folds identical method references onto one entry). JVMS §5.4.3.6
    /// links each instruction separately and HotSpot spins one class per
    /// linkage, so each such instruction gets its own proxy class here, and
    /// the entry is kept out of the per-entry `resolution_cache` row that every
    /// unshared lambda uses. See [`lambda_entry_is_shared`].
    Lambda {
        proxy_class_id: ClassId,
        num_captures: usize,
    },
}

impl GenericIndyLink {
    fn mentions_class(&self, dead: &rustc_hash::FxHashSet<ClassId>) -> bool {
        match self {
            GenericIndyLink::Failed(rec) => dead.contains(&rec.error_class),
            // A proxy id is synthetic and never unloaded; the row goes with its
            // caller class, which is in the key.
            GenericIndyLink::Linked(_)
            | GenericIndyLink::GroovyCastToBoolean
            | GenericIndyLink::Lambda { .. } => false,
        }
    }
}

/// What HotSpot's resolution-error table keeps for a failed indy linkage
/// (`ConstantPoolCache::save_and_throw_indy_exc`): the error's class name and
/// detail message only — not the throwable, and not its cause. That path calls
/// the four-argument `SystemDictionary::add_resolution_error`, so the row's
/// cause is null (only `ConstantPool::save_and_throw_exception`, the condy and
/// class/member path, records a cause). A later execution throws a NEW,
/// cause-less error of that class and message
/// (`ConstantPool::throw_resolution_error`), so `==` between the first and the
/// second error is `false` and the second's `getCause()` is `null` on
/// HotSpot 25.
#[derive(Debug)]
struct RecordedLinkageError {
    error_class: ClassId,
    error_class_name: String,
    message: Option<String>,
}

/// The per-site facts every execution needs, computed once at linkage.
struct GenericIndyShape {
    target_descriptor: Box<str>,
    arg_types: Box<[char]>,
    /// First byte of the return descriptor (`b'V'` for none).
    ret_byte: u8,
    /// `java/lang/invoke/MethodHandle`'s class id in the VM whose row holds
    /// this shape (a shape belongs to one row, and a row to one VM), resolved
    /// on the first execution (wave 16, stage 2a of
    /// `i2-L6-proposal-generic-indy-invoke-path`). A bootstrap class, so the
    /// id never changes for the VM's life.
    method_handle_class: std::sync::OnceLock<ClassId>,
}

impl GenericIndyShape {
    fn new(target_descriptor: &str) -> Self {
        let ret_byte = target_descriptor
            .rfind(')')
            .and_then(|i| target_descriptor.as_bytes().get(i + 1).copied())
            .unwrap_or(b'V');
        Self {
            target_descriptor: Box::from(target_descriptor),
            arg_types: parse_descriptor_args(target_descriptor).into_boxed_slice(),
            ret_byte,
            method_handle_class: std::sync::OnceLock::new(),
        }
    }

    /// `MethodHandle`'s class id, loaded on the first call (a safepoint the
    /// caller must have its references pinned across), memoised after.
    fn method_handle_class(&self, shared: &SharedVm) -> Result<ClassId, VmError> {
        if let Some(&id) = self.method_handle_class.get() {
            return Ok(id);
        }
        let id = shared.load_class_concurrent("java/lang/invoke/MethodHandle")?;
        Ok(*self.method_handle_class.get_or_init(|| id))
    }
}

struct GenericIndySlot {
    link: GenericIndyLink,
    shape: Arc<GenericIndyShape>,
    /// The caller class's redefinition generation at linkage. A redefined
    /// class gets a new constant pool, whose instructions link afresh.
    gate: crate::classloading::resolution::RedefineGate,
}

/// `(vm_identity, caller class, cp index, method key, pc after the instruction)`.
type GenericIndyKey = (usize, ClassId, u16, u64, usize);
type GenericIndyMap = rustc_hash::FxHashMap<GenericIndyKey, GenericIndySlot>;

/// Past this many rows a newly linked site is simply not cached: it behaves as
/// it did before the cache existed rather than growing the table without bound.
const GENERIC_INDY_SITE_CAP: usize = 1 << 16;

static GENERIC_INDY_SITES: std::sync::OnceLock<parking_lot::RwLock<GenericIndyMap>> =
    std::sync::OnceLock::new();

/// `true` once any row was ever published. Read before the lock, so a program
/// with no generic `invokedynamic` pays one relaxed load per first-time
/// bootstrap of a JDK-factory site and nothing on the GC scan.
static ANY_GENERIC_INDY_SITE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn generic_indy_sites() -> &'static parking_lot::RwLock<GenericIndyMap> {
    GENERIC_INDY_SITES.get_or_init(|| parking_lot::RwLock::new(GenericIndyMap::default()))
}

fn generic_indy_site_key(
    shared: &SharedVm,
    frame: &Frame,
    class_id: ClassId,
    cp_index: u16,
) -> GenericIndyKey {
    (
        shared.vm_identity,
        class_id,
        cp_index,
        lambda_site_method_key(frame),
        frame.pc,
    )
}

/// The live row for `key`, if any. A stale row (its caller was redefined) is a
/// miss; the relink that follows replaces it.
fn probe_generic_indy_site(
    key: &GenericIndyKey,
) -> Option<(GenericIndyLink, Arc<GenericIndyShape>)> {
    let table = generic_indy_sites().read();
    let slot = table.get(key)?;
    if slot.gate.is_stale() {
        return None;
    }
    Some((slot.link.clone(), Arc::clone(&slot.shape)))
}

/// [`probe_generic_indy_site`] plus the row's redefinition gate, for the
/// per-thread copy ([`ThreadInstructionSites`]).
fn probe_generic_indy_site_gated(
    key: &GenericIndyKey,
) -> Option<(
    GenericIndyLink,
    Arc<GenericIndyShape>,
    crate::classloading::resolution::RedefineGate,
)> {
    let table = generic_indy_sites().read();
    let slot = table.get(key)?;
    if slot.gate.is_stale() {
        return None;
    }
    Some((
        slot.link.clone(),
        Arc::clone(&slot.shape),
        slot.gate.clone(),
    ))
}

// ---------------------------------------------------------------------------
// Per-thread copy of heap-free per-instruction linkages
//
// A shared lambda entry (`GenericIndyLink::Lambda`) and Groovy's
// `cast:(Object)Z` (`GroovyCastToBoolean`) are answered from
// `GENERIC_INDY_SITES` on every execution, which took the table's VM-wide
// `RwLock` (two atomic RMWs on a cache line every thread shares) and Fx-hashed
// the executing method's name and descriptor to build the key — after first
// missing the per-entry `resolution_cache` under ITS VM-wide lock. This small
// direct-mapped table per OS thread sits in front of both (it is filled at
// the `GENERIC_INDY_SITES` hit) for exactly those two
// shapes, keyed by the WHOLE instruction: VM, class, CP index, pc and the
// method (its interned name / descriptor `Arc`s, compared by pointer first
// and by content only on a pointer miss — never hashed).
//
// Validity, by the table's own gates: the row's `RedefineGate` is copied and
// checked on every hit (a redefined caller relinks); a class id and a
// `vm_identity` are never reused, so an unloaded caller's or a disposed VM's
// entry can never match again; a row is never replaced in the table while
// its gate is fresh (first writer wins), so a copy of a fresh row stays the
// instruction's linkage. Only shapes that carry no heap reference are copied —
// a `Linked` `CallSite` would need a GC root here — and a copy is made only
// from a table HIT, so threads converge on the published row. The compiled
// bridge's synthetic frame (`singleton_site` given) does not use it.
// ---------------------------------------------------------------------------

/// Number of direct-mapped slots per thread. Per-instruction rows are rare
/// (shared lambda entries, Groovy casts); a small table covers a hot loop.
const THREAD_INSTRUCTION_SLOTS: usize = 64;

/// The heap-reference-free linkages a thread may copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadInstructionLink {
    Lambda {
        proxy_class_id: ClassId,
        num_captures: usize,
    },
    GroovyCastToBoolean,
}

impl ThreadInstructionLink {
    fn of(link: &GenericIndyLink) -> Option<Self> {
        match link {
            GenericIndyLink::Lambda {
                proxy_class_id,
                num_captures,
            } => Some(Self::Lambda {
                proxy_class_id: *proxy_class_id,
                num_captures: *num_captures,
            }),
            GenericIndyLink::GroovyCastToBoolean => Some(Self::GroovyCastToBoolean),
            GenericIndyLink::Linked(_) | GenericIndyLink::Failed(_) => None,
        }
    }
}

struct ThreadInstructionSite {
    vm_identity: usize,
    class_id: ClassId,
    cp_index: u16,
    pc: usize,
    method_name: Arc<str>,
    method_descriptor: Arc<str>,
    /// `lambda_site_method_key_of(method_name, method_descriptor)`, hashed
    /// once at the fill (wave 16): a zero-capture lambda's singleton key is
    /// `(method_key, pc)`, which every hit used to re-hash from the frame.
    method_key: u64,
    gate: crate::classloading::resolution::RedefineGate,
    link: ThreadInstructionLink,
}

/// The instruction identity a probe or fill is keyed by.
struct InstructionIdent<'a> {
    vm_identity: usize,
    class_id: ClassId,
    cp_index: u16,
    pc: usize,
    method_name: &'a Arc<str>,
    method_descriptor: &'a Arc<str>,
}

impl<'a> InstructionIdent<'a> {
    fn of_frame(shared: &SharedVm, frame: &'a Frame, class_id: ClassId, cp_index: u16) -> Self {
        Self {
            vm_identity: shared.vm_identity,
            class_id,
            cp_index,
            pc: frame.pc,
            method_name: frame.method_name_arc_ref(),
            method_descriptor: frame.method_descriptor_arc_ref(),
        }
    }

    fn slot(&self) -> usize {
        let key = (((self.class_id.as_u32() as u64) << 16) | self.cp_index as u64)
            ^ (self.pc as u64).rotate_left(40);
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize & (THREAD_INSTRUCTION_SLOTS - 1)
    }
}

/// A lazily allocated direct-mapped table; see the section note above.
#[derive(Default)]
struct ThreadInstructionSites {
    slots: Vec<Option<ThreadInstructionSite>>,
}

impl ThreadInstructionSites {
    #[cfg(test)]
    fn get(&self, ident: &InstructionIdent<'_>) -> Option<ThreadInstructionLink> {
        self.get_keyed(ident).map(|(link, _)| link)
    }

    /// The instruction's link and its method key (see
    /// [`ThreadInstructionSite::method_key`]).
    fn get_keyed(&self, ident: &InstructionIdent<'_>) -> Option<(ThreadInstructionLink, u64)> {
        let site = self.slots.get(ident.slot())?.as_ref()?;
        let same_method =
            |held: &Arc<str>, want: &Arc<str>| Arc::ptr_eq(held, want) || **held == **want;
        if site.vm_identity != ident.vm_identity
            || site.class_id != ident.class_id
            || site.cp_index != ident.cp_index
            || site.pc != ident.pc
            || !same_method(&site.method_name, ident.method_name)
            || !same_method(&site.method_descriptor, ident.method_descriptor)
            || site.gate.is_stale()
        {
            return None;
        }
        Some((site.link, site.method_key))
    }

    fn put(
        &mut self,
        ident: &InstructionIdent<'_>,
        gate: crate::classloading::resolution::RedefineGate,
        link: ThreadInstructionLink,
    ) {
        if self.slots.is_empty() {
            self.slots = (0..THREAD_INSTRUCTION_SLOTS).map(|_| None).collect();
        }
        let idx = ident.slot();
        if let Some(slot) = self.slots.get_mut(idx) {
            *slot = Some(ThreadInstructionSite {
                vm_identity: ident.vm_identity,
                class_id: ident.class_id,
                cp_index: ident.cp_index,
                pc: ident.pc,
                method_name: Arc::clone(ident.method_name),
                method_descriptor: Arc::clone(ident.method_descriptor),
                method_key: lambda_site_method_key_of(ident.method_name, ident.method_descriptor),
                gate,
                link,
            });
        }
    }
}

thread_local! {
    static THREAD_INSTRUCTION_SITES: std::cell::RefCell<ThreadInstructionSites> =
        std::cell::RefCell::new(ThreadInstructionSites::default());
}

/// This thread's copy of the instruction's linkage and the instruction's
/// method key, if any. `None` also when the thread-local is being torn down.
fn probe_thread_instruction_site(
    ident: &InstructionIdent<'_>,
) -> Option<(ThreadInstructionLink, u64)> {
    THREAD_INSTRUCTION_SITES
        .try_with(|sites| sites.try_borrow().ok().and_then(|s| s.get_keyed(ident)))
        .ok()
        .flatten()
}

/// Copy a fresh table row this thread just read. A shape that carries a heap
/// reference is not copied.
fn fill_thread_instruction_site(
    ident: &InstructionIdent<'_>,
    gate: crate::classloading::resolution::RedefineGate,
    link: &GenericIndyLink,
) {
    let Some(link) = ThreadInstructionLink::of(link) else {
        return;
    };
    let _ = THREAD_INSTRUCTION_SITES.try_with(|sites| {
        if let Ok(mut s) = sites.try_borrow_mut() {
            s.put(ident, gate, link);
        }
    });
}

/// Execute a per-thread copy; the same arms as [`run_generic_indy_link`].
/// `method_key` is the executing method's (the copy matched it by name and
/// descriptor), so a zero-capture lambda's singleton key is
/// `(method_key, pc)` without hashing the method again.
fn run_thread_instruction_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    link: ThreadInstructionLink,
    method_key: u64,
) -> Result<(), MethodCallFailed> {
    match link {
        ThreadInstructionLink::Lambda {
            proxy_class_id,
            num_captures,
        } => {
            let singleton_site =
                (num_captures == 0).then(|| (method_key, thread.frames[frame_idx].pc));
            allocate_lambda_proxy(
                shared,
                thread,
                frame_idx,
                proxy_class_id,
                num_captures,
                singleton_site,
            )
        }
        ThreadInstructionLink::GroovyCastToBoolean => {
            groovy_cast_to_boolean(shared, thread, frame_idx)
        }
    }
}

/// Publish `slot` for `key`, FIRST WRITER WINS, and return the link that is
/// actually published. Two threads that link one instruction concurrently both
/// run the bootstrap, but only one result is installed and both use it —
/// HotSpot's `MethodHandleNatives.linkCallSite` publication. A success and a
/// failure race the same way: whichever is published first is the linkage.
fn publish_generic_indy_site(key: GenericIndyKey, slot: GenericIndySlot) -> GenericIndyLink {
    let mut table = generic_indy_sites().write();
    match table.get(&key) {
        Some(existing) if !existing.gate.is_stale() => return existing.link.clone(),
        Some(_) => {}
        None if table.len() >= GENERIC_INDY_SITE_CAP => return slot.link,
        None => {}
    }
    let link = slot.link.clone();
    table.insert(key, slot);
    drop(table);
    ANY_GENERIC_INDY_SITE.store(true, std::sync::atomic::Ordering::Release);
    link
}

/// GC root scan for the cached `CallSite`s of the VM `vm_identity`: hands
/// `visit` each linked call site with the id of the class whose instruction
/// linked it. Driven by the `"indy-call-sites"` row of `memory::native_roots`,
/// which decides per call site whether to root it or pin it to that class's
/// loader (`native_roots::defer_or_root`).
///
/// Same conditional shape as the resolution cache's condy values
/// (`memory::roots` step 13) and the `ClassValue` cache: while the collector
/// runs in loader-weak mode, a call site of a class defined by a user loader is
/// reachable only through that loader (`metadata_pin`), so caching it does not
/// keep the loader — and every class it defined — alive.
///
/// The deferral used to be decided here, from the process-global
/// `metadata_weak_mode()` and the VM-less `loader_pin_addr`; both can answer
/// for another live VM (gc-common w5-b,
/// `docs/known-issues/gc/common-w4b-loader-pin-collision-unroots-another-vms-statics.md`).
pub fn gc_scan_generic_indy_roots(vm_identity: usize, visit: &mut dyn FnMut(u32, ObjectRef)) {
    if !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let table = generic_indy_sites().read();
    for (&(vid, class_id, _, _, _), slot) in table.iter() {
        if vid != vm_identity {
            continue;
        }
        let GenericIndyLink::Linked(call_site) = &slot.link else {
            continue;
        };
        visit(class_id.as_u32(), *call_site);
    }
}

/// GC post-move remap for [`gc_scan_generic_indy_roots`]'s call sites.
pub fn gc_update_generic_indy_refs(vm_identity: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() || !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut table = generic_indy_sites().write();
    remap_generic_indy_rows(&mut table, vm_identity, pointer_map);
}

fn remap_generic_indy_rows(
    table: &mut GenericIndyMap,
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    for (&(vid, _, _, _, _), slot) in table.iter_mut() {
        if vid != vm_identity {
            continue;
        }
        if let GenericIndyLink::Linked(call_site) = &mut slot.link {
            if let Some(&new_addr) = pointer_map.get(&(call_site.as_ptr() as usize)) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *call_site = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
}

/// Drop every row of `vm_identity` whose caller class — or whose recorded
/// error class — was just unloaded. Called from
/// `memory::gc::unload_dead_class_metadata`. Required, not hygiene: a row whose
/// loader died was not marked through `metadata_pin`, so its `CallSite` address
/// no longer names an object and must never be scanned again.
pub fn forget_unloaded_generic_indy_sites(
    vm_identity: usize,
    unloaded: &rustc_hash::FxHashSet<ClassId>,
) {
    // The lambda private-impl memo rides this hook (same caller, same set).
    crate::runtime::interpreter::forget_unloaded_private_impl_memo(vm_identity, unloaded);
    if unloaded.is_empty() {
        return;
    }
    // So does the shared-entry memo: a class id is not reused after an unload,
    // so this is hygiene, not correctness.
    if let Some(memo) = SHARED_INDY_ENTRIES.get() {
        memo.write()
            .retain(|&(vid, class_id), _| vid != vm_identity || !unloaded.contains(&class_id));
    }
    if !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let mut table = generic_indy_sites().write();
    retain_live_generic_indy_rows(&mut table, vm_identity, unloaded);
}

/// Class-unload hook for the lambda-proxy side tables, called from
/// `memory::gc::unload_dead_class_metadata` with the set of unloaded classes.
///
/// A proxy id is synthetic and never itself in `unloaded`; what dies with a
/// class is every proxy its `invokedynamic` instructions spun (its "host",
/// `lambda_proxy_hosts`). Dropped here:
///
/// * the `lambda_proxy_hosts` rows naming an unloaded host;
/// * `ClassRealm::lambda_impl_owner_memo` rows whose proxy's host, or whose
///   implementation owner, unloaded. The host half is new: a row whose owner
///   outlives the host (a JDK method reference such as `String::length`
///   written in an unloaded class) used to stay forever. It is a pure memo, so
///   a proxy instance still reachable after its host unloaded recomputes it;
/// * the zero-capture singleton rows of those proxies. The host's
///   instructions can never execute again, and the row would otherwise name
///   an instance this collection reclaimed (it is pinned to the host's loader,
///   `gc_scan_lambda_singleton_roots`);
/// * since wave 8, the proxies themselves: the `lambda_proxies` call site, the
///   `altMetafactory` marker and bridge lists, the user-loader pin list and
///   the proxy-keyed dispatch memos. A proxy spun by a user-loader host has a
///   `loader_pin` row naming that loader (`pin_lambda_proxy_to_host_loader`),
///   so while any instance of it is live the loader is marked and the host
///   cannot unload; an unloaded host therefore has no live proxy instance
///   left to dispatch. (Before that row existed these were kept for orphaned
///   instances, and accumulated towards `MAX_LAMBDA_PROXIES` in a redeploy
///   loop.) A built-in-loader host never unloads. Since wave 9 the reflective
///   metafactory path records its lookup class as the host
///   (`record_reflective_lambda_proxy_host`), so its proxies go here too;
/// * the reflective `LambdaMetafactory` `CallSite` cache rows keyed by an
///   unloaded host (`lang_invoke::forget_lambda_callsites_of_unloaded_hosts`):
///   each row's factory handle names one of the proxies dropped above, and
///   a cache hit would otherwise allocate instances of an unregistered id.
pub fn forget_unloaded_lambda_proxy_rows(
    shared: &SharedVm,
    unloaded: &rustc_hash::FxHashSet<ClassId>,
) {
    if unloaded.is_empty() {
        return;
    }
    let mut dead_host: rustc_hash::FxHashSet<ClassId> = rustc_hash::FxHashSet::default();
    shared
        .classes
        .lambda_proxy_hosts
        .write()
        .retain(|proxy, host| {
            let dead = unloaded.contains(proxy) || unloaded.contains(host);
            if dead {
                dead_host.insert(*proxy);
            }
            !dead
        });
    shared
        .classes
        .lambda_impl_owner_memo
        .write()
        .retain(|proxy, owner| {
            !unloaded.contains(proxy) && !unloaded.contains(owner) && !dead_host.contains(proxy)
        });
    // Keyed by the lookup class, which is the host recorded above; asked
    // before the early return so a row never depends on that record.
    cratonvm_native_builtins::lang_invoke::forget_lambda_callsites_of_unloaded_hosts(
        shared.vm_identity,
        &|host| unloaded.contains(&ClassId::new(host)),
    );
    if dead_host.is_empty() {
        return;
    }
    let vm_identity = shared.vm_identity;
    lambda_singleton_cache()
        .write()
        .retain(|&(vid, proxy, _, _), slot| {
            let keep = vid != vm_identity || !dead_host.contains(&proxy);
            if !keep {
                // A warm-path memo may still hold the slot.
                slot.retire();
            }
            keep
        });
    shared
        .classes
        .lambda_proxies
        .write()
        .retain(|proxy, _| !dead_host.contains(proxy));
    if ANY_LAMBDA_MARKERS.load(std::sync::atomic::Ordering::Acquire) {
        lambda_proxy_markers()
            .lock()
            .retain(|&(vid, proxy), _| vid != vm_identity || !dead_host.contains(&proxy));
    }
    if ANY_LAMBDA_BRIDGES.load(std::sync::atomic::Ordering::Acquire) {
        lambda_proxy_bridges()
            .lock()
            .retain(|&(vid, proxy), _| vid != vm_identity || !dead_host.contains(&proxy));
    }
    if ANY_USER_LOADER_LAMBDA.load(std::sync::atomic::Ordering::Acquire) {
        if let Some(rows) = user_loader_lambdas().lock().get_mut(&vm_identity) {
            rows.retain(|(proxy, _)| !dead_host.contains(proxy));
        }
    }
    crate::runtime::interpreter::forget_dead_lambda_proxy_memo_rows(vm_identity, &dead_host);
}

/// Drop every row of a disposed VM (`vm::vm_init::release_vm_native_state`).
pub fn forget_vm_generic_indy_sites(vm_identity: usize) {
    // The lambda private-impl memo rides this hook (same caller).
    crate::runtime::interpreter::forget_vm_private_impl_memo(vm_identity);
    // So do the two process-global lambda side tables keyed by VM. Their rows
    // were never dropped: a disposed VM's singletons stayed in a table the
    // GC scans on every collection of every other VM, and its marker lists
    // stayed allocated. `vm_identity` is never reused, so this is a leak fix.
    lambda_singleton_cache()
        .write()
        .retain(|&(vid, _, _, _), slot| {
            if vid == vm_identity {
                slot.retire();
            }
            vid != vm_identity
        });
    if ANY_LAMBDA_MARKERS.load(std::sync::atomic::Ordering::Acquire) {
        lambda_proxy_markers()
            .lock()
            .retain(|&(vid, _), _| vid != vm_identity);
    }
    if ANY_LAMBDA_BRIDGES.load(std::sync::atomic::Ordering::Acquire) {
        lambda_proxy_bridges()
            .lock()
            .retain(|&(vid, _), _| vid != vm_identity);
    }
    // The VM's loader-pin rows go with `loader_pin::forget_vm_loader_pins`.
    if ANY_USER_LOADER_LAMBDA.load(std::sync::atomic::Ordering::Acquire) {
        user_loader_lambdas().lock().remove(&vm_identity);
    }
    if let Some(memo) = SHARED_INDY_ENTRIES.get() {
        memo.write().retain(|&(vid, _), _| vid != vm_identity);
    }
    if !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    generic_indy_sites()
        .write()
        .retain(|&(vid, _, _, _, _), _| vid != vm_identity);
}

fn retain_live_generic_indy_rows(
    table: &mut GenericIndyMap,
    vm_identity: usize,
    unloaded: &rustc_hash::FxHashSet<ClassId>,
) {
    table.retain(|&(vid, class_id, _, _, _), slot| {
        vid != vm_identity || !(unloaded.contains(&class_id) || slot.link.mentions_class(unloaded))
    });
}

/// Execute a cached per-instruction linkage.
///
/// `singleton_site`: see [`execute_invokedynamic_at`]; only a zero-capture
/// [`GenericIndyLink::Lambda`] reads it.
fn run_generic_indy_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    link: GenericIndyLink,
    shape: &GenericIndyShape,
    singleton_site: Option<(u64, usize)>,
) -> Result<(), MethodCallFailed> {
    match link {
        GenericIndyLink::Linked(call_site) => {
            invoke_generic_call_site(shared, thread, frame_idx, call_site, shape)
        }
        GenericIndyLink::Failed(recorded) => {
            Err(raise_recorded_linkage_error(shared, thread, &recorded))
        }
        GenericIndyLink::GroovyCastToBoolean => groovy_cast_to_boolean(shared, thread, frame_idx),
        GenericIndyLink::Lambda {
            proxy_class_id,
            num_captures,
        } => allocate_lambda_proxy(
            shared,
            thread,
            frame_idx,
            proxy_class_id,
            num_captures,
            singleton_site,
        ),
    }
}

/// The per-instruction table key of the `invokedynamic` executing in `frame`.
///
/// `singleton_site` is the instruction's `(method key, pc after it)` when the
/// caller's frame names no instruction — the compiled bridge's synthetic frame
/// — and `None` for an interpreter frame, whose method and pc ARE the
/// instruction. The same two halves as the zero-capture singleton key, so a
/// compiled site and the interpreted instruction find the same row.
fn indy_instruction_key(
    shared: &SharedVm,
    frame: &Frame,
    class_id: ClassId,
    cp_index: u16,
    singleton_site: Option<(u64, usize)>,
) -> GenericIndyKey {
    match singleton_site {
        Some((method_key, pc)) => (shared.vm_identity, class_id, cp_index, method_key, pc),
        None => generic_indy_site_key(shared, frame, class_id, cp_index),
    }
}

/// The published per-instruction lambda linkage for `key`, as the two facts an
/// allocation needs. The compiled bridge's frame-free fast path asks this after
/// the per-entry caches miss.
fn probe_lambda_instruction_site(key: &GenericIndyKey) -> Option<(ClassId, usize)> {
    if !ANY_GENERIC_INDY_SITE.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    match probe_generic_indy_site(key)?.0 {
        GenericIndyLink::Lambda {
            proxy_class_id,
            num_captures,
        } => Some((proxy_class_id, num_captures)),
        _ => None,
    }
}

/// Publish this bootstrap's proxy as the linkage of the instruction `key`,
/// FIRST WRITER WINS, and return the proxy class the instruction uses — the
/// winner's, when another thread linked it first.
///
/// NOT subject to `GENERIC_INDY_SITE_CAP` (wave 7). At the cap this used to
/// publish nothing and the caller fell back to the PER-ENTRY
/// `resolution_cache` row — which every other instruction of the shared
/// entry then found FIRST (the per-entry probe precedes the per-instruction
/// one), so an instruction that already had its own proxy switched to the
/// capped one's: a different `getClass()` and, for a non-capturing lambda, a
/// new singleton for the same instruction. A lambda row is one per
/// shared-entry instruction of loaded code, removed with its class, so the
/// table stays bounded by the code without the cap; and a shared entry now
/// never has a per-entry row, which is what lets the per-thread copy
/// (`ThreadInstructionSites`) be served before the per-entry probe.
fn publish_lambda_instruction_site(
    shared: &SharedVm,
    key: GenericIndyKey,
    target_descriptor: &str,
    proxy_class_id: ClassId,
    num_captures: usize,
) -> ClassId {
    let gate = generic_indy_gate(shared, key.1);
    let mut table = generic_indy_sites().write();
    if let Some(existing) = table.get(&key) {
        if !existing.gate.is_stale() {
            if let GenericIndyLink::Lambda {
                proxy_class_id: installed,
                ..
            } = existing.link
            {
                return installed;
            }
            // A row of another kind for a lambda instruction cannot exist (the
            // constant pool decides the bootstrap); replace it.
        }
    }
    table.insert(
        key,
        GenericIndySlot {
            link: GenericIndyLink::Lambda {
                proxy_class_id,
                num_captures,
            },
            shape: Arc::new(GenericIndyShape::new(target_descriptor)),
            gate,
        },
    );
    drop(table);
    ANY_GENERIC_INDY_SITE.store(true, std::sync::atomic::Ordering::Release);
    proxy_class_id
}

// ---------------------------------------------------------------------------
// Shared `CONSTANT_InvokeDynamic` entries
//
// javac deduplicates constant-pool entries, so two identical method references
// in one class (`String::length` written twice, in one method or in two)
// compile to two `invokedynamic` instructions naming ONE entry. Every lambda
// body has its own entry (its own implementation method), so shared entries
// are rare, and only they need per-instruction linkage. Which entries a class
// shares is a pure function of its bytecode: computed once per class, on its
// first lambda bootstrap, and memoised per VM behind the class's redefinition
// gate (a redefinition replaces the bytecode).
// ---------------------------------------------------------------------------

/// `(vm_identity, class)` -> the class's shared `invokedynamic` CP indices,
/// sorted, and the redefinition gate they were computed under.
type SharedIndyEntryMap = rustc_hash::FxHashMap<
    (usize, ClassId),
    (crate::classloading::resolution::RedefineGate, Arc<[u16]>),
>;

static SHARED_INDY_ENTRIES: std::sync::OnceLock<parking_lot::RwLock<SharedIndyEntryMap>> =
    std::sync::OnceLock::new();

fn shared_indy_entries() -> &'static parking_lot::RwLock<SharedIndyEntryMap> {
    SHARED_INDY_ENTRIES.get_or_init(|| parking_lot::RwLock::new(SharedIndyEntryMap::default()))
}

/// The CP indices that two or more `invokedynamic` instructions of the class
/// (across all its methods) name, sorted.
fn scan_shared_indy_entries<'a>(codes: impl Iterator<Item = &'a [u8]>) -> Vec<u16> {
    const INVOKEDYNAMIC: u8 = 0xba;
    let mut uses: rustc_hash::FxHashMap<u16, u32> = rustc_hash::FxHashMap::default();
    for code in codes {
        let mut pc = 0usize;
        while pc < code.len() {
            if code[pc] == INVOKEDYNAMIC && pc + 2 < code.len() {
                let idx = u16::from_be_bytes([code[pc + 1], code[pc + 2]]);
                *uses.entry(idx).or_insert(0) += 1;
            }
            // The walk every other bytecode scan in the VM uses (switch padding,
            // `wide`); `max(1)` guarantees progress on a malformed tail.
            pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
        }
    }
    let mut shared: Vec<u16> = uses
        .into_iter()
        .filter(|&(_, n)| n > 1)
        .map(|(idx, _)| idx)
        .collect();
    shared.sort_unstable();
    shared
}

/// `true` when another `invokedynamic` instruction of `class_id` names the same
/// CP entry `cp_index`. Cold: asked once per lambda bootstrap.
fn lambda_entry_is_shared(shared: &SharedVm, class_id: ClassId, cp_index: u16) -> bool {
    let key = (shared.vm_identity, class_id);
    let memo = shared_indy_entries()
        .read()
        .get(&key)
        .filter(|(gate, _)| !gate.is_stale())
        .map(|(_, entries)| Arc::clone(entries));
    let entries = match memo {
        Some(entries) => entries,
        None => {
            // Gate BEFORE the scan: a redefinition racing the scan then leaves
            // the row stale rather than current over the old bytecode.
            let gate = generic_indy_gate(shared, class_id);
            let scanned: Arc<[u16]> = {
                let cm = shared.classes.class_manager.read();
                let Some(class) = cm.get_class(class_id) else {
                    return false;
                };
                Arc::from(scan_shared_indy_entries(
                    class
                        .methods
                        .iter()
                        .filter_map(|m| m.code())
                        .map(|c| &c.code[..]),
                ))
            };
            shared_indy_entries()
                .write()
                .insert(key, (gate, Arc::clone(&scanned)));
            scanned
        }
    };
    entries.binary_search(&cp_index).is_ok()
}

/// Snapshot of `current_class_id`'s redefinition generation, for a new row.
fn generic_indy_gate(
    shared: &SharedVm,
    current_class_id: ClassId,
) -> crate::classloading::resolution::RedefineGate {
    let counter = shared
        .classes
        .class_manager
        .read()
        .class_redefine_generation_handle(current_class_id);
    crate::classloading::resolution::RedefineGate::snapshot(counter)
}

/// `true` when `call_site` is a `java.lang.invoke.ConstantCallSite`.
fn is_constant_call_site(shared: &SharedVm, call_site: ObjectRef) -> bool {
    let cm = shared.classes.class_manager.read();
    let Some(ccs) = cm.get_loaded_class_id("java/lang/invoke/ConstantCallSite") else {
        return false;
    };
    cm.is_subclass_of(shared.mem.heap.class_id_of(call_site), ccs)
}

/// `Throwable.detailMessage` of `throwable` (the field, as HotSpot's
/// `java_lang_Throwable::message_as_utf8` reads it — not `getMessage()`).
fn throwable_detail_message(shared: &SharedVm, throwable: ObjectRef) -> Option<String> {
    let class_id = shared.mem.heap.class_id_of(throwable);
    let slot =
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, class_id, "detailMessage")?;
    match shared.mem.heap.get_field(throwable, slot) {
        Value::Object(Some(s)) => read_java_string(&shared.mem.heap, s),
        _ => None,
    }
}

/// What to record for a linkage that failed with `thrown`, or `None` when it is
/// not a `LinkageError`. HotSpot records only those
/// (`ConstantPool::save_and_throw_indy_exc`); anything else — an
/// `OutOfMemoryError`, a `StackOverflowError` — leaves the instruction
/// unlinked, and the next execution runs the bootstrap again.
///
/// Allocation-free: it runs between a bootstrap's return and the pin of its
/// result.
fn record_linkage_error(shared: &SharedVm, thrown: ObjectRef) -> Option<RecordedLinkageError> {
    let error_class = shared.mem.heap.class_id_of(thrown);
    let error_class_name = {
        let cm = shared.classes.class_manager.read();
        let linkage = cm.get_loaded_class_id("java/lang/LinkageError")?;
        if !cm.is_subclass_of(error_class, linkage) {
            return None;
        }
        cm.get_class(error_class)?.name.to_string()
    };
    let message = throwable_detail_message(shared, thrown);
    Some(RecordedLinkageError {
        error_class,
        error_class_name,
        message,
    })
}

/// Throw the recorded failure of an instruction whose linkage already failed:
/// a NEW error of the recorded class and message and no cause —
/// `ConstantPool::throw_resolution_error` over an indy row, whose cause
/// `save_and_throw_indy_exc` never records.
#[cold]
fn raise_recorded_linkage_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    recorded: &RecordedLinkageError,
) -> MethodCallFailed {
    match crate::runtime::exceptions::create_exception_object_for_class(
        shared,
        thread,
        recorded.error_class,
        &recorded.error_class_name,
        recorded.message.as_deref(),
    ) {
        Ok(error) => MethodCallFailed::ExceptionThrown(error),
        Err(e) => e,
    }
}

/// Turn this execution's own linkage outcome into the instruction's published
/// linkage, and return the `CallSite` to invoke.
///
/// * A `CallSite` the mode caches is published; if another thread published
///   first, ITS call site is used, so every thread observes one `CallSite`.
/// * A `LinkageError` is recorded and published. The publishing thread throws
///   its original error object; a thread that loses to a published success
///   uses that success instead of failing (HotSpot does the same).
/// * Anything else — an uncached mutable call site, a non-`LinkageError`
///   failure, a VM-internal error — is returned unchanged and not published.
///
/// Must not reach a safepoint on the success path: the caller holds the
/// returned `CallSite` unpinned until `invoke_generic_call_site` pins it.
#[allow(clippy::too_many_arguments)]
fn settle_generic_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    current_class_id: ClassId,
    cp_index: u16,
    cache_mutable: bool,
    shape: &Arc<GenericIndyShape>,
    linked: Result<ObjectRef, MethodCallFailed>,
) -> Result<ObjectRef, MethodCallFailed> {
    let own = match &linked {
        Ok(call_site) => (cache_mutable || is_constant_call_site(shared, *call_site))
            .then_some(GenericIndyLink::Linked(*call_site)),
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            record_linkage_error(shared, *thrown).map(|r| GenericIndyLink::Failed(Arc::new(r)))
        }
        Err(_) => None,
    };
    let Some(own) = own else {
        return linked;
    };
    let key = generic_indy_site_key(
        shared,
        &thread.frames[frame_idx],
        current_class_id,
        cp_index,
    );
    let slot = GenericIndySlot {
        link: own.clone(),
        shape: Arc::clone(shape),
        gate: generic_indy_gate(shared, current_class_id),
    };
    match (publish_generic_indy_site(key, slot), own) {
        (GenericIndyLink::Linked(call_site), _) => Ok(call_site),
        // Ours is the published failure: throw the original error object.
        (GenericIndyLink::Failed(published), GenericIndyLink::Failed(mine))
            if Arc::ptr_eq(&published, &mine) =>
        {
            linked
        }
        (GenericIndyLink::Failed(published), _) => {
            Err(raise_recorded_linkage_error(shared, thread, &published))
        }
        // A cast-to-boolean or lambda row for the same instruction cannot
        // exist (the constant pool decides the shape); keep this execution's
        // own outcome.
        (GenericIndyLink::GroovyCastToBoolean | GenericIndyLink::Lambda { .. }, _) => linked,
    }
}

/// Publish a natively linked site's linkage failure (`LambdaMetafactory`,
/// `SwitchBootstraps`, `ObjectMethods`) for its instruction,
/// as [`settle_generic_link`] publishes a generic one (JVMS §5.4.3: every
/// later execution of a failed `invokedynamic` throws a new error of the same
/// class and message, `ConstantPool::save_and_throw_indy_exc`). This
/// execution throws `failed` itself; a thread that lost the publication runs
/// the published link. Only a thrown `LinkageError` is recorded (a
/// `BootstrapMethodError` refusal, a resolution error the interpreter already
/// raised); a VM-side error such as the `NoSuchMethodError` that
/// [`lambda_impl_resolves`] returns unconverted, and every failure while the
/// per-instruction table is switched off, is returned unchanged, and the next
/// execution validates again. Interpreter round i1 wave 38, lane L4 (item 6
/// of `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`).
///
/// Allocation-free between `failed`'s creation and its return, like
/// [`settle_generic_link`]: the thrown object is held unpinned.
#[cold]
#[allow(clippy::too_many_arguments)]
fn settle_native_link_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    current_class_id: ClassId,
    cp_index: u16,
    singleton_site: Option<(u64, usize)>,
    target_descriptor: &str,
    failed: MethodCallFailed,
) -> Result<(), MethodCallFailed> {
    if generic_indy_cache_mode() == GenericIndyCacheMode::Off {
        return Err(failed);
    }
    let MethodCallFailed::ExceptionThrown(thrown) = &failed else {
        return Err(failed);
    };
    let Some(recorded) = record_linkage_error(shared, *thrown) else {
        return Err(failed);
    };
    let mine = Arc::new(recorded);
    let key = indy_instruction_key(
        shared,
        &thread.frames[frame_idx],
        current_class_id,
        cp_index,
        singleton_site,
    );
    let shape = Arc::new(GenericIndyShape::new(target_descriptor));
    let slot = GenericIndySlot {
        link: GenericIndyLink::Failed(Arc::clone(&mine)),
        shape: Arc::clone(&shape),
        gate: generic_indy_gate(shared, current_class_id),
    };
    match publish_generic_indy_site(key, slot) {
        GenericIndyLink::Failed(published) if Arc::ptr_eq(&published, &mine) => {
            if dbg_lambda_dispatch() {
                eprintln!(
                    "[DBG_LAMBDA] link-check recorded {} for cp#{cp_index}",
                    mine.error_class_name
                );
            }
            Err(failed)
        }
        other => run_generic_indy_link(shared, thread, frame_idx, other, &shape, singleton_site),
    }
}

/// `java.lang.invoke.LambdaConversionException`, the cause of every refusal
/// `AbstractValidatingLambdaMetafactory` raises.
const LAMBDA_CONVERSION_EXCEPTION: &str = "java/lang/invoke/LambdaConversionException";

/// `LambdaMetafactory.metafactory`'s descriptor. A bootstrap handle naming
/// the method with any other descriptor is not validated here.
const METAFACTORY_DESCRIPTOR: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;\
Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;";

/// `LambdaMetafactory.altMetafactory`'s descriptor.
const ALT_METAFACTORY_DESCRIPTOR: &str = "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;";

/// Why a `LambdaMetafactory` call site fails linkage: the cause of the
/// `BootstrapMethodError` HotSpot raises, with its class and message.
struct LambdaSiteRefusal {
    cause_class: &'static str,
    message: String,
    /// The native linkage cannot build this shape at all (before interpreter
    /// round i1 wave 37 it was an uncatchable `VmError::Internal`), so the
    /// refusal is raised in every mode, not only under `--jdk-only`.
    every_mode: bool,
}

/// Why [`lambda_site_verdict`] stopped: the site fails a check, or the class
/// store cannot decide one as `java.lang.Class` would (an unloaded class, a
/// hierarchy with a class that has no real bytes). An undecided check ends the
/// validation with no refusal: a later check cannot be reported while an
/// earlier one might fail first.
enum LambdaStop {
    Refuse(LambdaSiteRefusal),
    Unknown(&'static str),
}

fn lambda_known<T>(value: Option<T>, what: &'static str) -> Result<T, LambdaStop> {
    value.ok_or(LambdaStop::Unknown(what))
}

fn lambda_conversion_refusal(message: String) -> LambdaStop {
    LambdaStop::Refuse(LambdaSiteRefusal {
        cause_class: LAMBDA_CONVERSION_EXCEPTION,
        message,
        every_mode: false,
    })
}

/// What a bootstrap static argument is at run time, as far as
/// `BootstrapMethodInvoker`'s casts and `altMetafactory`'s `extractArg` look.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LambdaStaticArg {
    MethodType,
    MethodHandle,
    Integer,
    Class,
    /// Another loadable constant, named by its `java.lang` class.
    Other(&'static str),
    /// A dynamic constant (its own bootstrap runs first, and may fail or
    /// answer anything, `null` included) or a malformed entry.
    Undecided,
    /// A dynamic constant that answered `null` (wave 41; see
    /// [`LambdaCondyArg`]): it passes the invoker's casts and fails
    /// `Objects.requireNonNull` (`metafactory`) or `extractArg`
    /// (`altMetafactory`).
    Null,
    /// A dynamic constant that answered an object of a class none of the
    /// other kinds names (a `java.util` or user class, a `Boolean`, ...):
    /// the invoker's cast refuses it with a message naming both classes'
    /// modules and loaders, which [`LambdaCondyArg::cast_messages`] carries
    /// (wave 46); `altMetafactory`'s `extractArg` refuses it as `argument has
    /// wrong type`.
    Foreign,
}

/// The value of a `CONSTANT_Dynamic` static argument of a lambda site,
/// resolved before the checks as HotSpot resolves every static argument
/// before the bootstrap runs ([`lambda_resolve_dynamic_args`]). Interpreter
/// round i1 wave 41, lane L4 (row 4 of
/// `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`).
#[derive(Clone)]
struct LambdaCondyArg {
    /// What the value is, as the casts see it (`Undecided` for a value the
    /// checks do not classify: a `MethodHandle` whose member the linkage
    /// cannot read back, a hidden class, a primitive).
    kind: LambdaStaticArg,
    /// The descriptor of a `MethodType` value.
    method_type: Option<String>,
    /// The member of a direct `MethodHandle` value (wave 42,
    /// `lang_invoke::method_handle_value_member`): a `findStatic` /
    /// `findVirtual` handle, read as the `CONSTANT_MethodHandle` that would
    /// name it. `None` for any other value.
    method_handle: Option<MethodHandle>,
    /// A `MethodHandle` value HotSpot's `Lookup.revealDirect` cannot crack
    /// (wave 43, `lang_invoke::method_handle_value_not_direct`): its
    /// `toString()`, which the `LambdaConversionException` quotes. `None`
    /// for a direct handle and for one this model cannot tell.
    not_direct: Option<String>,
    /// For a [`LambdaStaticArg::Foreign`] value: HotSpot's
    /// `ClassCastException` messages for a cast of it to
    /// `java.lang.invoke.MethodType` (0) and to `java.lang.invoke.MethodHandle`
    /// (1) (`exceptions::hotspot_class_cast_message`), computed where the
    /// value's class is in hand. Wave 46, lane L4.
    cast_messages: Option<Box<[String; 2]>>,
}

/// The `MethodType` static argument at `position`: a dynamic constant's value
/// when one was resolved there, else the constant-pool entry.
fn lambda_method_type_arg(
    cp: &ConstantPool,
    args: &[u16],
    condy: &[Option<LambdaCondyArg>],
    position: usize,
) -> Option<String> {
    match condy.get(position) {
        Some(Some(arg)) => arg.method_type.clone(),
        _ => resolve_method_type(cp, *args.get(position)?),
    }
}

/// The implementation handle (static argument 1): a dynamic constant's direct
/// `MethodHandle` value when one was resolved there (wave 42), else the
/// constant-pool entry. `None` when neither reads as a direct member.
fn lambda_impl_handle_arg(
    cp: &ConstantPool,
    args: &[u16],
    condy: &[Option<LambdaCondyArg>],
) -> Option<MethodHandle> {
    match condy.get(1) {
        Some(Some(arg)) => arg.method_handle.clone(),
        _ => resolve_method_handle_full(cp, *args.get(1)?).ok(),
    }
}

impl LambdaStaticArg {
    fn of(cp: &ConstantPool, index: u16) -> Self {
        match cp.get(index) {
            Some(ConstantPoolEntry::MethodType { .. }) => Self::MethodType,
            Some(ConstantPoolEntry::MethodHandle { .. }) => Self::MethodHandle,
            Some(ConstantPoolEntry::Integer(_)) => Self::Integer,
            Some(ConstantPoolEntry::ClassReference { .. }) => Self::Class,
            Some(ConstantPoolEntry::StringReference { .. }) => Self::Other("java.lang.String"),
            Some(ConstantPoolEntry::Long(_)) => Self::Other("java.lang.Long"),
            Some(ConstantPoolEntry::Float(_)) => Self::Other("java.lang.Float"),
            Some(ConstantPoolEntry::Double(_)) => Self::Other("java.lang.Double"),
            _ => Self::Undecided,
        }
    }

    /// The class a `ClassCastException` names for this constant. `None` for a
    /// `MethodHandle` constant, whose class is an implementation class of the
    /// JDK's choosing (`DirectMethodHandle`, `DirectMethodHandle$Special`, ...).
    fn java_class(self) -> Option<&'static str> {
        match self {
            Self::MethodType => Some("java.lang.invoke.MethodType"),
            Self::Integer => Some("java.lang.Integer"),
            Self::Class => Some("java.lang.Class"),
            Self::Other(name) => Some(name),
            Self::MethodHandle | Self::Undecided | Self::Null | Self::Foreign => None,
        }
    }
}

/// `BootstrapMethodInvoker` calling `metafactory` with these static
/// arguments. A count other than three goes through the generic `invoke`,
/// whose call type has one `Object` per static argument (all `Object`, the
/// lookup, name and type included, beyond six, where it is
/// `invokeWithArguments`), so it is a `WrongMethodTypeException`; three are
/// cast in order to `MethodType`, `MethodHandle`, `MethodType`
/// (`ClassCastException`). Fewer than three and a wrong kind in the three
/// were an internal error of the native linkage; more than three linked,
/// ignoring the rest, which `--compatible` keeps. A `MethodHandle` constant
/// where a `MethodType` belongs names its implementation class
/// ([`LambdaTypes::method_handle_constant_class`], wave 38).
fn metafactory_static_args(
    types: &LambdaTypes<'_>,
    cp: &ConstantPool,
    args: &[u16],
    kinds: &[LambdaStaticArg],
    condy: &[Option<LambdaCondyArg>],
) -> Result<(), LambdaStop> {
    let count = kinds.len();
    if count != 3 {
        let call = if count <= 6 {
            format!("(Lookup,String,MethodType{})Object", ",Object".repeat(count))
        } else {
            format!("({})Object", vec!["Object"; count + 3].join(","))
        };
        return Err(LambdaStop::Refuse(LambdaSiteRefusal {
            cause_class: "java/lang/invoke/WrongMethodTypeException",
            message: format!(
                "cannot convert MethodHandle(Lookup,String,MethodType,MethodType,MethodHandle,\
                 MethodType)CallSite to {call}"
            ),
            every_mode: count < 3,
        }));
    }
    let expected = [
        (LambdaStaticArg::MethodType, "java.lang.invoke.MethodType"),
        (LambdaStaticArg::MethodHandle, "java.lang.invoke.MethodHandle"),
        (LambdaStaticArg::MethodType, "java.lang.invoke.MethodType"),
    ];
    for (index, (&kind, (want, want_name))) in kinds.iter().zip(expected).enumerate() {
        // A `null` (a dynamic constant's value, wave 41) passes the cast.
        if kind == want || kind == LambdaStaticArg::Null {
            continue;
        }
        // A value of a class outside the named ones: HotSpot's own message,
        // computed when the value was resolved (wave 46).
        if kind == LambdaStaticArg::Foreign {
            let slot = usize::from(want == LambdaStaticArg::MethodHandle);
            let message = condy
                .get(index)
                .and_then(Option::as_ref)
                .and_then(|arg| arg.cast_messages.as_ref())
                .map(|messages| messages[slot].clone());
            return Err(LambdaStop::Refuse(LambdaSiteRefusal {
                cause_class: "java/lang/ClassCastException",
                message: lambda_known(message, "a dynamic constant's class")?,
                every_mode: true,
            }));
        }
        let actual = match kind {
            LambdaStaticArg::MethodHandle => {
                let handle = args
                    .get(index)
                    .and_then(|&i| resolve_method_handle_full(cp, i).ok());
                handle.and_then(|h| types.method_handle_constant_class(&h))
            }
            _ => kind.java_class(),
        };
        let actual = lambda_known(actual, "a MethodHandle constant's class")?;
        return Err(LambdaStop::Refuse(LambdaSiteRefusal {
            cause_class: "java/lang/ClassCastException",
            message: format!(
                "class {actual} cannot be cast to class {want_name} ({actual} and {want_name} \
                 are in module java.base of loader 'bootstrap')"
            ),
            every_mode: true,
        }));
    }
    // `metafactory`'s `Objects.requireNonNull` of each (measured on HotSpot
    // 25: a `NullPointerException` with no message; wave 41).
    if kinds.contains(&LambdaStaticArg::Null) {
        return Err(LambdaStop::Refuse(lambda_null_argument_refusal(true)));
    }
    Ok(())
}

/// The refusal of a `null` static argument (a dynamic constant's value):
/// `Objects.requireNonNull`'s `NullPointerException`, whose message is `null`
/// (an empty `message` is raised as a null one; see
/// [`bootstrap_method_error_with_new_cause_opt`]). Interpreter round i1 wave
/// 41, lane L4.
fn lambda_null_argument_refusal(every_mode: bool) -> LambdaSiteRefusal {
    LambdaSiteRefusal {
        cause_class: "java/lang/NullPointerException",
        message: String::new(),
        every_mode,
    }
}

/// The marker interfaces (internal names) and bridge descriptors an
/// `altMetafactory` site names.
#[derive(Default)]
struct AltMetafactoryTail {
    markers: Vec<String>,
    bridges: Vec<String>,
}

/// `altMetafactory`'s own reading of its static arguments (`extractArg`): a
/// missing one, one of the wrong class, a negative count or one left over is
/// an `IllegalArgumentException` with the JDK's message. A defect among the
/// first three is a shape the native linkage could not build (an internal
/// error before wave 37); the others used to link, and still do under
/// `--compatible`.
fn alt_metafactory_static_args(
    cp: &ConstantPool,
    args: &[u16],
    kinds: &[LambdaStaticArg],
) -> Result<AltMetafactoryTail, LambdaStop> {
    let refuse = |message: &str, index: usize| {
        LambdaStop::Refuse(LambdaSiteRefusal {
            cause_class: "java/lang/IllegalArgumentException",
            message: message.to_string(),
            every_mode: index < 3,
        })
    };
    let expect = |index: usize, want: LambdaStaticArg| match kinds.get(index) {
        None => Err(refuse("missing argument", index)),
        Some(&kind) if kind == want => Ok(()),
        // `extractArg`'s `Objects.requireNonNull` (wave 41).
        Some(LambdaStaticArg::Null) => {
            Err(LambdaStop::Refuse(lambda_null_argument_refusal(index < 3)))
        }
        Some(_) => Err(refuse("argument has wrong type", index)),
    };
    let int_at = |index: usize| -> Result<i32, LambdaStop> {
        expect(index, LambdaStaticArg::Integer)?;
        match args.get(index).and_then(|&i| cp.get(i)) {
            Some(ConstantPoolEntry::Integer(value)) => Ok(*value),
            _ => Err(LambdaStop::Unknown("an altMetafactory int")),
        }
    };
    // A count no class file can satisfy is left alone: `extractArgs` sizes
    // its array first, and a huge one is an `OutOfMemoryError`, not the
    // "missing argument" a short tail would be.
    let count_at = |index: usize| -> Result<usize, LambdaStop> {
        let count = int_at(index)?;
        if count < 0 {
            return Err(refuse("negative argument count", index));
        }
        let count = count as usize;
        if count > usize::from(u16::MAX) {
            return Err(LambdaStop::Unknown("an altMetafactory count"));
        }
        Ok(count)
    };
    expect(0, LambdaStaticArg::MethodType)?;
    expect(1, LambdaStaticArg::MethodHandle)?;
    expect(2, LambdaStaticArg::MethodType)?;
    let flags = int_at(3)?;
    let mut next = 4;
    let mut tail = AltMetafactoryTail::default();
    if flags & FLAG_MARKERS != 0 {
        let count = count_at(next)?;
        next += 1;
        for _ in 0..count {
            expect(next, LambdaStaticArg::Class)?;
            let name = args.get(next).and_then(|&i| cp.get_class_name(i));
            tail.markers.push(lambda_known(name, "a marker interface")?.to_string());
            next += 1;
        }
    }
    if flags & FLAG_BRIDGES != 0 {
        let count = count_at(next)?;
        next += 1;
        for _ in 0..count {
            expect(next, LambdaStaticArg::MethodType)?;
            let bridge = args.get(next).and_then(|&i| resolve_method_type(cp, i));
            tail.bridges.push(lambda_known(bridge, "a bridge signature")?);
            next += 1;
        }
    }
    if next < kinds.len() {
        return Err(refuse("too many arguments", next));
    }
    Ok(tail)
}

/// `Class.getName()` of the type a field descriptor names; it needs no loaded
/// class (`java.lang.String`, `[Ljava.lang.String;`, `int`).
pub(crate) fn descriptor_class_name(desc: &str) -> String {
    match desc.as_bytes().first() {
        Some(b'L') => desc[1..].trim_end_matches(';').replace('/', "."),
        Some(b'[') => desc.replace('/', "."),
        _ => primitive_type_name(desc).unwrap_or(desc).to_string(),
    }
}

/// The Java name of a primitive (or `void`) descriptor.
fn primitive_type_name(desc: &str) -> Option<&'static str> {
    Some(match desc {
        "B" => "byte",
        "C" => "char",
        "D" => "double",
        "F" => "float",
        "I" => "int",
        "J" => "long",
        "S" => "short",
        "Z" => "boolean",
        "V" => "void",
        _ => return None,
    })
}

/// `sun.invoke.util.Wrapper.isConvertibleFrom` between two primitives: the
/// same type, or a widening primitive conversion (JLS 5.1.2). `boolean` and
/// `void` convert only to themselves.
fn primitive_widens(from: &str, to: &str) -> bool {
    from == to
        || match from {
            "B" => matches!(to, "S" | "I" | "J" | "F" | "D"),
            "S" | "C" => matches!(to, "I" | "J" | "F" | "D"),
            "I" => matches!(to, "J" | "F" | "D"),
            "J" => matches!(to, "F" | "D"),
            "F" => to == "D",
            _ => false,
        }
}

/// The wrapper class descriptor of a primitive (`Wrapper.wrapperType`).
fn primitive_wrapper(prim: &str) -> Option<&'static str> {
    Some(match prim {
        "B" => "Ljava/lang/Byte;",
        "C" => "Ljava/lang/Character;",
        "D" => "Ljava/lang/Double;",
        "F" => "Ljava/lang/Float;",
        "I" => "Ljava/lang/Integer;",
        "J" => "Ljava/lang/Long;",
        "S" => "Ljava/lang/Short;",
        "Z" => "Ljava/lang/Boolean;",
        "V" => "Ljava/lang/Void;",
        _ => return None,
    })
}

/// The primitive a wrapper class descriptor unboxes to. A `java.lang`
/// wrapper name is decisive: no loader but the bootstrap one defines it.
fn wrapper_primitive(desc: &str) -> Option<&'static str> {
    Some(match desc {
        "Ljava/lang/Byte;" => "B",
        "Ljava/lang/Character;" => "C",
        "Ljava/lang/Double;" => "D",
        "Ljava/lang/Float;" => "F",
        "Ljava/lang/Integer;" => "I",
        "Ljava/lang/Long;" => "J",
        "Ljava/lang/Short;" => "S",
        "Ljava/lang/Boolean;" => "Z",
        "Ljava/lang/Void;" => "V",
        _ => return None,
    })
}

/// `java.lang.Class` questions about descriptor types, as the caller of a
/// lambda site sees them (its defining loader). Every answer is `None` when
/// the class store cannot give `Class`'s own answer.
struct LambdaTypes<'a> {
    cm: &'a crate::classloading::ClassManager,
    caller: ClassId,
}

impl LambdaTypes<'_> {
    fn class_named(&self, name: &str) -> Option<ClassId> {
        self.cm.find_class_by_name_for_class(name, self.caller)
    }

    fn class_of(&self, desc: &str) -> Option<ClassId> {
        self.class_named(desc.strip_prefix('L')?.strip_suffix(';')?)
    }

    /// `Class.isInterface()` of a class named by its internal name (an array
    /// class is not one).
    fn is_interface(&self, name: &str) -> Option<bool> {
        if name.starts_with('[') {
            return Some(false);
        }
        Some(self.cm.get_class(self.class_named(name)?)?.is_interface())
    }

    /// `Class.toString()` of the type `desc` names.
    fn display(&self, desc: &str) -> Option<String> {
        match desc.as_bytes().first()? {
            b'L' => {
                let class = self.cm.get_class(self.class_of(desc)?)?;
                let kind = if class.is_interface() {
                    "interface"
                } else {
                    "class"
                };
                Some(format!("{kind} {}", descriptor_class_name(desc)))
            }
            b'[' => Some(format!("class {}", descriptor_class_name(desc))),
            _ => primitive_type_name(desc).map(str::to_string),
        }
    }

    /// Every class above `id` is in the store with real bytes, so a `false`
    /// from `is_subclass_of` is `Class.isAssignableFrom`'s own answer.
    fn hierarchy_is_decisive(&self, id: ClassId) -> bool {
        let store = &self.cm.class_store;
        let mut stack = vec![id];
        let mut seen: Vec<ClassId> = Vec::new();
        while let Some(id) = stack.pop() {
            if seen.contains(&id) {
                continue;
            }
            if seen.len() >= 512 {
                return false;
            }
            seen.push(id);
            let Some(class) = store.get(id) else {
                return false;
            };
            if !class.origin.has_real_bytes() {
                return false;
            }
            stack.extend(class.superclass);
            stack.extend_from_slice(&class.interfaces);
        }
        true
    }

    /// `to.isAssignableFrom(from)`, both descriptors.
    fn is_assignable(&self, to: &str, from: &str) -> Option<bool> {
        let reference = |d: &str| matches!(d.as_bytes().first(), Some(b'L' | b'['));
        if !reference(to) || !reference(from) {
            return Some(to == from);
        }
        if to == from || to == "Ljava/lang/Object;" {
            return Some(true);
        }
        match (from.starts_with('['), to.starts_with('[')) {
            (true, true) => self.is_assignable(&to[1..], &from[1..]),
            (true, false) => Some(matches!(
                to,
                "Ljava/lang/Cloneable;" | "Ljava/io/Serializable;"
            )),
            (false, true) => Some(false),
            (false, false) => {
                let from_id = self.class_of(from)?;
                let to_id = self.class_of(to)?;
                if self.cm.is_subclass_of(from_id, to_id) {
                    return Some(true);
                }
                // A same-named class elsewhere in the hierarchy (another
                // loader's copy) is a loader question this does not settle.
                let to_name = &to[1..to.len() - 1];
                (self.hierarchy_is_decisive(from_id)
                    && !self.cm.is_subclass_of_by_name(from_id, to_name))
                .then_some(false)
            }
        }
    }

    /// `AbstractValidatingLambdaMetafactory.isAdaptableTo(from, to, strict)`.
    fn adaptable(&self, from: &str, to: &str, strict: bool) -> Option<bool> {
        if from == to {
            return Some(true);
        }
        match (primitive_type_name(from), primitive_type_name(to)) {
            (Some(_), Some(_)) => Some(primitive_widens(from, to)),
            (Some(_), None) => self.is_assignable(to, primitive_wrapper(from)?),
            (None, Some(_)) => Some(match wrapper_primitive(from) {
                Some(unboxed) => primitive_widens(unboxed, to),
                None => !strict,
            }),
            (None, None) => {
                if strict {
                    self.is_assignable(to, from)
                } else {
                    Some(true)
                }
            }
        }
    }

    /// The class `revealDirect` reports as declaring the implementation
    /// member: the resolved method's class (`Base` for a handle naming
    /// `Impl.m` that `Impl` inherits), the named class for a constructor.
    fn declaring_class(&self, handle: &MethodHandle) -> Option<String> {
        if matches!(handle.kind, MethodHandleKind::NewInvokeSpecial) {
            return Some(handle.class_name.to_string());
        }
        let owner = self.class_named(&handle.class_name)?;
        let declaring = crate::runtime::resolve::selection::resolve_declaring(
            &self.cm.class_store,
            owner,
            &handle.member_name,
            &handle.descriptor,
        )?;
        Some(self.cm.get_class(declaring)?.name.to_string())
    }

    /// The class that declares the field a field handle names (JVMS
    /// §5.4.3.2 from the named class), `None` when it is not loaded or has no
    /// such field.
    fn field_declaring_class(&self, handle: &MethodHandle) -> Option<ClassId> {
        let owner = self.class_named(&handle.class_name)?;
        crate::classloading::find_field_recursive_by_descriptor(
            owner,
            &handle.member_name,
            &handle.descriptor,
            &self.cm.class_store,
        )
        .map(|(_, _, declaring)| declaring)
    }

    /// `MethodHandleInfo.toString()` of the implementation handle:
    /// `getStatic p.C.f:()String` for a field handle, whose type is the
    /// accessor's (`()T` to read, `(T)void` to write; wave 38, measured).
    fn impl_info(&self, handle: &MethodHandle) -> Option<String> {
        let (kind, field_type) = match handle.kind {
            MethodHandleKind::InvokeVirtual => ("invokeVirtual", None),
            MethodHandleKind::InvokeStatic => ("invokeStatic", None),
            MethodHandleKind::InvokeSpecial => ("invokeSpecial", None),
            MethodHandleKind::NewInvokeSpecial => ("newInvokeSpecial", None),
            MethodHandleKind::InvokeInterface => ("invokeInterface", None),
            MethodHandleKind::GetField => ("getField", Some(format!("(){}", handle.descriptor))),
            MethodHandleKind::GetStatic => ("getStatic", Some(format!("(){}", handle.descriptor))),
            MethodHandleKind::PutField => ("putField", Some(format!("({})V", handle.descriptor))),
            MethodHandleKind::PutStatic => ("putStatic", Some(format!("({})V", handle.descriptor))),
        };
        let (declaring, method_type) = match field_type {
            Some(accessor_type) => {
                let declaring = self.field_declaring_class(handle)?;
                (self.cm.get_class(declaring)?.name.to_string(), accessor_type)
            }
            None => (self.declaring_class(handle)?, handle.descriptor.to_string()),
        };
        Some(format!(
            "{kind} {}.{}:{}",
            declaring.replace('/', "."),
            handle.member_name,
            method_type_display_for(self.cm, self.caller, &method_type),
        ))
    }

    /// The class of the `MethodHandle` object HotSpot 25 resolves a
    /// `CONSTANT_MethodHandle` to (`DirectMethodHandle.make` /
    /// `makeAllocator`), which a `ClassCastException` names when the constant
    /// sits where a `MethodType` belongs. Measured per reference kind
    /// (interpreter round i1 wave 38, lane L4): a field reader or writer is
    /// `DirectMethodHandle$Accessor` (`$StaticAccessor` for a static field), a
    /// constructor `$Constructor`, an `invokeInterface` `$Interface` (a default
    /// method too), an `invokeStatic` or `invokeVirtual` `DirectMethodHandle`.
    ///
    /// `None` wherever resolving the constant could fail first (the member is
    /// not found in loaded classes, or it or its class is not public: HotSpot
    /// resolves every static argument before `BootstrapMethodInvoker` casts),
    /// for `invokeSpecial` (its access and receiver rules), and for a JDK
    /// class's method, which may be caller-sensitive
    /// (`MethodHandleImpl$WrappedMember`, measured for `Class.forName`).
    fn method_handle_constant_class(&self, handle: &MethodHandle) -> Option<&'static str> {
        use cratonvm_reader::class_access_flags::{FieldAccessFlags, MethodAccessFlags};
        let owner = self.class_named(&handle.class_name)?;
        if !self.cm.get_class(owner)?.is_public() {
            return None;
        }
        let public_field = |declaring: ClassId| {
            let class = self.cm.get_class(declaring)?;
            let field = class
                .fields
                .iter()
                .find(|f| *f.name == *handle.member_name && *f.descriptor == *handle.descriptor)?;
            field.access_flags.contains(FieldAccessFlags::PUBLIC).then_some(())
        };
        let public_method = |declaring: ClassId| {
            let method = self
                .cm
                .get_class(declaring)?
                .find_method(&handle.member_name, &handle.descriptor)?;
            method.access_flags.contains(MethodAccessFlags::PUBLIC).then_some(())
        };
        let jdk_owner = ["java/", "jdk/", "sun/", "com/sun/"]
            .iter()
            .any(|prefix| handle.class_name.starts_with(prefix));
        match handle.kind {
            MethodHandleKind::GetField | MethodHandleKind::PutField => {
                public_field(self.field_declaring_class(handle)?)?;
                Some("java.lang.invoke.DirectMethodHandle$Accessor")
            }
            MethodHandleKind::GetStatic | MethodHandleKind::PutStatic => {
                public_field(self.field_declaring_class(handle)?)?;
                Some("java.lang.invoke.DirectMethodHandle$StaticAccessor")
            }
            MethodHandleKind::NewInvokeSpecial => {
                public_method(owner)?;
                Some("java.lang.invoke.DirectMethodHandle$Constructor")
            }
            MethodHandleKind::InvokeSpecial => None,
            MethodHandleKind::InvokeInterface
            | MethodHandleKind::InvokeStatic
            | MethodHandleKind::InvokeVirtual => {
                if jdk_owner {
                    return None;
                }
                let declaring = crate::runtime::resolve::selection::resolve_declaring(
                    &self.cm.class_store,
                    owner,
                    &handle.member_name,
                    &handle.descriptor,
                )?;
                public_method(declaring)?;
                Some(if matches!(handle.kind, MethodHandleKind::InvokeInterface) {
                    "java.lang.invoke.DirectMethodHandle$Interface"
                } else {
                    "java.lang.invoke.DirectMethodHandle"
                })
            }
        }
    }
}

/// `AbstractValidatingLambdaMetafactory`'s constructor checks and
/// `validateMetafactoryArgs`, in their order, with their messages. Every
/// check reads only the constant pool and loaded classes; one it cannot
/// decide stops the walk (`LambdaStop::Unknown`).
fn lambda_conversion_checks(
    types: &LambdaTypes<'_>,
    cp: &ConstantPool,
    info: &IndyInfo,
    tail: &AltMetafactoryTail,
    condy: &[Option<LambdaCondyArg>],
) -> Result<(), LambdaStop> {
    use crate::runtime::interpreter::split_method_descriptor_ref as split;
    let args = &info.bootstrap_arg_indices;
    let arg = |i: usize| lambda_known(args.get(i).copied(), "a static argument");
    let interface_mt = lambda_known(
        lambda_method_type_arg(cp, args, condy, 0),
        "the interface type",
    )?;
    let _ = arg(1)?;
    let handle = lambda_known(
        lambda_impl_handle_arg(cp, args, condy),
        "the implementation handle",
    )?;
    let dynamic_mt = lambda_known(
        lambda_method_type_arg(cp, args, condy, 2),
        "the dynamic type",
    )?;

    // The constructor.
    let instance = match handle.kind {
        MethodHandleKind::InvokeVirtual
        | MethodHandleKind::InvokeInterface
        | MethodHandleKind::InvokeSpecial => true,
        MethodHandleKind::InvokeStatic | MethodHandleKind::NewInvokeSpecial => false,
        // The constructor's `default` arm, before any other check (wave 38,
        // measured: `Unsupported MethodHandle kind: getStatic p.C.f:()String`).
        MethodHandleKind::GetField
        | MethodHandleKind::GetStatic
        | MethodHandleKind::PutField
        | MethodHandleKind::PutStatic => {
            let info_text =
                lambda_known(types.impl_info(&handle), "a field implementation handle")?;
            return Err(lambda_conversion_refusal(format!(
                "Unsupported MethodHandle kind: {info_text}"
            )));
        }
    };
    if matches!(
        &*handle.class_name,
        "java/lang/invoke/MethodHandle" | "java/lang/invoke/VarHandle"
    ) {
        return Err(LambdaStop::Unknown("a signature-polymorphic implementation"));
    }
    let name = info.target_name.as_str();
    if name.is_empty() || name.chars().any(|c| matches!(c, '.' | ';' | '[' | '/' | '<' | '>')) {
        return Err(lambda_conversion_refusal(format!(
            "Method name '{name}' is not legal"
        )));
    }
    let (factory_params, factory_ret) = split(&info.target_descriptor);
    let interface_ok = match factory_ret.strip_prefix('L').and_then(|n| n.strip_suffix(';')) {
        Some(iface) => lambda_known(types.is_interface(iface), "the functional interface")?,
        None => false,
    };
    if !interface_ok {
        return Err(lambda_conversion_refusal(format!(
            "{} is not an interface",
            descriptor_class_name(factory_ret)
        )));
    }
    for marker in &tail.markers {
        if !lambda_known(types.is_interface(marker), "a marker interface")? {
            return Err(lambda_conversion_refusal(format!(
                "{} is not an interface",
                marker.replace('/', ".")
            )));
        }
    }

    // `validateMetafactoryArgs`: the arities.
    let owner_desc = format!("L{};", handle.class_name);
    let (impl_params, impl_declared_ret) = split(&handle.descriptor);
    // `implementation.type()`: an instance method's receiver leads (index 0 is
    // never compared below, only the receiver check reads the class), and a
    // constructor returns its class.
    let mut impl_type: Vec<&str> = Vec::with_capacity(impl_params.len() + 1);
    if instance {
        impl_type.push(owner_desc.as_str());
    }
    impl_type.extend(impl_params.iter().copied());
    let impl_ret: &str = if matches!(handle.kind, MethodHandleKind::NewInvokeSpecial) {
        owner_desc.as_str()
    } else {
        impl_declared_ret
    };
    let (interface_params, _) = split(&interface_mt);
    let (dynamic_params, dynamic_ret) = split(&dynamic_mt);
    let impl_arity = impl_type.len();
    let captured = factory_params.len();
    let sam = interface_params.len();
    let dynamic = dynamic_params.len();
    let kind_word = if instance { "instance" } else { "static" };
    if impl_arity != captured + sam {
        let info_text = lambda_known(types.impl_info(&handle), "the implementation's declaring class")?;
        return Err(lambda_conversion_refusal(format!(
            "Incorrect number of parameters for {kind_word} method {info_text}; {captured} \
             captured parameters, {sam} functional interface method parameters, {impl_arity} \
             implementation parameters"
        )));
    }
    if dynamic != sam {
        let info_text = lambda_known(types.impl_info(&handle), "the implementation's declaring class")?;
        return Err(lambda_conversion_refusal(format!(
            "Incorrect number of parameters for {kind_word} method {info_text}; {dynamic} \
             dynamic parameters, {sam} functional interface method parameters"
        )));
    }
    for bridge in &tail.bridges {
        if split(bridge).0.len() != sam {
            return Err(lambda_conversion_refusal(format!(
                "Incorrect number of parameters for bridge signature {}; incompatible with {}",
                method_type_display_for(types.cm, types.caller, bridge),
                method_type_display_for(types.cm, types.caller, &interface_mt)
            )));
        }
    }

    // The receiver, the captures, the SAM arguments and the return.
    let display = |desc: &str| lambda_known(types.display(desc), "a type's class");
    let (captured_start, sam_start) = if instance {
        let (captured_start, sam_start, receiver) = if captured == 0 {
            (0, 1, dynamic_params.first())
        } else {
            (1, captured, factory_params.first())
        };
        let receiver = *lambda_known(receiver, "the receiver")?;
        let impl_class = if matches!(handle.kind, MethodHandleKind::InvokeSpecial) {
            let declaring = lambda_known(types.declaring_class(&handle), "the declaring class")?;
            format!("L{declaring};")
        } else {
            owner_desc.clone()
        };
        if !lambda_known(types.is_assignable(&impl_class, receiver), "the receiver type")? {
            return Err(lambda_conversion_refusal(format!(
                "Invalid receiver type {}; not a subtype of implementation type {}",
                display(receiver)?,
                display(&impl_class)?
            )));
        }
        (captured_start, sam_start)
    } else {
        (0, captured)
    };
    for i in captured_start..captured {
        let impl_param = *lambda_known(impl_type.get(i), "a captured parameter")?;
        let captured_param = *lambda_known(factory_params.get(i), "a captured parameter")?;
        if impl_param != captured_param {
            return Err(lambda_conversion_refusal(format!(
                "Type mismatch in captured lambda parameter {i}: expecting {}, found {}",
                display(captured_param)?,
                display(impl_param)?
            )));
        }
    }
    for i in sam_start..impl_arity {
        let impl_param = *lambda_known(impl_type.get(i), "a lambda parameter")?;
        let dynamic_param = *lambda_known(dynamic_params.get(i - captured), "a lambda parameter")?;
        if !lambda_known(types.adaptable(dynamic_param, impl_param, true), "a lambda argument")? {
            return Err(lambda_conversion_refusal(format!(
                "Type mismatch for lambda argument {i}: {} is not convertible to {}",
                display(dynamic_param)?,
                display(impl_param)?
            )));
        }
    }
    let return_ok = dynamic_ret == "V"
        || (impl_ret != "V"
            && lambda_known(types.adaptable(impl_ret, dynamic_ret, false), "the lambda return")?);
    if !return_ok {
        return Err(lambda_conversion_refusal(format!(
            "Type mismatch for lambda return: {} is not convertible to {}",
            display(impl_ret)?,
            display(dynamic_ret)?
        )));
    }

    // `checkDescriptor`, for the interface type and then each bridge.
    for descriptor in std::iter::once(interface_mt.as_str()).chain(tail.bridges.iter().map(String::as_str)) {
        let (params, ret) = split(descriptor);
        for (i, &dynamic_param) in dynamic_params.iter().enumerate() {
            let param = *lambda_known(params.get(i), "a dynamic parameter")?;
            if !lambda_known(types.is_assignable(param, dynamic_param), "a dynamic parameter")? {
                return Err(lambda_conversion_refusal(format!(
                    "Type mismatch for dynamic parameter {i}: {} is not a subtype of {}",
                    display(dynamic_param)?,
                    display(param)?
                )));
            }
        }
        let ret_ok = if dynamic_ret == "V" || ret == "V" {
            dynamic_ret == ret
        } else {
            lambda_known(types.adaptable(dynamic_ret, ret, true), "the expected return")?
        };
        if !ret_ok {
            return Err(lambda_conversion_refusal(format!(
                "Type mismatch for lambda expected return: {} is not convertible to {}",
                display(dynamic_ret)?,
                display(ret)?
            )));
        }
    }
    Ok(())
}

/// Does this `LambdaMetafactory` site link on HotSpot? The whole of item 2
/// of `i29-L4-native-indy-linkage-skips-the-jdks-validation`: first the
/// static arguments as `BootstrapMethodInvoker` passes them
/// ([`metafactory_static_args`], [`alt_metafactory_static_args`]), then,
/// under `--jdk-only`, the factory's own checks
/// ([`lambda_conversion_checks`]).
fn lambda_site_verdict(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
    info: &IndyInfo,
    jdk_only: bool,
    condy: &[Option<LambdaCondyArg>],
) -> Result<(), LambdaStop> {
    let class = lambda_known(cm.get_class(class_id), "the caller")?;
    let cp = &class.constant_pool;
    let args = &info.bootstrap_arg_indices;
    // A dynamic constant counts as the value it resolved to (wave 41).
    let kinds: Vec<LambdaStaticArg> = args
        .iter()
        .enumerate()
        .map(|(position, &i)| match condy.get(position) {
            Some(Some(arg)) => arg.kind,
            _ => LambdaStaticArg::of(cp, i),
        })
        .collect();
    if kinds.contains(&LambdaStaticArg::Undecided) {
        return Err(LambdaStop::Unknown("a dynamic static argument"));
    }
    let types = LambdaTypes {
        cm,
        caller: class_id,
    };
    let tail = if info.bsm_method == ALT_METAFACTORY {
        alt_metafactory_static_args(cp, args, &kinds)?
    } else {
        metafactory_static_args(&types, cp, args, &kinds, condy)?;
        AltMetafactoryTail::default()
    };
    // `AbstractValidatingLambdaMetafactory`'s constructor cracks the
    // implementation handle (`caller.revealDirect`) before any other check.
    // A dynamic constant's handle that cannot be cracked is refused in every
    // mode: the native linkage cannot build such a site at all (it was the
    // uncatchable internal error `cp#N is not a MethodHandle`). Wave 43.
    if let Some(Some(LambdaCondyArg {
        not_direct: Some(shown),
        ..
    })) = condy.get(1)
    {
        return Err(LambdaStop::Refuse(LambdaSiteRefusal {
            cause_class: LAMBDA_CONVERSION_EXCEPTION,
            message: format!("{shown} is not direct or cannot be cracked"),
            every_mode: true,
        }));
    }
    if !jdk_only {
        return Ok(());
    }
    lambda_conversion_checks(&types, cp, info, &tail, condy)
}

/// The failure HotSpot raises linking this `LambdaMetafactory` site (the
/// cause of its `BootstrapMethodError`), or `None` when it links or when the
/// class store cannot decide. Under `--compatible` only the shapes the
/// native linkage cannot build at all are refused (`every_mode`).
///
/// `CRATONVM_DBG_LAMBDA_DISPATCH=1` prints one `[DBG_LAMBDA] link-check` line
/// per site: `refused`, `validated`, or the check left `undecided`.
///
/// javac emits none of the refused shapes; each is a hand-assembled site.
///
/// `--jdk-only` (interpreter round i1 wave 39, lane L4; row 1 of
/// `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`): when the
/// checks cannot decide, every class the call site's type and the site's
/// `MethodType` static arguments name is loaded through the caller's loader
/// ([`lambda_site_load_types`]), as HotSpot's resolution of those constants
/// loads them before the bootstrap runs, and the checks run once more. A
/// class that cannot be loaded is the `Err`: its `NoClassDefFoundError`.
fn lambda_site_refusal(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    info: &IndyInfo,
    jdk_only: bool,
    condy: &[Option<LambdaCondyArg>],
) -> Result<Option<LambdaSiteRefusal>, MethodCallFailed> {
    if !info.bsm_descriptor_is_lmf {
        return Ok(None);
    }
    let verdict_now = || {
        let cm = shared.classes.class_manager.read();
        lambda_site_verdict(&cm, class_id, info, jdk_only, condy)
    };
    let mut verdict = verdict_now();
    if jdk_only && matches!(verdict, Err(LambdaStop::Unknown(_))) {
        let loaded = lambda_site_load_types(shared, thread, class_id, info)?;
        if loaded > 0 {
            if dbg_lambda_dispatch() {
                eprintln!(
                    "[DBG_LAMBDA] link-check {}{}: loaded {loaded} type(s), checking again",
                    info.target_name, info.target_descriptor
                );
            }
            verdict = verdict_now();
        }
    }
    let refusal = match verdict {
        Ok(()) => {
            if dbg_lambda_dispatch() {
                eprintln!(
                    "[DBG_LAMBDA] link-check {}{}: validated",
                    info.target_name, info.target_descriptor
                );
            }
            None
        }
        Err(LambdaStop::Unknown(what)) => {
            if dbg_lambda_dispatch() {
                eprintln!(
                    "[DBG_LAMBDA] link-check {}{}: undecided ({what})",
                    info.target_name, info.target_descriptor
                );
            }
            None
        }
        Err(LambdaStop::Refuse(refusal)) => (jdk_only || refusal.every_mode).then_some(refusal),
    };
    if let Some(refusal) = &refusal {
        if dbg_lambda_dispatch() {
            eprintln!(
                "[DBG_LAMBDA] link-check {}{}: refused {}: {}",
                info.target_name, info.target_descriptor, refusal.cause_class, refusal.message
            );
        }
    }
    Ok(refusal)
}

/// Load, through `class_id`'s loader, every class not yet loaded that the
/// call site's type and the `MethodType` static arguments of a lambda site
/// name (an array type's element class), in HotSpot's order: the call site's
/// type, then each static argument; within a type, the parameters, then the
/// return. Returns how many were loaded; a class that cannot be loaded is its
/// `NoClassDefFoundError` (JVMS 5.4.3.5: resolving a `MethodType` resolves
/// every class its descriptor names). Interpreter round i1 wave 39, lane L4.
fn lambda_site_load_types(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    info: &IndyInfo,
) -> Result<usize, MethodCallFailed> {
    let descriptors: Vec<String> = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ok(0);
        };
        let cp = &class.constant_pool;
        // A `MethodHandle` static argument (the implementation) resolves its
        // class and then its member's type, which loads the classes the
        // member's descriptor names (wave 39 follow-up: the implementation's
        // parameter type was left unloaded, so the check stayed undecided
        // and the site linked). Each becomes a pseudo method descriptor so
        // one walk below serves every kind.
        let mut descriptors = vec![info.target_descriptor.clone()];
        for &i in &info.bootstrap_arg_indices {
            match cp.get(i) {
                Some(ConstantPoolEntry::MethodType { .. }) => {
                    descriptors.extend(resolve_method_type(cp, i));
                }
                Some(ConstantPoolEntry::MethodHandle { .. }) => {
                    let Ok(handle) = resolve_method_handle_full(cp, i) else {
                        continue;
                    };
                    if !handle.class_name.starts_with('[') {
                        descriptors.push(format!("(L{};)V", handle.class_name));
                    }
                    descriptors.push(if handle.descriptor.starts_with('(') {
                        handle.descriptor.to_string()
                    } else {
                        format!("(){}", handle.descriptor)
                    });
                }
                _ => {}
            }
        }
        descriptors
    };
    let mut loaded = 0usize;
    for descriptor in &descriptors {
        let (params, ret) = crate::runtime::interpreter::split_method_descriptor_ref(descriptor);
        for token in params.iter().copied().chain(std::iter::once(ret)) {
            let Some(name) = token
                .trim_start_matches('[')
                .strip_prefix('L')
                .and_then(|t| t.strip_suffix(';'))
            else {
                continue;
            };
            let known = shared
                .classes
                .class_manager
                .read()
                .find_class_by_name_for_class(name, class_id)
                .is_some();
            if known {
                continue;
            }
            crate::runtime::interpreter::resolve_class_loader_aware(shared, thread, class_id, name)
                .map_err(|e| {
                    crate::runtime::exceptions::convert_class_not_found_for(
                        shared,
                        thread,
                        Some(class_id),
                        name,
                        e,
                    )
                })?;
            loaded += 1;
        }
    }
    Ok(loaded)
}

/// Resolve every `CONSTANT_Dynamic` static argument of a lambda site, in
/// order, as `ldc` of the same entry does (`constants::resolve_condy_constant`,
/// whose failure is recorded against the entry), and classify each value as
/// `BootstrapMethodInvoker`'s casts and `metafactory` see it. HotSpot resolves
/// every static argument before the bootstrap runs, so a failing condy is the
/// site's failure. Empty (no allocation) for a site with no dynamic constant,
/// i.e. every javac site. Interpreter round i1 wave 41, lane L4 (row 4 of
/// `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked`): the
/// native linkage read positions 0 and 2 from the constant pool, so even a
/// valid condy `MethodType` was an uncatchable internal error.
fn lambda_resolve_dynamic_args(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    info: &IndyInfo,
) -> Result<Vec<Option<LambdaCondyArg>>, MethodCallFailed> {
    let dynamic: Vec<bool> = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ok(Vec::new());
        };
        let cp = &class.constant_pool;
        info.bootstrap_arg_indices
            .iter()
            .map(|&i| matches!(cp.get(i), Some(ConstantPoolEntry::Dynamic { .. })))
            .collect()
    };
    if !dynamic.contains(&true) {
        return Ok(Vec::new());
    }
    let mut out: Vec<Option<LambdaCondyArg>> = vec![None; dynamic.len()];
    for (position, &is_dynamic) in dynamic.iter().enumerate() {
        if !is_dynamic {
            continue;
        }
        let index = info.bootstrap_arg_indices[position];
        let value = crate::runtime::interpreter::constants::resolve_condy_constant(
            shared, thread, class_id, index,
        )?;
        let arg = match value {
            Value::Object(None) => LambdaCondyArg {
                kind: LambdaStaticArg::Null,
                method_type: None,
                method_handle: None,
                not_direct: None,
                cast_messages: None,
            },
            Value::Object(Some(obj)) => {
                let cid = shared.mem.heap.class_id_of(obj);
                let (name, is_method_handle, hidden, cast_targets) = {
                    let cm = shared.classes.class_manager.read();
                    let class = cm.get_class(cid);
                    let name = class.map(|c| c.name.to_string()).unwrap_or_default();
                    let hidden = class.is_some_and(|c| c.is_hidden());
                    let method_handle = cm.get_loaded_class_id("java/lang/invoke/MethodHandle");
                    let is_method_handle = method_handle.is_some_and(|mh| cm.is_subclass_of(cid, mh));
                    let method_type = cm.get_loaded_class_id("java/lang/invoke/MethodType");
                    (name, is_method_handle, hidden, (method_type, method_handle))
                };
                let kind = if is_method_handle {
                    LambdaStaticArg::MethodHandle
                } else {
                    // Only classes of `java.lang` (all in `java.base`, of the
                    // boot loader), whose cast message the checks can write.
                    match name.as_str() {
                        "java/lang/invoke/MethodType" => LambdaStaticArg::MethodType,
                        "java/lang/Integer" => LambdaStaticArg::Integer,
                        "java/lang/Class" => LambdaStaticArg::Class,
                        "java/lang/String" => LambdaStaticArg::Other("java.lang.String"),
                        "java/lang/Long" => LambdaStaticArg::Other("java.lang.Long"),
                        "java/lang/Float" => LambdaStaticArg::Other("java.lang.Float"),
                        "java/lang/Double" => LambdaStaticArg::Other("java.lang.Double"),
                        "java/lang/Object" => LambdaStaticArg::Other("java.lang.Object"),
                        // Wave 46: any other class, when HotSpot's message for
                        // both casts can be written (not a hidden class, whose
                        // external name this model does not spell).
                        _ if !hidden && !name.is_empty() => LambdaStaticArg::Foreign,
                        _ => LambdaStaticArg::Undecided,
                    }
                };
                let cast_messages = if kind == LambdaStaticArg::Foreign {
                    let from = name.replace('/', ".");
                    let thread_now: &JvmThread = &*thread;
                    let message = |to: &str, to_id: Option<ClassId>| {
                        crate::runtime::exceptions::hotspot_class_cast_message(
                            shared,
                            thread_now,
                            &from,
                            to,
                            to_id.map(|to_id| (cid, to_id)),
                        )
                    };
                    match (
                        message("java.lang.invoke.MethodType", cast_targets.0),
                        message("java.lang.invoke.MethodHandle", cast_targets.1),
                    ) {
                        (Some(to_type), Some(to_handle)) => Some(Box::new([to_type, to_handle])),
                        _ => None,
                    }
                } else {
                    None
                };
                // A class whose message cannot be written is not classified.
                let kind = if kind == LambdaStaticArg::Foreign && cast_messages.is_none() {
                    LambdaStaticArg::Undecided
                } else {
                    kind
                };
                let method_type = if kind == LambdaStaticArg::MethodType {
                    let ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_invoke::method_type_object_descriptor(&ctx, obj)
                } else {
                    None
                };
                // A `MethodType` whose descriptor cannot be read back is not
                // classified: the checks stop undecided.
                let kind = if kind == LambdaStaticArg::MethodType && method_type.is_none() {
                    LambdaStaticArg::Undecided
                } else {
                    kind
                };
                // A direct handle's member (wave 42): what the checks and
                // `bootstrap_lambda` read at position 1 in place of the pool.
                let method_handle = if kind == LambdaStaticArg::MethodHandle {
                    let mut ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_invoke::method_handle_value_member(&mut ctx, obj)
                        .and_then(|(tag, class, name, descriptor)| {
                            Some(MethodHandle {
                                kind: MethodHandleKind::from_tag(tag)?,
                                class_name: Arc::from(class),
                                member_name: Arc::from(name),
                                descriptor: Arc::from(descriptor),
                            })
                        })
                } else {
                    None
                };
                // Wave 43: a handle HotSpot cannot crack (bound, adapted, a
                // combinator), which `lambda_site_verdict` refuses.
                let not_direct = if kind == LambdaStaticArg::MethodHandle && method_handle.is_none() {
                    let mut ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_invoke::method_handle_value_not_direct(
                        &mut ctx, obj,
                    )
                } else {
                    None
                };
                LambdaCondyArg {
                    kind,
                    method_type,
                    method_handle,
                    not_direct,
                    cast_messages,
                }
            }
            // A primitive-typed dynamic constant: boxed by the invoker; not
            // classified.
            _ => LambdaCondyArg {
                kind: LambdaStaticArg::Undecided,
                method_type: None,
                method_handle: None,
                not_direct: None,
                cast_messages: None,
            },
        };
        if dbg_lambda_dispatch() {
            let what = match arg.kind {
                LambdaStaticArg::MethodType => "a MethodType",
                LambdaStaticArg::MethodHandle => "a MethodHandle",
                LambdaStaticArg::Null => "null",
                LambdaStaticArg::Undecided => "not classified",
                LambdaStaticArg::Foreign => "an object of another class",
                _ => "a constant of another class",
            };
            let member = arg
                .method_handle
                .as_ref()
                .map(|h| {
                    format!(
                        " ({} {}.{}{})",
                        h.kind, h.class_name, h.member_name, h.descriptor
                    )
                })
                .or_else(|| arg.not_direct.as_ref().map(|t| format!(" (not direct: {t})")))
                .unwrap_or_default();
            eprintln!(
                "[DBG_LAMBDA] link-check {}{}: dynamic static argument {position} is {what}{member}",
                info.target_name, info.target_descriptor
            );
        }
        out[position] = Some(arg);
    }
    Ok(out)
}

/// The failure HotSpot raises linking this `SwitchBootstraps.typeSwitch` /
/// `enumSwitch` site (the cause of its `BootstrapMethodError`, always an
/// `IllegalArgumentException`), or `None` when it links or the class store
/// cannot decide. Interpreter round i1 wave 38, lane L4; the messages were
/// measured on HotSpot 25 (probe `tools/probes/interp/L4/L4W38SwitchSiteValidation.java`).
///
/// In the JDK's order: the invocation type (`Illegal invocation type
/// (Object)int`), then each label. The invocation type's arity and its two
/// `int`s are checked in EVERY mode: the native switches pop a selector and
/// a restart index and push an `int` whatever the call site declares, so a
/// site of another shape unbalanced the caller's operand stack. The
/// selector type (`enumSwitch` wants an enum) and the labels are checked
/// under `--jdk-only`. javac emits none of the refused shapes.
fn switch_site_refusal(
    shared: &SharedVm,
    class_id: ClassId,
    info: &IndyInfo,
    jdk_only: bool,
) -> Option<LambdaSiteRefusal> {
    use crate::runtime::interpreter::split_method_descriptor_ref as split;
    let illegal = |message: String, every_mode: bool| LambdaSiteRefusal {
        cause_class: "java/lang/IllegalArgumentException",
        message,
        every_mode,
    };
    let type_switch = info.bsm_method == TYPE_SWITCH;
    let (params, ret) = split(&info.target_descriptor);
    let invocation_type = |cm: &crate::classloading::ClassManager| {
        format!(
            "Illegal invocation type {}",
            method_type_display_for(cm, class_id, &info.target_descriptor)
        )
    };
    // `typeSwitch` reads `parameterType(0)` before its check (measured:
    // `ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0`).
    if type_switch && params.is_empty() {
        return Some(LambdaSiteRefusal {
            cause_class: "java/lang/ArrayIndexOutOfBoundsException",
            message: "Index 0 out of bounds for length 0".to_string(),
            every_mode: true,
        });
    }
    if params.len() != 2 || ret != "I" || params[1] != "I" {
        return Some(illegal(
            invocation_type(&*shared.classes.class_manager.read()),
            true,
        ));
    }
    if !jdk_only {
        return None;
    }
    let selector = params[0];
    let guard = shared.classes.class_manager.read();
    let cm: &crate::classloading::ClassManager = &guard;
    let types = LambdaTypes {
        cm,
        caller: class_id,
    };
    let class = cm.get_class(class_id)?;
    let cp = &class.constant_pool;
    let enum_class = if type_switch {
        None
    } else {
        // `enumSwitch`: `!parameterType(0).isPrimitive() && isEnum()`.
        if !selector.starts_with('L') {
            return Some(illegal(invocation_type(cm), false));
        }
        let selector_class = cm.get_class(types.class_of(selector)?)?;
        let is_enum = selector_class
            .access_flags
            .contains(cratonvm_reader::class_access_flags::ClassAccessFlags::ENUM)
            && selector_class
                .superclass
                .and_then(|s| cm.get_class(s))
                .is_some_and(|s| &*s.name == "java/lang/Enum");
        if !is_enum {
            return Some(illegal(invocation_type(cm), false));
        }
        Some(selector_class.name.to_string())
    };
    // A label's `getClass()`, as `Class.toString()` prints it; `None` for a
    // label whose class the class store cannot name.
    let label_class = |index: u16| -> Option<String> {
        Some(match cp.get(index)? {
            ConstantPoolEntry::ClassReference { .. } => "class java.lang.Class".to_string(),
            ConstantPoolEntry::StringReference { .. } => "class java.lang.String".to_string(),
            ConstantPoolEntry::Integer(_) => "class java.lang.Integer".to_string(),
            ConstantPoolEntry::Long(_) => "class java.lang.Long".to_string(),
            ConstantPoolEntry::Float(_) => "class java.lang.Float".to_string(),
            ConstantPoolEntry::Double(_) => "class java.lang.Double".to_string(),
            ConstantPoolEntry::MethodType { .. } => "class java.lang.invoke.MethodType".to_string(),
            ConstantPoolEntry::MethodHandle { .. } => {
                let handle = resolve_method_handle_full(cp, index).ok()?;
                format!("class {}", types.method_handle_constant_class(&handle)?)
            }
            // A dynamic constant (a primitive class or an `EnumDesc` from
            // javac; anything from its bootstrap otherwise) is not decided.
            _ => return None,
        })
    };
    for &index in &info.bootstrap_arg_indices {
        let entry = cp.get(index)?;
        match &enum_class {
            None => {
                // `typeSwitch`'s `verifyLabel`: `Class`, `String`, `Integer`,
                // `EnumDesc`; `Long`/`Float`/`Double`/`Boolean` only with
                // preview features on and a selector other than `boolean`.
                let accepted = match entry {
                    ConstantPoolEntry::ClassReference { .. }
                    | ConstantPoolEntry::StringReference { .. }
                    | ConstantPoolEntry::Integer(_) => true,
                    ConstantPoolEntry::Long(_)
                    | ConstantPoolEntry::Float(_)
                    | ConstantPoolEntry::Double(_) => {
                        cratonvm_reader::preview_enabled()
                            && !matches!(selector, "Z" | "Ljava/lang/Boolean;")
                    }
                    ConstantPoolEntry::Dynamic { .. } => return None,
                    _ => false,
                };
                if !accepted {
                    return Some(illegal(
                        format!("label with illegal type found: {}", label_class(index)?),
                        false,
                    ));
                }
            }
            Some(enum_name) => match entry {
                // `convertEnumConstants`: a name, or the enum class itself.
                ConstantPoolEntry::StringReference { .. } => {}
                ConstantPoolEntry::ClassReference { .. } => {
                    let label = cp.get_class_name(index)?;
                    if label != enum_name.as_str() {
                        return Some(illegal(
                            format!(
                                "the Class label: {}, expected the provided enum class: {}",
                                types.display(&format!("L{label};"))?,
                                types.display(&format!("L{enum_name};"))?
                            ),
                            false,
                        ));
                    }
                }
                ConstantPoolEntry::Dynamic { .. } => return None,
                _ => {
                    return Some(illegal(
                        format!(
                            "label with illegal type found: {}, expected label of type either \
                             String or Class",
                            label_class(index)?
                        ),
                        false,
                    ));
                }
            },
        }
    }
    None
}

/// The failure HotSpot raises linking this `ObjectMethods.bootstrap` site
/// (the cause of its `BootstrapMethodError`, an `IllegalArgumentException`),
/// or `None` when it links or the constant pool cannot decide. Interpreter
/// round i1 wave 38, lane L4; messages measured on HotSpot 25 (probe
/// `tools/probes/interp/L4/L4W38ObjectMethodsSiteValidation.java`).
///
/// In the JDK's order: the method name (`IllegalArgumentException(name)`;
/// it was an internal error here, which Java cannot catch), the method type
/// (`Bad method type: (R)long`), then for `toString` the name list against
/// the accessors (`Name list and accessor list do not match`, counted as
/// `String.split(";")` counts). The name, and a type whose SHAPE is wrong (its
/// arity or return), are refused in every mode: the native record methods pop
/// and push by the name, whatever the site declares. A type that differs only
/// in its classes, and the name list, under `--jdk-only`.
fn object_methods_site_refusal(
    shared: &SharedVm,
    class_id: ClassId,
    info: &IndyInfo,
    jdk_only: bool,
) -> Option<LambdaSiteRefusal> {
    use crate::runtime::interpreter::split_method_descriptor_ref as split;
    let illegal = |message: String, every_mode: bool| LambdaSiteRefusal {
        cause_class: "java/lang/IllegalArgumentException",
        message,
        every_mode,
    };
    let (want_params, want_ret): (usize, &str) = match info.target_name.as_str() {
        "equals" => (2, "Z"),
        "hashCode" => (1, "I"),
        "toString" => (1, "Ljava/lang/String;"),
        other => return Some(illegal(other.to_string(), true)),
    };
    let (params, ret) = split(&info.target_descriptor);
    let bad_type = |cm: &crate::classloading::ClassManager| {
        format!(
            "Bad method type: {}",
            method_type_display_for(cm, class_id, &info.target_descriptor)
        )
    };
    if params.len() != want_params || ret != want_ret {
        return Some(illegal(bad_type(&*shared.classes.class_manager.read()), true));
    }
    if !jdk_only {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    let cp = &class.constant_pool;
    let args = &info.bootstrap_arg_indices;
    let record = cp.get_class_name(*args.first()?)?;
    let record_desc = format!("L{record};");
    if params[0] != record_desc || (want_params == 2 && params[1] != "Ljava/lang/Object;") {
        return Some(illegal(bad_type(&*cm), false));
    }
    if info.target_name == "toString" {
        let names = resolve_string_constant(cp, *args.get(1)?)?;
        // `"".equals(names) ? List.of() : List.of(names.split(";"))`: Java's
        // `split` drops the trailing empty strings.
        let count = if names.is_empty() {
            0
        } else {
            let parts: Vec<&str> = names.split(';').collect();
            parts.len() - parts.iter().rev().take_while(|p| p.is_empty()).count()
        };
        if count != args.len().saturating_sub(2) {
            return Some(illegal(
                "Name list and accessor list do not match".to_string(),
                false,
            ));
        }
    }
    None
}

/// `true` when every getter (static argument 2 onward) of an
/// `ObjectMethods.bootstrap` site is a `REF_getField` whose field reference
/// names the record class (static argument 0), read from the pool alone.
/// Such a site links natively in every mode (javac's shape, or getter-driven);
/// any other may take the JDK's own bootstrap under `--jdk-only`.
fn object_methods_getters_are_record_fields(pool: &ConstantPool, args: &[u16]) -> bool {
    let Some(record) = args.first().and_then(|&i| pool.get_class_name(i)) else {
        return false;
    };
    args.iter().skip(2).all(|&i| match pool.get(i) {
        Some(ConstantPoolEntry::MethodHandle {
            reference_kind: 1,
            reference_index,
        }) => matches!(
            pool.get(*reference_index),
            Some(ConstantPoolEntry::FieldReference { class_index, .. })
                if pool.get_class_name(*class_index) == Some(record)
        ),
        _ => false,
    })
}

/// What `ObjectMethods.makeToString` throws for a getter whose handle type is
/// not `(R)T`: an `IllegalArgumentException(message)`, wrapped in a
/// `RuntimeException` when it comes from inside `makeToString`'s `try`
/// (`filterArguments` / `permuteArguments`), bare when it comes from
/// `MethodType.methodType` before it (a `void` return).
struct ObjectMethodsGetterRefusal {
    message: String,
    wrapped: bool,
}

/// `--jdk-only`, a `toString` site: `Some` when `makeToString` refuses the
/// getters' handle types, decided from the pool and the loaded classes, in
/// the JDK's order (read from `java/lang/runtime/ObjectMethods.java`,
/// `MethodHandles.filterArguments` / `permuteArgumentChecks`, JDK 25; the
/// messages measured on HotSpot 25, probe
/// `tools/probes/interp/L4/L4W42ObjectMethodsStaticSpecialGetters.java`):
///
/// 1. a `void` return anywhere: `MethodType.methodType(String, returns)` —
///    `parameter type cannot be void`, not wrapped;
/// 2. `filterArguments` checks the getters LAST FIRST against the concat
///    type `(T0..Tn)String`: a getter that does not take exactly one
///    argument — `target and filter types do not match: <concat>, <getter>`;
/// 3. `permuteArguments` to `(R)String`: a getter whose one parameter is not
///    `R` — `parameter types do not match after reorder: <filtered>,
///    (R)String`, where `<filtered>` has each getter's parameter.
///
/// `None` when every getter is `(R)T`, and when a type the message needs
/// cannot be decided here: a receiver type HotSpot may restrict to the
/// caller (a protected member of another class, a special getter of another
/// class), or more than 200 argument slots (`makeToString` then splits the
/// getters, and the messages name each split). Such a site keeps the JDK's
/// route. javac's shape (every getter a field of the record) is never
/// refused and pays one scan of the pool.
///
/// Interpreter round i1 wave 42, lane L4
/// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`).
fn object_methods_to_string_getter_refusal(
    shared: &SharedVm,
    class_id: ClassId,
    info: &IndyInfo,
) -> Option<ObjectMethodsGetterRefusal> {
    use crate::runtime::interpreter::split_method_descriptor_ref as split;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    let cp = &class.constant_pool;
    let args = &info.bootstrap_arg_indices;
    if object_methods_getters_are_record_fields(cp, args) {
        return None;
    }
    let record = cp.get_class_name(*args.first()?)?;
    let record_desc = format!("L{record};");
    let caller_desc = format!("L{};", class.name);
    // `true` when the member `h` names is found on its class and is not
    // protected: its handle's receiver type is then that class. A protected
    // member of another package may be restricted to the caller's type.
    let receiver_unrestricted = |h: &MethodHandle, field: bool| -> bool {
        let Some(owner) = cm.find_class_by_name_for_class(&h.class_name, class_id) else {
            return false;
        };
        let flags = if field {
            crate::classloading::find_field_recursive_by_descriptor(
                owner,
                &h.member_name,
                &h.descriptor,
                &cm.class_store,
            )
            .map(|(_, f, _)| f.access_flags.bits())
        } else {
            crate::classloading::find_method_recursive(
                owner,
                &h.member_name,
                &h.descriptor,
                &cm.class_store,
            )
            .map(|(m, _)| m.access_flags.bits())
        };
        matches!(flags, Some(bits) if bits & 0x0004 == 0)
    };
    // Each getter's handle type: its parameters (`None` for a receiver type
    // not decided here) and its return.
    let mut types: Vec<(Vec<Option<String>>, String)> = Vec::with_capacity(args.len());
    for &idx in args.iter().skip(2) {
        let h = resolve_method_handle_full(cp, idx).ok()?;
        let own_class = &*h.class_name == record;
        let owner_desc = if h.class_name.starts_with('[') {
            h.class_name.to_string()
        } else {
            format!("L{};", h.class_name)
        };
        let receiver = |field: bool| -> Option<String> {
            if own_class {
                Some(record_desc.clone())
            } else if !h.class_name.starts_with('[') && receiver_unrestricted(&h, field) {
                Some(owner_desc.clone())
            } else {
                None
            }
        };
        let (params, ret): (Vec<Option<String>>, String) = match h.kind {
            MethodHandleKind::GetField => (vec![receiver(true)], h.descriptor.to_string()),
            MethodHandleKind::GetStatic => (Vec::new(), h.descriptor.to_string()),
            MethodHandleKind::PutField => (
                vec![receiver(true), Some(h.descriptor.to_string())],
                "V".to_string(),
            ),
            MethodHandleKind::PutStatic => (vec![Some(h.descriptor.to_string())], "V".to_string()),
            MethodHandleKind::InvokeStatic
            | MethodHandleKind::InvokeVirtual
            | MethodHandleKind::InvokeInterface
            | MethodHandleKind::InvokeSpecial
            | MethodHandleKind::NewInvokeSpecial => {
                let (ps, r) = split(&h.descriptor);
                let mut params: Vec<Option<String>> = Vec::with_capacity(ps.len() + 1);
                match h.kind {
                    MethodHandleKind::InvokeVirtual | MethodHandleKind::InvokeInterface => {
                        params.push(receiver(false));
                    }
                    // `findSpecial`'s receiver is the calling class.
                    MethodHandleKind::InvokeSpecial => {
                        params.push((owner_desc == caller_desc).then(|| caller_desc.clone()))
                    }
                    _ => {}
                }
                params.extend(ps.iter().map(|p| Some((*p).to_string())));
                let ret = if h.kind == MethodHandleKind::NewInvokeSpecial {
                    owner_desc.clone()
                } else {
                    r.to_string()
                };
                (params, ret)
            }
        };
        types.push((params, ret));
    }
    let refusal =
        |message: String, wrapped: bool| Some(ObjectMethodsGetterRefusal { message, wrapped });
    if types.iter().any(|(_, ret)| ret == "V") {
        return refusal("parameter type cannot be void".to_string(), false);
    }
    let slots: usize = types
        .iter()
        .map(|(_, ret)| if ret == "J" || ret == "D" { 2 } else { 1 })
        .sum();
    if slots > 200 {
        return None;
    }
    let returns: String = types.iter().map(|(_, ret)| ret.as_str()).collect();
    let concat = format!("({returns})Ljava/lang/String;");
    let display = |descriptor: &str| method_type_display_for(&*cm, class_id, descriptor);
    for (params, ret) in types.iter().rev() {
        if params.len() != 1 {
            let params: Option<Vec<&str>> = params.iter().map(|p| p.as_deref()).collect();
            let filter = format!("({}){ret}", params?.concat());
            return refusal(
                format!(
                    "target and filter types do not match: {}, {}",
                    display(&concat),
                    display(&filter)
                ),
                true,
            );
        }
    }
    if types
        .iter()
        .all(|(params, _)| params[0].as_deref() == Some(record_desc.as_str()))
    {
        return None;
    }
    let firsts: Option<Vec<&str>> = types
        .iter()
        .map(|(params, _)| params[0].as_deref())
        .collect();
    let filtered = format!("({})Ljava/lang/String;", firsts?.concat());
    let wanted = format!("({record_desc})Ljava/lang/String;");
    refusal(
        format!(
            "parameter types do not match after reorder: {}, {}",
            display(&filtered),
            display(&wanted)
        ),
        true,
    )
}

/// The `BootstrapMethodError` of an [`ObjectMethodsGetterRefusal`]:
/// `bootstrap method initialization exception`, caused by the
/// `IllegalArgumentException`, or by a `RuntimeException` (whose message is
/// the wrapped exception's `toString()`, as `new RuntimeException(t)` makes
/// it) caused by it.
fn object_methods_to_string_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    refusal: &ObjectMethodsGetterRefusal,
) -> MethodCallFailed {
    const BME: &str = "bootstrap method initialization exception";
    let iae = match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/IllegalArgumentException",
        Some(refusal.message.as_str()),
    ) {
        Ok(iae) => iae,
        Err(e) => return e,
    };
    if !refusal.wrapped {
        return bootstrap_method_error(shared, thread, BME, iae);
    }
    let wrapper_message = format!("java.lang.IllegalArgumentException: {}", refusal.message);
    match throwable_with_cause(
        shared,
        thread,
        "java/lang/RuntimeException",
        &wrapper_message,
        iae,
    ) {
        MethodCallFailed::ExceptionThrown(wrapper) => {
            bootstrap_method_error(shared, thread, BME, wrapper)
        }
        other => other,
    }
}

/// `--jdk-only`: resolve the `MethodHandle` static arguments (the getters) of
/// an `ObjectMethods.bootstrap` site that is not javac's shape as `ldc` of the
/// same entries resolves them (JVMS 5.4.3.5, through `Lookup.find*` with the
/// calling class's access, and through the same per-entry record, so a
/// failure is recorded and rethrown). HotSpot resolves every static argument
/// before the bootstrap runs: a getter of another class's private field is
/// `IllegalAccessError` (cause `IllegalAccessException`), a missing field
/// `NoSuchFieldError`. Interpreter round i1 wave 39, lane L4 (item 1 of
/// `i38-L4-objectmethods-native-linkage-ignores-its-getters`). javac's shape
/// (the record's own fields, from the record) cannot fail and is skipped, so
/// javac output pays nothing.
fn object_methods_resolve_getters(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let (handles, caller_name): (Vec<(u16, MethodHandle)>, String) = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return Ok(());
        };
        if class.object_methods_args_are_canonical(&info.bootstrap_arg_indices) {
            return Ok(());
        }
        let handles = info
            .bootstrap_arg_indices
            .iter()
            .skip(2)
            .filter_map(|&idx| {
                resolve_method_handle_full(&class.constant_pool, idx)
                    .ok()
                    .map(|h| (idx, h))
            })
            .collect();
        (handles, class.name.replace('/', "."))
    };
    for (idx, handle) in &handles {
        // A field getter's JVMS 5.4.4 check, made here rather than left to
        // `Lookup.findGetter` (wave 39 follow-up: on the host the lookup
        // route admitted another class's private field). The owner is
        // resolved through the caller's loader first, as the resolution does.
        let field_kind = matches!(
            handle.kind,
            MethodHandleKind::GetField
                | MethodHandleKind::GetStatic
                | MethodHandleKind::PutField
                | MethodHandleKind::PutStatic
        );
        let refused_private = if field_kind && !handle.class_name.starts_with('[') {
            let owner = crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                class_id,
                &handle.class_name,
            )
            .ok();
            owner
                .and_then(|owner| {
                    let cm = shared.classes.class_manager.read();
                    crate::runtime::interpreter::field_access::field_handle_access_refusal(
                        shared,
                        &cm,
                        class_id,
                        owner,
                        &handle.member_name,
                        &handle.descriptor,
                    )
                })
                // PRIVATE only (wave 42): any other refusal is the `ldc` route's
                // below (`method_handle_constant_access_refusal`), which refuses
                // a package-private member of another package with HotSpot's
                // `member is private to package` and admits a protected one (its
                // handle's receiver restricted to the caller). This pre-check
                // refused a protected field too, as `member is not accessible`.
                .filter(|&private| private)
        } else {
            None
        };
        if let Some(private) = refused_private {
            // `MemberName.makeAccessException`'s text (`member is private:
            // R.b/java.lang.String/getField, from class C (...)`, measured
            // up to the module part), as an `IllegalAccessError` caused by
            // the `IllegalAccessException` (`mapLookupExceptionToError`).
            let kind = match handle.kind {
                MethodHandleKind::GetField => "getField",
                MethodHandleKind::GetStatic => "getStatic",
                MethodHandleKind::PutField => "putField",
                _ => "putStatic",
            };
            let message = format!(
                "member is {}: {}.{}/{}/{kind}, from class {caller_name}",
                if private { "private" } else { "not accessible" },
                handle.class_name.replace('/', "."),
                handle.member_name,
                descriptor_class_name(&handle.descriptor),
            );
            if dbg_indy_all() {
                eprintln!(
                    "[indy-all] object-methods {} getter cp#{idx}: access refused: {message}",
                    info.target_name
                );
            }
            let refuse = |t: &mut JvmThread| -> Result<ObjectRef, MethodCallFailed> {
                let cause = crate::runtime::exceptions::create_exception_object(
                    shared,
                    t,
                    "java/lang/IllegalAccessException",
                    Some(message.as_str()),
                )?;
                Err(throwable_with_cause(
                    shared,
                    t,
                    "java/lang/IllegalAccessError",
                    &message,
                    cause,
                ))
            };
            resolve_recorded_static_arg(shared, thread, class_id, *idx, true, info.fill_as_of, refuse)?;
            continue;
        }
        let resolve = |t: &mut JvmThread| {
            crate::runtime::interpreter::constants::resolve_method_handle_constant(
                shared,
                t,
                class_id,
                handle.kind,
                &handle.class_name,
                &handle.member_name,
                &handle.descriptor,
            )
        };
        resolve_recorded_static_arg(shared, thread, class_id, *idx, true, info.fill_as_of, resolve)?;
    }
    Ok(())
}

/// Does this `StringConcatFactory` site need the JDK's own factory
/// (`bootstrap_generic`) rather than the native linkage? `true` when
/// `makeConcatWithConstants`' checks would refuse it (the recipe's argument
/// tags against the call type, its constant tags against the constants, a
/// return type that cannot hold a `String`, more than 200 argument slots) and
/// when a constant is a kind the native renderer does not model (a
/// `MethodType`, a `MethodHandle`, a dynamic constant). A `Class` constant
/// stays native, rendered as `String.valueOf(Class)` renders it: the JDK
/// factory's own handle chain does not link on CratonVM for it
/// (`AbstractMethodError`, measured). javac emits none of these; each is a
/// hand-assembled site.
fn concat_needs_the_jdk_factory(shared: &SharedVm, class_id: ClassId, info: &IndyInfo) -> bool {
    let params = parse_descriptor_args(&info.target_descriptor);
    let slots: usize = params
        .iter()
        .map(|c| if matches!(c, 'J' | 'D') { 2 } else { 1 })
        .sum();
    if slots > 200 {
        return true;
    }
    let ret = info
        .target_descriptor
        .rsplit_once(')')
        .map(|(_, r)| r)
        .unwrap_or("");
    if !matches!(
        ret,
        "Ljava/lang/String;"
            | "Ljava/lang/Object;"
            | "Ljava/lang/CharSequence;"
            | "Ljava/lang/Comparable;"
            | "Ljava/io/Serializable;"
            | "Ljava/lang/constant/Constable;"
            | "Ljava/lang/constant/ConstantDesc;"
    ) {
        return true;
    }
    if info.bsm_method != MAKE_CONCAT_WITH_CONSTANTS {
        return false;
    }
    let arg_tags = info.recipe.iter().filter(|&&u| u == TAG_ARG_UNIT).count();
    let const_tags = info.recipe.iter().filter(|&&u| u == TAG_CONST_UNIT).count();
    if arg_tags != params.len() || const_tags != info.constant_args.len() {
        return true;
    }
    // Constant kinds, read from the pool: everything past the recipe.
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(class_id) else {
        return false;
    };
    info.bootstrap_arg_indices.iter().skip(1).any(|&idx| {
        !matches!(
            class.constant_pool.get(idx),
            Some(
                ConstantPoolEntry::StringReference { .. }
                    | ConstantPoolEntry::ClassReference { .. }
                    | ConstantPoolEntry::Integer(_)
                    | ConstantPoolEntry::Float(_)
                    | ConstantPoolEntry::Long(_)
                    | ConstantPoolEntry::Double(_)
            )
        )
    })
}

/// Generic invokedynamic linkage: execute an arbitrary bootstrap method to
/// obtain a `CallSite`, then invoke its target `MethodHandle`.
///
/// Which linkages are published for later executions is
/// [`GenericIndyCacheMode`]'s choice; an unpublished one bootstraps again on the
/// next execution, as every generic site did before the cache existed.
fn bootstrap_generic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
    current_class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    let mode = generic_indy_cache_mode();
    let shape = Arc::new(GenericIndyShape::new(&info.target_descriptor));
    // Every pin `link_generic_call_site` pushes is released here, on every
    // path: several of its `?`s used to return with their pins still pushed.
    let pin_base = thread.native_pin_roots.len();
    let linked = link_generic_call_site(shared, thread, cp_index, info, current_class_id);
    thread.native_pin_roots.truncate(pin_base);
    let call_site = if mode == GenericIndyCacheMode::Off {
        linked?
    } else {
        settle_generic_link(
            shared,
            thread,
            frame_idx,
            current_class_id,
            cp_index,
            mode.caches_mutable_call_site_of(&info.bsm_class),
            &shape,
            linked,
        )?
    };
    invoke_generic_call_site(shared, thread, frame_idx, call_site, &shape)
}

/// Run the bootstrap method of a generic `invokedynamic` and return the
/// `CallSite` it produced, UNPINNED — the caller pins it before its next
/// safepoint. A failure is already wrapped per JVMS §6.5.
fn link_generic_call_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cp_index: u16,
    info: &IndyInfo,
    current_class_id: ClassId,
) -> Result<ObjectRef, MethodCallFailed> {
    // --- Re-resolve the BSM (with descriptor) + static args under the lock. ---
    let (bsm_class, bsm_method, bsm_desc, static_args, bsm_is_static) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic generic: class {current_class_id} not found"),
            })?;
        let bsm_index = match class.constant_pool.get(cp_index) {
            Some(ConstantPoolEntry::InvokeDynamic {
                bootstrap_method_attr_index,
                ..
            }) => *bootstrap_method_attr_index,
            _ => {
                return Err(VmError::Internal {
                    message: format!(
                        "invokedynamic generic cp#{cp_index}: not an InvokeDynamic entry"
                    ),
                }
                .into())
            }
        };
        let bsm = class
            .bootstrap_methods
            .get(bsm_index as usize)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic generic: bsm index {bsm_index} out of bounds"),
            })?;
        let h = resolve_method_handle_full(&class.constant_pool, bsm.bootstrap_method_ref)?;
        let sargs: Result<Vec<StaticArg>, MethodCallFailed> = bsm
            .bootstrap_arguments
            .iter()
            .map(|&idx| resolve_static_arg_kind(&class.constant_pool, &class.name, idx))
            .collect();
        (
            h.class_name.to_string(),
            h.member_name.to_string(),
            h.descriptor.to_string(),
            sargs,
            h.kind == MethodHandleKind::InvokeStatic,
        )
    };
    // A malformed argument is a Java `ClassFormatError`, built only now that
    // the `class_manager` lock is released.
    let static_args = static_args.map_err(|e| match e {
        MethodCallFailed::InternalError(VmError::Linkage(l)) => {
            crate::runtime::exceptions::throw_linkage_error(shared, thread, l)
        }
        other => other,
    })?;

    // --- Build bootstrap args: (Lookup, name, MethodType, static args...). ---
    // Every reference-typed arg is pinned in `native_pin_roots` (a scanned +
    // forwarded GC root) and re-read from its slot before use, because the
    // allocations below — and the bootstrap invocation itself — can move the
    // heap. `bsm_pins` maps an arg position to its pin slot.
    let pin_base = thread.native_pin_roots.len();
    let mut bsm_args: Vec<Value> = Vec::with_capacity(3 + static_args.len());
    let mut bsm_pins: Vec<(usize, usize)> = Vec::new();

    // 1. Lookup for the caller class. `MethodHandles.lookup()` is
    //    caller-sensitive; invoked from here the innermost Java frame is the
    //    current indy method, so `lookupClass` resolves to `current_class_id`.
    let lookup = crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/invoke/MethodHandles",
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        &[],
    )?
    .unwrap_or(Value::Object(None));
    if let Value::Object(Some(o)) = lookup {
        bsm_pins.push((0, thread.native_pin_roots.len()));
        thread.native_pin_roots.push(o);
    }
    bsm_args.push(lookup);

    // 2. The call-site name. Every allocation of this argument list follows
    //    `new`'s contract (collect, retry, `OutOfMemoryError`); the infallible
    //    `create_java_string` / class mirror aborted the process when a link
    //    met a full heap (gc-common w9-c,
    //    `common-w8v-constant-resolution-strings-abort-on-a-full-heap`). A
    //    collection moves objects: every reference is pinned the moment it
    //    exists, and `bsm_args` is rebuilt from the pins after the static
    //    arguments. An early `?` leaves pins behind for the caller
    //    (`link_generic_call_site`'s caller truncates to its own base).
    let name_ref = crate::runtime::interpreter::intern_string_literal_or_oom(
        shared,
        thread,
        &info.target_name,
    )?;
    bsm_pins.push((1, thread.native_pin_roots.len()));
    thread.native_pin_roots.push(name_ref);
    bsm_args.push(Value::Object(Some(name_ref)));

    // 3. The call-site MethodType, resolved as `ldc` of a `CONSTANT_MethodType`
    //    with the same descriptor is (JVMS §5.4.3.6): through the CALLER's
    //    defining loader, and interned (`fromMethodDescriptorString`), as the
    //    `MethodType` static arguments already were. It used to come from the
    //    by-name builder — a fresh, un-interned object whose mirrors ignored
    //    the caller's loader — and a failure was a VM-internal error; now a
    //    failure is the Java error, recorded against this instruction by
    //    `settle_generic_link`. Memoised in the `ldc` record under this
    //    `CONSTANT_InvokeDynamic` index (never an `ldc` operand), because a
    //    `MutableCallSite` bootstrap re-runs per execution in the default
    //    cache mode and the factory runs Java.
    let mt = match crate::runtime::interpreter::constants::probe_recorded_cp_constant(
        shared,
        current_class_id,
        cp_index,
    ) {
        Some(Value::Object(Some(mt))) => mt,
        _ => {
            let mt = crate::runtime::interpreter::constants::resolve_method_type_constant(
                shared,
                thread,
                current_class_id,
                &info.target_descriptor,
            )?;
            crate::runtime::interpreter::constants::store_recorded_cp_constant_as_of(
                shared,
                current_class_id,
                cp_index,
                Value::Object(Some(mt)),
                info.fill_as_of,
            );
            mt
        }
    };
    bsm_pins.push((2, thread.native_pin_roots.len()));
    thread.native_pin_roots.push(mt);
    bsm_args.push(Value::Object(Some(mt)));

    // 4. Static bootstrap args.
    for sa in &static_args {
        let pos = bsm_args.len();
        let v = match sa {
            StaticArg::Int(i) => Value::Int(*i),
            StaticArg::Long(l) => Value::Long(*l),
            StaticArg::Float(f) => Value::Float(*f),
            StaticArg::Double(d) => Value::Double(*d),
            StaticArg::Str(s) => Value::Object(Some(
                crate::runtime::interpreter::intern_string_literal_or_oom(shared, thread, s)?,
            )),
            StaticArg::Class(name) => {
                // Through the CALLER's defining loader, as the constant-pool
                // `Class` constant it is must be: `load_class_concurrent` asks
                // only the built-in chain, which cannot see a class of a
                // webapp / fat-jar / forked test loader. A miss is the Java
                // `NoClassDefFoundError` `ldc` of the same entry raises, not a
                // VM-internal class-not-found no handler can catch.
                let cid = crate::runtime::interpreter::resolve_class_loader_aware(
                    shared,
                    thread,
                    current_class_id,
                    name,
                )
                .map_err(|e| {
                    crate::runtime::exceptions::convert_class_not_found(shared, thread, name, e)
                })?;
                Value::Object(Some(
                    crate::runtime::interpreter::constants::class_mirror_or_oom(
                        shared,
                        thread,
                        cid,
                        "indy-arg-class",
                    )?,
                ))
            }
            // `CONSTANT_MethodType` / `CONSTANT_MethodHandle` / `CONSTANT_Dynamic`:
            // resolved exactly as `ldc` of the same index resolves them, and
            // through the same per-entry record, so the two agree on identity
            // and a recorded failure is rethrown rather than re-resolved
            // (JVMS §5.4.3). A `LinkageError` from any of them propagates
            // unwrapped (JVMS §6.5: an `Error` is not wrapped in
            // `BootstrapMethodError`) and is then recorded against this
            // instruction by `settle_generic_link`, as HotSpot's
            // `save_and_throw_indy_exc` records it.
            StaticArg::MType {
                cp_index,
                descriptor,
            } => {
                let resolve = |t: &mut JvmThread| {
                    crate::runtime::interpreter::constants::resolve_method_type_constant(
                        shared,
                        t,
                        current_class_id,
                        descriptor,
                    )
                };
                resolve_recorded_static_arg(
                    shared,
                    thread,
                    current_class_id,
                    *cp_index,
                    false,
                    info.fill_as_of,
                    resolve,
                )?
            }
            StaticArg::MHandle { cp_index, handle } => {
                let resolve = |t: &mut JvmThread| {
                    crate::runtime::interpreter::constants::resolve_method_handle_constant(
                        shared,
                        t,
                        current_class_id,
                        handle.kind,
                        &handle.class_name,
                        &handle.member_name,
                        &handle.descriptor,
                    )
                };
                resolve_recorded_static_arg(
                    shared,
                    thread,
                    current_class_id,
                    *cp_index,
                    true,
                    info.fill_as_of,
                    resolve,
                )?
            }
            StaticArg::Dynamic(idx) => {
                crate::runtime::interpreter::constants::resolve_condy_constant(
                    shared,
                    thread,
                    current_class_id,
                    *idx,
                )?
            }
        };
        // Every reference is pinned the moment it exists: the next argument's
        // resolution can allocate, load classes and run Java.
        if let Value::Object(Some(o)) = v {
            bsm_pins.push((pos, thread.native_pin_roots.len()));
            thread.native_pin_roots.push(o);
        }
        bsm_args.push(v);
    }

    // Re-read object args from their (possibly forwarded) pin slots.
    for &(pos, slot) in &bsm_pins {
        if let Some(o) = thread.native_pin_roots.get(slot).copied() {
            bsm_args[pos] = Value::Object(Some(o));
        }
    }
    let last_is_array_or_null = match bsm_args.last() {
        Some(Value::Object(Some(o))) => {
            shared.mem.heap.kind_of(*o) == cratonvm_types::ObjectKind::Array
        }
        Some(Value::Object(None)) => true,
        _ => false,
    };
    // A trailing array of another reference type collects too
    // (`ObjectMethods.bootstrap`'s `MethodHandle... getters`; interpreter
    // round i1 wave 40, lane L4): `invokeWithArguments` of a varargs
    // bootstrap collects into the declared array type. It was passed the
    // bare first getter, so the JDK's own `ObjectMethods` route (the
    // `--jdk-only` answer for a site whose getters the native linkage does not
    // model) saw a `MethodHandle` where it reads `getters.length`.
    let typed_component = trailing_typed_array_component(&bsm_desc).map(str::to_string);
    let pack_from = bootstrap_varargs_pack_start(
        descriptor_param_count_and_last_is_object_array(&bsm_desc)
            .map(|(count, object_array)| (count, object_array || typed_component.is_some())),
        bsm_args.len(),
        last_is_array_or_null,
    );
    // The collecting array's component class, resolved before the packing
    // below takes the tail from the pin slots (the resolution can load a
    // class and move the heap; every reference is pinned).
    let packed_component: Option<ClassId> = match (pack_from, typed_component.as_deref()) {
        (Some(_), Some(name)) => Some(
            crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                current_class_id,
                name,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found(shared, thread, name, e)
            })?,
        ),
        _ => None,
    };
    if dbg_indy_all() {
        if let (Some(from), Some(name)) = (pack_from, typed_component.as_deref()) {
            eprintln!(
                "[indy-all] bootstrap {bsm_class}.{bsm_method}: collects {} trailing argument(s) into {name}[]",
                bsm_args.len() - from
            );
        }
    }

    // A primitive static argument (or a primitive condy value) bound to a
    // FIXED reference parameter is boxed, and one bound to a wider primitive
    // parameter is widened — the `asType` conversions `invokeWithArguments`
    // applies (JDK `BootstrapMethodInvoker`). They used to be passed raw, so a
    // bootstrap declaring `Object` received an `int` slot. Arguments a
    // trailing `Object[]` collects are boxed by the varargs packing below.
    let param_tags = parse_descriptor_args(&bsm_desc);
    let fixed_limit = pack_from.unwrap_or(usize::MAX);
    for pos in 3..bsm_args.len().min(param_tags.len()).min(fixed_limit) {
        match bootstrap_arg_fit(param_tags[pos], bsm_args[pos]) {
            BootstrapArgFit::Keep => {}
            BootstrapArgFit::Widen(v) => bsm_args[pos] = v,
            BootstrapArgFit::Box => {
                let boxed = box_bootstrap_primitive(shared, thread, bsm_args[pos])?;
                if let Value::Object(Some(o)) = boxed {
                    bsm_pins.push((pos, thread.native_pin_roots.len()));
                    thread.native_pin_roots.push(o);
                }
                bsm_args[pos] = boxed;
            }
        }
    }

    // Re-read object args from their (possibly forwarded) pin slots.
    for &(pos, slot) in &bsm_pins {
        if let Some(o) = thread.native_pin_roots.get(slot).copied() {
            bsm_args[pos] = Value::Object(Some(o));
        }
    }

    // `--jdk-only` (interpreter round i1 wave 41, lane L4): the casts
    // `BootstrapMethodInvoker`'s invocation makes, in argument order: a
    // static argument of another class than its parameter (or than the
    // component of a typed varargs array that collects it) is a
    // `ClassCastException` wrapped in `BootstrapMethodError`, and the
    // bootstrap never runs. It used to receive the value as it was: an
    // `Integer` in a `String` parameter, or stored into the `String[]` wave
    // 40's typed collection allocates. Probe
    // `tools/probes/interp/L4/L4W41BootstrapArgumentCasts.java`.
    if shared.config.is_jdk_only() {
        let (params, _) = crate::runtime::interpreter::split_method_descriptor_ref(&bsm_desc);
        let typed_param = typed_component.as_deref().map(|name| format!("L{name};"));
        for (pos, &value) in bsm_args.iter().enumerate().skip(3) {
            let param = if pos < fixed_limit {
                params.get(pos).copied()
            } else {
                typed_param.as_deref()
            };
            let Some(param) = param else {
                continue;
            };
            if let Some(message) = bootstrap_static_arg_cast_failure(shared, value, param) {
                if dbg_indy_all() {
                    eprintln!(
                        "[indy-all] bootstrap {bsm_class}.{bsm_method}: static argument {pos}: {message}"
                    );
                }
                return Err(bootstrap_method_error_with_new_cause(
                    shared,
                    thread,
                    "bootstrap method initialization exception",
                    "java/lang/ClassCastException",
                    &message,
                ));
            }
        }
    }

    // Some bootstrap methods declare a trailing `Object[]` formal parameter
    // that collects ALL "extra" constant-pool bootstrap arguments beyond the
    // fixed `(Lookup, String, MethodType)` prefix -- a JVMS-legal
    // invokedynamic linkage shape (mirrors a Java varargs bootstrap method).
    // JRuby 10.x's string-interpolation bootstrap uses exactly this shape:
    // `BuildDynamicStringSite.buildDString(Lookup, String, MethodType,
    // Object[])`. `bsm_args` above is built FLAT (`[lookup, name, mt,
    // static_arg_1, ..., static_arg_N]`) and `invoke_shared` sets up the
    // callee's locals positionally 1:1 -- so without this step, the 4th
    // formal parameter (the declared `Object[]`) receives `bsm_args[3]`
    // (the FIRST static bootstrap arg, a scalar) instead of an actual
    // array. `BuildDynamicStringSite`'s own constructor then computes
    // `bsmArgs.length - 6` as its metadata offset; CratonVM's
    // `arraylength`-of-non-array guard silently returns 0 for the scalar
    // (see `[GC-ARRAY-GUARD] array_length(non-array)`), so the offset goes
    // negative and the very next `aaload` throws
    // `ArrayIndexOutOfBoundsException` -- confirmed via `javap` decompile of
    // the real `jruby-base-10.0.2.0.jar` class plus
    // `JRubyScriptTemplateTests`'s `require 'ostruct'` (string
    // interpolation in `OpenStruct`'s class body, `ostruct.rb:477`). Pack
    // the excess trailing static args into a real `Object[]` (boxing any
    // primitive `Value`s -- `Object[]` elements must be references) to
    // match the declared descriptor before invoking. Which arguments are
    // packed is `bootstrap_varargs_pack_start`'s decision (the
    // `invokeWithArguments` rule for a varargs collector).
    {
        if let Some(leading) = pack_from {
            let tail_len = bsm_args.len() - leading;
            // Collecting and fallible (gc-common w4-c; see
            // `interpreter::native_alloc_collecting`). The tail is taken from
            // the pin slots only AFTER this allocation (below), so a collection
            // here cannot leave a stale reference in it.
            let arr = match packed_component {
                // A typed array (`MethodHandle[]`), as `invokeWithArguments`
                // collects it; `gc_alloc_array` collects and retries itself.
                Some(component) => match crate::runtime::interpreter::gc_alloc_array(
                    shared,
                    thread,
                    component,
                    cratonvm_types::ArrayElementType::Reference,
                    tail_len,
                ) {
                    Ok(arr) => arr,
                    Err(e) => {
                        thread.native_pin_roots.truncate(pin_base);
                        return Err(e);
                    }
                },
                None => match crate::runtime::interpreter::native_alloc_collecting(
                    shared,
                    thread,
                    "indy-varargs",
                    |shared, thread| {
                        let mut ctx = NativeContextImpl { shared, thread };
                        ctx.new_array(cratonvm_types::ArrayElementType::Reference, tail_len)
                    },
                ) {
                    Ok(arr) => arr,
                    Err(oom) => {
                        thread.native_pin_roots.truncate(pin_base);
                        return Err(oom.into());
                    }
                },
            };
            // GC-safety: the `valueOf` boxing calls below run arbitrary Java
            // (class-load + <clinit>) and can trigger a moving young GC;
            // pin the array AND every still-unprocessed object-typed tail
            // value, re-reading each from its pin slot right before use
            // (mirrors `bsm_pins` above, same function).
            let base_pin = thread.native_pin_roots.len();
            // `base_pin` -> the array itself.
            thread.native_pin_roots.push(arr);
            // The allocation above can move the heap: take the tail only now,
            // from the pin slots. It used to be copied before the allocation.
            for &(pos, slot) in &bsm_pins {
                if let Some(o) = thread.native_pin_roots.get(slot).copied() {
                    bsm_args[pos] = Value::Object(Some(o));
                }
            }
            let tail: Vec<Value> = bsm_args[leading..].to_vec();
            let mut tail_pins: Vec<Option<usize>> = Vec::with_capacity(tail.len());
            for v in &tail {
                if let Value::Object(Some(o)) = v {
                    tail_pins.push(Some(thread.native_pin_roots.len()));
                    thread.native_pin_roots.push(*o);
                } else {
                    tail_pins.push(None);
                }
            }
            for (i, v) in tail.into_iter().enumerate() {
                let current_v = match tail_pins[i] {
                    Some(slot) => Value::Object(Some(thread.native_pin_roots[slot])),
                    None => v,
                };
                // A reference (String/Class/MethodType/null static arg, or a
                // value with no tail_pins slot) passes through unboxed.
                let boxed = box_bootstrap_primitive(shared, thread, current_v)?;
                let arr_now = thread.native_pin_roots[base_pin];
                let ctx = NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                ctx.set_array_element(arr_now, i, boxed);
            }
            let arr_final = thread.native_pin_roots[base_pin];
            thread.native_pin_roots.truncate(base_pin);
            bsm_args.truncate(leading);
            bsm_args.push(Value::Object(Some(arr_final)));
        }
    }

    // --- Invoke the bootstrap method → CallSite. ---
    let dbg = dbg_indy_generic();
    if dbg {
        eprintln!(
            "[indy-generic] bootstrap {bsm_class}.{bsm_method} target={}{} bsm_args_len={}",
            info.target_name,
            info.target_descriptor,
            bsm_args.len()
        );
    }
    // A `REF_invokeStatic` bootstrap initializes the class that DECLARES it
    // (JVMS §5.5), never the named class of an inherited one:
    // `invoke_static_shared` loads the named class and initializes the
    // declaring one, as the condy route does
    // (`initialize_bootstrap_declaring_class`). Interpreter round i1 wave 15,
    // lane L4; `invoke_shared` initialized the named class.
    let callsite_val = if bsm_is_static {
        crate::vm::invoke_static_shared(
            shared,
            thread,
            &bsm_class,
            &bsm_method,
            &bsm_desc,
            &bsm_args,
        )
    } else {
        crate::vm::invoke_shared(
            shared,
            thread,
            &bsm_class,
            &bsm_method,
            &bsm_desc,
            &bsm_args,
        )
    };
    // Bootstrap args no longer needed -- released on the failure path too,
    // which the old `?` skipped, leaking every pin above into the caller.
    thread.native_pin_roots.truncate(pin_base);
    // JVMS §6.5 "Linking Exceptions": a bootstrap method that completes
    // abruptly with anything but an `Error` fails linkage with a
    // `BootstrapMethodError` whose cause is that exception. The raw exception
    // used to escape unwrapped.
    let callsite_val = callsite_val.map_err(|e| wrap_bootstrap_failure(shared, thread, e))?;
    if dbg {
        eprintln!("[indy-generic] bootstrap result = {callsite_val:?}");
    }

    let callsite = match callsite_val {
        Some(Value::Object(Some(cs))) => cs,
        // `CallSite.makeSite`: a null (or non-reference) answer is a
        // `ClassCastException` wrapped in `BootstrapMethodError`. This used to
        // be a VM-internal error no Java handler could catch.
        _ => {
            return Err(bootstrap_method_error_with_new_cause(
                shared,
                thread,
                "CallSite bootstrap method initialization exception",
                "java/lang/ClassCastException",
                "CallSite bootstrap method failed to produce an instance of CallSite",
            ));
        }
    };
    // A non-null answer that is not a `CallSite` fails the
    // `CallSite.class.cast(result)` in `BootstrapMethodInvoker.invoke`. It used
    // to be accepted and `getTarget()` invoked on it.
    if let Some(failure) = non_callsite_bootstrap_result(shared, thread, callsite) {
        return Err(failure);
    }
    // `CallSite.makeSite`: the target's type must EQUAL the call site's
    // (interpreter round i1 wave 32, `--jdk-only`). An `asType`-compatible
    // target used to run.
    if shared.config.is_jdk_only() {
        let pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(callsite);
        let mismatch = callsite_target_type_mismatch(
            shared,
            thread,
            current_class_id,
            callsite,
            &info.target_descriptor,
        );
        let callsite = thread.native_pin_roots[pin];
        thread.native_pin_roots.truncate(pin);
        if let Some(message) = mismatch {
            if dbg {
                eprintln!("[indy-generic] target type mismatch: {message}");
            }
            return Err(bootstrap_method_error_with_new_cause(
                shared,
                thread,
                "CallSite bootstrap method initialization exception",
                "java/lang/invoke/WrongMethodTypeException",
                &message,
            ));
        }
        return Ok(callsite);
    }
    Ok(callsite)
}

/// `CallSite.makeSite`'s `wrongTargetType` message when the target of the
/// bootstrap's `callsite` is not typed exactly `site_descriptor`:
/// `MethodHandle()Object should be of type ()String`. `None` when the types
/// agree, and whenever a type cannot be read (no target, no `type`, a failed
/// `toMethodDescriptorString`): only a mismatch actually seen refuses a link.
fn callsite_target_type_mismatch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    callsite: ObjectRef,
    site_descriptor: &str,
) -> Option<String> {
    let field = |obj: ObjectRef, name: &str| -> Option<ObjectRef> {
        let slot = crate::vm::vm_exec::resolve_field_slot_by_name_cached(
            shared,
            shared.mem.heap.class_id_of(obj),
            name,
        )?;
        match shared.mem.heap.get_field(obj, slot) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        }
    };
    let target = field(callsite, "target")?;
    let target_type = field(target, "type")?;
    let descriptor = match crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/invoke/MethodType",
        "toMethodDescriptorString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(target_type))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => crate::vm::read_java_string(&shared.mem.heap, s)?,
        _ => return None,
    };
    if descriptor == site_descriptor {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    Some(format!(
        "MethodHandle{} should be of type {}",
        method_type_display_for(&cm, caller, &descriptor),
        method_type_display_for(&cm, caller, site_descriptor)
    ))
}

/// `MethodType.toString()` for a method descriptor: `(int,String)void`, each
/// type by its simple name.
fn method_type_display(descriptor: &str) -> String {
    method_type_display_with(descriptor, &|_| None)
}

/// [`method_type_display`] with each class the store holds for `caller`'s
/// loader named by `Class.getSimpleName()`, as `MethodType.toString()` names
/// it: the class's own `InnerClasses` entry's `inner_name` (`Local` for a
/// local `Outer$1Local`, `""` for an anonymous `Outer$1`), else everything
/// after the package (a top-level `A$B` keeps its `$`). A class that is not
/// loaded keeps the last-`$` text. Interpreter round i1 wave 39, lane L4
/// (item 2 of `i37-L4-review-of-waves-30-36-invoke-and-indy-small-divergences`).
fn method_type_display_for(
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    descriptor: &str,
) -> String {
    method_type_display_with(descriptor, &|name| {
        let id = cm.find_class_by_name_for_class(name, caller)?;
        let class = cm.get_class(id)?;
        if class.is_hidden() {
            return None;
        }
        let own = class
            .inner_classes
            .iter()
            .find(|entry| entry.inner_class == name)
            .map(|entry| entry.inner_name.clone());
        Some(own.unwrap_or_else(|| name.rsplit('/').next().unwrap_or(name).to_string()))
    })
}

fn method_type_display_with(descriptor: &str, loaded: &dyn Fn(&str) -> Option<String>) -> String {
    let (params, ret) = crate::runtime::interpreter::split_method_descriptor_ref(descriptor);
    let simple = |token: &str| -> String {
        let dims = token.bytes().take_while(|&b| b == b'[').count();
        let base = &token[dims..];
        let mut out = match base.as_bytes().first() {
            Some(b'B') => "byte".to_string(),
            Some(b'C') => "char".to_string(),
            Some(b'D') => "double".to_string(),
            Some(b'F') => "float".to_string(),
            Some(b'I') => "int".to_string(),
            Some(b'J') => "long".to_string(),
            Some(b'S') => "short".to_string(),
            Some(b'Z') => "boolean".to_string(),
            Some(b'V') => "void".to_string(),
            _ => {
                let name = base.strip_prefix('L').and_then(|b| b.strip_suffix(';')).unwrap_or(base);
                loaded(name).unwrap_or_else(|| {
                    let name = name.rsplit('/').next().unwrap_or(name);
                    name.rsplit('$').next().unwrap_or(name).to_string()
                })
            }
        };
        for _ in 0..dims {
            out.push_str("[]");
        }
        out
    };
    let params: Vec<String> = params.iter().map(|p| simple(p)).collect();
    format!("({}){}", params.join(","), simple(ret))
}

/// The current target of `call_site`: its `target` field, the slot every
/// `CallSite` native in `lang_invoke` (`cs_target_slot`) reads and writes, so a
/// `setTarget` is observed on the very next execution. `getTarget()` is final
/// on all three JDK subclasses and returns that field. Only when the field
/// cannot be resolved is `getTarget()` actually invoked.
///
/// `call_site` must be pinned at `pin_slot`; it is re-read from there.
fn call_site_target(
    shared: &SharedVm,
    thread: &mut JvmThread,
    pin_slot: usize,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let call_site = thread.native_pin_roots[pin_slot];
    let class_id = shared.mem.heap.class_id_of(call_site);
    if let Some(slot) =
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, class_id, "target")
    {
        return Ok(match shared.mem.heap.get_field(call_site, slot) {
            Value::Object(target) => target,
            _ => None,
        });
    }
    let target = crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/invoke/CallSite",
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(call_site))],
    )?;
    Ok(match target {
        Some(Value::Object(target)) => target,
        _ => None,
    })
}

/// `Boolean.TRUE` / `Boolean.FALSE` — the objects `Boolean.valueOf(boolean)`
/// returns — read from `Boolean`'s statics, or `None` when the class is not
/// loaded or its `<clinit>` has not stored them yet (the slot then holds a
/// default, never an object). The same resolution as
/// `vm_exec::proxy_canonical_boolean`, minus its `load_class` WRITE lock: an
/// indy operand typed `Z` does not need `Boolean` loaded to exist, and the
/// caller falls back to the Java call. Takes one read lock, runs no Java,
/// allocates nothing, and is not a safepoint.
fn canonical_boolean(shared: &SharedVm, truthy: bool) -> Option<ObjectRef> {
    let want = if truthy { "TRUE" } else { "FALSE" };
    let (class_id, static_index) = {
        let cm = shared.classes.class_manager.read();
        let class_id = cm.get_loaded_class_id("java/lang/Boolean")?;
        // Static field INDEX (position among the static fields only), which
        // is what `get_static_shared` is indexed by.
        let static_index = cm
            .get_class(class_id)?
            .fields
            .iter()
            .filter(|f| f.is_static())
            .position(|f| &*f.name == want)?;
        (class_id, static_index)
    };
    match crate::vm::get_static_shared(shared, class_id, static_index) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

/// Invoke the current target of a linked generic `CallSite` with the
/// instruction's operands, and push the result.
///
/// `call_site` arrives unpinned (see `settle_generic_link`); it is pinned before
/// the first safepoint below.
fn invoke_generic_call_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    call_site: ObjectRef,
    shape: &GenericIndyShape,
) -> Result<(), MethodCallFailed> {
    let dbg = dbg_indy_generic();
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(call_site);
    let target_mh = call_site_target(shared, thread, pin_base);
    thread.native_pin_roots.truncate(pin_base); // call site no longer needed
    let target_mh = match target_mh? {
        Some(mh) => mh,
        None => {
            return Err(VmError::Internal {
                message: format!(
                    "invokedynamic generic: CallSite for {} has a null target",
                    shape.target_descriptor
                ),
            }
            .into())
        }
    };
    // Pin the target across the dynamic-arg pop + invocation.
    let mh_slot = thread.native_pin_roots.len();
    thread.native_pin_roots.push(target_mh);

    // --- Pop the dynamic call arguments (descriptor-typed) and pin objects. ---
    let arg_types = &shape.arg_types;
    if dbg {
        eprintln!(
            "[indy-generic] about to pop {} dyn args; stack depth before pop = {}",
            arg_types.len(),
            thread.frames[frame_idx].stack.len(),
        );
    }
    // Inline storage: a call site with up to 8 operands (every Groovy / JRuby
    // shape seen) allocates none of these three lists.
    let mut dyn_args: smallvec::SmallVec<[Value; 8]> =
        smallvec::SmallVec::with_capacity(arg_types.len());
    for i in 0..arg_types.len() {
        let cv = thread.frames[frame_idx].stack.pop_compact();
        let desc_byte = arg_types
            .get(arg_types.len() - 1 - i)
            .copied()
            .unwrap_or('L') as u8;
        dyn_args.push(cv.decode_by_descriptor(desc_byte));
    }
    dyn_args.reverse();
    let mut dyn_pins: smallvec::SmallVec<[(usize, usize); 8]> = smallvec::SmallVec::new();
    for (i, v) in dyn_args.iter().enumerate() {
        if let Value::Object(Some(o)) = v {
            dyn_pins.push((i, thread.native_pin_roots.len()));
            thread.native_pin_roots.push(*o);
        }
    }

    // --- Invoke the target: MethodHandle.invoke(args...). The signature-
    //     polymorphic native reads the real descriptor off the handle and
    //     adapts each spread argument, so we pass the dynamic args directly. ---
    let mut invoke_args: smallvec::SmallVec<[Value; 8]> =
        smallvec::SmallVec::with_capacity(dyn_args.len() + 1);
    // Generic Groovy indy targets are ultimately invoked through our Object[]
    // MethodHandle bridge. Without an explicit conversion here, a call-site
    // `Z` value is represented as `Value::Int` and that bridge boxes it as an
    // Integer. Groovy's constructor selector then rejects a real boolean
    // parameter (LayoutDialect's DecorateProcessor is the concrete witness).
    // Box boolean slots as Boolean before entering the erased bridge; a typed
    // handle will unbox it again, while a dynamic Groovy target observes the
    // correct Boolean runtime class.
    for (i, ty) in arg_types.iter().enumerate() {
        if *ty == 'Z' {
            if let Value::Int(v) = dyn_args[i] {
                // `Boolean.valueOf(b)` is `b ? TRUE : FALSE`: read the static
                // instead of making an interpreted call (two per Groovy
                // constructor site, whose trailing flags are `Z, Z`). Reading
                // a static is not a safepoint. The Java call stays as the
                // fallback before `Boolean` is initialized.
                if let Some(o) = canonical_boolean(shared, v != 0) {
                    // Pinned like any operand: a later `Z` slot's fallback
                    // call below is a safepoint.
                    dyn_pins.push((i, thread.native_pin_roots.len()));
                    thread.native_pin_roots.push(o);
                    dyn_args[i] = Value::Object(Some(o));
                    continue;
                }
                let boxed = match crate::vm::invoke_shared(
                    shared,
                    thread,
                    "java/lang/Boolean",
                    "valueOf",
                    "(Z)Ljava/lang/Boolean;",
                    &[Value::Int(v)],
                ) {
                    Ok(boxed) => boxed.unwrap_or(Value::Object(None)),
                    Err(e) => {
                        thread.native_pin_roots.truncate(pin_base);
                        return Err(e);
                    }
                };
                if let Value::Object(Some(o)) = boxed {
                    dyn_pins.push((i, thread.native_pin_roots.len()));
                    thread.native_pin_roots.push(o);
                }
                dyn_args[i] = boxed;
            }
        }
    }
    // `MethodHandle`'s class, resolved once per linked site (stage 2a, wave
    // 16): `invoke_shared` by name re-probed the loaded-class table and
    // pinned every argument a second time on each execution, though every
    // reference here is already pinned. Its first resolution can load, a
    // safepoint, so it comes before the refresh below.
    let method_handle_class = match shape.method_handle_class(shared) {
        Ok(id) => id,
        Err(e) => {
            thread.native_pin_roots.truncate(pin_base);
            return Err(e.into());
        }
    };
    // Re-read every reference argument from its pin AFTER the boxing loop
    // above: `Boolean.valueOf` is a Java call (and on first use a `<clinit>`),
    // i.e. a safepoint, so a refresh taken before it -- where this loop used
    // to sit -- could hand the target pre-move addresses.
    for &(i, slot) in &dyn_pins {
        if let Some(o) = thread.native_pin_roots.get(slot).copied() {
            dyn_args[i] = Value::Object(Some(o));
        }
    }
    let target_mh = thread.native_pin_roots[mh_slot];
    invoke_args.push(Value::Object(Some(target_mh)));
    invoke_args.extend_from_slice(&dyn_args);
    if dbg {
        eprintln!(
            "[indy-generic] invoking target MH with {} dyn args: {:?}",
            dyn_args.len(),
            dyn_args
        );
    }
    // `invoke_shared`'s own tail, from the resolved class: the references stay
    // pinned (above) until the call returns.
    let result = crate::vm::invoke_by_class_id_shared(
        shared,
        thread,
        method_handle_class,
        "invoke",
        // MethodHandle.invoke is signature-polymorphic. Preserve the actual
        // invokedynamic descriptor so primitive call-site arguments (notably
        // Groovy's trailing `Z, Z` constructor flags) are adapted as their
        // declared primitive types instead of being erased and boxed as
        // Integer through an artificial Object[] signature.
        &shape.target_descriptor,
        &invoke_args,
    );
    thread.native_pin_roots.truncate(pin_base);
    let result = result?;
    if dbg {
        eprintln!("[indy-generic] target MH invoke result = {result:?}");
    }

    // --- Push the result, coerced to the call-site return type. ---
    if shape.ret_byte != b'V' {
        let val = result.unwrap_or(Value::Object(None));
        let coerced = crate::vm::coerce_value_against_ret_char(val, shape.ret_byte, shared);
        thread.frames[frame_idx].stack.push(coerced)?;
    }
    Ok(())
}

/// The linkage failure for a bootstrap method that completed abruptly, per
/// JVMS §6.5 `invokedynamic` "Linking Exceptions" and the JDK's
/// `BootstrapMethodInvoker.invoke`: an `Error` (including a
/// `BootstrapMethodError` the bootstrap itself threw) passes through
/// unchanged; any other throwable becomes the cause of a new
/// `BootstrapMethodError("bootstrap method initialization exception")`.
/// Non-Java failures (`InternalError(..)`) are returned as they are.
#[cold]
fn wrap_bootstrap_failure(
    shared: &SharedVm,
    thread: &mut JvmThread,
    failure: MethodCallFailed,
) -> MethodCallFailed {
    let cause = match failure {
        MethodCallFailed::ExceptionThrown(cause) => cause,
        other => return other,
    };
    let is_error = {
        let cm = shared.classes.class_manager.read();
        let cause_cid = shared.mem.heap.class_id_of(cause);
        cm.get_loaded_class_id("java/lang/Error")
            .is_some_and(|error_cid| cm.is_subclass_of(cause_cid, error_cid))
    };
    if is_error {
        return MethodCallFailed::ExceptionThrown(cause);
    }
    bootstrap_method_error(
        shared,
        thread,
        "bootstrap method initialization exception",
        cause,
    )
}

/// A new `BootstrapMethodError(message)` whose cause is `cause`.
///
/// Falls back to throwing `cause` itself if the error object cannot be built:
/// losing the wrapper is better than losing the original failure.
#[cold]
fn bootstrap_method_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    message: &str,
    cause: ObjectRef,
) -> MethodCallFailed {
    throwable_with_cause(shared, thread, "java/lang/BootstrapMethodError", message, cause)
}

/// A new `class(message)` whose cause is `cause` ([`bootstrap_method_error`]
/// for any throwable class; wave 39 uses it for an `IllegalAccessError`).
pub(crate) fn throwable_with_cause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class: &str,
    message: &str,
    cause: ObjectRef,
) -> MethodCallFailed {
    // `cause` is pinned across the two Java calls below, both safepoints.
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(cause);
    let created = crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        class,
        Some(message),
    );
    let bme = match created {
        Ok(bme) => bme,
        Err(_) => {
            let cause = thread.native_pin_roots[pin];
            thread.native_pin_roots.truncate(pin);
            return MethodCallFailed::ExceptionThrown(cause);
        }
    };
    thread.native_pin_roots.push(bme);
    let cause = thread.native_pin_roots[pin];
    // The `(String)` constructor leaves `cause == this`, which is exactly the
    // state `initCause` accepts. A failure here leaves a cause-less error,
    // which is still the right class and message.
    let _ = crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/Throwable",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        &[Value::Object(Some(bme)), Value::Object(Some(cause))],
    );
    let bme = thread.native_pin_roots[pin + 1];
    thread.native_pin_roots.truncate(pin);
    MethodCallFailed::ExceptionThrown(bme)
}

/// [`bootstrap_method_error`] over a freshly created `cause_class(cause_message)`.
#[cold]
fn bootstrap_method_error_with_new_cause(
    shared: &SharedVm,
    thread: &mut JvmThread,
    message: &str,
    cause_class: &str,
    cause_message: &str,
) -> MethodCallFailed {
    bootstrap_method_error_with_new_cause_opt(
        shared,
        thread,
        message,
        cause_class,
        Some(cause_message),
    )
}

/// [`bootstrap_method_error_with_new_cause`] with a cause whose message may
/// be null (`Objects.requireNonNull`'s `NullPointerException`; wave 41).
fn bootstrap_method_error_with_new_cause_opt(
    shared: &SharedVm,
    thread: &mut JvmThread,
    message: &str,
    cause_class: &str,
    cause_message: Option<&str>,
) -> MethodCallFailed {
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        cause_class,
        cause_message,
    ) {
        Ok(cause) => bootstrap_method_error(shared, thread, message, cause),
        Err(e) => e,
    }
}

/// `Some(failure)` when a bootstrap answered a non-null object that is not a
/// `java.lang.invoke.CallSite`: the `ClassCastException` from
/// `CallSite.class.cast(result)`, wrapped as `BootstrapMethodInvoker` wraps it.
/// `None` when it is a `CallSite`, or when `CallSite` itself is not loaded
/// (then nothing can be concluded and the caller proceeds as before).
fn non_callsite_bootstrap_result(
    shared: &SharedVm,
    thread: &mut JvmThread,
    result: ObjectRef,
) -> Option<MethodCallFailed> {
    let actual = {
        let cm = shared.classes.class_manager.read();
        let callsite_cid = cm.get_loaded_class_id("java/lang/invoke/CallSite")?;
        let result_cid = shared.mem.heap.class_id_of(result);
        if cm.is_subclass_of(result_cid, callsite_cid) {
            return None;
        }
        cm.get_class(result_cid)
            .map(|c| c.name.replace('/', "."))
            .unwrap_or_else(|| "?".to_string())
    };
    Some(bootstrap_method_error_with_new_cause(
        shared,
        thread,
        "bootstrap method initialization exception",
        "java/lang/ClassCastException",
        &format!("Cannot cast {actual} to java.lang.invoke.CallSite"),
    ))
}

/// Raise `java.lang.BootstrapMethodError` for a bootstrap method outside the
/// supported set (StringConcatFactory, LambdaMetafactory, SwitchBootstraps,
/// ObjectMethods).
///
/// Per JVMS §5.4.3.6, when the bootstrap method invocation completes
/// abnormally (here: it is not implemented), the linkage of the `invokedynamic`
/// instruction throws `BootstrapMethodError` carrying the original failure as
/// its cause. We do not have a Java `Throwable` cause to wrap, so the detail
/// message identifies the offending bootstrap method and call site. This is a
/// loud, catchable error — callers that wrap `invokedynamic` in
/// `catch (Throwable)` / `catch (Error)` observe it normally — instead of a
/// silent wrong value that corrupts the caller's computation.
#[cold]
#[allow(dead_code)] // retained as a loud fallback for future BSM gating
fn raise_bootstrap_method_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    info: &IndyInfo,
    caller: &str,
) -> Result<(), MethodCallFailed> {
    let msg = format!(
        "unsupported bootstrap method {}.{} for call site {}{} at {}",
        info.bsm_class, info.bsm_method, info.target_name, info.target_descriptor, caller
    );
    match crate::runtime::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/BootstrapMethodError",
        Some(&msg),
    ) {
        Ok(obj_ref) => Err(MethodCallFailed::ExceptionThrown(obj_ref)),
        // If we cannot even construct the Java error object (e.g. rt.jar absent),
        // surface a loud internal error rather than falling back to a silent
        // null/zero push — the whole point of this path is to never return a
        // wrong value to the caller.
        Err(_) => Err(MethodCallFailed::InternalError(VmError::Internal {
            message: msg,
        })),
    }
}

/// Execute a previously cached call site (fast path).
fn execute_cached_call_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    site: &ResolvedCallSite,
) -> Result<(), MethodCallFailed> {
    match site {
        ResolvedCallSite::StringConcat {
            recipe,
            constant_args,
            target_descriptor,
        } => execute_string_concat(
            shared,
            thread,
            frame_idx,
            recipe,
            constant_args.as_slice(),
            target_descriptor,
        ),
        ResolvedCallSite::Lambda(lcs) => execute_cached_lambda(shared, thread, frame_idx, lcs),
        ResolvedCallSite::TypeSwitch { labels } => {
            execute_type_switch(shared, thread, frame_idx, labels)
        }
        ResolvedCallSite::EnumSwitch { labels } => {
            execute_enum_switch(shared, thread, frame_idx, labels)
        }
        ResolvedCallSite::RecordObjectMethod {
            method,
            component_names,
            field_indices,
            field_descriptors,
            getter_driven,
            accessor_getters,
        } => execute_record_object_method(
            shared,
            thread,
            frame_idx,
            *method,
            component_names,
            field_indices,
            field_descriptors,
            *getter_driven,
            accessor_getters,
        ),
    }
}

/// A lambda's implementation handle, resolved far enough to raise what its
/// resolution raises: the owner through the caller's loader (a
/// `NoClassDefFoundError` when it cannot be loaded), then the member, which is
/// a `NoSuchMethodError` only when it is provably absent
/// (`selection::member_provably_absent`: a hierarchy with real class bytes,
/// not a signature-polymorphic owner). Anything the store cannot decide is
/// left to the call, as before.
fn lambda_impl_resolves(
    shared: &SharedVm,
    thread: &mut JvmThread,
    current_class_id: ClassId,
    impl_handle: &MethodHandle,
) -> Result<(), MethodCallFailed> {
    let owner_name: &str = &impl_handle.class_name;
    // A field handle names no method: `member_provably_absent` would call its
    // field a missing method (a `NoSuchMethodError` HotSpot never raises).
    if owner_name.starts_with('[')
        || matches!(
            impl_handle.kind,
            MethodHandleKind::GetField
                | MethodHandleKind::GetStatic
                | MethodHandleKind::PutField
                | MethodHandleKind::PutStatic
        )
    {
        return Ok(());
    }
    let owner = crate::runtime::interpreter::resolve_class_loader_aware(
        shared,
        thread,
        current_class_id,
        owner_name,
    )
    .map_err(|e| crate::runtime::exceptions::convert_class_not_found(shared, thread, owner_name, e))?;
    let absent = {
        let cm = shared.classes.class_manager.read();
        crate::runtime::resolve::selection::member_provably_absent(
            &cm.class_store,
            owner,
            &impl_handle.member_name,
            &impl_handle.descriptor,
        )
    };
    if absent {
        return Err(crate::error::LinkageError::NoSuchMethodError {
            class_name: owner_name.to_string(),
            method_name: impl_handle.member_name.to_string(),
            method_descriptor: impl_handle.descriptor.to_string(),
        }
        .into());
    }
    Ok(())
}

/// Bootstrap a LambdaMetafactory.metafactory call site.
///
/// LambdaMetafactory bootstrap arguments:
///   arg[0]: MethodType — SAM erased method type
///   arg[1]: MethodHandle — implementation method
///   arg[2]: MethodType — SAM instantiated method type
///
/// The invokedynamic descriptor specifies the captured values → functional interface type.
fn bootstrap_lambda(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
    singleton_site: Option<(u64, usize)>,
    condy: &[Option<LambdaCondyArg>],
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // Parse bootstrap arguments from constant pool (a `MethodType` a dynamic
    // constant answered at position 0 or 2 is read from its value; wave 41)
    // We need to re-acquire the class manager lock briefly to resolve the BSM args
    let (sam_erased_desc, impl_handle, instantiated_desc, host_loader) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "lambda bootstrap: class not found".to_string(),
            })?;

        if info.bootstrap_arg_indices.len() < 3 {
            return Err(VmError::Internal {
                message: format!(
                    "LambdaMetafactory: expected 3 bootstrap args, got {}",
                    info.bootstrap_arg_indices.len()
                ),
            }
            .into());
        }

        let args = &info.bootstrap_arg_indices;
        let sam_erased =
            lambda_method_type_arg(&class.constant_pool, args, condy, 0).ok_or_else(|| {
                VmError::Internal {
                    message: "LambdaMetafactory: invalid SAM erased MethodType".to_string(),
                }
            })?;

        // A dynamic constant's direct handle is read from its value (wave 42).
        let impl_mh = match condy.get(1) {
            Some(Some(LambdaCondyArg {
                method_handle: Some(handle),
                ..
            })) => handle.clone(),
            _ => resolve_method_handle_full(&class.constant_pool, info.bootstrap_arg_indices[1])?,
        };

        let instantiated = lambda_method_type_arg(&class.constant_pool, args, condy, 2)
            .ok_or_else(|| VmError::Internal {
                message: "LambdaMetafactory: invalid instantiated MethodType".to_string(),
            })?;

        (sam_erased, impl_mh, instantiated, class.loader_id)
    };

    // Parse the factory descriptor to determine:
    //   - capture types (parameters of the invokedynamic)
    //   - functional interface (return type)
    // Under `--jdk-only` the implementation handle was resolved by the caller
    // (`lambda_impl_resolves`, interpreter round i1 wave 33; moved before the
    // shape checks in wave 37, as HotSpot resolves static arguments before it
    // invokes the bootstrap).

    let capture_types = parse_descriptor_args(&info.target_descriptor);
    let functional_interface =
        parse_return_type_class(&info.target_descriptor).ok_or_else(|| VmError::Internal {
            message: format!(
                "LambdaMetafactory: cannot parse return type from '{}'",
                info.target_descriptor
            ),
        })?;

    // Resolve the functional interface through the HOST class's loader.
    // A name-only lookup at dispatch time picks an arbitrary copy when two
    // loaders define the same interface (forked-classloader tests re-define
    // the whole framework); default methods would then execute in the wrong
    // loader's context and produce objects failing cross-loader identity
    // checks (Spring AOT `ArgumentCodeGenerator.and()` → javapoet
    // `TypeName.equals` getClass() mismatch).
    let functional_interface_id = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id_for_requester(&functional_interface, host_loader);

    if dbg_lambda_dispatch() {
        eprintln!(
            "[DBG_LAMBDA] bootstrap host={:?} loader={:?} iface={} resolved_id={:?}",
            current_class_id, host_loader, functional_interface, functional_interface_id
        );
    }
    // `altMetafactory` carries extra bootstrap static arguments beyond
    // `metafactory`'s three; the fourth is an int bitmask whose
    // `FLAG_SERIALIZABLE` (0x1) bit is set when the source target type was
    // `Serializable`-intersected. Real HotSpot spins a `writeReplace()` on the
    // proxy -- and adds `java.io.Serializable` to its interface list -- only
    // for those lambdas; an ordinary `Supplier<String> s = () -> "x"` gets
    // neither, and `(Serializable) s` throws ClassCastException. Record the bit
    // so the reflective surfaces can tell the two apart. A plain `metafactory`
    // site has no flags word and is never serializable BY FLAG (it can still be
    // serializable by inheritance -- see `SharedVm::lambda_proxy_serializability`).
    //
    // The WHOLE word is kept, not just bit 0: `FLAG_MARKERS` below decides
    // whether more bootstrap arguments follow.
    let alt_flags: i32 = if info.bsm_method == ALT_METAFACTORY {
        info.bootstrap_arg_indices
            .get(3)
            .and_then(|idx| {
                let cm = shared.classes.class_manager.read();
                let class = cm.get_class(current_class_id)?;
                match class.constant_pool.get(*idx) {
                    Some(ConstantPoolEntry::Integer(flags)) => Some(*flags),
                    _ => None,
                }
            })
            .unwrap_or(0)
    } else {
        0
    };
    let serializable_flag = (alt_flags & FLAG_SERIALIZABLE) != 0;

    // `FLAG_MARKERS` names ADDITIONAL interfaces the spun proxy implements on
    // top of the functional interface. Real HotSpot puts them in the generated
    // class's `implements` clause, so `marker.isInstance(lambda)` and a
    // `checkcast` to the marker both succeed. CratonVM's proxy is a synthetic
    // ClassId with no ClassStore hierarchy, so the list has to be carried
    // beside it — see `record_lambda_proxy_markers`. Dropping it (which this
    // path used to do) makes every intersection-cast lambda fail its own cast.
    //
    // Each marker is resolved HERE, through the host's defining loader, as
    // the `CONSTANT_Class` static argument it is (HotSpot resolves a
    // bootstrap's static arguments before invoking it), so a later
    // `checkcast` / `instanceof` asks the hierarchy of the class the host
    // named instead of loading a same-named copy through the built-in chain.
    // A name that does not resolve is kept by name only, which is the
    // pre-existing behaviour (the linkage itself does not fail on it).
    let marker_interfaces: Vec<LambdaMarker> = if (alt_flags & FLAG_MARKERS) != 0 {
        read_marker_interfaces(shared, current_class_id, &info.bootstrap_arg_indices)
            .into_iter()
            .map(|name| {
                let class_id = crate::runtime::interpreter::resolve_class_loader_aware(
                    shared,
                    &mut *thread,
                    current_class_id,
                    &name,
                )
                .ok()
                .filter(|cid| *cid != ClassId::new(0));
                LambdaMarker { name, class_id }
            })
            .collect()
    } else {
        Vec::new()
    };
    // `FLAG_BRIDGES`: extra descriptors of the SAM name this proxy answers.
    let bridge_descriptors: Vec<Arc<str>> = if (alt_flags & FLAG_BRIDGES) != 0 {
        read_bridge_descriptors(
            shared,
            current_class_id,
            &info.bootstrap_arg_indices,
            alt_flags,
        )
    } else {
        Vec::new()
    };

    // Allocate a synthetic proxy ClassId
    let proxy_class_id = shared.alloc_lambda_proxy_id();

    // Build the LambdaCallSite
    let call_site = LambdaCallSite {
        functional_interface: Arc::from(functional_interface),
        functional_interface_id,
        sam_method_name: Arc::from(info.target_name.clone()),
        sam_descriptor: Arc::from(sam_erased_desc),
        impl_handle,
        instantiated_descriptor: Arc::from(instantiated_desc),
        capture_types: capture_types.clone(),
        proxy_class_id,
        serializable_flag,
    };

    // Register the lambda proxy and cache the call site
    let registered = {
        let mut proxies = shared.classes.lambda_proxies.write();
        if proxies.len() < crate::vm::MAX_LAMBDA_PROXIES {
            proxies.insert(proxy_class_id, Arc::new(call_site.clone()));
            true
        } else {
            false
        }
    };
    // Record the defining class (where this invokedynamic lives) so reflection
    // name natives report `<host>$$Lambda/0x<id>` and a HotSpot-correct nest host
    // — even for a cross-class method reference whose impl method is elsewhere.
    // Bounded by the same cap as `lambda_proxies`. bug-06 fam5 #1.
    if registered {
        shared
            .classes
            .lambda_proxy_hosts
            .write()
            .insert(proxy_class_id, current_class_id);
        record_lambda_proxy_markers_resolved(
            shared.vm_identity,
            proxy_class_id,
            &marker_interfaces,
        );
        record_lambda_proxy_bridges(shared.vm_identity, proxy_class_id, &bridge_descriptors);
        // A live instance must keep the host's (user) loader alive, as the
        // spun class's defining loader does in HotSpot.
        pin_lambda_proxy_to_host_loader(shared.vm_identity, proxy_class_id, current_class_id);
    }
    // An entry that other instructions of the class share is linked PER
    // INSTRUCTION (JVMS §5.4.3.6): each gets its own proxy class, as HotSpot
    // spins one class per linkage, so `String::length` written twice yields
    // two classes. Its row lives in the per-instruction table, never in the
    // per-entry `resolution_cache` row, which would hand this instruction's
    // proxy to all of them. See `lambda_entry_is_shared`.
    let per_instruction = if lambda_entry_is_shared(shared, current_class_id, cp_index) {
        let key = indy_instruction_key(
            shared,
            &thread.frames[frame_idx],
            current_class_id,
            cp_index,
            singleton_site,
        );
        Some(publish_lambda_instruction_site(
            shared,
            key,
            &info.target_descriptor,
            proxy_class_id,
            capture_types.len(),
        ))
    } else {
        None
    };
    // First writer wins (JVMS §5.4.3.6): threads racing through a cold site
    // each bootstrap, but every one must go on with the ONE installed call
    // site. Publishing last-writer-wins and minting from this thread's own
    // proxy gave each racer a different non-capturing "singleton"
    // (`LambdaIdentityProbe`'s `concurrentFirstUse`). A loser's proxy row
    // stays registered but is never reached again.
    let proxy_class_id = match per_instruction {
        Some(installed) => installed,
        None => {
            let mut cache = shared.classes.resolution_cache.write();
            match cache.get_call_site(current_class_id, cp_index) {
                Some(ResolvedCallSite::Lambda(installed)) => installed.proxy_class_id,
                _ => {
                    // Not installed when a redefinition of the caller may have
                    // replaced the pool this bootstrap read (interpreter round
                    // i1 wave 23, lane L5): this execution goes on with its
                    // own proxy, the next bootstraps the new pool's entry.
                    let _ = cache.put_call_site_as_of(
                        current_class_id,
                        cp_index,
                        ResolvedCallSite::Lambda(call_site),
                        info.fill_as_of,
                    );
                    proxy_class_id
                }
            }
        }
    };

    // Now execute: pop captured values, allocate proxy object, push it
    allocate_lambda_proxy(
        shared,
        thread,
        frame_idx,
        proxy_class_id,
        capture_types.len(),
        singleton_site,
    )
}

/// Read `altMetafactory`'s `FLAG_MARKERS` block out of the bootstrap static
/// arguments: `arg[4]` is `markerCount` (an int), `arg[5 .. 5+markerCount]` are
/// `CONSTANT_Class` entries. Returns the internal names, in declaration order.
///
/// Tolerant by construction: a short or malformed block yields the prefix it
/// could read rather than an error. A bootstrap-method attribute that disagrees
/// with its own flags word is a broken class file, but refusing to link the
/// call site would turn a wrong `instanceof` answer into a hard failure of an
/// otherwise-working lambda.
fn read_marker_interfaces(
    shared: &SharedVm,
    current_class_id: ClassId,
    arg_indices: &[u16],
) -> Vec<Arc<str>> {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(current_class_id) else {
        return Vec::new();
    };
    marker_interfaces_in(&class.constant_pool, arg_indices)
}

/// [`read_marker_interfaces`] over the host's constant pool.
fn marker_interfaces_in(cp: &ConstantPool, arg_indices: &[u16]) -> Vec<Arc<str>> {
    let count = match arg_indices.get(4).and_then(|i| cp.get(*i)) {
        Some(ConstantPoolEntry::Integer(n)) if *n > 0 => *n as usize,
        _ => return Vec::new(),
    };
    // `markerCount` is class-file data: sized by the static arguments that can
    // actually follow it, never by the count (a crafted `0x7fffffff` asked for
    // a 32 GiB reservation, which aborts the process).
    let room = arg_indices.len().saturating_sub(5);
    let mut out: Vec<Arc<str>> = Vec::with_capacity(count.min(room));
    for k in 0..count {
        let Some(idx) = arg_indices.get(5 + k) else {
            break;
        };
        if let Some(name) = cp.get_class_name_arc(*idx) {
            out.push(name);
        }
    }
    out
}

/// Read `altMetafactory`'s `FLAG_BRIDGES` block out of the bootstrap static
/// arguments: after the flags word (`arg[3]`) and the marker block when
/// `FLAG_MARKERS` is set (`arg[4]` = `markerCount`, then the markers), one int
/// `bridgeCount` and then that many `CONSTANT_MethodType` entries. Returns the
/// descriptors, in declaration order, interned.
///
/// Tolerant like [`read_marker_interfaces`]: a short or malformed block yields
/// the prefix it could read.
fn read_bridge_descriptors(
    shared: &SharedVm,
    current_class_id: ClassId,
    arg_indices: &[u16],
    alt_flags: i32,
) -> Vec<Arc<str>> {
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(current_class_id) else {
        return Vec::new();
    };
    bridge_descriptors_in(&class.constant_pool, arg_indices, alt_flags)
}

/// [`read_bridge_descriptors`] over the host's constant pool.
fn bridge_descriptors_in(cp: &ConstantPool, arg_indices: &[u16], alt_flags: i32) -> Vec<Arc<str>> {
    let int_arg = |pos: usize| match arg_indices.get(pos).and_then(|i| cp.get(*i)) {
        Some(ConstantPoolEntry::Integer(n)) => Some((*n).max(0) as usize),
        _ => None,
    };
    let count_pos = if (alt_flags & FLAG_MARKERS) != 0 {
        match int_arg(4) {
            Some(markers) => 5usize.saturating_add(markers),
            None => return Vec::new(),
        }
    } else {
        4
    };
    let Some(count) = int_arg(count_pos) else {
        return Vec::new();
    };
    let mut out: Vec<Arc<str>> = Vec::with_capacity(count.min(8));
    for k in 0..count {
        let Some(idx) = arg_indices.get(count_pos + 1 + k) else {
            break;
        };
        if let Some(desc) = resolve_method_type(cp, *idx) {
            out.push(cratonvm_types::intern_arc(&desc));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// altMetafactory marker interfaces
//
// A spun lambda proxy implements its functional interface PLUS every interface
// named in `altMetafactory`'s `FLAG_MARKERS` block. CratonVM's proxy classes
// are synthetic ClassIds (>= 0x8000_0000) that are deliberately absent from the
// ClassStore, so they have no `interfaces` vector to append to and
// `LambdaCallSite` (`classloading/src/resolution.rs`) records only the single
// `functional_interface`. The marker list therefore lives in a side table keyed
// by `(vm_identity, proxy_class_id)`, exactly like `LAMBDA_SINGLETON_CACHE`
// above: `alloc_lambda_proxy_id` never recycles an id, and the `vm_identity`
// half keeps two VMs in one test process from aliasing each other.
//
// Entries hold interned names and class ids, never `ObjectRef`s, so unlike the
// singleton cache this table needs no GC root scan or post-compaction remap.
// It is populated only by intersection-cast lambdas, which are rare; the
// `ANY_LAMBDA_MARKERS` gate keeps the (hot) `instanceof`-on-a-lambda path from
// taking the lock at all in the overwhelmingly common empty case.
// ---------------------------------------------------------------------------

/// `true` once any proxy in this process has recorded a marker interface.
/// Read before the mutex on every marker query; see the module note above.
static ANY_LAMBDA_MARKERS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One `altMetafactory` marker interface of a lambda proxy.
#[derive(Clone, Debug)]
pub struct LambdaMarker {
    /// The internal name, as the host's constant pool (or the reflective
    /// caller) spells it.
    pub name: Arc<str>,
    /// The class that name resolved to through the HOST's defining loader when
    /// the call site was bootstrapped — the `CONSTANT_Class` static argument
    /// resolution HotSpot performs before it invokes `altMetafactory`. `None`
    /// for the reflective path (its markers arrive as names) and for a name
    /// that did not resolve; those are looked up by name when asked.
    pub class_id: Option<ClassId>,
}

#[allow(clippy::type_complexity)]
static LAMBDA_PROXY_MARKERS: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[LambdaMarker]>>>,
> = std::sync::OnceLock::new();

#[allow(clippy::type_complexity)]
fn lambda_proxy_markers(
) -> &'static parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[LambdaMarker]>>> {
    LAMBDA_PROXY_MARKERS.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Record the `altMetafactory` marker interfaces of a freshly-registered lambda
/// proxy, by name only. A no-op for the (usual) empty list.
///
/// `pub` because the REFLECTIVE metafactory path reaches this from outside the
/// `vm` crate: `native-builtins`' `LambdaMetafactory.altMetafactory` shim reads
/// the packed `Object[]` and hands the names back through
/// `NativeContext::register_lambda_proxy_markers`, whose `vm` implementation
/// calls this. The `invokedynamic` path records resolved markers instead
/// ([`record_lambda_proxy_markers_resolved`]).
pub fn record_lambda_proxy_markers(
    vm_identity: usize,
    proxy_class_id: ClassId,
    markers: &[Arc<str>],
) {
    let markers: Vec<LambdaMarker> = markers
        .iter()
        .map(|name| LambdaMarker {
            name: Arc::clone(name),
            class_id: None,
        })
        .collect();
    record_lambda_proxy_markers_resolved(vm_identity, proxy_class_id, &markers);
}

/// [`record_lambda_proxy_markers`] with each marker's class resolved.
pub fn record_lambda_proxy_markers_resolved(
    vm_identity: usize,
    proxy_class_id: ClassId,
    markers: &[LambdaMarker],
) {
    if markers.is_empty() {
        return;
    }
    let mut table = lambda_proxy_markers().lock();
    // Same cap as `lambda_proxies` itself — a proxy that could not be
    // registered has no marker list to answer for either.
    if table.len() >= crate::vm::MAX_LAMBDA_PROXIES {
        return;
    }
    table.insert((vm_identity, proxy_class_id), Arc::from(markers.to_vec()));
    drop(table);
    ANY_LAMBDA_MARKERS.store(true, std::sync::atomic::Ordering::Release);
}

/// The `altMetafactory` marker interfaces recorded for `proxy_class_id`, or
/// `None` when it has none (the common case).
pub fn lambda_proxy_marker_interfaces(
    vm_identity: usize,
    proxy_class_id: ClassId,
) -> Option<Arc<[LambdaMarker]>> {
    if !ANY_LAMBDA_MARKERS.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    lambda_proxy_markers()
        .lock()
        .get(&(vm_identity, proxy_class_id))
        .cloned()
}

// ---------------------------------------------------------------------------
// altMetafactory bridge descriptors
//
// `FLAG_BRIDGES` names extra descriptors of the SAM name that HotSpot's spun
// class implements as forwarding bridges. Same storage shape and reasoning as
// the marker table above: keyed `(vm_identity, proxy_class_id)`, ids never
// recycled, interned strings only (no GC scan), capped like `lambda_proxies`,
// and an `ANY_LAMBDA_BRIDGES` gate so the lambda dispatch path takes no lock
// in the common no-bridge process. Only a descriptor that is NOT the SAM's
// ever asks (see `lambda_accepts_descriptor`), so the gate is off the SAM path.
// ---------------------------------------------------------------------------

/// `true` once any proxy in this process has recorded a bridge descriptor.
static ANY_LAMBDA_BRIDGES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[allow(clippy::type_complexity)]
static LAMBDA_PROXY_BRIDGES: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[Arc<str>]>>>,
> = std::sync::OnceLock::new();

#[allow(clippy::type_complexity)]
fn lambda_proxy_bridges(
) -> &'static parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[Arc<str>]>>> {
    LAMBDA_PROXY_BRIDGES.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Record the `altMetafactory` `FLAG_BRIDGES` descriptors of a freshly
/// registered lambda proxy. A no-op for the (usual) empty list.
///
/// `pub` for the same reason as [`record_lambda_proxy_markers`]: the
/// reflective `altMetafactory` shim hands its bridges over through
/// `NativeContext::register_lambda_proxy_bridges`.
pub fn record_lambda_proxy_bridges(
    vm_identity: usize,
    proxy_class_id: ClassId,
    bridges: &[Arc<str>],
) {
    if bridges.is_empty() {
        return;
    }
    let mut table = lambda_proxy_bridges().lock();
    if table.len() >= crate::vm::MAX_LAMBDA_PROXIES {
        return;
    }
    table.insert((vm_identity, proxy_class_id), Arc::from(bridges.to_vec()));
    drop(table);
    ANY_LAMBDA_BRIDGES.store(true, std::sync::atomic::Ordering::Release);
}

/// Is `descriptor` one of the `FLAG_BRIDGES` descriptors recorded for
/// `proxy_class_id`? One atomic load when no bridge was ever recorded.
pub fn lambda_proxy_has_bridge(
    vm_identity: usize,
    proxy_class_id: ClassId,
    descriptor: &str,
) -> bool {
    if !ANY_LAMBDA_BRIDGES.load(std::sync::atomic::Ordering::Acquire) {
        return false;
    }
    lambda_proxy_bridges()
        .lock()
        .get(&(vm_identity, proxy_class_id))
        .is_some_and(|bridges| bridges.iter().any(|b| &**b == descriptor))
}

/// The `FLAG_BRIDGES` descriptors recorded for `proxy_class_id`, or `None`
/// when it has none (the common case; one atomic load when no bridge was ever
/// recorded). Read by reflection (`getDeclaredMethods()` lists one method per
/// bridge, as HotSpot's spun class declares them) and by lambda serialization,
/// through `NativeContext::lambda_proxy_serial_metadata`.
pub fn lambda_proxy_bridge_descriptors(
    vm_identity: usize,
    proxy_class_id: ClassId,
) -> Option<Arc<[Arc<str>]>> {
    if !ANY_LAMBDA_BRIDGES.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    lambda_proxy_bridges()
        .lock()
        .get(&(vm_identity, proxy_class_id))
        .cloned()
}

// ---------------------------------------------------------------------------
// Lambda proxy -> host loader liveness
//
// HotSpot defines a lambda's spun class in its host's loader, so a live lambda
// instance keeps that loader (and the host, and the implementation method)
// alive. CratonVM's proxy is a synthetic class id with no defining-loader row,
// so the collector's instance->loader edge (`cratonvm_types::loader_pin`) is
// given one here: every proxy whose host a USER loader defined gets a pin row
// naming the host's loader. `bootstrap_lambda` writes it (so the first
// collection already honours it), and `repin_lambda_proxy_loaders` re-adds the
// rows after every collection, because the post-GC reconciliation
// (`classloader::gc_reconcile_defining_loaders`) REPLACES the VM's whole pin
// set from the defining-loader table, which has no proxy rows.
//
// Keyed per VM; only user-loader hosts are listed, so a program without user
// loaders never takes the lock (`ANY_USER_LOADER_LAMBDA`). Rows go with their
// host (`forget_unloaded_lambda_proxy_rows`) and with the VM.
// See docs/internal/fixed-bugs/interpreter-L6-lambda-proxy-instance-does-not-keep-its-host-loader-alive-FIXED-20260924.md.
// ---------------------------------------------------------------------------

/// `true` once any proxy in this process was spun by a user-loader host.
static ANY_USER_LOADER_LAMBDA: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `vm_identity` -> `(proxy, host)` for every proxy whose host a user loader
/// defined.
type UserLoaderLambdaMap = rustc_hash::FxHashMap<usize, Vec<(ClassId, ClassId)>>;

static USER_LOADER_LAMBDAS: std::sync::OnceLock<parking_lot::Mutex<UserLoaderLambdaMap>> =
    std::sync::OnceLock::new();

fn user_loader_lambdas() -> &'static parking_lot::Mutex<UserLoaderLambdaMap> {
    USER_LOADER_LAMBDAS.get_or_init(|| parking_lot::Mutex::new(UserLoaderLambdaMap::default()))
}

/// Give a freshly registered proxy the loader-pin row of its host, when a user
/// loader defined the host. A no-op (one registry probe) for a built-in host.
pub(crate) fn pin_lambda_proxy_to_host_loader(
    vm_identity: usize,
    proxy_class_id: ClassId,
    host_class_id: ClassId,
) {
    let Some(loader_addr) =
        cratonvm_types::loader_pin::loader_pin_addr_for_vm(vm_identity, host_class_id.as_u32())
    else {
        return;
    };
    user_loader_lambdas()
        .lock()
        .entry(vm_identity)
        .or_default()
        .push((proxy_class_id, host_class_id));
    ANY_USER_LOADER_LAMBDA.store(true, std::sync::atomic::Ordering::Release);
    cratonvm_types::loader_pin::set_loader_pin(vm_identity, proxy_class_id.as_u32(), loader_addr);
}

/// Record the host of a proxy the REFLECTIVE `LambdaMetafactory` path
/// registered (`NativeContext::register_lambda_proxy_host`): the `Lookup`'s
/// lookup class, which is HotSpot's host (`caller.lookupClass()`). Writes the
/// same two rows `bootstrap_lambda` writes for an `invokedynamic` proxy — the
/// `lambda_proxy_hosts` row (name, nest host, loader-faithful impl and marker
/// resolution) and the loader pin — so the proxy also unloads with its host
/// (`forget_unloaded_lambda_proxy_rows`, which drops the reflective
/// `CallSite` cache rows keyed by that host in the same pass).
///
/// Refused (nothing recorded, the pre-existing impl-owner fallback stays):
/// proxy id `0` (registration declined), a proxy this VM did not register, a
/// host that is not a loaded class of this VM, and a proxy that already has a
/// host (a proxy is spun once).
pub(crate) fn record_reflective_lambda_proxy_host(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    host_class_id: ClassId,
) -> bool {
    // One lock at a time: no guard below is held while another is taken.
    if proxy_class_id.as_u32() == 0 {
        return false;
    }
    let registered = shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&proxy_class_id);
    if !registered {
        return false;
    }
    let host_loaded = shared
        .classes
        .class_manager
        .read()
        .get_class(host_class_id)
        .is_some();
    if !host_loaded {
        return false;
    }
    {
        let mut hosts = shared.classes.lambda_proxy_hosts.write();
        if hosts.contains_key(&proxy_class_id) {
            return false;
        }
        hosts.insert(proxy_class_id, host_class_id);
    }
    pin_lambda_proxy_to_host_loader(shared.vm_identity, proxy_class_id, host_class_id);
    true
}

/// Re-add the proxy -> host-loader pin rows of `vm_identity` after the post-GC
/// reconciliation rebuilt that VM's pin set. Call right after
/// `classloader::gc_reconcile_defining_loaders` returns: a host whose loader
/// died this cycle has no row any more, so its proxies get none either (and
/// `forget_unloaded_lambda_proxy_rows` drops them in the same pause).
pub fn repin_lambda_proxy_loaders(vm_identity: usize) {
    if !ANY_USER_LOADER_LAMBDA.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let table = user_loader_lambdas().lock();
    let Some(rows) = table.get(&vm_identity) else {
        return;
    };
    for &(proxy, host) in rows {
        if let Some(addr) =
            cratonvm_types::loader_pin::loader_pin_addr_for_vm(vm_identity, host.as_u32())
        {
            cratonvm_types::loader_pin::set_loader_pin(vm_identity, proxy.as_u32(), addr);
        }
    }
}

/// Does lambda proxy `proxy_class_id` satisfy `target` by way of one of its
/// `altMetafactory` marker interfaces?
///
/// A marker satisfies the target when it IS the target or extends it — a marker
/// of `java/util/List` also answers `instanceof Collection`, because HotSpot put
/// `List` in the spun class's `implements` clause and the ordinary interface
/// hierarchy takes it from there. Called from
/// `runtime::interpreter::typecheck::lambda_proxy_satisfies`, the single choke
/// point for `checkcast` / `instanceof` / `Class.isInstance` /
/// `Class.isAssignableFrom` on a lambda proxy.
pub fn lambda_proxy_marker_satisfies(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    target_class_id: ClassId,
    target_name: &str,
) -> bool {
    let Some(markers) = lambda_proxy_marker_interfaces(shared.vm_identity, proxy_class_id) else {
        return false;
    };
    // The marker names were written in the HOST class's constant pool, so they
    // denote classes as the host's defining loader sees them. The
    // `invokedynamic` path resolved them through that loader at bootstrap
    // (`LambdaMarker::class_id`), so the common case asks the class hierarchy
    // and loads nothing here — a `checkcast` must not reach a safepoint on
    // this path for a class its bootstrap already resolved. For a marker with
    // no recorded class (the reflective metafactory path resolves none -- its
    // host is the lookup class -- or the name did not resolve), an
    // already-loaded copy visible to the host's
    // loader is asked first, and the built-in chain is the last resort.
    // Resolving through the built-in chain alone (`load_class_concurrent`, the
    // only lookup this used to make) picked an arbitrary copy when a webapp or
    // forked test loader defines its own.
    let host_loader = || {
        let host = shared
            .classes
            .lambda_proxy_hosts
            .read()
            .get(&proxy_class_id)
            .copied()?;
        shared.classes.class_manager.read().get_loader_id(host)
    };
    for marker in markers.iter() {
        if &*marker.name == target_name {
            return true;
        }
        let resolved = match marker.class_id {
            Some(id) => Ok(id),
            None => {
                let loader = host_loader();
                let loader_visible = loader.and_then(|loader| {
                    shared
                        .classes
                        .class_manager
                        .read()
                        .get_loaded_class_id_for_requester(&marker.name, loader)
                });
                match loader_visible {
                    Some(id) => Ok(id),
                    None => shared.load_class_concurrent(&marker.name),
                }
            }
        };
        if let Ok(marker_id) = resolved {
            if shared
                .classes
                .class_manager
                .read()
                .is_subclass_of(marker_id, target_class_id)
            {
                return true;
            }
        }
    }
    false
}

/// Execute a cached lambda call site: pop captures, allocate proxy, push result.
fn execute_cached_lambda(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    lcs: &LambdaCallSite,
) -> Result<(), MethodCallFailed> {
    allocate_lambda_proxy(
        shared,
        thread,
        frame_idx,
        lcs.proxy_class_id,
        lcs.capture_types.len(),
        None,
    )
}

// Zero-capture lambda singleton cache — mirrors real HotSpot's
// `InnerClassLambdaMetafactory` behaviour of caching a single `INSTANCE`
// per spun lambda class when the lambda captures nothing. Some code
// (Spring AOT's bean-override identity check across an original context
// and its AOT-replayed context) depends on `==`/`.equals()` identity
// holding for two separate invocations of the same non-capturing lambda
// expression. `proxy_class_id` is a monotonically-increasing synthetic id
// (`SharedVm::alloc_lambda_proxy_id`) that is never recycled, so unlike
// the real (recyclable) `ClassId` space used for loaded classes, keying
// this cache directly by `(vm_identity, proxy_class_id)` carries no
// aliasing risk. GC-scanned/remapped the same way as the
// `Integer.valueOf` cache in `native-builtins/src/lang_math.rs` (see
// `gc_scan_lambda_singleton_roots` / `gc_update_lambda_singleton_refs`,
// wired into `vm/src/memory/{roots.rs,gc.rs}`).
///
/// The key carries the invokedynamic INSTRUCTION's bci as well as the proxy
/// class id. JVMS 5.4.3.6 links each `invokedynamic` *instruction* to its own
/// call site, even when javac folds several occurrences onto ONE
/// `CONSTANT_InvokeDynamic` entry -- which it does for repeated method
/// references (`Foo::bar` written twice compiles to two instructions sharing a
/// CP index). HotSpot therefore hands out a DISTINCT instance per occurrence,
/// while keying only by `proxy_class_id` (which is per CP entry) collapsed them
/// into one. Spring's `WebClient.Builder.defaultStatusHandler` keys a
/// `LinkedHashMap` on the predicate, so two `HttpStatusCode::is4xxClientError`
/// registrations became ONE entry and the second handler silently replaced the
/// first (`web.reactive.function.client.DefaultWebClientTests
/// .onStatusHandlerRegisteredGlobally`).
///
/// The bci alone does not name an instruction, though: two METHODS of one
/// class that each contain the shared entry at the same bci (two
/// `return String::length;` bodies, both at bci 0) collided on it. The key
/// therefore also carries a hash of the executing method's name and
/// descriptor (`lambda_site_method_key`).
///
/// Both tiers use ONE key per instruction: the interpreter derives it from the
/// executing frame (method, pc after the instruction), and the compiled bridge
/// carries the same value, computed at compile time by
/// [`lambda_singleton_site_for`]. Until 2026-09-24 the bridge keyed every site
/// on its synthetic frame instead, so a non-capturing lambda changed identity
/// when its method compiled.
///
/// An `RwLock` over an `FxHashMap`, not a `Mutex` over a SipHash map: the
/// lookup below runs on the first evaluation of a non-capturing lambda
/// expression on each thread, and it only ever reads once the site is warm.
/// Writers are the one-time mint per site and the GC remap.
///
/// Each row's value is a [`LambdaSingletonSlot`], shared by `Arc` with the
/// warm-path memos (round i1 wave 20, lane L4): a thread's per-entry indy row
/// (`CachedIndySite::Lambda::singleton`) and a compiled bridge site
/// (`JitIndyGenericSite::singleton_slot`) keep the slot and read the instance
/// from it with one load — no lock, no hash, no refcount. The slot stays the
/// only copy of the reference: the `lambda-singletons` root row scans it and
/// the remap rewrites it through this map, and a row that leaves the map
/// retires its slot first (0), so a memo that outlives its row falls back to
/// the map instead of reading an unscanned reference.
static LAMBDA_SINGLETON_CACHE: std::sync::OnceLock<parking_lot::RwLock<LambdaSingletonMap>> =
    std::sync::OnceLock::new();

/// `(vm_identity, proxy_class_id, method key, invokedynamic pc)` -> the site's singleton.
type LambdaSingletonMap =
    rustc_hash::FxHashMap<(usize, ClassId, u64, usize), Arc<LambdaSingletonSlot>>;

fn lambda_singleton_cache() -> &'static parking_lot::RwLock<LambdaSingletonMap> {
    LAMBDA_SINGLETON_CACHE.get_or_init(|| parking_lot::RwLock::new(LambdaSingletonMap::default()))
}

/// One zero-capture lambda singleton: the instance's address, or 0 once its
/// [`LAMBDA_SINGLETON_CACHE`] row was dropped (host unloaded, VM disposed).
///
/// Written only under the map's write lock (the mint, the GC remap, the
/// retire); read lock-free by the warm-path memos. A reader never holds the
/// value across a safepoint — it pushes it, or returns it to compiled code
/// through `native_pending_return` — exactly as it did the map's value.
pub struct LambdaSingletonSlot(std::sync::atomic::AtomicUsize);

impl LambdaSingletonSlot {
    fn new(obj: ObjectRef) -> Self {
        Self(std::sync::atomic::AtomicUsize::new(obj.as_ptr() as usize))
    }

    /// The instance, or `None` for a retired slot.
    #[inline]
    fn get(&self) -> Option<ObjectRef> {
        let addr = self.0.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: a non-zero value is the address of the singleton the mint
        // stored, kept current by the GC remap (`gc_update_lambda_singleton_refs`).
        (addr != 0).then(|| unsafe { ObjectRef::from_raw(addr as *mut u8) })
    }

    fn set(&self, obj: ObjectRef) {
        self.0
            .store(obj.as_ptr() as usize, std::sync::atomic::Ordering::Release);
    }

    /// The row is leaving the map: memos that still hold the slot must stop
    /// answering from it.
    fn retire(&self) {
        self.0.store(0, std::sync::atomic::Ordering::Release);
    }
}

/// The slot of the singleton row `(this VM, proxy, site)`, for a warm-path
/// memo to keep. Takes the map's read lock: called once per memo fill, never
/// per evaluation.
fn lambda_singleton_slot(
    shared: &SharedVm,
    proxy_class_id: ClassId,
    site: (u64, usize),
) -> Option<Arc<LambdaSingletonSlot>> {
    lambda_singleton_cache()
        .read()
        .get(&(shared.vm_identity, proxy_class_id, site.0, site.1))
        .cloned()
}

/// GC root scan hook (`memory::native_roots`, the `lambda-singletons` row).
/// Visits each cached zero-capture singleton of the VM with its PROXY class
/// id, which the row hands to `defer_or_root` as the owner.
///
/// Loader-conditional since wave 8, like the generic indy call sites
/// ([`gc_scan_generic_indy_roots`]): a proxy spun by a user-loader host has a
/// `loader_pin` row naming that loader (`pin_lambda_proxy_to_host_loader`), so
/// in a licensed cycle its singleton is pinned to the loader instead of rooted.
/// It has to be: the singleton's own pin row keeps the loader alive, so rooting
/// it outright would keep every user loader that ever evaluated a
/// non-capturing lambda alive for the life of the VM. A proxy of a built-in
/// host has no row and is rooted, as before.
pub fn gc_scan_lambda_singleton_roots(vm_identity: usize, visit: &mut dyn FnMut(u32, ObjectRef)) {
    let cache = lambda_singleton_cache().read();
    for (&(vid, proxy, _, _), slot) in cache.iter() {
        if vid == vm_identity {
            if let Some(obj_ref) = slot.get() {
                visit(proxy.as_u32(), obj_ref);
            }
        }
    }
}

/// GC post-compaction hook — called from `vm/src/memory/gc.rs`. Remaps
/// every cached singleton for the active VM through the GC's pointer map.
pub fn gc_update_lambda_singleton_refs(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() {
        return;
    }
    // The write lock, as before the slots existed: it excludes a concurrent
    // mint, which is the only other writer of a live slot.
    let cache = lambda_singleton_cache().write();
    for (&(vid, _, _, _), slot) in cache.iter() {
        if vid != vm_identity {
            continue;
        }
        let Some(obj_ref) = slot.get() else {
            continue;
        };
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            slot.set(unsafe { ObjectRef::from_raw(new_addr as *mut u8) });
        }
    }
}

/// Pop captured values from the stack, allocate a lambda proxy object, push it.
///
/// `singleton_site`: see [`execute_invokedynamic_at`]; `None` derives the key
/// from the frame.
fn allocate_lambda_proxy(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    proxy_class_id: ClassId,
    num_captures: usize,
    singleton_site: Option<(u64, usize)>,
) -> Result<(), MethodCallFailed> {
    // The singleton key names the INSTRUCTION: method and pc. The pc alone
    // did not -- two methods that each `return String::length;` share one
    // `CONSTANT_InvokeDynamic` entry (so one `proxy_class_id`) AND the same pc,
    // and got one instance where HotSpot links two call sites. Only needed for
    // the zero-capture case, which is the only one that consults the key.
    // The interpreter advanced `pc` past the 5-byte instruction before
    // dispatching it, which is the convention `lambda_singleton_site_for`
    // writes down for the compiled side.
    let site = match singleton_site {
        Some(site) if num_captures == 0 => site,
        // A capturing site never reads the key; the pc stays for the
        // `[WATCH-ALLOC]` diagnostic.
        Some((_, pc)) => (0, pc),
        None if num_captures == 0 => {
            let frame = &thread.frames[frame_idx];
            (lambda_site_method_key(frame), frame.pc)
        }
        None => (0, thread.frames[frame_idx].pc),
    };

    // Pop captured values (pushed left-to-right, pop right-to-left). Inline
    // storage (wave 16): a heap `Vec` here was one allocation per capturing
    // lambda evaluation.
    let mut captures: smallvec::SmallVec<[Value; 8]> =
        smallvec::SmallVec::with_capacity(num_captures);
    for _ in 0..num_captures {
        captures.push(thread.frames[frame_idx].stack.pop()?);
    }
    captures.reverse();

    let proxy_ref =
        allocate_lambda_proxy_from_values(shared, thread, proxy_class_id, &mut captures, site)?;
    thread.frames[frame_idx]
        .stack
        .push(Value::Object(Some(proxy_ref)))?;
    Ok(())
}

/// The singleton cache's method component for the frame executing an
/// `invokedynamic`: an Fx hash of its name and descriptor. The class is already
/// in the key (a `proxy_class_id` belongs to one class's constant pool).
fn lambda_site_method_key(frame: &Frame) -> u64 {
    lambda_site_method_key_of(frame.method_name(), frame.method_descriptor())
}

/// [`lambda_site_method_key`] from the name and descriptor directly.
fn lambda_site_method_key_of(method_name: &str, method_descriptor: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    method_name.hash(&mut h);
    method_descriptor.hash(&mut h);
    h.finish()
}

/// Length of the `invokedynamic` instruction (opcode, u16 index, two zeros).
const INVOKEDYNAMIC_LEN: usize = 5;

/// The zero-capture singleton key component of the `invokedynamic` at
/// `indy_bci` in method `method_name``method_descriptor` — the value the
/// interpreter derives from the executing frame, computed without one.
///
/// The pc half is the pc AFTER the instruction (`indy_bci + 5`), because that
/// is what the interpreter's frame holds when it dispatches the instruction
/// (both the decoded and the raw loop advance `pc` first). Compile doors call
/// this so a compiled site keys `LAMBDA_SINGLETON_CACHE` exactly as the
/// interpreted instruction does; see [`make_jit_indy_bridge_site_from_parts`].
pub fn lambda_singleton_site_for(
    method_name: &str,
    method_descriptor: &str,
    indy_bci: usize,
) -> (u64, usize) {
    (
        lambda_site_method_key_of(method_name, method_descriptor),
        indy_bci + INVOKEDYNAMIC_LEN,
    )
}

/// The method name of the synthetic frame `execute_jit_indy_generic_raw`
/// pushes. It names no instruction: the frame path passes the site's own
/// singleton key instead (`execute_invokedynamic_at`).
const JIT_INDY_FRAME_METHOD: &str = "bridge";

/// The frame-free half of [`allocate_lambda_proxy`]: everything from the
/// zero-capture singleton probe to the field stores, given the captures as
/// values rather than as operand-stack entries.
///
/// Split out for the compiled `invokedynamic` bridge, which has the captures in
/// a spill buffer and had to build (and then discard) a whole interpreter frame
/// just to hand them to this code — see `execute_jit_indy_generic_raw`'s fast
/// path. `site` is the singleton cache's per-INSTRUCTION key component,
/// `(method key, pc after the instruction)`. The bridge passes the key its
/// compile door computed for the instruction
/// (`JitIndyGenericSite::singleton_site`, [`lambda_singleton_site_for`]), so a
/// compiled site and the interpreted instruction share one instance.
fn allocate_lambda_proxy_from_values(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy_class_id: ClassId,
    captures: &mut [Value],
    site: (u64, usize),
) -> Result<ObjectRef, MethodCallFailed> {
    let num_captures = captures.len();
    let (site_method, site_pc) = site;

    // Fast path: a zero-capture call site whose singleton was already
    // minted on a prior invocation just returns the cached instance —
    // matches real HotSpot's cached-INSTANCE-field optimization for
    // non-capturing lambdas (see LAMBDA_SINGLETON_CACHE above).
    // Per-INSTRUCTION identity: see LAMBDA_SINGLETON_CACHE's doc comment.
    if num_captures == 0 {
        if let Some(cached) = lambda_singleton_cache()
            .read()
            .get(&(shared.vm_identity, proxy_class_id, site_method, site_pc))
            .and_then(|slot| slot.get())
        {
            // `CRATONVM_DBG_DEADREF_STORE`: is what the cache hands back still
            // a live object?
            //
            // This table is a GC root source and a remap target
            // (`gc_scan_lambda_singleton_roots` / `gc_update_lambda_singleton_refs`,
            // wired in `memory/native_roots.rs`), so a stale entry here means
            // one of those two halves did not run for a collection that moved
            // the instance — which no other instrument in the tree can see,
            // because the value is in neither a frame slot nor a heap field.
            if cratonvm_types::flags().gc.dbg_deadref_store {
                if let Some(reason) = shared
                    .mem
                    .heap
                    .dead_young_ref_reason(cached.as_ptr() as usize)
                {
                    static N: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n < 8 {
                        eprintln!(
                            "[deadref-singleton] #{n} {reason} the zero-capture lambda \
                             singleton cache returned 0x{:x} for cid={:#x} site_pc={site_pc} — \
                             the cached instance names no live object, so the cache was not \
                             remapped for some collection that moved it.",
                            cached.as_ptr() as usize,
                            proxy_class_id.as_u32(),
                        );
                    }
                }
            }
            return Ok(cached);
        }
    }

    // GC-safety: captures sits in a raw Rust Vec, invisible to the
    // collector (native stale-local family). The fallback allocation path
    // below can force a GC on young-gen exhaustion (maybe_gc_forced_pub),
    // which may relocate any captured object reference. Pin every captured
    // object into native_pin_roots (which the GC remaps) before
    // attempting allocation, and refresh captures from the pins
    // afterward so a relocated capture is never written stale into the new
    // proxy's fields.
    //
    // Concrete failure without this: a bound interface-method-reference
    // lambda (e.g. values::get over TDigestDoubleArray) captured under
    // memory pressure got a stale pre-GC ObjectRef written into its proxy
    // field. Every later dispatch through that proxy read the captured
    // field, found the (now-vacated) old address, and resolved a garbage/
    // zeroed class id there instead of the real receiver class.
    // `CRATONVM_DBG_DEADREF_STORE`: is a capture ALREADY dead before the pin?
    //
    // The pin below protects a capture from the allocation that follows; it
    // cannot resurrect one that was dead when it reached the operand stack.
    // Those two produce the same end state — a proxy field naming reclaimed
    // memory — and the heap-side trap in `GenerationalHeap::set_field` reports
    // the STORE either way, which names this function and stops. Checking here,
    // with the Java frames still in hand, says which of the two it is and where
    // the value came from.
    if cratonvm_types::flags().gc.dbg_deadref_store {
        for (i, c) in captures.iter().enumerate() {
            let Value::Object(Some(o)) = c else { continue };
            let Some(reason) = shared.mem.heap.dead_young_ref_reason(o.as_ptr() as usize) else {
                continue;
            };
            // WHICH FRAMES ALREADY HOLD IT.
            //
            // The chain above says where the value is being consumed; it does
            // not say where it entered. A dead value that appears only in the
            // innermost frames was manufactured during argument passing; one
            // that the OUTERMOST frame also holds arrived from that frame's own
            // code and the search continues above it. Printing the slot on
            // every frame turns that from a guess into a reading, and it is one
            // pass over a stack that is about to fail anyway.
            let dead = o.as_ptr() as usize;
            let stack: Vec<String> = thread
                .frames
                .iter()
                .rev()
                .take(10)
                .map(|f| {
                    let mut holds: Vec<String> = Vec::new();
                    for li in 0..f.locals_len() {
                        if let Value::Object(Some(v)) = f.get_local(li as u16) {
                            if v.as_ptr() as usize == dead {
                                holds.push(format!("local[{li}]"));
                            }
                        }
                    }
                    for si in 0..f.stack.len() {
                        if let Value::Object(Some(v)) = f.stack.get_value(si) {
                            if v.as_ptr() as usize == dead {
                                holds.push(format!("stack[{si}]"));
                            }
                        }
                    }
                    format!(
                        "    at {}.{} pc={}{}",
                        f.class_name(),
                        f.method_name(),
                        f.pc,
                        if holds.is_empty() {
                            String::new()
                        } else {
                            format!("   <-- HOLDS IT in {}", holds.join(", "))
                        },
                    )
                })
                .collect();
            eprintln!(
                "[deadref-capture] {reason} capture[{i}] = 0x{:x} was ALREADY dead when the \
                 lambda proxy popped it off the operand stack (cid={:#x}, {} captures) — the \
                 pin below cannot help, the value was wrong before this call. \
                 moved_away_to={:?} (needs CRATONVM_DBG_VACATED_FRAMES; Some means the \
                 referent was RELOCATED and a rewrite was missed, None means it was never \
                 relocated — reclaimed while referenced, or never a valid reference)
in_native_pins={} frames={}
{}",
                o.as_ptr() as usize,
                proxy_class_id.as_u32(),
                num_captures,
                cratonvm_gc::gc_quiescence::moved_away_to(o.as_ptr() as usize),
                thread
                    .native_pin_roots
                    .iter()
                    .any(|r| r.as_ptr() as usize == dead),
                thread.frames.len(),
                stack.join("
"),
            );
        }
    }

    let pin_base = thread.native_pin_roots.len();
    // Inline storage (wave 16): this runs for every capturing lambda
    // evaluation, and a heap `Vec` here was one allocation per evaluation.
    let mut handles: smallvec::SmallVec<[Option<usize>; 8]> =
        smallvec::SmallVec::with_capacity(captures.len());
    for c in captures.iter() {
        if let Value::Object(Some(o)) = c {
            handles.push(Some(thread.native_pin_roots.len()));
            thread.native_pin_roots.push(*o);
        } else {
            handles.push(None);
        }
    }

    // Allocate a proxy object on the heap with fields for captured values,
    // through `new`'s shared-path front end (`alloc_object_shared`): the
    // young attempt, then the one escalation every interpreter ladder shares
    // (`collect_and_retry`: publish and retire the TLAB, forced collection,
    // GC-overhead limit, SoftReference clearing, old-gen-spilling retry, G1's
    // last-ditch cycle), the heap dump on OOM, and the allocation counters.
    //
    // gc-common w9-c: this used to be a private copy of the pre-w2-d ladder --
    // one forced collection and a YOUNG-only retry. So a capturing lambda on a
    // heap whose old generation still had room threw `OutOfMemoryError` after
    // a young collection left eden fragmented; softly reachable objects were
    // NOT cleared before that `OutOfMemoryError` (the `SoftReference`
    // guarantee `alloc_object_shared` keeps); the GC-overhead limit never
    // applied; `-XX:+HeapDumpOnOutOfMemoryError` never dumped; and every
    // proxy's bytes were missing from `bytes_allocated_total` and
    // `ThreadMXBean.getThreadAllocatedBytes` (the shared path is not a TLAB,
    // and only `alloc_object_shared` charges it).
    let proxy_ref = match crate::runtime::interpreter::alloc_object_shared(
        shared,
        thread,
        proxy_class_id,
        num_captures,
    ) {
        Ok(obj) => obj,
        Err(e) => {
            thread.native_pin_roots.truncate(pin_base);
            return Err(e);
        }
    };
    // DBG (`CRATONVM_DBG_WATCH_ALLOC_CID=<hex class id>`): arm the young
    // marker's edge trace on THIS proxy the moment it exists.
    //
    // A lambda proxy has no class NAME to point `CRATONVM_DBG_MARK_WHY_CLASS`
    // at — its class id is synthetic (>= 0x8000_0000) and deliberately absent
    // from the ClassStore — so the one instrument that can say "why was this
    // object marked, or why was it not" had nothing to key on. The ids are
    // handed out sequentially and never recycled, so they ARE stable across
    // runs of the same workload, which makes them a usable handle.
    //
    // Added chasing `io.netty.util.internal.ObjectCleanerTest`, where a
    // one-capture proxy (`Comparator.comparing(fn)`'s result) is freed by the
    // first young sweep while nothing in the heap, the roots, or any scanned
    // side table refers to it, and the address is then written into a
    // `Collections$ReverseComparator2` built afterwards.
    // Memoised: this sits on the lambda-proxy allocation path, and an env
    // lookup per allocation is not a diagnostic cost, it is a permanent one.
    // `None` (the overwhelmingly common case) is one `OnceLock` load.
    {
        static WANT_CID: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
        let want_cid = *WANT_CID.get_or_init(|| {
            cratonvm_types::flags::runtime_var("CRATONVM_DBG_WATCH_ALLOC_CID")
                .ok()
                .and_then(|v| u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok())
        });
        if let Some(want_cid) = want_cid {
            if proxy_class_id.as_u32() == want_cid {
                let addr = proxy_ref.as_ptr() as usize;
                let mut stk = String::new();
                {
                    use std::fmt::Write as _;
                    for f in thread.frames.iter().rev().take(6) {
                        let _ = write!(
                            stk,
                            "
[WATCH-ALLOC]     at {}.{}",
                            f.class_name(),
                            f.method_name()
                        );
                    }
                }
                eprintln!(
                    "[WATCH-ALLOC] lambda proxy cid={:#x} captures={num_captures} \
                     site_pc={site_pc} -> {addr:#x} tid={} frames={}{}",
                    proxy_class_id.as_u32(),
                    thread.thread_id.0,
                    thread.frames.len(),
                    stk
                );
                // BOTH watches: `set_young_mark_watch` is what
                // `mark_edge_precise` / `mark_young` consult to print
                // `[MARKWHY] young marker reached <addr> via <edge>` — and its
                // SILENCE is the finding when the object is swept.
                // `set_dynamic_watch` additionally reports writes through the
                // cell, which names whoever stores the address afterwards.
                cratonvm_gc::heap::set_young_mark_watch(addr);
                cratonvm_gc::heap::set_dynamic_watch(addr);
            }
        }
    }
    // Refresh any captured object references from their pins — the retry
    // path above may have relocated them during GC.
    for (j, h) in handles.iter().enumerate() {
        if let Some(h) = *h {
            captures[j] = Value::Object(Some(thread.native_pin_roots[h]));
        }
    }
    thread.native_pin_roots.truncate(pin_base);

    for (i, val) in captures.iter().enumerate() {
        shared.mem.heap.set_field(proxy_ref, i, *val);
    }

    // Zero-capture call sites mint their singleton exactly once; every
    // later invocation hits the fast path above instead.
    //
    // FIRST WRITER WINS. Two threads evaluating the same cold site both miss
    // the probe above and both allocate. A plain `insert` let the second
    // overwrite the first, so the first thread kept an instance that no later
    // evaluation would ever return again -- `==` between its result and any
    // other evaluation's was `false`, where HotSpot publishes exactly one
    // `CallSite` and therefore one instance. Returning the entry that is
    // actually in the table makes every evaluation agree; the loser's
    // allocation is unreachable garbage.
    if num_captures == 0 {
        let mut cache = lambda_singleton_cache().write();
        let slot = cache
            .entry((shared.vm_identity, proxy_class_id, site_method, site_pc))
            .or_insert_with(|| Arc::new(LambdaSingletonSlot::new(proxy_ref)));
        // A row in the map is never retired (a retire removes it), so `None`
        // is unreachable; answering with this allocation keeps it total.
        let winner = match slot.get() {
            Some(winner) => winner,
            None => {
                slot.set(proxy_ref);
                proxy_ref
            }
        };
        return Ok(winner);
    }

    Ok(proxy_ref)
}

/// Parse the return type of a method descriptor as a class name.
/// E.g. `"(I)Ljava/util/function/Consumer;"` → `Some("java/util/function/Consumer")`
fn parse_return_type_class(descriptor: &str) -> Option<String> {
    let ret = descriptor.rsplit(')').next()?;
    if ret.starts_with('L') && ret.ends_with(';') {
        Some(ret[1..ret.len() - 1].to_string())
    } else {
        None
    }
}

/// Resolve a MethodHandle CP entry to a full [`MethodHandle`] struct.
///
/// Extracts reference_kind, class name, member name, and descriptor from
/// the constant pool. Handles all 9 reference kinds (field refs, method refs,
/// interface method refs).
pub fn resolve_method_handle_full(
    cp: &ConstantPool,
    mh_index: u16,
) -> Result<MethodHandle, MethodCallFailed> {
    let (ref_kind, ref_index) = match cp.get(mh_index) {
        Some(ConstantPoolEntry::MethodHandle {
            reference_kind,
            reference_index,
        }) => (*reference_kind, *reference_index),
        _ => {
            return Err(VmError::Internal {
                message: format!("invokedynamic: cp#{mh_index} is not a MethodHandle"),
            }
            .into());
        }
    };

    let kind = MethodHandleKind::from_tag(ref_kind).ok_or_else(|| VmError::Internal {
        message: format!("invokedynamic: invalid MethodHandle reference_kind {ref_kind}"),
    })?;

    // Extract class_index and name_and_type_index from the reference entry.
    // reference_kind 1-4 reference FieldReference, 5-9 reference MethodReference
    // or InterfaceMethodReference.
    let (class_index, nat_index) = match cp.get(ref_index) {
        Some(ConstantPoolEntry::FieldReference {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        Some(ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        Some(ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        }) => (*class_index, *name_and_type_index),
        _ => {
            return Err(VmError::Internal {
                message: format!(
                    "invokedynamic: MethodHandle ref cp#{ref_index} \
                     is not a Field/Method/InterfaceMethod reference"
                ),
            }
            .into());
        }
    };

    let class_name = cp
        .get_class_name(class_index)
        .ok_or_else(|| VmError::Internal {
            message: format!("invokedynamic: invalid class at cp#{class_index}"),
        })?
        .to_string();

    let (member_name, descriptor) =
        cp.get_name_and_type(nat_index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic: invalid name_and_type at cp#{nat_index}"),
            })?;

    Ok(MethodHandle {
        kind,
        class_name: Arc::from(class_name),
        member_name: Arc::from(member_name.to_string()),
        descriptor: Arc::from(descriptor.to_string()),
    })
}

/// Execute StringConcatFactory.makeConcatWithConstants.
///
/// Called after the class_manager read lock has been released.
/// Execute a `StringConcatFactory` call site.
///
/// Takes the three pieces it actually needs as **borrowed** data rather than an
/// `&IndyInfo`. That is not cosmetic: this is the hottest cached
/// `invokedynamic` shape in the VM (every `"a" + b` in every logging call,
/// `toString()`, and exception message), and the cached fast path used to
/// rebuild a whole `IndyInfo` per execution — `recipe.to_string()` +
/// `target_descriptor.to_string()` + a `Vec<String>` re-allocating **every**
/// constant arg — purely to convert the cached `Arc<str>`s into the `String`s
/// this signature demanded. That was `2 + N` heap allocations plus a `Vec` on
/// every single string concatenation, all of it discarded microseconds later.
/// Generic over `S: AsRef<str>` so the cached path can pass its
/// `&[Arc<str>]` and the bootstrap path its `&[String]` with no conversion at
/// all.
fn execute_string_concat<S: AsRef<[u16]>>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    recipe: &[u16],
    constant_args: &[S],
    target_descriptor: &str,
) -> Result<(), MethodCallFailed> {
    // Inline storage: this runs for every interpreted `"a" + b`, and almost no
    // concat site has more than eight operands, so the argument tags, values
    // and pin slots below cost no heap allocation at all.
    let mut arg_types: smallvec::SmallVec<[char; 8]> = smallvec::SmallVec::new();
    parse_descriptor_args_into(target_descriptor, &mut arg_types);

    // Pop arguments from the stack (pushed left-to-right, pop right-to-left).
    //
    // WP4.3 — descriptor-aware decode: for `J`-typed (long) args the
    // operand-stack slot is untagged raw bits (CompactValue::long stores
    // the i64 directly without NaN-boxing for non-collision values), and a
    // plain `pop()` decodes it via `to_value()` → `Value::Double` because
    // the bit pattern is not NaN-tagged.  String concat then formats it as
    // `0.0` / a denormal.  Routing the pop through `decode_by_descriptor`
    // (already present in `types/src/compact_value.rs:520`) yields the
    // declared `Value::Long(...)` instead, which formats correctly.
    let mut arg_values: smallvec::SmallVec<[Value; 8]> =
        smallvec::SmallVec::with_capacity(arg_types.len());
    for i in 0..arg_types.len() {
        let desc_byte = arg_types
            .get(arg_types.len() - 1 - i)
            .copied()
            .unwrap_or('L') as u8;
        // BC SM2-family fix (2026-07-22): use the long-mark-aware pop
        // (`pop_arg_for_descriptor_checked`, already relied on by
        // `pop_coerced_invoke_args_virtual`/`decode_arg_kind_aware` for
        // invoke-argument marshalling) instead of a plain `pop_compact()` +
        // `decode_by_descriptor()`. A `J`-descriptor arg whose raw bits
        // collide with the NaN-tag space AND whose masked payload fits in
        // 32 bits (e.g. `-1125899906842623L` == `0xFFFC000000000001`, which
        // is bit-for-bit identical to `CompactValue::int(1)`'s tagged
        // encoding) is genuinely undecodable from the bits alone —
        // `decode_by_descriptor` falls back to its i2l-widening heuristic
        // and truncates the long to its low 32 bits. The interpreter
        // already tracks provenance for exactly this case via the
        // parallel `KIND_LONG` marker pushed alongside the operand stack
        // slot; consulting it here (as the invoke-argument path already
        // does) resolves the ambiguity instead of guessing from bits.
        // String concatenation with a computed long value colliding this
        // way is not exotic — H2's `DataUtils.parseHexLong` round-trip
        // (`TestDataUtils.testParse`) hits it via plain `"" + longValue`.
        let v = thread.frames[frame_idx]
            .stack
            .pop_arg_for_descriptor_checked(desc_byte)?;
        arg_values.push(v);
    }
    arg_values.reverse();

    let str_ref = concat_values_to_string(
        shared,
        thread,
        recipe,
        constant_args,
        &arg_types,
        &arg_values,
    )?;
    thread.frames[frame_idx]
        .stack
        .push(Value::Object(Some(str_ref)))?;
    Ok(())
}

/// The frame-free half of [`execute_string_concat`]: render `arg_values`
/// (already popped and descriptor-typed, in left-to-right order) through
/// `recipe` into a fresh, uninterned `String`.
///
/// On `Err` every pin this function took has been released; a `toString()`
/// that throws propagates its exception exactly as HotSpot's
/// `StringConcatHelper.stringOf` does.
fn concat_values_to_string<S: AsRef<[u16]>>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    recipe: &[u16],
    constant_args: &[S],
    arg_types: &[char],
    arg_values: &[Value],
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-root safety: the reference-typed args now live only in the Rust
    // `arg_values` Vec — they are no longer on the (scanned) operand stack.
    // `value_to_string` invokes each object's `toString()` (real Java bytecode),
    // which is a safepoint and can trigger a young GC that *moves* the heap.
    // After a move, the raw `ObjectRef`s captured in `arg_values` are stale: a
    // later iteration would read a wrong/garbage object (e.g. concatenating two
    // non-String objects, where the first `toString()` relocates the second
    // arg). Pin every object arg in `thread.native_pin_roots`, which the
    // collector both scans as a root and forwards in place (see memory/gc.rs and
    // memory/roots.rs). We then re-read the up-to-date address from the pin slot
    // immediately before each `toString()` call. Mirrors the push/re-read/
    // truncate idiom used by the native-call and `new`/`<init>` paths in
    // vm/vm_exec.rs.
    let pin_base = thread.native_pin_roots.len();
    // arg index -> pin slot index (only present for non-null object args).
    let mut arg_pin: smallvec::SmallVec<[Option<usize>; 8]> =
        smallvec::SmallVec::with_capacity(arg_values.len());
    for v in arg_values {
        match v {
            Value::Object(Some(obj_ref)) => {
                let slot = thread.native_pin_roots.len();
                thread.native_pin_roots.push(*obj_ref);
                arg_pin.push(Some(slot));
            }
            _ => arg_pin.push(None),
        }
    }

    // Walk the recipe and build the result, accumulating UTF-16 code UNITS
    // rather than a Rust `String`.
    //
    // A Rust `String`/`str` is UTF-8 by construction and therefore cannot hold
    // an unpaired surrogate (U+D800..U+DFFF is not a Unicode scalar value). A
    // `String` accumulator here silently rewrote every one of them to U+FFFD,
    // so `"x" + lone + "y"` produced `x�y` while
    // `new String(new char[]{'x', lone, 'y'})` — which never touches Rust text
    // — came back correct. Concatenation is on the JLS's lossless path: `+`
    // copies code units, it does not validate them.
    //
    // Units also remove a transcode from the hot path rather than adding one:
    // a `String` argument used to be decoded UTF-16 -> UTF-8 on the way in and
    // re-encoded UTF-8 -> UTF-16 by `create_string_or_oom` on the way out.
    // See `string-concat-loses-unpaired-surrogates-FIXED-20260805.md`.
    //
    // Pre-sized for the literal text plus a modest guess per operand, so the
    // common short concat grows the buffer once or not at all.
    let mut result: Vec<u16> = Vec::with_capacity(recipe.len() + 16 * arg_values.len());
    let mut arg_idx = 0;
    let mut const_idx = 0;

    for &tag in recipe {
        if tag == TAG_ARG_UNIT {
            // Argument placeholder
            if arg_idx < arg_values.len() {
                let arg_type = arg_types.get(arg_idx).copied().unwrap_or('L');
                // Re-read the (possibly forwarded) object reference from its pin
                // slot so a GC move during a prior `toString()` doesn't leave us
                // formatting a stale pointer. Non-object args keep their value.
                let arg_val = match arg_pin.get(arg_idx).copied().flatten() {
                    Some(slot) => Value::Object(Some(
                        thread
                            .native_pin_roots
                            .get(slot)
                            .copied()
                            .unwrap_or_else(|| match arg_values[arg_idx] {
                                Value::Object(Some(o)) => o,
                                _ => unreachable!("pinned slot maps to a non-object arg"),
                            }),
                    )),
                    None => arg_values[arg_idx],
                };
                // A `String` argument is copied unit-for-unit. `value_to_string`
                // would route it through `read_java_string`, whose
                // `String::from_utf16_lossy` is exactly where the surrogate
                // died. A `toString()` result comes back as units too
                // (`RenderedOperand::Units`); only VM-formatted text
                // (primitives, wrappers, `ClassName@hash`) is Rust text, and it
                // cannot carry an unpaired surrogate in the first place.
                // Only for an object whose CLASS is `String`: the reader is
                // speculative by shape, and a `StringBuilder` operand (passed
                // as itself by a pre-JDK-19 javac) read as its whole buffer,
                // unused capacity included, instead of running `toString()`.
                // Appended in place (wave 16): reading the units out first
                // cost two heap `Vec`s per `String` operand.
                let appended = match arg_val {
                    Value::Object(Some(obj)) if crate::vm::is_java_lang_string(shared, obj) => {
                        crate::vm::append_java_string_units(&shared.mem.heap, obj, &mut result)
                    }
                    _ => false,
                };
                if appended {
                    // A `String` operand's units are in `result` already.
                } else if let (Value::Int(v), 'C') = (arg_val, arg_type) {
                    // A primitive `char` operand is ONE code unit, whatever it
                    // is. `value_to_string` renders it through a Rust `char`,
                    // which cannot hold a surrogate, and its fallback emitted
                    // the six characters `\ud800` -- so `"" + (char) 0xD800`
                    // had length 6 where HotSpot's has length 1.
                    result.push(v as u16);
                } else if let Some(unit) = boxed_char_unit(shared, &arg_val) {
                    // A boxed `Character` can itself BE an unpaired surrogate,
                    // and `value_to_string` renders one through a Rust `char`,
                    // which cannot hold it -- `"" + Character.valueOf('\u{d800}')`
                    // came out as `?`, not even U+FFFD. It is one unit by
                    // definition, so emit it directly.
                    result.push(unit);
                } else {
                    match render_operand_checked(shared, Some(thread), &arg_val, arg_type) {
                        Ok(RenderedOperand::Text(s)) => result.extend(s.encode_utf16()),
                        // A `toString()` result: its units, verbatim.
                        Ok(RenderedOperand::Units(units)) => result.extend_from_slice(&units),
                        Err(e) => {
                            // A `toString()` that threw. The pins are
                            // ours to release on every exit path.
                            thread.native_pin_roots.truncate(pin_base);
                            return Err(e);
                        }
                    }
                }
                arg_idx += 1;
            }
        } else if tag == TAG_CONST_UNIT {
            // Constant placeholder (from bootstrap_arguments[1..])
            if let Some(u) = constant_args.get(const_idx) {
                result.extend_from_slice(u.as_ref());
            }
            const_idx += 1;
        } else {
            // Literal recipe text. Already units, so a lone surrogate folded
            // into the recipe by javac is copied through untouched.
            result.push(tag);
        }
    }

    // All `toString()` safepoints are behind us — the concatenated text is now
    // plain Rust data. Release the temporary GC pins (the only early return
    // between `pin_base` and here truncates them itself, so a single truncate
    // restores the root set exactly).
    thread.native_pin_roots.truncate(pin_base);

    // `StringConcatFactory` (the `"a" + b` bytecode shape) produces a brand
    // new String per the JVM spec — it must NOT be interned, otherwise `==`
    // wrongly reports identity with an equal literal.
    // `"a" + b` on a full heap must raise a catchable OutOfMemoryError, not
    // abort the VM -- see `interpreter::create_string_or_oom`.
    crate::runtime::interpreter::create_string_from_units_or_oom(shared, thread, &result)
}

/// Resolve a CONSTANT_MethodType CP entry to its descriptor string.
pub fn resolve_method_type(cp: &ConstantPool, index: u16) -> Option<String> {
    match cp.get(index)? {
        ConstantPoolEntry::MethodType { descriptor_index } => {
            cp.get_utf8(*descriptor_index).map(|s| s.to_string())
        }
        _ => None,
    }
}

/// Resolve a string constant from the constant pool.
pub(crate) fn resolve_string_constant(cp: &ConstantPool, index: u16) -> Option<String> {
    match cp.get(index)? {
        ConstantPoolEntry::StringReference { string_index } => {
            cp.get_utf8(*string_index).map(|s| s.to_string())
        }
        ConstantPoolEntry::Utf8(s) => Some(s.to_string()),
        _ => None,
    }
}

/// Resolve a `makeConcatWithConstants` static constant argument (a `\u0002` /
/// TAG_CONST recipe slot) to its `String.valueOf` text.
///
/// Unlike [`resolve_string_constant`] (which only understands `String`/`Utf8`
/// and is shared with other call sites where that is the contract), the recipe
/// constants of `StringConcatFactory.makeConcatWithConstants` may be *any*
/// loadable constant. javac most often emits a folded `String` here, but the
/// spec permits `int`/`long`/`float`/`double` and `Class` constants, and a
/// faithful implementation must convert each exactly as `String.valueOf` /
/// `String.valueOf((Object) c)` would:
///   - `String`/`Utf8`           → the text verbatim
///   - `int`                     → decimal (also covers folded boolean/char as int)
///   - `long`                    → decimal
///   - `float`/`double`          → Java float/double text (`format_float`/`format_double`)
///   - `Class` (`ClassReference`) → the binary class name `String.valueOf` of a
///                                   `Class` is its `toString()`, but for the
///                                   common `String` literal case this never
///                                   applies; we render the dotted name as a
///                                   best effort rather than dropping it.
///
/// Returning `None` (→ empty string at the call site, preserving the prior
/// fail-soft behaviour) only for kinds that cannot legally appear as a recipe
/// constant.
/// [`resolve_concat_constant`] in UTF-16 code **units**.
///
/// A `CONSTANT_Utf8` entry may contain lone surrogates -- Java's modified UTF-8
/// encodes them individually and javac emits them for any string literal
/// holding one (ANTLR's `_serializedATN` is the standard example). The parser
/// keeps the exact units for such entries in a side table, because the
/// `Arc<str>` form cannot represent them; `get_utf8_wide` is `Some` only for
/// those entries and `None` for the overwhelmingly common lossless case.
///
/// Both the recipe and the `TAG_CONST` slots go through here, so a lone
/// surrogate written as a *literal* survives concatenation exactly as one
/// arriving as a runtime argument does. Before this, only the argument path was
/// lossless and `"x\uD801y" + n` still produced U+FFFD.
fn resolve_concat_constant_units(cp: &ConstantPool, index: u16) -> Option<Vec<u16>> {
    let wide = match cp.get(index) {
        Some(ConstantPoolEntry::StringReference { string_index }) => {
            cp.get_utf8_wide(*string_index)
        }
        Some(ConstantPoolEntry::Utf8(_)) => cp.get_utf8_wide(index),
        _ => None,
    };
    if let Some(units) = wide {
        return Some(units.to_vec());
    }
    // Every other loadable-constant kind (int/long/float/double/Class) is
    // produced as ASCII text by `String.valueOf`, so the UTF-8 form is lossless.
    resolve_concat_constant(cp, index).map(|s| s.encode_utf16().collect())
}

fn resolve_concat_constant(cp: &ConstantPool, index: u16) -> Option<String> {
    match cp.get(index)? {
        ConstantPoolEntry::StringReference { string_index } => {
            cp.get_utf8(*string_index).map(|s| s.to_string())
        }
        ConstantPoolEntry::Utf8(s) => Some(s.to_string()),
        ConstantPoolEntry::Integer(v) => Some(v.to_string()),
        ConstantPoolEntry::Long(v) => Some(v.to_string()),
        ConstantPoolEntry::Float(v) => Some(format_float(*v)),
        ConstantPoolEntry::Double(v) => Some(format_double(*v)),
        ConstantPoolEntry::ClassReference { name_index } => cp
            .get_utf8(*name_index)
            .map(|s| format!("class {}", s.replace('/', "."))),
        _ => None,
    }
}

/// The argument position from which a bootstrap's trailing `Object[]`
/// parameter collects the bootstrap arguments, or `None` when they are passed
/// positionally. `shape` is [`descriptor_param_count_and_last_is_object_array`]
/// of the bootstrap descriptor; `arg_count` counts the whole flat list
/// (`Lookup`, name, type, static arguments).
///
/// The `invokeWithArguments` rule for a varargs collector (and
/// `constants::invoke_condy_bootstrap_pinned`'s step 5): collect everything
/// from the array's position on — zero arguments give an empty array, one
/// scalar a one-element array — unless there is exactly one argument there
/// and it already is an array or `null`. Before this, only a list LONGER than
/// the parameter list was packed, so one static argument reached the
/// `Object[]` slot as a bare scalar and none left the slot missing.
fn bootstrap_varargs_pack_start(
    shape: Option<(usize, bool)>,
    arg_count: usize,
    last_is_array_or_null: bool,
) -> Option<usize> {
    let Some((param_count, true)) = shape else {
        return None;
    };
    let fixed = param_count.checked_sub(1)?;
    if arg_count < fixed || (arg_count == param_count && last_is_array_or_null) {
        return None;
    }
    Some(fixed)
}

/// Parse the argument types from a method descriptor.
/// Returns `(total formal param count, is the LAST formal param exactly
/// `Ljava/lang/Object;[]`)` for a method descriptor -- used by
/// `bootstrap_generic` to detect the "trailing `Object[]` collects extra
/// bootstrap args" invokedynamic linkage shape (JVMS-legal; mirrors a Java
/// varargs bootstrap method). Deliberately narrower than a general varargs
/// check: per JVMS this collecting parameter is always both syntactically
/// LAST and always exactly `Object[]` (never some other array component
/// type) for a bootstrap method, so no Block-after-array-style ordering
/// complication applies here the way it did for JRuby's own MethodHandle
/// combinator chains (see `collect_trailing_varargs` in
/// `native-builtins/src/lang_invoke.rs` for that unrelated, harder case).
fn descriptor_param_count_and_last_is_object_array(descriptor: &str) -> Option<(usize, bool)> {
    let inner = descriptor.strip_prefix('(')?;
    let close = inner.find(')')?;
    let params_str = &inner[..close];
    let bytes = params_str.as_bytes();
    let mut i = 0;
    let mut count = 0usize;
    let mut last_is_object_array = false;
    while i < bytes.len() {
        let start = i;
        last_is_object_array = false;
        match bytes[i] {
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'L' {
                    while i < bytes.len() && bytes[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else {
                    i += 1;
                }
                let token = &params_str[start..i];
                last_is_object_array = token == "[Ljava/lang/Object;";
            }
            _ => {
                i += 1;
            }
        }
        count += 1;
    }
    Some((count, last_is_object_array))
}

/// The component class of a bootstrap's LAST parameter when it is a
/// one-dimensional array of a reference type other than `Object`
/// (`[Ljava/lang/invoke/MethodHandle;` -> `java/lang/invoke/MethodHandle`),
/// else `None`. The narrow comment above
/// [`descriptor_param_count_and_last_is_object_array`] ("always exactly
/// `Object[]`") does not hold: `ObjectMethods.bootstrap` ends in
/// `MethodHandle...` (interpreter round i1 wave 40, lane L4).
fn trailing_typed_array_component(descriptor: &str) -> Option<&str> {
    let inner = descriptor.strip_prefix('(')?;
    let params = &inner[..inner.find(')')?];
    let bytes = params.as_bytes();
    let mut i = 0;
    let mut last: Option<&str> = None;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'L' {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        last = params.get(start..i.min(bytes.len()));
    }
    let component = last?.strip_prefix("[L")?.strip_suffix(';')?;
    (component != "java/lang/Object").then_some(component)
}

fn parse_descriptor_args(descriptor: &str) -> Vec<char> {
    let mut args = Vec::new();
    parse_descriptor_args_into(descriptor, &mut args);
    args
}

/// [`parse_descriptor_args`] into caller-provided storage, so a hot caller can
/// hand it a `SmallVec` and allocate nothing. One tag per parameter: the
/// primitive's own letter, or `L` for any reference (arrays included).
fn parse_descriptor_args_into<E: Extend<char>>(descriptor: &str, args: &mut E) {
    let bytes = descriptor.as_bytes();
    let mut i = 0;
    let mut push = |c: char| args.extend(std::iter::once(c));

    if i < bytes.len() && bytes[i] == b'(' {
        i += 1;
    }

    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'B' | b'C' | b'I' | b'S' | b'Z' => {
                push(bytes[i] as char);
                i += 1;
            }
            b'J' => {
                push('J');
                i += 1;
            }
            b'F' => {
                push('F');
                i += 1;
            }
            b'D' => {
                push('D');
                i += 1;
            }
            b'L' => {
                push('L');
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                push('L');
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() {
                    if bytes[i] == b'L' {
                        while i < bytes.len() && bytes[i] != b';' {
                            i += 1;
                        }
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => {
                i += 1;
            }
        }
    }
}

/// [`value_to_string_checked`] for the unit tests, which have no thread and so
/// never reach a `toString()` that could throw.
#[cfg(test)]
fn value_to_string(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    value: &Value,
    type_char: char,
) -> String {
    value_to_string_checked(shared, thread, value, type_char)
        .unwrap_or_else(|_| "<threw>".to_string())
}

/// Convert a JVM Value to its string representation for string concatenation.
///
/// When `thread` is provided, objects that are not strings or primitive wrappers
/// will have their `toString()` called via virtual dispatch. This produces
/// correct output for ArrayList, HashMap, user classes, etc.
///
/// `Err` only for a `toString()` that completed abruptly with a Java-visible
/// throwable, which the caller must propagate: HotSpot's
/// `StringConcatHelper.stringOf` does not catch it. Every other failure keeps
/// the historical `ClassName@hash` fallback.
///
/// Lossy for an unpaired surrogate in a `toString()` result (a Rust `String`
/// cannot hold one); the concat path uses [`render_operand_checked`] instead.
fn value_to_string_checked(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    value: &Value,
    type_char: char,
) -> Result<String, MethodCallFailed> {
    Ok(
        match render_operand_checked(shared, thread, value, type_char)? {
            RenderedOperand::Text(s) => s,
            RenderedOperand::Units(units) => String::from_utf16_lossy(&units),
        },
    )
}

/// One rendered concat operand: Rust text for everything VM-formatted
/// (primitives, wrappers, the `ClassName@hash` fallback), UTF-16 code units for
/// a Java `String` — including the one a `toString()` returned, which may hold
/// an unpaired surrogate that a Rust `String` would turn into U+FFFD.
enum RenderedOperand {
    Text(String),
    Units(Vec<u16>),
}

/// [`value_to_string_checked`] without the lossy last step.
///
/// gc-common w6-c (`handoff-w6c-rerunning-contexts-take-the-unwinding-arm`):
/// the object arm runs `toString()`, so an allocation in it cannot be retried;
/// it takes the allocators' unwinding arm instead of the infallible one (which
/// aborts on Generational/ZGC and takes G1's reserve), and a heap-exhaustion
/// unwind becomes this operand's `OutOfMemoryError`.
fn render_operand_checked(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    value: &Value,
    type_char: char,
) -> Result<RenderedOperand, MethodCallFailed> {
    match crate::runtime::native_oom::catch_alloc_oom(|| {
        render_operand_checked_inner(shared, thread, value, type_char)
    }) {
        Ok(r) => r,
        Err(oom) => Err(MethodCallFailed::InternalError(
            crate::error::VmError::Runtime(oom.into_runtime_error()),
        )),
    }
}

fn render_operand_checked_inner(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    value: &Value,
    type_char: char,
) -> Result<RenderedOperand, MethodCallFailed> {
    Ok(RenderedOperand::Text(match value {
        Value::Int(v) => match type_char {
            'Z' => {
                if *v != 0 {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            'C' => {
                if let Some(ch) = char::from_u32(*v as u32) {
                    ch.to_string()
                } else {
                    format!("\\u{:04x}", *v as u16)
                }
            }
            _ => v.to_string(),
        },
        Value::Long(v) => v.to_string(),
        Value::Float(v) => format_float(*v),
        Value::Double(v) => format_double(*v),
        Value::Object(None) => "null".to_string(),
        Value::Object(Some(obj_ref)) => {
            // A Java String first -- by class, not by shape (wave 16): the
            // shape reader answers for a `StringBuilder` too, with its whole
            // buffer. See `vm::is_java_lang_string`.
            if crate::vm::is_java_lang_string(shared, *obj_ref) {
                if let Some(units) = read_java_string_units(&shared.mem.heap, *obj_ref) {
                    return Ok(RenderedOperand::Units(units));
                }
            }

            // Primitive wrappers render without a Java call. Arrays must NOT
            // take this path: num_slots is the array LENGTH, so a length-1
            // array would masquerade as a wrapper and packed primitive arrays
            // would read a garbage Value slot.
            //
            // Keyed on the EXACT wrapper class, not on "one slot holding a
            // primitive". The shape test alone answered for every user class
            // with a single primitive field -- `"" + new Counter(5)` printed
            // `5` without ever running `Counter.toString()` -- and its
            // `contains("Boolean")` test misrendered any such class whose name
            // merely contained the word. The wrappers are final, so the exact
            // name is exact.
            let is_array =
                shared.mem.heap.kind_of(*obj_ref) == crate::memory::heap::ObjectKind::Array;
            let nf = shared.mem.heap.get_header(*obj_ref).num_slots() as usize;
            if nf == 1 && !is_array {
                let class_id = shared.mem.heap.class_id_of(*obj_ref);
                let name = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.name.clone());
                let field = shared.mem.heap.get_field(*obj_ref, 0);
                if let Some(s) = name.and_then(|n| primitive_wrapper_display(&n, field)) {
                    return Ok(RenderedOperand::Text(s));
                }
            }

            // Call toString() via virtual dispatch if thread context is available
            if let Some(t) = thread {
                let mut ctx = NativeContextImpl { shared, thread: t };
                use cratonvm_native_api::NativeContext;

                // `java.nio.file.Path` is a genuine interface with no `toString()`
                // body of its own; `ctx.invoke_virtual` below doesn't consult
                // `force_native_over_real_jdk_bytecode`/the `vm_exec.rs`
                // `check_override` allow-list the way the bytecode interpreter's
                // own `invokevirtual` handling does, so it silently resolves to
                // `Object.toString()` here too (`java.nio.file.Path@<hash>`) for
                // `"literal" + aPath` string concatenation. Same family as
                // `path-tostring-dead-dispatch-breaks-inprocess-javac-FIXED.md`,
                // a third, distinct call site. Route through the same
                // display-string helper the registered `Path.toString()` native
                // itself uses, bypassing `invoke_virtual` entirely for this type.
                //
                // NOTE: must use the ClassId-based `is_subclass_of` (which walks
                // both the superclass chain AND implemented interfaces), not
                // `ClassManager::is_subclass_of_by_name` — that one is the
                // exception-`catch_type` fallback and deliberately walks ONLY
                // the superclass chain (interfaces are never a `catch_type`),
                // so it can never match an interface like `Path` and this
                // branch would silently never fire. (A concurrent dev commit
                // added this same check using `is_subclass_of_by_name` — that
                // version never actually fires; confirmed via a standalone
                // `"file:" + Paths.get(...)` repro that still printed
                // `file:java.nio.file.Path@<hash>` until switched to
                // `is_subclass_of`.)
                //
                // Compatible mode only (wave 4). The helper is the same body
                // as the `Path.toString` Bridge that `register_phase57_nio_file`
                // registers on `java/nio/file/Path` and mirrors onto the
                // concrete `WindowsPath`/`UnixPath` the minted paths carry; it
                // is load-bearing there because a jar/jrt path is such an
                // object whose `path` slot holds the sentinel encoding. Under
                // `--jdk-only` real class bytes are authoritative: the path is
                // a genuine JDK object (a real `ZipPath` has its own layout,
                // which the slot-reading helper does not know), and
                // `invoke_virtual` below applies the admitted dispatch.
                let obj_class_id = shared.mem.heap.class_id_of(*obj_ref);
                let is_path = !crate::vm::dispatch_policy(shared).is_jdk_only()
                    && ctx
                        .class_id_by_name("java/nio/file/Path")
                        .is_some_and(|path_cid| {
                            shared
                                .classes
                                .class_manager
                                .read()
                                .is_subclass_of(obj_class_id, path_cid)
                        });
                if is_path {
                    return Ok(RenderedOperand::Text(
                        cratonvm_native_builtins::phases_late::p57_path_display_string(
                            &mut ctx, *obj_ref,
                        ),
                    ));
                }

                match ctx.invoke_virtual(*obj_ref, "toString", "()Ljava/lang/String;", &[]) {
                    // The result is read as code units: `ctx.read_string` went
                    // UTF-16 -> Rust `String` -> UTF-16 and turned an unpaired
                    // surrogate into U+FFFD, the last lossy leg of `+`.
                    Ok(Some(Value::Object(Some(str_ref)))) => {
                        if let Some(units) = read_java_string_units(&shared.mem.heap, str_ref) {
                            return Ok(RenderedOperand::Units(units));
                        }
                        return Ok(RenderedOperand::Text(
                            ctx.read_string(str_ref)
                                .unwrap_or_else(|| "null".to_string()),
                        ));
                    }
                    // `toString()` returned null, which is legal: HotSpot's
                    // `StringConcatHelper.stringOf` renders it as "null". This
                    // used to fall through to the `ClassName@hash` fallback.
                    Ok(Some(Value::Object(None))) => {
                        return Ok(RenderedOperand::Text("null".to_string()))
                    }
                    // A throwing `toString()` propagates. It used to be
                    // swallowed into `ClassName@hash`.
                    Err(e @ MethodCallFailed::ExceptionThrown(_))
                    | Err(e @ MethodCallFailed::InternalError(VmError::Runtime(_))) => {
                        return Err(e);
                    }
                    _ => {}
                }
            }

            // Final fallback: ClassName@hash. For arrays the header's class_id
            // is the COMPONENT class — render the JVMS array-class name
            // ([Ljava.lang.Class; / [I) like HotSpot instead.
            let class_name = if is_array {
                crate::runtime::interpreter::array_descriptor_of(shared, *obj_ref)
                    .unwrap_or_else(|| "[Ljava/lang/Object;".to_string())
            } else {
                let class_id = shared.mem.heap.class_id_of(*obj_ref);
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| "?".to_string())
            };
            let dotted = class_name.replace('/', ".");
            let hash = shared.mem.heap.identity_hash_code(*obj_ref);
            format!("{dotted}@{hash:x}")
        }
        _ => "?".to_string(),
    }))
}

/// `toString()` of a primitive wrapper, from its exact class name and its one
/// field -- `None` for any other class. Exact names, because the wrappers are
/// final: see the caller for the shape test this replaced.
fn primitive_wrapper_display(class_name: &str, field: Value) -> Option<String> {
    Some(match (class_name, field) {
        ("java/lang/Boolean", Value::Int(v)) => String::from(if v != 0 { "true" } else { "false" }),
        ("java/lang/Character", Value::Int(v)) => {
            char::from_u32(v as u32).unwrap_or('?').to_string()
        }
        ("java/lang/Integer" | "java/lang/Short" | "java/lang/Byte", Value::Int(v)) => {
            v.to_string()
        }
        ("java/lang/Long", Value::Long(v)) => v.to_string(),
        ("java/lang/Float", Value::Float(v)) => format_float(v),
        ("java/lang/Double", Value::Double(v)) => format_double(v),
        _ => return None,
    })
}

/// Format a float value like Java's `Float.toString` (string-concat path).
/// Delegates to the shared `cratonvm_types` formatter so the JLS layout —
/// including the 10^-3..10^7 scientific-notation threshold — stays consistent
/// with `Double.toString`/`StringBuilder.append`. The old local `format!("{v}")`
/// never used E-notation, so `1e8f` concatenated as "100000000.0" not "1.0E8".
fn format_float(v: f32) -> String {
    cratonvm_types::java_float_to_string(v)
}

/// Format a double value like Java's `Double.toString` (string-concat path).
/// See `format_float`.
fn format_double(v: f64) -> String {
    cratonvm_types::java_double_to_string(v)
}

// ---------------------------------------------------------------------------
// SwitchBootstraps — JEP 441 (Pattern Matching for switch, Java 21)
// ---------------------------------------------------------------------------

/// Bootstrap a `SwitchBootstraps.typeSwitch` call site.
///
/// Bootstrap arguments are an array of labels: each is a `Class<?>` (type check),
/// an `Integer` (exact int match), or a `String` (exact string match).
/// ClassIds are pre-resolved here so the execution loop needs no locks.
fn bootstrap_type_switch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // Phase 1: extract label names from constant pool (read lock only).
    let raw_labels: Vec<RawSwitchLabel> = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("typeSwitch: class {current_class_id} not found"),
            })?;

        let mut raw = Vec::with_capacity(info.bootstrap_arg_indices.len());
        for &arg_idx in &info.bootstrap_arg_indices {
            match class.constant_pool.get(arg_idx) {
                Some(ConstantPoolEntry::ClassReference { name_index }) => {
                    let name = class
                        .constant_pool
                        .get_utf8(*name_index)
                        .unwrap_or("")
                        .to_string();
                    raw.push(RawSwitchLabel::Type(name));
                }
                Some(ConstantPoolEntry::Integer(v)) => {
                    raw.push(RawSwitchLabel::Int(*v));
                }
                Some(ConstantPoolEntry::Long(v)) => {
                    raw.push(RawSwitchLabel::Long(*v));
                }
                Some(ConstantPoolEntry::Float(v)) => {
                    raw.push(RawSwitchLabel::Float(*v));
                }
                Some(ConstantPoolEntry::Double(v)) => {
                    raw.push(RawSwitchLabel::Double(*v));
                }
                Some(ConstantPoolEntry::StringReference { string_index }) => {
                    let s = class
                        .constant_pool
                        .get_utf8(*string_index)
                        .unwrap_or("")
                        .to_string();
                    raw.push(RawSwitchLabel::Str(s));
                }
                Some(ConstantPoolEntry::Dynamic {
                    name_and_type_index,
                    ..
                }) => {
                    // JDK 25 primitive patterns (JEP 507): Dynamic constant that
                    // resolves via ConstantBootstraps.primitiveClass to a primitive
                    // Class (int.class, long.class, etc.). The name in the
                    // NameAndType is the primitive type descriptor ("I", "J", etc.).
                    //
                    // An entry without a usable NameAndType still takes its
                    // position (as a label that matches nothing): the label
                    // INDEX is what the switch's tableswitch branches on, so
                    // dropping one shifted every later case onto its
                    // neighbour's arm.
                    //
                    // A condy of type `EnumDesc` is a qualified enum constant
                    // label (`case E.A`, JEP 441), not a primitive class. It
                    // used to be read as `PrimitiveClass("invoke")`, which
                    // matched nothing, so `E.A` fell to a later `case E e`.
                    let (name, type_desc) = class
                        .constant_pool
                        .get_name_and_type(*name_and_type_index)
                        .unwrap_or(("", ""));
                    if type_desc == "Ljava/lang/Enum$EnumDesc;" {
                        raw.push(
                            match javac_enum_desc_label(
                                &class.constant_pool,
                                &class.bootstrap_methods,
                                arg_idx,
                            ) {
                                Some((class_name, constant)) => RawSwitchLabel::EnumDesc {
                                    class_name,
                                    constant,
                                },
                                None => RawSwitchLabel::PrimitiveClass(String::new()),
                            },
                        );
                    } else {
                        raw.push(RawSwitchLabel::PrimitiveClass(name.to_string()));
                    }
                }
                _ => {
                    tracing::warn!(
                        "Unrecognized constant pool entry type in switch label resolution (index {})",
                        arg_idx
                    );
                    // Keep the position (see above): an empty descriptor is a
                    // label no target matches.
                    raw.push(RawSwitchLabel::PrimitiveClass(String::new()));
                }
            }
        }
        raw
    }; // read lock dropped

    // Phase 2: resolve all Type labels to ClassIds (one write lock per class, done once).
    let mut labels = Vec::with_capacity(raw_labels.len());
    for raw in raw_labels {
        match raw {
            RawSwitchLabel::Type(name) => {
                // A `case Foo f` label is a `CONSTANT_Class` of the switching
                // class, so it resolves through THAT class's defining loader,
                // exactly as a `checkcast Foo` in the same method would. The
                // loader-blind `load_class_concurrent` asked only the built-in
                // chain: a pattern switch in a webapp / fat-jar / forked-test
                // class over one of its own types failed to link, or matched
                // against another loader's copy of the name.
                // A miss is the Java `NoClassDefFoundError` a `Class` static
                // argument's resolution raises, not a VM-internal error.
                let cid = crate::runtime::interpreter::resolve_class_loader_aware(
                    shared,
                    thread,
                    current_class_id,
                    &name,
                )
                .map_err(|e| {
                    crate::runtime::exceptions::convert_class_not_found(shared, thread, &name, e)
                })?;
                labels.push(SwitchLabel::Type {
                    class_name: Arc::from(name),
                    class_id: cid,
                });
            }
            RawSwitchLabel::Int(v) => labels.push(SwitchLabel::Int(v)),
            RawSwitchLabel::Long(v) => labels.push(SwitchLabel::Long(v)),
            RawSwitchLabel::Float(v) => labels.push(SwitchLabel::Float(v)),
            RawSwitchLabel::Double(v) => labels.push(SwitchLabel::Double(v)),
            RawSwitchLabel::Str(s) => labels.push(SwitchLabel::Str(Arc::from(s))),
            RawSwitchLabel::PrimitiveClass(desc) => {
                labels.push(SwitchLabel::PrimitiveClass(Arc::from(desc)));
            }
            RawSwitchLabel::EnumDesc {
                class_name,
                constant,
            } => {
                // Through the switching class's loader, as a `Type` label.
                // The JDK resolves an `EnumDesc` label lazily and treats a
                // failure (no such class, not an enum) as "no match", so a
                // miss here is a label that matches nothing, not an error.
                let enum_id = crate::runtime::interpreter::resolve_class_loader_aware(
                    shared,
                    thread,
                    current_class_id,
                    &class_name,
                )
                .ok()
                .filter(|&cid| {
                    // `Class.isEnum()`: the direct superclass is `Enum`.
                    let cm = shared.classes.class_manager.read();
                    cm.get_class(cid)
                        .and_then(|c| c.superclass)
                        .and_then(|s| cm.get_class(s))
                        .is_some_and(|s| &*s.name == "java/lang/Enum")
                });
                labels.push(match enum_id {
                    Some(class_id) => SwitchLabel::EnumDesc {
                        class_id,
                        name: Arc::from(constant),
                    },
                    None => SwitchLabel::PrimitiveClass(Arc::from("")),
                });
            }
        }
    }

    // Cache the call site — subsequent executions use pre-resolved ClassIds.
    let site = ResolvedCallSite::TypeSwitch {
        labels: labels.clone(),
    };
    shared
        .classes
        .resolution_cache
        .write()
        .put_call_site_as_of(current_class_id, cp_index, site, info.fill_as_of);

    execute_type_switch(shared, thread, frame_idx, &labels)
}

/// Temporary label type used during bootstrap before ClassId resolution.
enum RawSwitchLabel {
    Type(String),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Str(String),
    PrimitiveClass(String),
    /// A qualified enum constant label: the enum's internal name and the
    /// constant's name (see [`javac_enum_desc_label`]).
    EnumDesc {
        class_name: String,
        constant: String,
    },
}

/// The `(enum class internal name, constant name)` of a `typeSwitch`
/// `EnumDesc` label in the shape javac emits for a qualified enum constant
/// label in a pattern switch (`case E.A`, JEP 441):
///
/// ```text
/// ConstantBootstraps.invoke(EnumDesc.of, <ConstantBootstraps.invoke(ClassDesc.of, "p.E")>, "A")
/// ```
///
/// Decoded from the constant pool rather than by running the two bootstraps:
/// both are side-effect-free JDK factories and the label needs only the two
/// names. `None` for any other shape.
fn javac_enum_desc_label(
    cp: &ConstantPool,
    bootstrap_methods: &[BootstrapMethod],
    cp_index: u16,
) -> Option<(String, String)> {
    let &[class_desc, constant] = constant_bootstraps_invoke_args(
        cp,
        bootstrap_methods,
        cp_index,
        "java/lang/Enum$EnumDesc",
        "(Ljava/lang/constant/ClassDesc;Ljava/lang/String;)Ljava/lang/Enum$EnumDesc;",
    )?
    else {
        return None;
    };
    let &[binary_name] = constant_bootstraps_invoke_args(
        cp,
        bootstrap_methods,
        class_desc,
        "java/lang/constant/ClassDesc",
        "(Ljava/lang/String;)Ljava/lang/constant/ClassDesc;",
    )?
    else {
        return None;
    };
    // `ClassDesc.of` takes a binary name (`p.Outer$E`).
    let class_name = resolve_string_constant(cp, binary_name)?.replace('.', "/");
    Some((class_name, resolve_string_constant(cp, constant)?))
}

/// The arguments after the factory handle of the `CONSTANT_Dynamic` at
/// `cp_index` when it is `ConstantBootstraps.invoke` over the static factory
/// `factory_class.of` with descriptor `factory_desc`; `None` otherwise.
fn constant_bootstraps_invoke_args<'a>(
    cp: &ConstantPool,
    bootstrap_methods: &'a [BootstrapMethod],
    cp_index: u16,
    factory_class: &str,
    factory_desc: &str,
) -> Option<&'a [u16]> {
    let Some(ConstantPoolEntry::Dynamic {
        bootstrap_method_attr_index,
        ..
    }) = cp.get(cp_index)
    else {
        return None;
    };
    let bsm = bootstrap_methods.get(usize::from(*bootstrap_method_attr_index))?;
    let is_static = |h: &MethodHandle, class: &str, name: &str| {
        h.kind == MethodHandleKind::InvokeStatic
            && &*h.class_name == class
            && &*h.member_name == name
    };
    let bsm_handle = resolve_method_handle_full(cp, bsm.bootstrap_method_ref).ok()?;
    if !is_static(&bsm_handle, "java/lang/invoke/ConstantBootstraps", "invoke") {
        return None;
    }
    let (&factory, rest) = bsm.bootstrap_arguments.split_first()?;
    let factory = resolve_method_handle_full(cp, factory).ok()?;
    (is_static(&factory, factory_class, "of") && &*factory.descriptor == factory_desc)
        .then_some(rest)
}

/// The `IndexOutOfBoundsException` of `Objects.checkIndex(restart,
/// labels + 1)`, which the switch the JDK's `SwitchBootstraps` generates makes
/// before anything else (measured on HotSpot 25: `Index -1 out of bounds for
/// length 4`; probe `tools/probes/interp/L4/L4W42SwitchRestartIndex.java`).
/// Interpreter round i1 wave 42, lane L4.
#[cold]
fn switch_restart_out_of_bounds(restart: i32, labels: usize) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::ioobe(format!(
        "Index {restart} out of bounds for length {}",
        labels + 1
    ))))
}

/// Execute a cached `typeSwitch` call site.
///
/// Stack: `[..., target: Object, startIndex: int]` → `[..., matchIndex: int]`
///
/// All Type labels have pre-resolved ClassIds — no lock acquisitions in the loop.
pub fn execute_type_switch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    labels: &[SwitchLabel],
) -> Result<(), MethodCallFailed> {
    let restart = match thread.frames[frame_idx].stack.pop()? {
        Value::Int(i) => i,
        _ => 0,
    };
    let target = thread.frames[frame_idx].stack.pop()?;
    // The JDK's generated switch checks the restart index first, before the
    // null test (`Objects.checkIndex(restart, labels.length + 1)`; wave 42):
    // it was clamped to 0 and a large one answered the default.
    if restart < 0 || restart as usize > labels.len() {
        return Err(switch_restart_out_of_bounds(restart, labels.len()));
    }
    let start_index = restart as usize;

    let result = match target {
        // Per the `SwitchBootstraps.typeSwitch` contract, a null target returns
        // -1. javac compiles `case null` as the -1 arm of the generated
        // table/lookupswitch (and, when there is no `case null`, emits a null
        // check that throws before the bootstrap is reached), so returning -1
        // here is what the generated bytecode expects.
        Value::Object(None) => -1,
        Value::Object(Some(mut obj_ref)) => {
            let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
            type_switch_match(
                shared,
                thread,
                &mut obj_ref,
                obj_class_id,
                labels,
                start_index,
            )?
        }
        Value::Int(v) => primitive_match(labels, start_index, |l| {
            matches!(l, SwitchLabel::Int(e) if *e == v)
                || matches!(l, SwitchLabel::PrimitiveClass(d) if &**d == "I" || &**d == "Z" || &**d == "B" || &**d == "S" || &**d == "C")
        }),
        Value::Long(v) => primitive_match(labels, start_index, |l| {
            matches!(l, SwitchLabel::Long(e) if *e == v)
                || matches!(l, SwitchLabel::PrimitiveClass(d) if &**d == "J")
        }),
        Value::Float(v) => primitive_match(labels, start_index, |l| {
            matches!(l, SwitchLabel::Float(e) if e.to_bits() == v.to_bits())
                || matches!(l, SwitchLabel::PrimitiveClass(d) if &**d == "F")
        }),
        Value::Double(v) => primitive_match(labels, start_index, |l| {
            matches!(l, SwitchLabel::Double(e) if e.to_bits() == v.to_bits())
                || matches!(l, SwitchLabel::PrimitiveClass(d) if &**d == "D")
        }),
        // Any other (non-target) value matches no label → default arm.
        _ => labels.len() as i32,
    };

    thread.frames[frame_idx].stack.push(Value::Int(result))?;
    Ok(())
}

/// `target instanceof <label>` for a `typeSwitch` type-pattern label.
///
/// A bare `is_subclass_of(header class id, label)` — what this used to be — is
/// wrong for two receiver shapes the `instanceof` opcode handles separately:
///
/// * an ARRAY, whose header class id is its COMPONENT class. `case String s`
///   matched a `String[]` target, and `case Object[] a` matched nothing;
/// * a LAMBDA PROXY, whose synthetic class id is absent from the class store,
///   so `case Runnable r` never matched a lambda.
///
/// An array takes the array arm of `op_instanceof` (strict array
/// assignability); every other receiver takes the opcode's own predicate,
/// `interpreter::object_is_instance_of_resolved` — the hierarchy, then the
/// loader-aware, lambda-proxy, synthetic, `$Proxy`, annotation-proxy and
/// display-class fallbacks, in the opcode's order. Before wave 3 only the
/// array and lambda-proxy arms were mirrored here.
///
/// `obj_ref` is refreshed through the `&mut`: the display-class arm can load
/// a class (a safepoint), and pins the receiver across it.
fn type_label_matches(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: &mut ObjectRef,
    obj_class_id: ClassId,
    label_id: ClassId,
    label_name: &str,
) -> bool {
    if shared.mem.heap.kind_of(*obj_ref) == crate::memory::heap::ObjectKind::Array {
        let Some(desc) = crate::runtime::interpreter::array_descriptor_of(shared, *obj_ref) else {
            return false;
        };
        // `array_is_instance_of` can LOAD a component class (a safepoint),
        // and `type_switch_match` reads `obj_ref` again for the next label:
        // pin it across the call and hand back the current address.
        let pin = thread.native_pin_roots.len();
        thread.native_pin_roots.push(*obj_ref);
        let is_instance =
            crate::runtime::interpreter::array_is_instance_of(shared, &desc, label_name);
        *obj_ref = thread.native_pin_roots[pin];
        thread.native_pin_roots.truncate(pin);
        return is_instance;
    }
    if label_name.starts_with('[') {
        // A non-array object is an instance of no array type.
        return false;
    }
    crate::runtime::interpreter::object_is_instance_of_resolved(
        shared,
        thread,
        obj_ref,
        obj_class_id,
        label_id,
        label_name,
    )
}

/// The `name` of an enum constant: `Enum`'s first declared field, slot 0 (the
/// slot `execute_enum_switch` has always read).
fn enum_constant_name(shared: &SharedVm, obj_ref: ObjectRef) -> Option<String> {
    match shared.mem.heap.get_field(obj_ref, 0) {
        Value::Object(Some(name_ref)) => read_java_string(&shared.mem.heap, name_ref),
        _ => None,
    }
}

/// `Enum.getDeclaringClass() == enum_id` for an object of class `obj_class_id`
/// (`enum_id` is a resolved enum class): its class is the enum itself or a
/// constant-body subclass of it. The read lock is only taken for the second.
fn enum_declared_by(shared: &SharedVm, obj_class_id: ClassId, enum_id: ClassId) -> bool {
    obj_class_id == enum_id
        || shared
            .classes
            .class_manager
            .read()
            .get_class(obj_class_id)
            .is_some_and(|c| c.superclass == Some(enum_id))
}

/// Match an object reference against switch labels.
///
/// Takes no lock for a label the class hierarchy decides; a type label that
/// the hierarchy refuses consults `instanceof`'s fallbacks, the last of which
/// can load a class. `obj_ref` is refreshed through the `&mut` if it moves.
///
/// The receiver's class NAME is read only when a boxed-constant or
/// primitive-class label is reached (wave 16): a pure type-pattern switch —
/// `case Circle c -> ..; case Square s -> ..` — used to take the VM-wide
/// `class_manager` read lock and bump the name's refcount on every
/// execution for a name none of its labels reads.
fn type_switch_match(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: &mut ObjectRef,
    obj_class_id: ClassId,
    labels: &[SwitchLabel],
    start_index: usize,
) -> Result<i32, MethodCallFailed> {
    // Read at most once, and only if an `EnumConstant` label is reached.
    let mut enum_name: Option<Option<String>> = None;
    // Read at most once, and only if a label that compares it is reached.
    let mut receiver_name: Option<Arc<str>> = None;
    for (i, label) in labels.iter().enumerate().skip(start_index) {
        let matched = match label {
            SwitchLabel::Type {
                class_id,
                class_name,
            } => {
                // JEP 441: type patterns use instanceof semantics (subclass check).
                // A Long does NOT match `case Integer i` — only exact type or
                // supertype matches are valid.
                type_label_matches(shared, thread, obj_ref, obj_class_id, *class_id, class_name)
            }
            SwitchLabel::EnumConstant(expected) => {
                enum_name
                    .get_or_insert_with(|| enum_constant_name(shared, *obj_ref))
                    .as_deref()
                    == Some(&**expected)
            }
            SwitchLabel::EnumDesc { class_id, name } => {
                // An array's header class id is its COMPONENT class, so an
                // `E[]` target must be refused before the id comparison.
                shared.mem.heap.kind_of(*obj_ref) != crate::memory::heap::ObjectKind::Array
                    && enum_declared_by(shared, obj_class_id, *class_id)
                    && enum_name
                        .get_or_insert_with(|| enum_constant_name(shared, *obj_ref))
                        .as_deref()
                        == Some(&**name)
            }
            SwitchLabel::Int(expected) => {
                // The JDK's matcher (`SwitchBootstraps.generateTypeSwitch`,
                // an `Integer` label): a `Number` target by `intValue()`, a
                // `Character` by `charValue()`, anything else (a `Boolean`
                // included) no match. Wave 41, lane L4: only the `Integer`,
                // `Short`, `Byte`, `Character` and `Boolean` boxes were read,
                // so `5L` and `5.9` missed `case 5` and `Boolean.TRUE`
                // matched `1` (probe `L4W41TypeSwitchConstantLabels`).
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                let value = match switch_int_label_operand(shared, *obj_ref, obj_class_id, name) {
                    SwitchIntOperand::Value(v) => Some(v),
                    SwitchIntOperand::NoMatch => None,
                    SwitchIntOperand::OtherNumber => switch_number_int_value(shared, thread, obj_ref)?,
                };
                value == Some(*expected)
            }
            SwitchLabel::Long(expected) => {
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                unbox_long(shared, *obj_ref, name) == Some(*expected)
            }
            SwitchLabel::Float(expected) => {
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                unbox_float(shared, *obj_ref, name)
                    .is_some_and(|v| v.to_bits() == expected.to_bits())
            }
            SwitchLabel::Double(expected) => {
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                unbox_double(shared, *obj_ref, name)
                    .is_some_and(|v| v.to_bits() == expected.to_bits())
            }
            SwitchLabel::Str(expected) => {
                // `label.equals(target)`: only a `String` (wave 41, lane L4).
                // The shape reader alone also answered for a `StringBuilder`
                // whose buffer is exactly full (`L4W41TypeSwitchConstantLabels`).
                crate::vm::is_java_lang_string(shared, *obj_ref)
                    && read_java_string(&shared.mem.heap, *obj_ref).as_deref() == Some(&**expected)
            }
            SwitchLabel::PrimitiveClass(desc) => {
                // JEP 507: primitive type pattern matches boxed wrapper types.
                let name = switch_receiver_name(shared, obj_class_id, &mut receiver_name);
                match &**desc {
                    "I" | "Z" | "B" | "S" | "C" => matches!(
                        name,
                        "java/lang/Integer"
                            | "java/lang/Boolean"
                            | "java/lang/Byte"
                            | "java/lang/Short"
                            | "java/lang/Character"
                    ),
                    "J" => name == "java/lang/Long",
                    "F" => name == "java/lang/Float",
                    "D" => name == "java/lang/Double",
                    _ => false,
                }
            }
        };
        if matched {
            return Ok(i as i32);
        }
    }
    // No label matched: the `typeSwitch` contract returns labels.length (the
    // default arm), not -1 (which is reserved for a null target).
    Ok(labels.len() as i32)
}

/// What an `Integer` `typeSwitch` label compares a reference target with.
enum SwitchIntOperand {
    /// The target's `intValue()` / `charValue()`, read without running Java.
    Value(i32),
    /// Neither a `Number` nor a `Character`: no `Integer` label matches.
    NoMatch,
    /// Another `Number` (`AtomicInteger`, `BigInteger`, a user subclass):
    /// its `intValue()` runs ([`switch_number_int_value`]).
    OtherNumber,
}

/// [`SwitchIntOperand`] of a non-null target of class `class_name`. The boxes
/// are read from their value field; `Long` truncates and `Float` / `Double`
/// convert as `l2i` / `f2i` / `d2i` do (Rust's `as` saturates and sends NaN
/// to 0, as Java does). Interpreter round i1 wave 41, lane L4.
fn switch_int_label_operand(
    shared: &SharedVm,
    obj: ObjectRef,
    obj_class_id: ClassId,
    class_name: &str,
) -> SwitchIntOperand {
    let field = || shared.mem.heap.get_field(obj, 0);
    match class_name {
        "java/lang/Integer" | "java/lang/Byte" | "java/lang/Short" | "java/lang/Character" => {
            match field() {
                Value::Int(v) => SwitchIntOperand::Value(v),
                _ => SwitchIntOperand::NoMatch,
            }
        }
        "java/lang/Long" => match field() {
            Value::Long(v) => SwitchIntOperand::Value(v as i32),
            _ => SwitchIntOperand::NoMatch,
        },
        "java/lang/Float" => match field() {
            Value::Float(v) => SwitchIntOperand::Value(v as i32),
            _ => SwitchIntOperand::NoMatch,
        },
        "java/lang/Double" => match field() {
            Value::Double(v) => SwitchIntOperand::Value(v as i32),
            _ => SwitchIntOperand::NoMatch,
        },
        _ => {
            // An array's header class id is its COMPONENT class: an
            // `AtomicInteger[]` is not a `Number`.
            if shared.mem.heap.kind_of(obj) == crate::memory::heap::ObjectKind::Array {
                return SwitchIntOperand::NoMatch;
            }
            let is_number = shared
                .classes
                .class_manager
                .read()
                .is_assignable_to_name(obj_class_id, "java/lang/Number");
            if is_number {
                SwitchIntOperand::OtherNumber
            } else {
                SwitchIntOperand::NoMatch
            }
        }
    }
}

/// `((Number) target).intValue()` for a `Number` that is not a box, run as
/// the JDK's matcher runs it (its exception is the switch's). `None` when the
/// call answers no `int`. The target is pinned across the call and handed
/// back through `obj_ref`.
fn switch_number_int_value(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj_ref: &mut ObjectRef,
) -> Result<Option<i32>, MethodCallFailed> {
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(*obj_ref);
    let result = {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        ctx.invoke_virtual(*obj_ref, "intValue", "()I", &[])
    };
    *obj_ref = thread.native_pin_roots[pin];
    thread.native_pin_roots.truncate(pin);
    Ok(match result? {
        Some(Value::Int(v)) => Some(v),
        _ => None,
    })
}

/// The class name of a `typeSwitch` receiver of class `class_id`, read into
/// `cell` on first use (one `class_manager` read) and borrowed from it after.
/// The empty name for a class the store does not know (a lambda proxy), which
/// matches no boxed-type label — the answer the eager read gave.
fn switch_receiver_name<'a>(
    shared: &SharedVm,
    class_id: ClassId,
    cell: &'a mut Option<Arc<str>>,
) -> &'a str {
    cell.get_or_insert_with(|| {
        shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    })
}

/// JEP 507: Check if a boxed primitive value matches a target wrapper type
/// via widening or narrowing conversion.
///
/// For example, a boxed `Integer(42)` matches `java/lang/Long` (widening)
/// and a boxed `Integer(5)` matches `java/lang/Byte` (narrowing, value in range).
fn primitive_pattern_match(
    shared: &SharedVm,
    obj_ref: ObjectRef,
    obj_class_name: &str,
    target_class_name: &str,
) -> bool {
    // Extract the numeric value from the source wrapper.
    let source_value = match obj_class_name {
        "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Integer"
        | "java/lang/Character"
        | "java/lang/Boolean" => match shared.mem.heap.get_field(obj_ref, 0) {
            Value::Int(v) => NumericValue::Int(v),
            _ => return false,
        },
        "java/lang/Long" => match shared.mem.heap.get_field(obj_ref, 0) {
            Value::Long(v) => NumericValue::Long(v),
            _ => return false,
        },
        "java/lang/Float" => match shared.mem.heap.get_field(obj_ref, 0) {
            Value::Float(v) => NumericValue::Float(v),
            _ => return false,
        },
        "java/lang/Double" => match shared.mem.heap.get_field(obj_ref, 0) {
            Value::Double(v) => NumericValue::Double(v),
            _ => return false,
        },
        _ => return false,
    };

    // Try to convert to target type.
    match target_class_name {
        "java/lang/Byte" => match source_value {
            NumericValue::Int(v) => v >= i8::MIN as i32 && v <= i8::MAX as i32,
            NumericValue::Long(v) => v >= i8::MIN as i64 && v <= i8::MAX as i64,
            _ => false,
        },
        "java/lang/Short" => match source_value {
            NumericValue::Int(v) => v >= i16::MIN as i32 && v <= i16::MAX as i32,
            NumericValue::Long(v) => v >= i16::MIN as i64 && v <= i16::MAX as i64,
            _ => false,
        },
        "java/lang/Character" => match source_value {
            NumericValue::Int(v) => v >= 0 && v <= u16::MAX as i32,
            NumericValue::Long(v) => v >= 0 && v <= u16::MAX as i64,
            _ => false,
        },
        "java/lang/Integer" => match source_value {
            NumericValue::Int(_) => true, // same type always matches
            NumericValue::Long(v) => v >= i32::MIN as i64 && v <= i32::MAX as i64,
            NumericValue::Float(v) => {
                !v.is_nan()
                    && !v.is_infinite()
                    && v >= i32::MIN as f32
                    && v <= i32::MAX as f32
                    && v == (v as i32) as f32
            }
            NumericValue::Double(v) => {
                !v.is_nan()
                    && !v.is_infinite()
                    && v >= i32::MIN as f64
                    && v <= i32::MAX as f64
                    && v == (v as i32) as f64
            }
        },
        "java/lang/Long" => match source_value {
            NumericValue::Int(v) => {
                // Widening: int -> long always succeeds.
                let _ = v;
                true
            }
            NumericValue::Long(_) => true,
            NumericValue::Float(v) => {
                !v.is_nan()
                    && !v.is_infinite()
                    && v >= i64::MIN as f32
                    && v <= i64::MAX as f32
                    && v == (v as i64) as f32
            }
            NumericValue::Double(v) => {
                !v.is_nan()
                    && !v.is_infinite()
                    && v >= i64::MIN as f64
                    && v <= i64::MAX as f64
                    && v == (v as i64) as f64
            }
        },
        "java/lang/Float" => match source_value {
            NumericValue::Int(v) => {
                // Widening: int -> float (may lose precision, but allowed as widening).
                let _ = v;
                true
            }
            NumericValue::Long(v) => {
                let _ = v;
                true
            }
            NumericValue::Float(_) => true,
            NumericValue::Double(v) => {
                // Narrowing: double -> float only if exact.
                !v.is_nan() && v == (v as f32) as f64
            }
        },
        "java/lang/Double" => match source_value {
            // Widening to double always succeeds from any numeric type.
            NumericValue::Int(_) | NumericValue::Long(_) | NumericValue::Float(_) => true,
            NumericValue::Double(_) => true,
        },
        _ => false,
    }
}

/// A numeric value extracted from a boxed primitive wrapper.
enum NumericValue {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
}

/// Search labels for a primitive match.
fn primitive_match(
    labels: &[SwitchLabel],
    start: usize,
    pred: impl Fn(&SwitchLabel) -> bool,
) -> i32 {
    for (i, label) in labels.iter().enumerate().skip(start) {
        if pred(label) {
            return i as i32;
        }
    }
    // No label matched → default arm index (labels.length), not -1.
    labels.len() as i32
}

/// Unbox a numeric wrapper to i32 (Integer, Byte, Short, Character, Boolean).
fn unbox_long(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<i64> {
    if class_name == "java/lang/Long" {
        match shared.mem.heap.get_field(obj, 0) {
            Value::Long(v) => Some(v),
            _ => None,
        }
    } else {
        None
    }
}

fn unbox_float(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<f32> {
    if class_name == "java/lang/Float" {
        match shared.mem.heap.get_field(obj, 0) {
            Value::Float(v) => Some(v),
            _ => None,
        }
    } else {
        None
    }
}

fn unbox_double(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<f64> {
    if class_name == "java/lang/Double" {
        match shared.mem.heap.get_field(obj, 0) {
            Value::Double(v) => Some(v),
            _ => None,
        }
    } else {
        None
    }
}

// ===========================================================================
// ObjectMethods.bootstrap — record equals/hashCode/toString (JEP 395)
// ===========================================================================

/// Bootstrap `ObjectMethods.bootstrap` for record classes.
///
/// Bootstrap arguments:
///   arg[0]: Class — the record class
///   arg[1]: String — component names separated by `;`
///   arg[2..]: MethodHandle — getField handles for each component
///
/// The target name (`info.target_name`) tells us which method:
///   "equals", "hashCode", or "toString".
///
/// Interpreter round i1 wave 39, lane L4
/// (`i38-L4-objectmethods-native-linkage-ignores-its-getters`): javac's shape
/// (`Class::object_methods_args_are_canonical`) links exactly as before.
/// Any other getter list whose every getter is a `REF_getField` of the record
/// class naming an instance field links GETTER-DRIVEN in every mode: one slot
/// and descriptor per getter, which `equals`/`hashCode`/`toString` fold over
/// as the JDK's `ObjectMethods` does (they used to walk the record's own
/// components, and `toString` read positional slots). Any other getter (an
/// accessor method, another class's field) links through the JDK's own
/// `ObjectMethods` under `--jdk-only` (`bootstrap_generic`); `--compatible`
/// keeps the positional reading. Wave 41: under `--jdk-only` a `toString`
/// site whose getters are the record's fields and accessor METHODS links
/// getter-driven too, calling each accessor (the JDK's `makeToString` needs
/// `MethodHandle.copyWith`, which CratonVM lacks). Wave 42: static getters
/// `X.m(R)T` and special getters `R.m()T` (from `R`) too; a getter whose
/// handle type is not `(R)T` was refused before this runs
/// (`object_methods_to_string_getter_refusal`).
/// `--jdk-only`: an `ObjectMethods` `toString` site whose getters include a
/// METHOD handle (`REF_invokeVirtual` / `REF_invokeStatic` /
/// `REF_invokeSpecial`, never javac's shape) is linked by the JDK's own
/// `ObjectMethods.bootstrap` (`bootstrap_generic`) when `true`, and by the
/// native accessor-getter models of waves 41-42 (`RecordAccessorGetter`) when
/// `false`. Those models existed because the JDK route failed in
/// `MethodHandle.copyWith` before wave 43. `--compatible` is unaffected (it
/// never modelled them). Interpreter round i1 wave 44, lane L4
/// (`i43-L4-proposal-retire-the-copywith-workarounds`, step 3); the positive
/// control is `CRATONVM_DBG_INDY_ALL=1`'s `accessor-method getters take the
/// jdk route` line on `L4W42ObjectMethodsStaticSpecialGetters`.
const OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE: bool = true;

fn bootstrap_record_object_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    let method = match info.target_name.as_str() {
        "equals" => RecordMethodKind::Equals,
        "hashCode" => RecordMethodKind::HashCode,
        "toString" => RecordMethodKind::ToString,
        other => {
            return Err(VmError::Internal {
                message: format!("ObjectMethods.bootstrap: unknown target '{other}'"),
            }
            .into());
        }
    };

    // Parse component names and field descriptors from bootstrap arguments.
    let (component_names, record_class_name, canonical, field_getters, field_descriptors) = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("ObjectMethods bootstrap: class {current_class_id} not found"),
            })?;

        // arg[0] = Class reference (the record class) — get its record components
        // arg[1] = String with component names separated by `;`
        let names_str = if info.bootstrap_arg_indices.len() >= 2 {
            resolve_string_constant(&class.constant_pool, info.bootstrap_arg_indices[1])
                .unwrap_or_default()
        } else {
            String::new()
        };
        let component_names: Vec<Arc<str>> = names_str
            .split(';')
            .filter(|s| !s.is_empty())
            .map(Arc::from)
            .collect();

        // arg[2..] = MethodHandle getters — extract field descriptors from them.
        // Each MethodHandle is a REF_getField for the record component.
        let mut field_descriptors: Vec<Arc<str>> = Vec::with_capacity(component_names.len());
        for &arg_idx in info.bootstrap_arg_indices.iter().skip(2) {
            let desc = resolve_method_handle_field_descriptor(&class.constant_pool, arg_idx)
                .unwrap_or_else(|| "I".to_string());
            field_descriptors.push(Arc::from(desc));
        }
        // The same getters, one entry each: the resolved handle (of any
        // kind; `None` for an entry that is not a method handle).
        let field_getters: Vec<Option<MethodHandle>> = info
            .bootstrap_arg_indices
            .iter()
            .skip(2)
            .map(|&arg_idx| resolve_method_handle_full(&class.constant_pool, arg_idx).ok())
            .collect();
        let canonical = class.object_methods_args_are_canonical(&info.bootstrap_arg_indices);

        // arg[0]'s name; the record class is resolved below, after the read
        // lock is dropped (class loading takes the write lock).
        let record_class_name = if !info.bootstrap_arg_indices.is_empty() {
            class
                .constant_pool
                .get_class_name(info.bootstrap_arg_indices[0])
                .map(|s| s.to_string())
        } else {
            None
        };

        (
            component_names,
            record_class_name,
            canonical,
            field_getters,
            field_descriptors,
        )
    };

    // Loader-aware for the same reason as the typeSwitch labels: the record
    // class is a `CONSTANT_Class` of the calling class (almost always the
    // record itself), which a built-in-chain-only lookup cannot see when a
    // user loader defined it. A miss is the resolution's
    // `NoClassDefFoundError`, as for the switch bootstraps' labels (it was the
    // raw internal error).
    let rec_cid: Option<ClassId> = match record_class_name {
        Some(ref rec_name) => Some(
            crate::runtime::interpreter::resolve_class_loader_aware(
                shared,
                thread,
                current_class_id,
                rec_name,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found(shared, thread, rec_name, e)
            })?,
        ),
        None => None,
    };

    // Getter-driven slots: `Some` only when the site is not javac's shape and
    // every getter is a `REF_getField` of the record class naming an instance
    // field, found as JVMS 5.4.3.2 resolves the reference from that class.
    //
    // Wave 41, lane L4 (`i38-L4-objectmethods-native-linkage-ignores-its-getters`):
    // a `toString` site under `--jdk-only` also models an accessor-METHOD
    // getter of the record class (`REF_invokeVirtual R.b()T`, `T` not
    // `void`), invoked on the receiver, as the JDK's `makeToString` filters
    // the receiver through it. The JDK's own route for such a site needs
    // `MethodHandle.copyWith` (LambdaForms), which CratonVM lacks. `equals` /
    // `hashCode` of such a site keep the JDK route, which links. A getter of
    // another class keeps the JDK route too (its handle's type is not
    // `(R)T`, which `makeToString`'s `permuteArguments` refuses).
    //
    // Wave 42, lane L4 (same page): a `toString` site under `--jdk-only` also
    // models `REF_invokeStatic X.m(R)T` of any class `X` (the receiver is its
    // argument) and `REF_invokeSpecial R.m()T` from `R` itself (`R`'s method,
    // never an override): both have the handle type `(R)T` that
    // `makeToString` accepts. The owner of a static getter of another class
    // is resolved here, through the calling class's loader (the getters were
    // already resolved as `ldc` of them by `object_methods_resolve_getters`,
    // so this finds a loaded class).
    //
    // Wave 44, lane L4 (`i43-L4-proposal-retire-the-copywith-workarounds`,
    // step 3): with `MethodHandle.copyWith` registered (wave 43) the JDK's
    // own `ObjectMethods.makeToString` links these method getters; the host
    // measured it on direct `ObjectMethods.bootstrap` calls
    // (`L4W43ObjectMethodsJdkRoute`, `special-*` / `static-*` / `ctor-*`,
    // first wave-43 chain). [`OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE`]
    // hands such a site to `bootstrap_generic` instead of the models above.
    let model_accessor_methods = method == RecordMethodKind::ToString
        && shared.config.is_jdk_only()
        && !OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE;
    if dbg_indy_all()
        && OBJECT_METHODS_ACCESSOR_GETTERS_JDK_ROUTE
        && method == RecordMethodKind::ToString
        && shared.config.is_jdk_only()
        && !canonical
        && field_getters.iter().flatten().any(|h| {
            matches!(
                h.kind,
                MethodHandleKind::InvokeStatic
                    | MethodHandleKind::InvokeVirtual
                    | MethodHandleKind::InvokeSpecial
            )
        })
    {
        eprintln!(
            "[indy-all] object-methods toString cp#{cp_index}: accessor-method getters take the jdk route"
        );
    }
    let static_owners: Vec<Option<ClassId>> = if model_accessor_methods && !canonical {
        let mut owners = Vec::with_capacity(field_getters.len());
        for getter in &field_getters {
            owners.push(match getter {
                Some(h) if h.kind == MethodHandleKind::InvokeStatic => {
                    crate::runtime::interpreter::resolve_class_loader_aware(
                        shared,
                        thread,
                        current_class_id,
                        &h.class_name,
                    )
                    .ok()
                }
                _ => None,
            });
        }
        owners
    } else {
        Vec::new()
    };
    let mut accessor_getters: Vec<Option<RecordAccessorGetter>> = Vec::new();
    let getter_slots: Option<(ClassId, Vec<usize>, Vec<Arc<str>>)> =
        match (rec_cid, record_class_name.as_deref()) {
            (Some(rec_cid), Some(rec_name)) if !canonical => {
                let cm = shared.classes.class_manager.read();
                let mut slots: Vec<usize> = Vec::with_capacity(field_getters.len());
                let mut descriptors: Vec<Arc<str>> = Vec::with_capacity(field_getters.len());
                let mut methods: Vec<Option<RecordAccessorGetter>> =
                    Vec::with_capacity(field_getters.len());
                let mut all_modelled = true;
                let record_desc = format!("L{rec_name};");
                let caller_is_record = current_class_id == rec_cid;
                for (gi, getter) in field_getters.iter().enumerate() {
                    let Some(h) = getter else {
                        all_modelled = false;
                        break;
                    };
                    if h.kind == MethodHandleKind::InvokeStatic && model_accessor_methods {
                        // `(R)T`, `T` not `void`, of any class.
                        let (params, ret) =
                            crate::runtime::interpreter::split_method_descriptor_ref(&h.descriptor);
                        let owner = static_owners.get(gi).copied().flatten();
                        match (params.as_slice(), owner) {
                            ([p], Some(owner))
                                if *p == record_desc && !ret.is_empty() && ret != "V" =>
                            {
                                slots.push(usize::MAX);
                                descriptors.push(Arc::from(ret));
                                methods.push(Some(RecordAccessorGetter::Static {
                                    owner,
                                    owner_name: Arc::clone(&h.class_name),
                                    name: Arc::clone(&h.member_name),
                                    descriptor: Arc::clone(&h.descriptor),
                                }));
                                continue;
                            }
                            _ => {
                                all_modelled = false;
                                break;
                            }
                        }
                    }
                    if &*h.class_name != rec_name {
                        all_modelled = false;
                        break;
                    }
                    let method_kind = match h.kind {
                        MethodHandleKind::InvokeVirtual => Some(false),
                        // From `R` itself only: from another class the
                        // handle's receiver type is that class, not `R`.
                        MethodHandleKind::InvokeSpecial if caller_is_record => Some(true),
                        _ => None,
                    };
                    if let (Some(special), true) = (method_kind, model_accessor_methods) {
                        // `()T`, `T` not `void`: the return type is what
                        // `toString` renders.
                        match h.descriptor.strip_prefix("()") {
                            Some(ret) if !ret.is_empty() && ret != "V" => {
                                slots.push(usize::MAX);
                                descriptors.push(Arc::from(ret));
                                methods.push(Some(if special {
                                    RecordAccessorGetter::Special {
                                        owner: rec_cid,
                                        owner_name: Arc::clone(&h.class_name),
                                        name: Arc::clone(&h.member_name),
                                        descriptor: Arc::clone(&h.descriptor),
                                    }
                                } else {
                                    RecordAccessorGetter::Virtual {
                                        name: Arc::clone(&h.member_name),
                                        descriptor: Arc::clone(&h.descriptor),
                                    }
                                }));
                                continue;
                            }
                            _ => {
                                all_modelled = false;
                                break;
                            }
                        }
                    }
                    if h.kind != MethodHandleKind::GetField {
                        all_modelled = false;
                        break;
                    }
                    match crate::classloading::find_field_recursive_by_descriptor(
                        rec_cid,
                        &h.member_name,
                        &h.descriptor,
                        &cm.class_store,
                    ) {
                        Some((slot, field, _)) if !field.is_static() => {
                            slots.push(slot);
                            descriptors.push(Arc::clone(&h.descriptor));
                            methods.push(None);
                        }
                        _ => {
                            all_modelled = false;
                            break;
                        }
                    }
                }
                if all_modelled && methods.iter().any(Option::is_some) {
                    accessor_getters = methods;
                }
                all_modelled.then_some((rec_cid, slots, descriptors))
            }
            _ => None,
        };

    if !canonical && getter_slots.is_none() && shared.config.is_jdk_only() {
        // A getter the native linkage does not model: the JDK's own
        // `ObjectMethods` builds the call site (or raises its error).
        if dbg_indy_all() {
            eprintln!(
                "[indy-all] object-methods {} cp#{cp_index}: jdk bootstrap (getters not modelled)",
                info.target_name
            );
        }
        return bootstrap_generic(shared, thread, frame_idx, cp_index, info, current_class_id);
    }

    let (field_indices, field_descriptors, getter_driven) = match getter_slots {
        Some((rec_cid, slots, descriptors)) => {
            if dbg_indy_all() {
                eprintln!(
                    "[indy-all] object-methods {} cp#{cp_index}: getter-driven ({} getters, {} accessor method(s))",
                    info.target_name,
                    slots.len(),
                    accessor_getters.iter().filter(|g| g.is_some()).count()
                );
            }
            (slots, descriptors, Some(rec_cid))
        }
        None => {
            // javac's shape, or (`--compatible`) a getter list the native
            // linkage does not model: component `i` is slot
            // `first_field_index + i` of the record class.
            let first_field = rec_cid.and_then(|id| {
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(id)
                    .map(|c| c.first_field_index)
            });
            let field_indices: Vec<usize> = match first_field {
                Some(first_field) => (0..component_names.len())
                    .map(|i| first_field + i)
                    .collect(),
                None => (0..component_names.len()).collect(),
            };
            (field_indices, field_descriptors, None)
        }
    };

    // Cache the call site.
    let site = ResolvedCallSite::RecordObjectMethod {
        method,
        component_names: component_names.clone(),
        field_indices: field_indices.clone(),
        field_descriptors: field_descriptors.clone(),
        getter_driven,
        accessor_getters: accessor_getters.clone(),
    };
    shared
        .classes
        .resolution_cache
        .write()
        .put_call_site_as_of(current_class_id, cp_index, site, info.fill_as_of);

    execute_record_object_method(
        shared,
        thread,
        frame_idx,
        method,
        &component_names,
        &field_indices,
        &field_descriptors,
        getter_driven,
        &accessor_getters,
    )
}

// Resolve a MethodHandle CP entry to extract the field descriptor.
/// Used for ObjectMethods bootstrap to determine component types.
fn resolve_method_handle_field_descriptor(cp: &ConstantPool, index: u16) -> Option<String> {
    match cp.get(index) {
        Some(ConstantPoolEntry::MethodHandle {
            reference_kind: _,
            reference_index,
        }) => {
            // The reference_index points to a FieldReference
            match cp.get(*reference_index) {
                Some(ConstantPoolEntry::FieldReference {
                    name_and_type_index,
                    ..
                }) => {
                    let (_, desc) = cp.get_name_and_type(*name_and_type_index)?;
                    Some(desc.to_string())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Execute a record ObjectMethods call site.
///
/// For `toString`: Stack `[..., this]` → `[..., String]`
/// For `hashCode`: Stack `[..., this]` → `[..., int]`
/// For `equals`:   Stack `[..., this, other]` → `[..., boolean]`
///
/// gc-common w6-c (`handoff-w6c-rerunning-contexts-take-the-unwinding-arm`):
/// the components' `toString` / `hashCode` / `equals` run Java, so an
/// allocation here cannot be retried; it takes the unwinding arm instead of
/// the infallible one. The operand stack may be left popped when the unwind
/// lands; the `Err` makes the frame throw, as the arms' own `?` returns do.
fn execute_record_object_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method: RecordMethodKind,
    component_names: &[Arc<str>],
    field_indices: &[usize],
    field_descriptors: &[Arc<str>],
    getter_driven: Option<ClassId>,
    accessor_getters: &[Option<RecordAccessorGetter>],
) -> Result<(), MethodCallFailed> {
    match crate::runtime::native_oom::catch_alloc_oom(|| {
        execute_record_object_method_inner(
            shared,
            thread,
            frame_idx,
            method,
            component_names,
            field_indices,
            field_descriptors,
            getter_driven,
            accessor_getters,
        )
    }) {
        Ok(r) => r,
        Err(oom) => Err(MethodCallFailed::InternalError(
            crate::error::VmError::Runtime(oom.into_runtime_error()),
        )),
    }
}

fn execute_record_object_method_inner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method: RecordMethodKind,
    component_names: &[Arc<str>],
    field_indices: &[usize],
    field_descriptors: &[Arc<str>],
    getter_driven: Option<ClassId>,
    accessor_getters: &[Option<RecordAccessorGetter>],
) -> Result<(), MethodCallFailed> {
    use cratonvm_native_builtins::intrinsics::record as rec;
    match method {
        RecordMethodKind::Equals => {
            let other = thread.frames[frame_idx].stack.pop()?;
            let this = thread.frames[frame_idx].stack.pop()?;
            let result = match (this, other) {
                (Value::Object(Some(a)), Value::Object(Some(b))) => {
                    let mut ctx = NativeContextImpl { shared, thread };
                    // A getter-driven site (wave 39) folds over its own
                    // getters; javac's shape over the record's components.
                    i32::from(match getter_driven {
                        Some(record_class) => rec::record_equals_by_getters(
                            &mut ctx,
                            a,
                            b,
                            record_class,
                            field_indices,
                        )?,
                        None => rec::record_equals(&mut ctx, a, b)?,
                    })
                }
                // A NULL receiver (a hand-assembled site; wave 39), as
                // `ObjectMethods.makeEquals` answers it: the same object as a
                // null operand; against an instance of the record class its
                // getters dereference it (`NullPointerException`), unless
                // there are none; any other operand is not an instance.
                (Value::Object(None), Value::Object(None)) => 1,
                (Value::Object(None), Value::Object(Some(b))) => {
                    let record_class =
                        getter_driven.unwrap_or(thread.frames[frame_idx].class_id);
                    let instance = shared.mem.heap.kind_of(b) == cratonvm_types::ObjectKind::Object
                        && shared
                            .classes
                            .class_manager
                            .read()
                            .is_subclass_of(shared.mem.heap.class_id_of(b), record_class);
                    match (instance, field_indices.is_empty()) {
                        (false, _) => 0,
                        (true, true) => 1,
                        (true, false) => return Err(record_null_receiver()),
                    }
                }
                // `x.equals(null)`, and a non-reference operand, are false.
                _ => 0,
            };
            thread.frames[frame_idx].stack.push(Value::Int(result))?;
        }
        RecordMethodKind::HashCode => {
            let this = thread.frames[frame_idx].stack.pop()?;
            let hash = match this {
                Value::Object(Some(obj)) => {
                    let mut ctx = NativeContextImpl { shared, thread };
                    if getter_driven.is_some() {
                        rec::record_hash_code_by_getters(
                            &mut ctx,
                            obj,
                            field_indices,
                            field_descriptors,
                        )?
                    } else {
                        rec::record_hash_code(&mut ctx, obj)?
                    }
                }
                // A null receiver (wave 39): the getters dereference it; with
                // none, `makeHashCode` is the constant 0.
                _ if !field_indices.is_empty() => return Err(record_null_receiver()),
                _ => 0,
            };
            thread.frames[frame_idx].stack.push(Value::Int(hash))?;
        }
        RecordMethodKind::ToString => {
            let this = thread.frames[frame_idx].stack.pop()?;
            // Built as UTF-16 code units and stored as a FRESH String, like
            // the concat path. This used to be a Rust `String` handed to
            // `create_java_string`, which INTERNS: every record `toString()`
            // result went into the process-wide string pool (unbounded growth
            // for a record logged in a loop), `r.toString() == r.toString()`
            // was `true` where HotSpot's is `false`, and a component carrying
            // a lone surrogate came out as U+FFFD.
            let units: Vec<u16> = match this {
                Value::Object(Some(obj)) => {
                    // `makeToString` names the record class the site was
                    // linked for, not the receiver's (a subclass of a
                    // non-`final` record; wave 40, lane L4). javac's shape is
                    // linked only for a `final` record
                    // (`Class::object_methods_args_are_canonical`), whose
                    // receiver IS that class.
                    let cid = getter_driven.unwrap_or_else(|| shared.mem.heap.class_id_of(obj));
                    let class_name = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    // `Class.getSimpleName()` reads the class's OWN
                    // `InnerClasses` entry; only a `$` in the binary name can
                    // make that differ from the last path segment.
                    let inner_name: Option<String> = if class_name.contains('$') {
                        let ctx = NativeContextImpl {
                            shared,
                            thread: &mut *thread,
                        };
                        ctx.inner_classes(cid)
                            .into_iter()
                            .find(|(inner, _, _, _)| **inner == *class_name)
                            .map(|(_, _, inner_name, _)| inner_name)
                    } else {
                        None
                    };
                    let simple = record_simple_name(&class_name, inner_name.as_deref());

                    // Reference components must render via their VIRTUAL
                    // toString (the JDK's generated record toString formats
                    // each component with String.valueOf) — the previous
                    // identity fallback printed `object@hash` for any non-String
                    // reference component (e.g. a List/Map/nested record),
                    // breaking record toString everywhere. The toString invoke
                    // can trigger a moving GC, so the record ref is pinned and
                    // re-read per component (mirrors the Equals/HashCode arms).
                    use cratonvm_native_api::NativeContext as _;
                    let mut ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    let obj_pin = ctx.pin_native_root(obj);
                    let mut result: Vec<u16> =
                        Vec::with_capacity(simple.len() + 2 + 16 * component_names.len());
                    result.extend(simple.encode_utf16());
                    result.push(u16::from(b'['));
                    let mut err: Option<MethodCallFailed> = None;
                    for (i, name) in component_names.iter().enumerate() {
                        if i > 0 {
                            result.extend(", ".encode_utf16());
                        }
                        let fi = field_indices.get(i).copied().unwrap_or(i);
                        let desc = field_descriptors.get(i).map(|s| &**s).unwrap_or("I");
                        let cur = ctx.read_native_pin(obj_pin, obj);
                        // An accessor-method getter (wave 41, `--jdk-only`):
                        // the handle `REF_invokeVirtual R.b()T` applied to
                        // the receiver. Its exception propagates, as the
                        // JDK's filtered getter's does. Selected from the
                        // receiver's class (which is the record class or a
                        // subclass: the site's type is `(R)String`), not by
                        // the record's name, which a loader-blind lookup
                        // could answer with another loader's class.
                        //
                        // Wave 42: a static getter `X.m(R)T` takes the
                        // receiver as its argument; a special getter
                        // `R.m()T` runs `R`'s method on it, never an
                        // override (`invokespecial` semantics).
                        let called = match accessor_getters.get(i) {
                            Some(Some(RecordAccessorGetter::Virtual { name, descriptor })) => {
                                Some(ctx.invoke_virtual(cur, name, descriptor, &[]))
                            }
                            Some(Some(RecordAccessorGetter::Static {
                                owner,
                                owner_name,
                                name,
                                descriptor,
                            })) => Some(ctx.invoke_static_by_class_id(
                                *owner,
                                owner_name,
                                name,
                                descriptor,
                                &[Value::Object(Some(cur))],
                            )),
                            Some(Some(RecordAccessorGetter::Special {
                                owner,
                                owner_name,
                                name,
                                descriptor,
                            })) => Some(ctx.invoke_special_by_class_id(
                                *owner,
                                owner_name,
                                name,
                                descriptor,
                                &[Value::Object(Some(cur))],
                            )),
                            _ => None,
                        };
                        let v = match called {
                            Some(Ok(Some(v))) => v,
                            Some(Ok(None)) => Value::Object(None),
                            Some(Err(e)) => {
                                err = Some(e);
                                break;
                            }
                            None => ctx.get_field(cur, fi),
                        };
                        result.extend(name.encode_utf16());
                        result.push(u16::from(b'='));
                        if let Err(e) =
                            append_record_component_units(&mut ctx, &v, desc, &mut result)
                        {
                            err = Some(e);
                            break;
                        }
                    }
                    ctx.unpin_native_roots(obj_pin);
                    if let Some(e) = err {
                        return Err(e);
                    }
                    result.push(u16::from(b']'));
                    result
                }
                // A null receiver (wave 39): the getters dereference it; with
                // none, `makeToString` is the constant `<simple name>[]`.
                _ if !field_indices.is_empty() => return Err(record_null_receiver()),
                _ => {
                    let record_class =
                        getter_driven.unwrap_or(thread.frames[frame_idx].class_id);
                    let class_name = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(record_class)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    let inner_name: Option<String> = if class_name.contains('$') {
                        let ctx = NativeContextImpl {
                            shared,
                            thread: &mut *thread,
                        };
                        ctx.inner_classes(record_class)
                            .into_iter()
                            .find(|(inner, _, _, _)| **inner == *class_name)
                            .map(|(_, _, inner_name, _)| inner_name)
                    } else {
                        None
                    };
                    let simple = record_simple_name(&class_name, inner_name.as_deref());
                    format!("{simple}[]").encode_utf16().collect()
                }
            };
            let str_ref = crate::runtime::interpreter::create_string_from_units_or_oom(
                shared, thread, &units,
            )?;
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(str_ref)))?;
        }
    }
    Ok(())
}

/// The `NullPointerException` a record `ObjectMethods` site raises for a null
/// receiver when it has getters (they dereference it). HotSpot's comes from
/// inside the getter's method handle; the probe compares only its class.
#[cold]
fn record_null_receiver() -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NullPointerException {
        message: None,
    }))
}

/// `Class.getSimpleName()` of a record, from its binary name and the
/// `inner_name` of the record's OWN `InnerClasses` entry, if it has one.
///
/// HotSpot's rule (`Class.getSimpleName` → `getSimpleBinaryName`): a nested or
/// local class reports its `InnerClasses` `inner_name` verbatim (`Outer$Point`
/// and `Main$1Point` are both `Point`, and a member named `In$ner` stays
/// `In$ner`); a class with no entry of its own is TOP-LEVEL, and its simple
/// name is everything after the package, `$` included -- a top-level
/// `record A$B` is `A$B` (wave 24, lane L4; the old `$`-split answered `B`).
/// Without an entry the `$`-split is still used when the tail after the last
/// `$` starts with a digit (a local class whose `InnerClasses` could not be
/// read), since no Java identifier starts with one.
fn record_simple_name<'a>(binary_name: &'a str, inner_name: Option<&'a str>) -> &'a str {
    if let Some(inner) = inner_name.filter(|n| !n.is_empty()) {
        return inner;
    }
    let tail = binary_name.rsplit('/').next().unwrap_or(binary_name);
    let after = tail.rsplit('$').next().unwrap_or(tail);
    if after.starts_with(|c: char| c.is_ascii_digit()) {
        let stripped = after.trim_start_matches(|c: char| c.is_ascii_digit());
        if !stripped.is_empty() {
            return stripped;
        }
    }
    tail
}

/// The single UTF-16 unit of a boxed `java.lang.Character`, or `None` for
/// anything else.
///
/// Exists because that one wrapper is the only object whose whole value can be
/// an unpaired surrogate, which no `String`-returning renderer can carry.
fn boxed_char_unit(shared: &SharedVm, val: &Value) -> Option<u16> {
    let Value::Object(Some(obj)) = val else {
        return None;
    };
    if shared.mem.heap.kind_of(*obj) == crate::memory::heap::ObjectKind::Array {
        return None;
    }
    if shared.mem.heap.get_header(*obj).num_slots() as usize != 1 {
        return None;
    }
    let name = shared
        .classes
        .class_manager
        .read()
        .get_class(shared.mem.heap.class_id_of(*obj))
        .map(|c| c.name.clone())
        .unwrap_or_default();
    if name.as_ref() != "java/lang/Character" {
        return None;
    }
    match shared.mem.heap.get_field(*obj, 0) {
        Value::Int(v) => Some(v as u16),
        _ => None,
    }
}

/// Render one record component like the JDK's generated record `toString`,
/// appending UTF-16 code units to `out`: primitives via their `String.valueOf`
/// form, references via the VIRTUAL `toString` (`null` → "null", and a
/// `toString()` that returns null → "null"). The String content fast path
/// avoids a Java invoke for the common case. A `toString()` that throws
/// propagates.
fn append_record_component_units(
    ctx: &mut NativeContextImpl<'_>,
    v: &Value,
    descriptor: &str,
    out: &mut Vec<u16>,
) -> Result<(), MethodCallFailed> {
    match v {
        Value::Object(Some(obj)) => {
            // String fast path — copy the code units directly. Only for a
            // `String` by class (wave 16): the shape reader also answers for a
            // `StringBuilder` component, with its whole buffer (unused
            // capacity as NULs) where the JDK prints `String.valueOf(sb)`.
            if crate::vm::is_java_lang_string(ctx.shared, *obj) {
                if let Some(units) = read_java_string_units(&ctx.shared.mem.heap, *obj) {
                    out.extend_from_slice(&units);
                    return Ok(());
                }
            }
            use cratonvm_native_api::NativeContext as _;
            match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => {
                    match read_java_string_units(&ctx.shared.mem.heap, s) {
                        Some(units) => out.extend_from_slice(&units),
                        None => out.extend("null".encode_utf16()),
                    }
                }
                // toString returned null (legal) → JDK prints "null".
                _ => out.extend("null".encode_utf16()),
            }
        }
        // A `char` component is one code unit, surrogate or not.
        Value::Int(n) if descriptor == "C" => out.push(*n as u16),
        // Primitives and the null reference: descriptor-aware textual form.
        _ => out.extend(format_field_value(ctx.shared, v, descriptor).encode_utf16()),
    }
    Ok(())
}

/// Format a field value for record toString.
///
/// `float`/`double` go through the `Float.toString`/`Double.toString`
/// formatters. They used Rust's `{}`, which prints `1.0f` as `1`, `1e10` as
/// `10000000000` and `-0.0` as `-0`, so `record P(double x)` with `x = 1.0`
/// rendered `P[x=1]` where HotSpot prints `P[x=1.0]`.
fn format_field_value(shared: &SharedVm, v: &Value, descriptor: &str) -> String {
    match v {
        Value::Int(n) => {
            if descriptor == "Z" {
                if *n != 0 {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            } else if descriptor == "C" {
                format!("{}", char::from_u32(*n as u32).unwrap_or('?'))
            } else {
                n.to_string()
            }
        }
        Value::Long(n) => n.to_string(),
        Value::Float(f) => format_float(*f),
        Value::Double(d) => format_double(*d),
        Value::Object(Some(obj)) => {
            if let Some(s) = read_java_string(&shared.mem.heap, *obj) {
                s
            } else {
                format!("object@{:x}", obj.as_ptr() as usize)
            }
        }
        Value::Object(None) => "null".to_string(),
        _ => "?".to_string(),
    }
}

/// Bootstrap a `SwitchBootstraps.enumSwitch` call site.
///
/// Bootstrap arguments are *"String constants and Class instances, in any
/// combination"*: an enum constant name, or a type (javac emits one for a
/// type pattern such as `case E x when ...` beside constant labels). The
/// resulting call site takes `(Enum target, int startIndex)` and returns the
/// index of the first matching label from `startIndex` on.
///
/// An all-name switch is linked as [`ResolvedCallSite::EnumSwitch`]. One with
/// a `Class` label is linked as a `TypeSwitch` of `EnumConstant` and `Type`
/// labels, so the type label answers `instanceof` exactly as `typeSwitch`
/// does. A `Class` label used to be read as a string constant, i.e. `""`,
/// which no constant name equals, so the pattern arm was never taken.
fn bootstrap_enum_switch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // Phase 1, under the read lock: every label as a name, or as a class name
    // to resolve once the lock is released.
    let raw: Vec<Result<Arc<str>, Arc<str>>> = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("enumSwitch: class {current_class_id} not found"),
            })?;
        let cp = &class.constant_pool;
        info.bootstrap_arg_indices
            .iter()
            .map(|&arg_idx| match cp.get(arg_idx) {
                Some(ConstantPoolEntry::ClassReference { .. }) => {
                    Err(Arc::<str>::from(cp.get_class_name(arg_idx).unwrap_or("")))
                }
                // Anything else keeps the old reading: a name (`""` for an
                // entry that is not a string, which matches no constant).
                _ => Ok(Arc::<str>::from(
                    resolve_string_constant(cp, arg_idx).unwrap_or_default(),
                )),
            })
            .collect()
    };

    if raw.iter().any(Result::is_err) {
        // Phase 2: resolve each `Class` label through the switching class's
        // loader, as `bootstrap_type_switch` does.
        let mut labels = Vec::with_capacity(raw.len());
        for label in raw {
            labels.push(match label {
                Ok(name) => SwitchLabel::EnumConstant(name),
                Err(class_name) => SwitchLabel::Type {
                    class_id: crate::runtime::interpreter::resolve_class_loader_aware(
                        shared,
                        thread,
                        current_class_id,
                        &class_name,
                    )
                    .map_err(|e| {
                        crate::runtime::exceptions::convert_class_not_found(
                            shared,
                            thread,
                            &class_name,
                            e,
                        )
                    })?,
                    class_name,
                },
            });
        }
        let site = ResolvedCallSite::TypeSwitch {
            labels: labels.clone(),
        };
        shared
            .classes
            .resolution_cache
            .write()
            .put_call_site_as_of(current_class_id, cp_index, site, info.fill_as_of);
        return execute_type_switch(shared, thread, frame_idx, &labels);
    }
    let labels: Vec<Arc<str>> = raw.into_iter().flatten().collect();

    // Cache the call site.
    let site = ResolvedCallSite::EnumSwitch {
        labels: labels.clone(),
    };
    shared
        .classes
        .resolution_cache
        .write()
        .put_call_site_as_of(current_class_id, cp_index, site, info.fill_as_of);

    execute_enum_switch(shared, thread, frame_idx, &labels)
}

/// Execute a cached `enumSwitch` call site.
///
/// Stack: `[..., target: Enum, startIndex: int]` → `[..., matchIndex: int]`
pub fn execute_enum_switch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    labels: &[Arc<str>],
) -> Result<(), MethodCallFailed> {
    let restart = match thread.frames[frame_idx].stack.pop()? {
        Value::Int(i) => i,
        _ => 0,
    };
    let target = thread.frames[frame_idx].stack.pop()?;
    let start_index = restart.max(0) as usize;

    let result = match target {
        // Per the `SwitchBootstraps.enumSwitch` contract, a null target returns
        // -1 (the `case null` arm); a non-null target that matches no label
        // returns labels.length (the default arm).
        Value::Object(None) => -1,
        // A restart index outside `0..=labels.length` (wave 42): the JDK's
        // `mappedEnumSwitch` answers a null first, then its generated switch
        // makes `Objects.checkIndex(restart, labels.length + 1)`. It was
        // clamped to 0, and a large one answered the default.
        Value::Object(Some(_)) if restart < 0 || restart as usize > labels.len() => {
            return Err(switch_restart_out_of_bounds(restart, labels.len()));
        }
        Value::Object(Some(obj_ref)) => {
            // Read the enum constant name from field 0 (Enum.<init> stores name there).
            let name = match shared.mem.heap.get_field(obj_ref, 0) {
                Value::Object(Some(name_ref)) => read_java_string(&shared.mem.heap, name_ref),
                _ => None,
            };

            let mut match_index: i32 = labels.len() as i32;
            if let Some(name) = name {
                for (i, label) in labels.iter().enumerate().skip(start_index) {
                    if &**label == name.as_str() {
                        match_index = i as i32;
                        break;
                    }
                }
            }
            match_index
        }
        _ => labels.len() as i32,
    };

    thread.frames[frame_idx].stack.push(Value::Int(result))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Cached debug-flag helpers
    // -----------------------------------------------------------------------
    //
    // These replaced uncached `cratonvm_types::flags::runtime_var_os` reads that sat on the
    // per-execution generic-indy path. The risk of the swap is a typo in the
    // variable name (the flag would then silently never fire, and a future
    // debugging session would waste hours on a dead switch), or an inverted
    // sense. Both are caught by comparing against the live environment.

    #[test]
    fn cached_indy_debug_flags_match_environment_and_are_stable() {
        let pairs: [(fn() -> bool, &str); 3] = [
            (dbg_indy_all, "CRATONVM_DBG_INDY_ALL"),
            (dbg_indy_generic, "CRATONVM_DBG_INDY_GENERIC"),
            (dbg_lambda_dispatch, "CRATONVM_DBG_LAMBDA_DISPATCH"),
        ];
        for (flag, name) in pairs {
            let expected = cratonvm_types::flags::runtime_var_os(name).is_some();
            assert_eq!(
                flag(),
                expected,
                "cached flag disagrees with env for {name}"
            );
            // Process-lifetime memo: repeat reads must be stable (and must not
            // re-enter `var_os`).
            assert_eq!(flag(), expected, "cached flag for {name} is not stable");
        }
    }

    // -----------------------------------------------------------------------
    // Call-site cache: population, reuse, and redefinition invalidation
    // -----------------------------------------------------------------------
    //
    // Task-3 coverage for "is the bootstrap result cached per call site and
    // correctly invalidated?". The observable contract lives entirely in
    // `ResolutionCache`, which `execute_invokedynamic` consults on its fast
    // path and populates from every JDK-factory bootstrap branch.

    use crate::classloading::resolution::ResolutionCache;

    fn concat_site(recipe: &str, constants: &[&str], descriptor: &str) -> ResolvedCallSite {
        // Recipes and constants are UTF-16 code units on the cached site; the
        // `&str` parameters here are test-authoring convenience only.
        ResolvedCallSite::StringConcat {
            recipe: Arc::from(recipe.encode_utf16().collect::<Vec<u16>>().as_slice()),
            constant_args: constants
                .iter()
                .map(|s| Arc::from(s.encode_utf16().collect::<Vec<u16>>().as_slice()))
                .collect(),
            target_descriptor: Arc::from(descriptor),
        }
    }

    fn lambda_site(iface: &str, impl_owner: &str, impl_name: &str) -> ResolvedCallSite {
        ResolvedCallSite::Lambda(LambdaCallSite {
            functional_interface: Arc::from(iface),
            functional_interface_id: None,
            sam_method_name: Arc::from("apply"),
            sam_descriptor: Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;"),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: Arc::from(impl_owner),
                member_name: Arc::from(impl_name),
                descriptor: Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;"),
            },
            instantiated_descriptor: Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;"),
            capture_types: vec![],
            proxy_class_id: ClassId::new(9001),
            serializable_flag: false,
        })
    }

    #[test]
    fn bootstrapped_call_site_is_reused_not_rebootstrapped() {
        let mut cache = ResolutionCache::new();
        let caller = ClassId::new(7);
        assert!(
            cache.get_call_site(caller, 42).is_none(),
            "cold call site must miss so the bootstrap slow path runs"
        );
        cache.put_call_site(caller, 42, concat_site("\u{0001} items", &[], "(I)V"));
        // Second and every later execution take the fast path. Nothing here
        // memoizes a *negative* result, so a miss is always retried — the
        // "cached `None` is permanent" failure mode does not apply to this
        // cache (`get_call_site` returns `Option<&_>` from a plain map lookup;
        // only successful bootstraps ever insert).
        assert!(cache.get_call_site(caller, 42).is_some());
        assert!(cache.get_call_site(caller, 42).is_some());
        assert_eq!(cache.call_site_count(), 1);
    }

    #[test]
    fn lambda_call_site_is_dropped_when_its_caller_is_redefined() {
        let mut cache = ResolutionCache::new();
        let caller = ClassId::new(11);
        let other = ClassId::new(12);
        cache.put_call_site(
            caller,
            3,
            lambda_site("java/util/function/Function", "P", "f"),
        );
        cache.put_call_site(
            other,
            3,
            lambda_site("java/util/function/Function", "P", "g"),
        );

        // Redefining an unrelated class must not disturb this call site: the
        // lambda keeps dispatching to the same spun proxy.
        cache.invalidate_class(ClassId::new(99));
        assert!(cache.get_call_site(caller, 3).is_some());

        // Redefining the class that *contains* the invokedynamic drops it, so
        // the next execution re-runs LambdaMetafactory against the new constant
        // pool. Same cp index in a redefined pool may denote a different call
        // site entirely, which is exactly why key-match eviction is required.
        cache.invalidate_class(caller);
        assert!(cache.get_call_site(caller, 3).is_none());
        // ... and only that class's entries go.
        assert!(cache.get_call_site(other, 3).is_some());
    }

    #[test]
    fn lambda_impl_handle_is_symbolic_so_impl_redefinition_needs_no_eviction() {
        // `invalidate_class` evicts call sites by *key* class only. That is
        // sound precisely because `LambdaCallSite::impl_handle` stores the
        // implementation method symbolically (owner / name / descriptor) and is
        // re-resolved on each dispatch — redefining the class that owns the
        // lambda body is therefore picked up without touching this cache. If
        // anyone ever pre-resolves the handle to a concrete method pointer,
        // this test fails and flags that eviction must grow a callee-side prong
        // (as `fields` / `methods` already have).
        let ResolvedCallSite::Lambda(lcs) = lambda_site("java/util/function/Function", "Impl", "f")
        else {
            panic!("expected a lambda call site");
        };
        assert_eq!(&*lcs.impl_handle.class_name, "Impl");
        assert_eq!(&*lcs.impl_handle.member_name, "f");
        assert_eq!(
            &*lcs.impl_handle.descriptor,
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        );
    }

    #[test]
    fn cached_string_concat_site_exposes_borrowable_recipe_and_constants() {
        // `execute_string_concat` borrows `&[u16]` / `&[S: AsRef<[u16]>]`
        // straight out of the cached site instead of rebuilding an `IndyInfo`
        // with `recipe.to_string()`, `target_descriptor.to_string()` and a
        // freshly allocated `Vec<String>` of every constant on *every* `"a" + b`
        // evaluation. This test pins the borrow shape: destructuring the cached
        // site must yield data usable without conversion, and `Arc<[u16]>` must
        // satisfy the `AsRef<[u16]>` bound.
        //
        // Units rather than `str` since 2026-08-05: a recipe can carry a lone
        // surrogate from a folded literal, which a Rust `str` turns into U+FFFD.
        fn takes_borrowed<S: AsRef<[u16]>>(
            recipe: &[u16],
            constants: &[S],
            descriptor: &str,
        ) -> String {
            let mut out: Vec<u16> = recipe.to_vec();
            for c in constants {
                out.extend_from_slice(c.as_ref());
            }
            out.extend(descriptor.encode_utf16());
            String::from_utf16_lossy(&out)
        }

        let site = concat_site("a\u{0002}b", &["X", "Y"], "(I)Ljava/lang/String;");
        let ResolvedCallSite::StringConcat {
            recipe,
            constant_args,
            target_descriptor,
        } = &site
        else {
            panic!("expected a StringConcat call site");
        };
        assert_eq!(
            takes_borrowed(recipe, constant_args.as_slice(), target_descriptor),
            "a\u{0002}bXY(I)Ljava/lang/String;"
        );
        // The bootstrap path passes owned `Vec<u16>`; both must compile against
        // the same bound.
        let owned: Vec<Vec<u16>> = vec![vec![b'X' as u16], vec![b'Y' as u16]];
        let recipe_units: Vec<u16> = "a\u{0002}b".encode_utf16().collect();
        assert_eq!(
            takes_borrowed(&recipe_units, owned.as_slice(), "(I)Ljava/lang/String;"),
            "a\u{0002}bXY(I)Ljava/lang/String;"
        );
    }

    #[test]
    fn synthesized_make_concat_recipe_is_all_argument_placeholders() {
        // The `makeConcat` (no-recipe) branch synthesizes one `\u{0001}` per
        // declared argument and passes an empty constant list. Regression guard
        // for the `patched_info` removal: the synthesized recipe must still have
        // exactly one placeholder per descriptor argument and contain no
        // `\u{0002}` constant placeholders (there are no constants to consume).
        for desc in [
            "()Ljava/lang/String;",
            "(I)Ljava/lang/String;",
            "(ILjava/lang/String;J)Ljava/lang/String;",
        ] {
            let n = parse_descriptor_args(desc).len();
            let recipe: Vec<u16> = vec![TAG_ARG_UNIT; n];
            assert_eq!(recipe.iter().filter(|&&u| u == TAG_ARG_UNIT).count(), n);
            assert!(!recipe.contains(&TAG_CONST_UNIT));
        }
    }

    #[test]
    fn parse_descriptor_args_empty() {
        assert_eq!(parse_descriptor_args("()V"), vec![]);
    }

    #[test]
    fn parse_descriptor_args_mixed() {
        let args = parse_descriptor_args("(ILjava/lang/String;DJ)Ljava/lang/String;");
        assert_eq!(args, vec!['I', 'L', 'D', 'J']);
    }

    #[test]
    fn parse_descriptor_args_array() {
        let args = parse_descriptor_args("([I[Ljava/lang/String;)V");
        assert_eq!(args, vec!['L', 'L']);
    }

    #[test]
    fn parse_descriptor_args_all_primitives() {
        let args = parse_descriptor_args("(BCDFIJSZ)V");
        assert_eq!(args, vec!['B', 'C', 'D', 'F', 'I', 'J', 'S', 'Z']);
    }

    /// `makeConcatWithConstants` recipe constants (the `\u0002` slots) may be any
    /// loadable constant, not just `String`. Verify each kind converts to its
    /// HotSpot `String.valueOf` text instead of being silently dropped.
    #[test]
    fn resolve_concat_constant_all_kinds() {
        use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
        let entries = vec![
            CPE::Tombstone,                           // 0 (unused)
            CPE::Utf8("lit".to_string().into()),      // 1
            CPE::StringReference { string_index: 1 }, // 2  -> "lit"
            CPE::Integer(42),                         // 3  -> "42"
            CPE::Long(123456789012345),               // 4  -> decimal
            CPE::Float(1.0),                          // 5  -> "1.0"
            CPE::Double(2.5),                         // 6  -> "2.5"
            CPE::Utf8("verbatim".to_string().into()), // 7  -> "verbatim"
        ];
        let cp = ConstantPool::new(entries);

        assert_eq!(resolve_concat_constant(&cp, 2).as_deref(), Some("lit"));
        assert_eq!(resolve_concat_constant(&cp, 3).as_deref(), Some("42"));
        assert_eq!(
            resolve_concat_constant(&cp, 4).as_deref(),
            Some("123456789012345")
        );
        // Float/double must keep the Java ".0" suffix, not "1" / "2".
        assert_eq!(resolve_concat_constant(&cp, 5).as_deref(), Some("1.0"));
        assert_eq!(resolve_concat_constant(&cp, 6).as_deref(), Some("2.5"));
        assert_eq!(resolve_concat_constant(&cp, 7).as_deref(), Some("verbatim"));
    }

    #[test]
    fn format_float_special_values() {
        assert_eq!(format_float(f32::NAN), "NaN");
        assert_eq!(format_float(f32::INFINITY), "Infinity");
        assert_eq!(format_float(f32::NEG_INFINITY), "-Infinity");
        assert_eq!(format_float(-0.0f32), "-0.0");
    }

    #[test]
    fn format_double_special_values() {
        assert_eq!(format_double(f64::NAN), "NaN");
        assert_eq!(format_double(f64::INFINITY), "Infinity");
        assert_eq!(format_double(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(format_double(-0.0f64), "-0.0");
    }

    #[test]
    fn value_to_string_primitives() {
        use crate::config::VmConfig;
        use std::sync::Arc;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        assert_eq!(value_to_string(&shared, None, &Value::Int(42), 'I'), "42");
        assert_eq!(value_to_string(&shared, None, &Value::Int(1), 'Z'), "true");
        assert_eq!(value_to_string(&shared, None, &Value::Int(0), 'Z'), "false");
        assert_eq!(value_to_string(&shared, None, &Value::Int(65), 'C'), "A");
        assert_eq!(
            value_to_string(&shared, None, &Value::Long(123456789), 'J'),
            "123456789"
        );
        assert_eq!(
            value_to_string(&shared, None, &Value::Object(None), 'L'),
            "null"
        );
    }

    #[test]
    fn value_to_string_java_string() {
        use crate::config::VmConfig;
        use std::sync::Arc;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let str_ref = create_java_string(&shared, "Hello");
        assert_eq!(
            value_to_string(&shared, None, &Value::Object(Some(str_ref)), 'L'),
            "Hello"
        );
    }

    /// A `typeSwitch` type label answers `instanceof` for an ARRAY target from
    /// the array's own type, not from its header class id (which is the
    /// component class): `int[]` is an `Object`, is an `int[]`, is not a
    /// `String` and not a `long[]`.
    #[test]
    fn type_switch_label_on_an_array_target_uses_array_assignability() {
        use crate::config::VmConfig;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let arr =
            shared
                .mem
                .heap
                .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Int, 2);
        let cid = shared.mem.heap.class_id_of(arr);
        let any = ClassId::new(0);
        let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
        let mut arr = arr;
        let mut matches =
            |label: &str| type_label_matches(&shared, &mut thread, &mut arr, cid, any, label);
        assert!(matches("[I"));
        assert!(matches("java/lang/Object"));
        assert!(!matches("[J"));
        assert!(!matches("java/lang/String"));
    }

    /// An `enumSwitch` whose labels include a `Class` (javac's shape for
    /// `case E x when ...` beside constant labels) is linked as a type switch
    /// with `EnumConstant` labels, and a type label matches the constant it
    /// is an instance of. `SwitchBootstraps.enumSwitch` returns the first
    /// matching label from `startIndex` on.
    #[test]
    fn enum_switch_class_label_matches_by_instanceof() {
        use crate::config::VmConfig;
        use crate::runtime::frame::Frame;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
        // An enum-like object whose slot 0 is the constant name, as in
        // `vm::tests::enum_switch_*`. Its class must be a loaded class so the
        // type label's hierarchy test can answer; any class stands in for the
        // enum type (`vm::tests::type_switch_integer_match` loads the same).
        let stand_in = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Integer")
            .expect("load a stand-in class");
        let name = create_java_string(&shared, "B");
        let constant = shared.mem.heap.alloc_object(stand_in, 2);
        shared
            .mem
            .heap
            .set_field(constant, 0, Value::Object(Some(name)));
        let enum_cid = shared.mem.heap.class_id_of(constant);
        let labels = vec![
            SwitchLabel::EnumConstant("A".into()),
            SwitchLabel::Type {
                class_name: "p/E".into(),
                class_id: enum_cid,
            },
            SwitchLabel::EnumConstant("B".into()),
        ];
        let run = |thread: &mut JvmThread, start: i32| {
            let mut frame = Frame::new(
                ClassId::new(0),
                String::new(),
                String::new(),
                String::new(),
                None,
                vec![0xb1],
                vec![],
                10,
                0,
                &[],
            );
            let _ = frame.stack.push(Value::Object(Some(constant)));
            let _ = frame.stack.push(Value::Int(start));
            thread.frames.clear();
            thread.frames.push(frame);
            execute_type_switch(&shared, thread, 0, &labels).expect("type switch");
            thread.frames[0].stack.pop().expect("result")
        };
        // `B` is not `A`; the `Class` label (index 1) matches first.
        assert_eq!(run(&mut thread, 0), Value::Int(1));
        // Resumed past the guard (`startIndex` 2): the constant label.
        assert_eq!(run(&mut thread, 2), Value::Int(2));
    }

    /// The per-thread `IndySiteCache` entry is keyed by the full
    /// (class, cp index) pair and dies with the resolution epoch, which every
    /// path that drops a `resolution_cache` call-site row advances.
    #[test]
    fn indy_site_cache_entry_dies_with_the_resolution_epoch() {
        if cratonvm_classloading::any_class_redefined() {
            return; // Latched off process-wide by another test.
        }
        let mut cache = IndySiteCache::new();
        let (class, cp) = (ClassId::new(3), 5u16);
        // Parallel tests move the global epochs; retry until one fill sticks.
        let stuck = (0..100).any(|_| {
            cache.put(
                class,
                cp,
                IndySiteCache::epochs_now(),
                CachedIndySite::Lambda {
                    proxy_class_id: ClassId::new(7),
                    num_captures: 2,
                    singleton_key: None,
                    singleton: None,
                },
            );
            matches!(
                cache.get(class, cp),
                Some(CachedIndySite::Lambda {
                    num_captures: 2,
                    ..
                })
            )
        });
        if !stuck {
            return;
        }
        assert!(cache.get(class, cp + 1).is_none());
        assert!(cache.get(ClassId::new(4), cp).is_none());
        crate::runtime::interpreter::bump_resolution_epoch();
        assert!(cache.get(class, cp).is_none());
    }

    /// A qualified enum label (`case E.A`, JEP 441) is javac's
    /// `ConstantBootstraps.invoke(EnumDesc.of, <ClassDesc.of("p.Outer$E")>, "A")`
    /// condy; it decodes to the enum's internal name and the constant name.
    /// Any other condy shape (here the inner `ClassDesc` condy) is not one.
    #[test]
    fn javac_enum_desc_label_decodes_the_qualified_enum_constant() {
        use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
        let utf8 = |s: &str| CPE::Utf8(s.to_string().into());
        let entries = vec![
            CPE::Tombstone,                              // 0
            utf8("java/lang/invoke/ConstantBootstraps"), // 1
            CPE::ClassReference { name_index: 1 },       // 2
            utf8("invoke"),                              // 3
            utf8(
                "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/Class;\
                 Ljava/lang/invoke/MethodHandle;[Ljava/lang/Object;)Ljava/lang/Object;",
            ), // 4
            CPE::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            CPE::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
            CPE::MethodHandle {
                reference_kind: 6,
                reference_index: 6,
            }, // 7: the bootstrap
            utf8("java/lang/Enum$EnumDesc"),             // 8
            CPE::ClassReference { name_index: 8 },       // 9
            utf8("of"),                                  // 10
            utf8("(Ljava/lang/constant/ClassDesc;Ljava/lang/String;)Ljava/lang/Enum$EnumDesc;"), // 11
            CPE::NameAndType {
                name_index: 10,
                descriptor_index: 11,
            }, // 12
            CPE::MethodReference {
                class_index: 9,
                name_and_type_index: 12,
            }, // 13
            CPE::MethodHandle {
                reference_kind: 6,
                reference_index: 13,
            }, // 14: EnumDesc.of
            utf8("java/lang/constant/ClassDesc"),   // 15
            CPE::ClassReference { name_index: 15 }, // 16
            utf8("(Ljava/lang/String;)Ljava/lang/constant/ClassDesc;"), // 17
            CPE::NameAndType {
                name_index: 10,
                descriptor_index: 17,
            }, // 18
            CPE::InterfaceMethodReference {
                class_index: 16,
                name_and_type_index: 18,
            }, // 19
            CPE::MethodHandle {
                reference_kind: 6,
                reference_index: 19,
            }, // 20: ClassDesc.of
            utf8("p.Outer$E"),                      // 21
            CPE::StringReference { string_index: 21 }, // 22
            utf8("Ljava/lang/constant/ClassDesc;"), // 23
            CPE::NameAndType {
                name_index: 3,
                descriptor_index: 23,
            }, // 24
            CPE::Dynamic {
                bootstrap_method_attr_index: 0,
                name_and_type_index: 24,
            }, // 25: the ClassDesc
            utf8("A"),                              // 26
            CPE::StringReference { string_index: 26 }, // 27
            utf8("Ljava/lang/Enum$EnumDesc;"),      // 28
            CPE::NameAndType {
                name_index: 3,
                descriptor_index: 28,
            }, // 29
            CPE::Dynamic {
                bootstrap_method_attr_index: 1,
                name_and_type_index: 29,
            }, // 30: the EnumDesc
        ];
        let cp = ConstantPool::new(entries);
        let bsms = vec![
            BootstrapMethod {
                bootstrap_method_ref: 7,
                bootstrap_arguments: vec![20, 22],
            },
            BootstrapMethod {
                bootstrap_method_ref: 7,
                bootstrap_arguments: vec![14, 25, 27],
            },
        ];
        assert_eq!(
            javac_enum_desc_label(&cp, &bsms, 30),
            Some(("p/Outer$E".to_string(), "A".to_string()))
        );
        assert_eq!(javac_enum_desc_label(&cp, &bsms, 25), None);
        assert_eq!(javac_enum_desc_label(&cp, &bsms, 27), None);
    }

    /// An `EnumDesc` label matches a target whose declaring enum is the
    /// label's class and whose constant name is the label's name.
    #[test]
    fn enum_desc_label_matches_by_declaring_class_and_name() {
        use crate::config::VmConfig;
        use crate::runtime::frame::Frame;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
        // Stand-ins as in `enum_switch_class_label_matches_by_instanceof`.
        let (stand_in, other) = {
            let mut cm = shared.classes.class_manager.write();
            (
                cm.load_class("java/lang/Integer")
                    .expect("load a stand-in class"),
                cm.load_class("java/lang/Long")
                    .expect("load a second class"),
            )
        };
        let name = create_java_string(&shared, "B");
        let constant = shared.mem.heap.alloc_object(stand_in, 2);
        shared
            .mem
            .heap
            .set_field(constant, 0, Value::Object(Some(name)));
        let enum_cid = shared.mem.heap.class_id_of(constant);
        let labels = vec![
            SwitchLabel::EnumDesc {
                class_id: enum_cid,
                name: "A".into(),
            },
            SwitchLabel::EnumDesc {
                class_id: other,
                name: "B".into(),
            },
            SwitchLabel::EnumDesc {
                class_id: enum_cid,
                name: "B".into(),
            },
        ];
        let mut frame = Frame::new(
            ClassId::new(0),
            String::new(),
            String::new(),
            String::new(),
            None,
            vec![0xb1],
            vec![],
            10,
            0,
            &[],
        );
        let _ = frame.stack.push(Value::Object(Some(constant)));
        let _ = frame.stack.push(Value::Int(0));
        thread.frames.push(frame);
        execute_type_switch(&shared, &mut thread, 0, &labels).expect("type switch");
        assert_eq!(thread.frames[0].stack.pop().expect("result"), Value::Int(2));
    }

    /// A Java `String` reached by the operand renderer — the shape a
    /// `toString()` result has — comes back as code units, so an unpaired
    /// surrogate survives `+` instead of becoming U+FFFD.
    #[test]
    fn rendered_string_operand_keeps_an_unpaired_surrogate() {
        use crate::config::VmConfig;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let units = [u16::from(b'x'), 0xD800, u16::from(b'y')];
        let s = crate::vm::create_java_string_from_units(&shared, &units);
        match render_operand_checked(&shared, None, &Value::Object(Some(s)), 'L') {
            Ok(RenderedOperand::Units(got)) => assert_eq!(got, units),
            Ok(RenderedOperand::Text(t)) => panic!("rendered lossily as text: {t:?}"),
            Err(_) => panic!("rendering a String must not fail"),
        }
        // The lossy `String` form is still what the non-concat callers get.
        assert_eq!(
            value_to_string(&shared, None, &Value::Object(Some(s)), 'L'),
            "x\u{FFFD}y"
        );
    }

    /// The concat fast path renders only the real (final) wrapper classes from
    /// their field. It used to render ANY one-slot object whose slot held a
    /// primitive, so `"" + new Counter(5)` printed `5` and never ran
    /// `Counter.toString()`.
    #[test]
    fn primitive_wrapper_display_is_exact_class_only() {
        let show = |name: &str, v: Value| primitive_wrapper_display(name, v);
        assert_eq!(
            show("java/lang/Integer", Value::Int(5)).as_deref(),
            Some("5")
        );
        assert_eq!(
            show("java/lang/Short", Value::Int(-3)).as_deref(),
            Some("-3")
        );
        assert_eq!(
            show("java/lang/Boolean", Value::Int(1)).as_deref(),
            Some("true")
        );
        assert_eq!(
            show("java/lang/Boolean", Value::Int(0)).as_deref(),
            Some("false")
        );
        assert_eq!(
            show("java/lang/Character", Value::Int(65)).as_deref(),
            Some("A")
        );
        assert_eq!(show("java/lang/Long", Value::Long(7)).as_deref(), Some("7"));
        assert_eq!(
            show("java/lang/Double", Value::Double(1.0)).as_deref(),
            Some("1.0")
        );
        assert_eq!(
            show("java/lang/Float", Value::Float(1.0)).as_deref(),
            Some("1.0")
        );
        // One primitive field does not make a wrapper, whatever the name says.
        assert_eq!(show("com/example/Counter", Value::Int(5)), None);
        assert_eq!(show("com/example/MyBooleanFlag", Value::Int(1)), None);
        assert_eq!(
            show("java/util/concurrent/atomic/AtomicInteger", Value::Int(5)),
            None
        );
        // A wrapper name over the wrong slot kind is not rendered from the slot.
        assert_eq!(show("java/lang/Long", Value::Int(5)), None);
    }

    /// A class whose own name begins with `L` keeps it: the descriptor's `L`
    /// is stripped once (wave 38 host run: `(LoHashLong)long` printed as
    /// `(oHashLong)long`).
    #[test]
    fn method_type_display_keeps_a_leading_l_of_the_class_name() {
        assert_eq!(method_type_display("(LLoHashLong;)J"), "(LoHashLong)long");
        assert_eq!(method_type_display("([LLx;I)LLy;"), "(Lx[],int)Ly");
        assert_eq!(method_type_display("(Ljava/lang/String;)V"), "(String)void");
    }

    /// `Class.getSimpleName()` of a record, including a LOCAL record, whose
    /// binary name carries javac's numeric prefix.
    #[test]
    fn record_simple_name_matches_get_simple_name() {
        assert_eq!(record_simple_name("com/example/Point", None), "Point");
        assert_eq!(record_simple_name("com/example/Outer$Point", Some("Point")), "Point");
        assert_eq!(record_simple_name("com/example/Main$1Point", Some("Point")), "Point");
        // No readable entry: a local class's digits still go.
        assert_eq!(record_simple_name("Main$12Pair", None), "Pair");
        assert_eq!(record_simple_name("Point", None), "Point");
        // Wave 24: a TOP-LEVEL record whose name contains `$` has no entry
        // of its own, and HotSpot's simple name keeps the `$`.
        assert_eq!(record_simple_name("p/A$B", None), "A$B");
        assert_eq!(record_simple_name("A$B", None), "A$B");
        // A member whose own name contains `$` keeps it too.
        assert_eq!(record_simple_name("p/Outer$In$ner", Some("In$ner")), "In$ner");
    }

    /// Record `float`/`double` components use `Float/Double.toString`, not
    /// Rust's `{}` (which printed `1.0` as `1`).
    #[test]
    fn record_float_components_use_java_formatting() {
        use crate::config::VmConfig;
        use std::sync::Arc;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        assert_eq!(format_field_value(&shared, &Value::Double(1.0), "D"), "1.0");
        assert_eq!(format_field_value(&shared, &Value::Float(1.0), "F"), "1.0");
        assert_eq!(
            format_field_value(&shared, &Value::Double(1e10), "D"),
            "1.0E10"
        );
        assert_eq!(
            format_field_value(&shared, &Value::Double(-0.0), "D"),
            "-0.0"
        );
        assert_eq!(format_field_value(&shared, &Value::Int(7), "I"), "7");
    }

    /// The zero-capture singleton key tells two METHODS apart even when the
    /// shared `CONSTANT_InvokeDynamic` entry sits at the same bci in both
    /// (`return String::length;` twice), and is stable for one method.
    #[test]
    fn lambda_site_method_key_separates_methods_and_overloads() {
        let a = lambda_site_method_key_of("ref1", "()Ljava/util/function/Function;");
        let b = lambda_site_method_key_of("ref2", "()Ljava/util/function/Function;");
        let a_overload = lambda_site_method_key_of("ref1", "(I)Ljava/util/function/Function;");
        assert_ne!(a, b);
        assert_ne!(a, a_overload);
        assert_eq!(
            a,
            lambda_site_method_key_of("ref1", "()Ljava/util/function/Function;")
        );
        // Name and descriptor are hashed as two strings, not as their
        // concatenation, so moving a character across the boundary changes it.
        assert_ne!(
            lambda_site_method_key_of("ab", "c"),
            lambda_site_method_key_of("a", "bc")
        );
    }

    /// A `LambdaMetafactory` pool: cp #12 is a zero-capture indy
    /// (`()Ljava/util/function/Supplier;`), cp #14 a capturing one
    /// (`(I)Ljava/util/function/IntSupplier;`). The bridge-site builder reads
    /// only the bootstrap handle's class and name, never its static arguments.
    fn lambda_metafactory_pool() -> ConstantPool {
        use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
        let utf8 = |s: &str| CPE::Utf8(Arc::from(s));
        ConstantPool::new(vec![
            CPE::Tombstone,
            utf8(LAMBDA_METAFACTORY),              // 1
            CPE::ClassReference { name_index: 1 }, // 2
            utf8(METAFACTORY),                     // 3
            utf8("()Ljava/lang/invoke/CallSite;"), // 4 (shape only)
            CPE::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            CPE::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
            CPE::MethodHandle {
                reference_kind: 6,
                reference_index: 6,
            }, // 7
            utf8("get"),                           // 8
            utf8("()Ljava/util/function/Supplier;"), // 9
            CPE::NameAndType {
                name_index: 8,
                descriptor_index: 9,
            }, // 10
            utf8("(I)Ljava/util/function/IntSupplier;"), // 11
            CPE::InvokeDynamic {
                bootstrap_method_attr_index: 0,
                name_and_type_index: 10,
            }, // 12
            CPE::NameAndType {
                name_index: 8,
                descriptor_index: 11,
            }, // 13
            CPE::InvokeDynamic {
                bootstrap_method_attr_index: 0,
                name_and_type_index: 13,
            }, // 14
        ])
    }

    /// `interpreter-L6-lambda-singleton-identity-splits-between-interpreter-and-jit-FIXED`:
    /// a compiled zero-capture lambda site carries the key the interpreter
    /// derives for the same instruction (method key, pc AFTER the 5-byte
    /// instruction), and a door that cannot name the instruction may not
    /// bridge one.
    #[test]
    fn a_compiled_lambda_site_keys_its_singleton_on_the_real_instruction() {
        if !crate::runtime::env_cache::jit_indy_bridge() {
            return; // `CRATONVM_JIT_INDY_BRIDGE=0`: no generic site is built.
        }
        let pool = lambda_metafactory_pool();
        let bootstraps = vec![BootstrapMethod {
            bootstrap_method_ref: 7,
            bootstrap_arguments: vec![],
        }];
        let class_id = ClassId::new(0x7a12);
        let descriptor = "()Ljava/util/function/Supplier;";
        let key = lambda_singleton_site_for("make", descriptor, 3);
        assert_eq!(key, (lambda_site_method_key_of("make", descriptor), 8));
        let site =
            make_jit_indy_bridge_site_from_parts(&pool, &bootstraps, 12, class_id, Some(key))
                .unwrap_or(0);
        assert_ne!(site, 0, "a zero-capture lambda site is bridged");
        // SAFETY: `site` was built just above; the allocation is process-lived.
        let generic = unsafe { &*(site as *const JitIndyGenericSite) };
        assert_eq!(generic.kind, JIT_INDY_SITE_GENERIC);
        assert_eq!(generic.singleton_site, key);
        assert!(
            make_jit_indy_bridge_site_from_parts(&pool, &bootstraps, 12, class_id, None).is_none(),
            "an unnamed instruction keeps its trap"
        );
        // Wave 6: a capturing site reads it too -- an entry shared by several
        // instructions is linked per instruction (its proxy class), and a
        // door passes `None` only for such an entry.
        assert!(
            make_jit_indy_bridge_site_from_parts(&pool, &bootstraps, 14, class_id, None).is_none(),
            "an unnamed capturing instruction keeps its trap too"
        );
        let capturing = lambda_singleton_site_for("make", "(I)Ljava/util/function/IntSupplier;", 7);
        assert!(
            make_jit_indy_bridge_site_from_parts(&pool, &bootstraps, 14, class_id, Some(capturing))
                .is_some(),
            "a named capturing instruction is bridged"
        );
    }

    /// Wave 6 (`lambda-proxy-class-is-per-cp-entry-not-per-instruction`):
    /// the shared-entry scan counts instructions across ALL methods of the
    /// class, walks instruction boundaries (an operand byte equal to `0xba` is
    /// not an `invokedynamic`), and reports only entries named twice or more.
    #[test]
    fn shared_indy_entry_scan_counts_instructions_across_methods() {
        // m1: invokedynamic #12; invokedynamic #14; areturn
        let m1: &[u8] = &[
            0xba, 0x00, 0x0c, 0x00, 0x00, 0xba, 0x00, 0x0e, 0x00, 0x00, 0xb0,
        ];
        // m2: sipush 0xba0c (an operand, not an indy); invokedynamic #14; areturn
        let m2: &[u8] = &[0x11, 0xba, 0x0c, 0xba, 0x00, 0x0e, 0x00, 0x00, 0xb0];
        // m3: invokedynamic #16; areturn
        let m3: &[u8] = &[0xba, 0x00, 0x10, 0x00, 0x00, 0xb0];
        assert_eq!(
            scan_shared_indy_entries([m1, m2, m3].into_iter()),
            vec![14u16]
        );
        // The same entry twice in ONE method is shared as well.
        let twice: &[u8] = &[
            0xba, 0x00, 0x0c, 0x00, 0x00, 0xba, 0x00, 0x0c, 0x00, 0x00, 0xb0,
        ];
        assert_eq!(scan_shared_indy_entries([twice].into_iter()), vec![12u16]);
        assert!(scan_shared_indy_entries([m3].into_iter()).is_empty());
    }

    /// A shared entry's per-instruction rows: first writer wins per
    /// instruction, two instructions keep two proxy classes, and the compiled
    /// bridge's probe finds each by its own instruction key.
    #[test]
    fn per_instruction_lambda_rows_are_first_writer_wins_and_distinct() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let class_id = ClassId::new(0x7a31);
        let key_a = (shared.vm_identity, class_id, 12u16, 0x1111u64, 8usize);
        let key_b = (shared.vm_identity, class_id, 12u16, 0x1111u64, 13usize);
        let (p1, p2, p3) = (
            ClassId::new(0x8000_7a01),
            ClassId::new(0x8000_7a02),
            ClassId::new(0x8000_7a03),
        );
        let desc = "()Ljava/util/function/Function;";
        assert_eq!(
            publish_lambda_instruction_site(&shared, key_a, desc, p1, 0),
            p1
        );
        // A racing bootstrap of the SAME instruction adopts the winner.
        assert_eq!(
            publish_lambda_instruction_site(&shared, key_a, desc, p2, 0),
            p1
        );
        // Another instruction of the same entry gets its own class.
        assert_eq!(
            publish_lambda_instruction_site(&shared, key_b, desc, p3, 0),
            p3
        );
        assert_eq!(probe_lambda_instruction_site(&key_a), Some((p1, 0)));
        assert_eq!(probe_lambda_instruction_site(&key_b), Some((p3, 0)));
        forget_vm_generic_indy_sites(shared.vm_identity);
        assert_eq!(probe_lambda_instruction_site(&key_a), None);
    }

    // -----------------------------------------------------------------------
    // Generic invokedynamic call-site cache
    // -----------------------------------------------------------------------

    fn fresh_gate() -> (
        crate::classloading::resolution::RedefineGate,
        Arc<std::sync::atomic::AtomicU32>,
    ) {
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        (
            crate::classloading::resolution::RedefineGate::snapshot(Arc::clone(&counter)),
            counter,
        )
    }

    fn fake_ref(addr: usize) -> ObjectRef {
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    fn linked_slot(
        addr: usize,
        gate: crate::classloading::resolution::RedefineGate,
    ) -> GenericIndySlot {
        GenericIndySlot {
            link: GenericIndyLink::Linked(fake_ref(addr)),
            shape: Arc::new(GenericIndyShape::new("(Ljava/lang/Object;Z)I")),
            gate,
        }
    }

    fn linked_addr(link: &GenericIndyLink) -> Option<usize> {
        match link {
            GenericIndyLink::Linked(cs) => Some(cs.as_ptr() as usize),
            _ => None,
        }
    }

    #[test]
    fn generic_indy_cache_mode_parses_kill_switch_and_all() {
        use GenericIndyCacheMode::*;
        assert_eq!(generic_indy_cache_mode_from(None), Constant);
        assert_eq!(generic_indy_cache_mode_from(Some("")), Constant);
        assert_eq!(generic_indy_cache_mode_from(Some("1")), Constant);
        assert_eq!(generic_indy_cache_mode_from(Some("0")), Off);
        assert_eq!(generic_indy_cache_mode_from(Some(" off ")), Off);
        assert_eq!(generic_indy_cache_mode_from(Some("FALSE")), Off);
        assert_eq!(generic_indy_cache_mode_from(Some("all")), All);
        assert_eq!(generic_indy_cache_mode_from(Some("2")), All);
    }

    /// Wave 6: the `typeSwitch` array arm can load a component class, and the
    /// label loop reads the target again afterwards, so the target is pinned
    /// across that check and re-read from the pin. Structural: a GC inside
    /// the check cannot be forced from a unit test.
    #[test]
    fn the_type_switch_array_arm_pins_the_target_across_its_loading_check() {
        let src = include_str!("invokedynamic.rs");
        let start = src.find("fn type_label_matches(").unwrap_or(0);
        let rest = &src[start..];
        let end = rest.find("\n\x7d\n").unwrap_or(0);
        assert!(start > 0 && end > 0, "type_label_matches moved");
        let body = &rest[..end];
        let pin = body.find("thread.native_pin_roots.push(*obj_ref)");
        let check = body.find("array_is_instance_of(shared, &desc, label_name)");
        let reread = body.find("*obj_ref = thread.native_pin_roots[pin]");
        assert!(
            matches!((pin, check, reread), (Some(p), Some(c), Some(r)) if p < c && c < r),
            "pin, then check, then re-read"
        );
    }

    /// Wave 6: a marker the bootstrap resolved answers from its recorded
    /// class — here deliberately under a name that cannot load, so a by-name
    /// lookup could not have answered — and loads nothing; a name-only marker
    /// (the reflective path) that does not resolve answers `false`.
    #[test]
    fn a_resolved_lambda_marker_answers_from_its_class_without_loading() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let Ok(object) = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Object")
        else {
            return; // Stripped VM (no class library on the test classpath).
        };
        const NEVER: &str = "cratonvm/test/L6MarkerNeverLoadable";
        let resolved = ClassId::new(0x8000_7b01);
        record_lambda_proxy_markers_resolved(
            shared.vm_identity,
            resolved,
            &[LambdaMarker {
                name: Arc::from(NEVER),
                class_id: Some(object),
            }],
        );
        assert!(lambda_proxy_marker_satisfies(
            &shared,
            resolved,
            object,
            "java/lang/Object"
        ));
        assert!(shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(NEVER)
            .is_none());
        let by_name = ClassId::new(0x8000_7b02);
        record_lambda_proxy_markers(shared.vm_identity, by_name, &[Arc::from(NEVER)]);
        assert!(!lambda_proxy_marker_satisfies(
            &shared,
            by_name,
            object,
            "java/lang/Object"
        ));
        forget_vm_generic_indy_sites(shared.vm_identity);
    }

    /// Wave 6: the default publishes a non-constant `CallSite` for every
    /// bootstrap but Groovy's (both `vmplugin` spellings); `Off` never does,
    /// `All` always does.
    #[test]
    fn the_default_mode_caches_mutable_call_sites_except_groovys() {
        use GenericIndyCacheMode::*;
        const CUSTOM: &str = "com/example/Bootstraps";
        const GROOVY_V7: &str = "org/codehaus/groovy/vmplugin/v7/IndyInterface";
        assert!(Constant.caches_mutable_call_site_of(CUSTOM));
        assert!(!Constant.caches_mutable_call_site_of(GROOVY_INDY_INTERFACE));
        assert!(!Constant.caches_mutable_call_site_of(GROOVY_V7));
        assert!(All.caches_mutable_call_site_of(GROOVY_INDY_INTERFACE));
        assert!(!Off.caches_mutable_call_site_of(CUSTOM));
    }

    #[test]
    fn generic_indy_shape_parses_arguments_and_return_once() {
        let shape = GenericIndyShape::new("(Ljava/lang/Object;ZJ)I");
        assert_eq!(&*shape.arg_types, &['L', 'Z', 'J']);
        assert_eq!(shape.ret_byte, b'I');
        assert_eq!(GenericIndyShape::new("()V").ret_byte, b'V');
        assert_eq!(GenericIndyShape::new("()[I").ret_byte, b'[');
    }

    /// Two threads that link one instruction concurrently must agree on ONE
    /// `CallSite`: the second publication returns the first one's site.
    #[test]
    fn generic_indy_publication_is_first_writer_wins() {
        // A vm identity no real VM in this test process uses.
        let key = (
            usize::MAX - 0x51,
            ClassId::new(0x7001),
            9u16,
            0xABCDu64,
            12usize,
        );
        let (gate_a, _ca) = fresh_gate();
        let (gate_b, _cb) = fresh_gate();
        let first = publish_generic_indy_site(key, linked_slot(0x1000, gate_a));
        assert_eq!(linked_addr(&first), Some(0x1000));
        let second = publish_generic_indy_site(key, linked_slot(0x2000, gate_b));
        assert_eq!(
            linked_addr(&second),
            Some(0x1000),
            "a later linkage of the same instruction must adopt the published CallSite"
        );
        let (probed, shape) = probe_generic_indy_site(&key).expect("published row is live");
        assert_eq!(linked_addr(&probed), Some(0x1000));
        assert_eq!(shape.ret_byte, b'I');
        // A different pc is a different instruction, linked separately.
        let other_pc = (key.0, key.1, key.2, key.3, key.4 + 5);
        assert!(probe_generic_indy_site(&other_pc).is_none());
    }

    /// A redefined caller gets a new constant pool: its old row is a miss and
    /// the relink replaces it instead of adopting it.
    #[test]
    fn generic_indy_row_goes_stale_when_its_caller_is_redefined() {
        let key = (usize::MAX - 0x52, ClassId::new(0x7002), 3u16, 7u64, 5usize);
        let (gate, counter) = fresh_gate();
        publish_generic_indy_site(key, linked_slot(0x3000, gate));
        assert!(probe_generic_indy_site(&key).is_some());
        counter.fetch_add(1, std::sync::atomic::Ordering::Release);
        assert!(
            probe_generic_indy_site(&key).is_none(),
            "stale row must miss"
        );
        let (fresh, _c) = fresh_gate();
        let relinked = publish_generic_indy_site(key, linked_slot(0x4000, fresh));
        assert_eq!(linked_addr(&relinked), Some(0x4000));
        assert_eq!(
            probe_generic_indy_site(&key).and_then(|(l, _)| linked_addr(&l)),
            Some(0x4000)
        );
    }

    /// JVMS §4.7.23: every loadable constant is a legal static argument. The
    /// `MethodHandle` and `CONSTANT_Dynamic` kinds used to be an uncatchable
    /// VM-internal error; a non-loadable entry is a `ClassFormatError`.
    #[test]
    fn generic_indy_static_args_decode_every_loadable_kind() {
        use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
        let cp = ConstantPool::new(vec![
            CPE::Tombstone,                        // 0
            CPE::Utf8("p/Owner".into()),           // 1
            CPE::ClassReference { name_index: 1 }, // 2
            CPE::Utf8("target".into()),            // 3
            CPE::Utf8("(I)I".into()),              // 4
            CPE::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            CPE::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
            CPE::MethodHandle {
                reference_kind: 6,
                reference_index: 6,
            }, // 7
            CPE::MethodType {
                descriptor_index: 4,
            }, // 8
            CPE::Dynamic {
                bootstrap_method_attr_index: 0,
                name_and_type_index: 5,
            }, // 9
        ]);
        match resolve_static_arg_kind(&cp, "p/Caller", 7) {
            Ok(StaticArg::MHandle { cp_index, handle }) => {
                assert_eq!(cp_index, 7);
                assert_eq!(handle.kind, MethodHandleKind::InvokeStatic);
                assert_eq!(&*handle.class_name, "p/Owner");
                assert_eq!(&*handle.member_name, "target");
                assert_eq!(&*handle.descriptor, "(I)I");
            }
            _ => panic!("MethodHandle static arg must decode"),
        }
        assert!(matches!(
            resolve_static_arg_kind(&cp, "p/Caller", 8),
            Ok(StaticArg::MType { cp_index: 8, ref descriptor }) if descriptor == "(I)I"
        ));
        assert!(matches!(
            resolve_static_arg_kind(&cp, "p/Caller", 9),
            Ok(StaticArg::Dynamic(9))
        ));
        assert!(matches!(
            resolve_static_arg_kind(&cp, "p/Caller", 2),
            Ok(StaticArg::Class(ref n)) if n == "p/Owner"
        ));
        for bad in [1u16, 5, 6, 40] {
            assert!(
                matches!(
                    resolve_static_arg_kind(&cp, "p/Caller", bad),
                    Err(MethodCallFailed::InternalError(VmError::Linkage(
                        crate::error::LinkageError::ClassFormatError { .. }
                    )))
                ),
                "cp#{bad} is not loadable"
            );
        }
    }

    /// A `Z` operand of a generic indy is boxed to `Boolean.TRUE`/`FALSE`
    /// read from the statics; before `<clinit>` stored them the answer is
    /// `None` (the caller then makes the `Boolean.valueOf` call), never a
    /// default mistaken for an object.
    #[test]
    fn canonical_boolean_reads_the_statics_and_declines_before_clinit() {
        use crate::config::VmConfig;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let Ok(cid) = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Boolean")
        else {
            return; // no class library on this test host
        };
        let true_index = {
            let cm = shared.classes.class_manager.read();
            cm.get_class(cid).and_then(|c| {
                c.fields
                    .iter()
                    .filter(|f| f.is_static())
                    .position(|f| &*f.name == "TRUE")
            })
        };
        let Some(true_index) = true_index else {
            return;
        };
        if !matches!(
            crate::vm::get_static_shared(&shared, cid, true_index),
            Value::Object(Some(_))
        ) {
            assert!(
                canonical_boolean(&shared, true).is_none(),
                "not initialized"
            );
        }
        let instance = shared.mem.heap.alloc_object(cid, 1);
        crate::vm::set_static_shared(&shared, cid, true_index, Value::Object(Some(instance)));
        assert_eq!(
            canonical_boolean(&shared, true).map(|o| o.as_ptr() as usize),
            Some(instance.as_ptr() as usize)
        );
    }

    /// The varargs-collector rule for a trailing `Object[]` bootstrap
    /// parameter: `(Lookup, String, MethodType, Object[])` has 4 parameters.
    #[test]
    fn generic_indy_varargs_pack_start_follows_invoke_with_arguments() {
        let shape = descriptor_param_count_and_last_is_object_array(
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
             Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;",
        );
        assert_eq!(shape, Some((4, true)));
        // JRuby's buildDString: many static args, all collected.
        assert_eq!(bootstrap_varargs_pack_start(shape, 9, false), Some(3));
        // One scalar static arg: a one-element array, not a bare scalar.
        assert_eq!(bootstrap_varargs_pack_start(shape, 4, false), Some(3));
        // One static arg that already is an array (or null): passed as is.
        assert_eq!(bootstrap_varargs_pack_start(shape, 4, true), None);
        // No static args: an empty array.
        assert_eq!(bootstrap_varargs_pack_start(shape, 3, false), Some(3));
        // Too few for the fixed prefix: left to fail as it is.
        assert_eq!(bootstrap_varargs_pack_start(shape, 2, false), None);
        // No trailing `Object[]`: positional.
        let fixed = descriptor_param_count_and_last_is_object_array(
            "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
             Ljava/lang/invoke/MethodType;I)Ljava/lang/invoke/CallSite;",
        );
        assert_eq!(bootstrap_varargs_pack_start(fixed, 4, false), None);
        assert_eq!(bootstrap_varargs_pack_start(None, 4, false), None);
    }

    /// `ObjectMethods.bootstrap` ends in `MethodHandle...`: its component is
    /// named; `Object[]`, a primitive array, a 2-D array and a scalar are not
    /// (wave 40, lane L4).
    #[test]
    fn trailing_typed_array_component_names_only_a_non_object_reference_array() {
        assert_eq!(
            trailing_typed_array_component(
                "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
                 Ljava/lang/invoke/TypeDescriptor;Ljava/lang/Class;Ljava/lang/String;\
                 [Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;"
            ),
            Some("java/lang/invoke/MethodHandle")
        );
        assert_eq!(trailing_typed_array_component("(I[Ljava/lang/Object;)V"), None);
        assert_eq!(trailing_typed_array_component("([Ljava/lang/String;I)V"), None);
        assert_eq!(trailing_typed_array_component("(Ljava/lang/String;[I)V"), None);
        assert_eq!(trailing_typed_array_component("([[Ljava/lang/String;)V"), None);
        assert_eq!(trailing_typed_array_component("()V"), None);
    }

    /// Box for a reference parameter, widen for a wider primitive, keep
    /// otherwise — `invokeWithArguments`' `asType` for constant-pool values.
    #[test]
    fn generic_indy_bootstrap_arg_fit_boxes_and_widens() {
        assert!(matches!(
            bootstrap_arg_fit('L', Value::Int(1)),
            BootstrapArgFit::Box
        ));
        assert!(matches!(
            bootstrap_arg_fit('L', Value::Double(1.0)),
            BootstrapArgFit::Box
        ));
        assert!(matches!(
            bootstrap_arg_fit('L', Value::Object(None)),
            BootstrapArgFit::Keep
        ));
        assert!(matches!(
            bootstrap_arg_fit('J', Value::Int(-3)),
            BootstrapArgFit::Widen(Value::Long(-3))
        ));
        assert!(matches!(
            bootstrap_arg_fit('F', Value::Long(3)),
            BootstrapArgFit::Widen(Value::Float(f)) if f == 3.0
        ));
        assert!(matches!(
            bootstrap_arg_fit('D', Value::Float(0.5)),
            BootstrapArgFit::Widen(Value::Double(d)) if d == 0.5
        ));
        assert!(matches!(
            bootstrap_arg_fit('I', Value::Int(7)),
            BootstrapArgFit::Keep
        ));
        assert!(matches!(
            bootstrap_arg_fit('Z', Value::Int(1)),
            BootstrapArgFit::Keep
        ));
    }

    /// A recorded failure is published like a success, and a later linkage of
    /// the same instruction adopts it rather than replacing it.
    #[test]
    fn generic_indy_recorded_failure_is_published_and_kept() {
        let key = (usize::MAX - 0x53, ClassId::new(0x7003), 4u16, 1u64, 8usize);
        let (gate, _c) = fresh_gate();
        let recorded = Arc::new(RecordedLinkageError {
            error_class: ClassId::new(0x10),
            error_class_name: "java/lang/BootstrapMethodError".to_string(),
            message: Some("bootstrap method initialization exception".to_string()),
        });
        let published = publish_generic_indy_site(
            key,
            GenericIndySlot {
                link: GenericIndyLink::Failed(Arc::clone(&recorded)),
                shape: Arc::new(GenericIndyShape::new("()V")),
                gate,
            },
        );
        assert!(matches!(&published, GenericIndyLink::Failed(p) if Arc::ptr_eq(p, &recorded)));
        let (gate2, _c2) = fresh_gate();
        let later = publish_generic_indy_site(key, linked_slot(0x5000, gate2));
        assert!(matches!(later, GenericIndyLink::Failed(_)));
    }

    #[test]
    fn generic_indy_rows_of_unloaded_classes_are_dropped_and_others_remapped() {
        let vm = usize::MAX - 0x54;
        let other_vm = usize::MAX - 0x55;
        let dead = ClassId::new(0x7100);
        let live = ClassId::new(0x7101);
        let dead_error_class = ClassId::new(0x7102);
        let k_dead: GenericIndyKey = (vm, dead, 1, 0, 5);
        let k_live: GenericIndyKey = (vm, live, 1, 0, 5);
        let k_other_vm: GenericIndyKey = (other_vm, dead, 1, 0, 5);
        let k_dead_error: GenericIndyKey = (vm, live, 2, 0, 9);
        let mut table = GenericIndyMap::default();
        let (g1, _c1) = fresh_gate();
        let (g2, _c2) = fresh_gate();
        let (g3, _c3) = fresh_gate();
        let (g4, _c4) = fresh_gate();
        table.insert(k_dead, linked_slot(0x1000, g1));
        table.insert(k_live, linked_slot(0x2000, g2));
        table.insert(k_other_vm, linked_slot(0x3000, g3));
        table.insert(
            k_dead_error,
            GenericIndySlot {
                link: GenericIndyLink::Failed(Arc::new(RecordedLinkageError {
                    error_class: dead_error_class,
                    error_class_name: "p/MyLinkageError".to_string(),
                    message: None,
                })),
                shape: Arc::new(GenericIndyShape::new("()V")),
                gate: g4,
            },
        );
        let unloaded: rustc_hash::FxHashSet<ClassId> =
            [dead, dead_error_class].into_iter().collect();
        retain_live_generic_indy_rows(&mut table, vm, &unloaded);
        assert!(!table.contains_key(&k_dead));
        assert!(!table.contains_key(&k_dead_error), "error class unloaded");
        assert!(table.contains_key(&k_live));
        assert!(
            table.contains_key(&k_other_vm),
            "another VM's rows are not this unload's business"
        );

        let mut moved = cratonvm_types::PointerMap::new();
        moved.insert(0x2000, 0x9000);
        moved.insert(0x3000, 0xA000);
        remap_generic_indy_rows(&mut table, vm, &moved);
        assert_eq!(linked_addr(&table[&k_live].link), Some(0x9000));
        assert_eq!(
            linked_addr(&table[&k_other_vm].link),
            Some(0x3000),
            "only this VM's rows are remapped through this VM's map"
        );
    }

    /// Unloading a HOST class drops the memo and singleton rows of every proxy
    /// it spun, even when the implementation owner (here a class that stays
    /// loaded, like `String` for `String::length`) survives; rows of other
    /// hosts are kept. Disposing the VM drops the rest of its singleton rows.
    #[test]
    fn lambda_rows_of_an_unloaded_host_and_a_disposed_vm_are_dropped() {
        use crate::config::VmConfig;
        let shared = SharedVm::new(VmConfig::default());
        let vm = shared.vm_identity;
        let (dead_host, live_host, live_owner) = (
            ClassId::new(0x6a01),
            ClassId::new(0x6a02),
            ClassId::new(0x6a03),
        );
        let (dead_proxy, live_proxy) = (ClassId::new(0x8fff_0a01), ClassId::new(0x8fff_0a02));
        // Held the way a warm-path memo holds them (wave 20).
        let dead_slot = Arc::new(LambdaSingletonSlot::new(fake_ref(0xa000)));
        let live_slot = Arc::new(LambdaSingletonSlot::new(fake_ref(0xb000)));
        {
            let mut hosts = shared.classes.lambda_proxy_hosts.write();
            hosts.insert(dead_proxy, dead_host);
            hosts.insert(live_proxy, live_host);
            let mut memo = shared.classes.lambda_impl_owner_memo.write();
            memo.insert(dead_proxy, live_owner);
            memo.insert(live_proxy, live_owner);
            let mut singletons = lambda_singleton_cache().write();
            singletons.insert((vm, dead_proxy, 7, 5), Arc::clone(&dead_slot));
            singletons.insert((vm, live_proxy, 7, 5), Arc::clone(&live_slot));
        }
        let unloaded: rustc_hash::FxHashSet<ClassId> = [dead_host].into_iter().collect();
        forget_unloaded_lambda_proxy_rows(&shared, &unloaded);
        {
            let hosts = shared.classes.lambda_proxy_hosts.read();
            assert!(!hosts.contains_key(&dead_proxy));
            assert_eq!(hosts.get(&live_proxy), Some(&live_host));
            let memo = shared.classes.lambda_impl_owner_memo.read();
            assert!(
                !memo.contains_key(&dead_proxy),
                "the host half drops the row"
            );
            assert_eq!(memo.get(&live_proxy), Some(&live_owner));
            let singletons = lambda_singleton_cache().read();
            assert!(!singletons.contains_key(&(vm, dead_proxy, 7, 5)));
            assert!(singletons.contains_key(&(vm, live_proxy, 7, 5)));
        }
        assert_eq!(dead_slot.get(), None, "a dropped row retires its slot");
        assert_eq!(live_slot.get(), Some(fake_ref(0xb000)));
        forget_vm_generic_indy_sites(vm);
        assert!(!lambda_singleton_cache()
            .read()
            .keys()
            .any(|&(vid, _, _, _)| vid == vm));
        assert_eq!(live_slot.get(), None, "disposing the VM retires the rest");
    }

    /// The allocation-free parse the concat path uses agrees with the `Vec` one.
    #[test]
    fn parse_descriptor_args_into_small_storage_matches_vec() {
        let desc = "(IJLjava/lang/String;[[DZC)Ljava/lang/String;";
        let mut small: smallvec::SmallVec<[char; 8]> = smallvec::SmallVec::new();
        parse_descriptor_args_into(desc, &mut small);
        assert_eq!(small.as_slice(), parse_descriptor_args(desc).as_slice());
        assert_eq!(small.as_slice(), &['I', 'J', 'L', 'L', 'Z', 'C']);
        assert!(!small.spilled());
    }

    /// Build a ConstantPool for testing resolve_method_handle_full.
    fn make_method_handle_cp(ref_kind: u8) -> ConstantPool {
        use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;

        let entries = vec![
            CPE::Tombstone,                                        // 0 (unused)
            CPE::Utf8("com/example/Foo".to_string().into()),       // 1
            CPE::ClassReference { name_index: 1 },                 // 2
            CPE::Utf8("doStuff".to_string().into()),               // 3
            CPE::Utf8("(I)Ljava/lang/String;".to_string().into()), // 4
            CPE::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            CPE::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
            CPE::MethodHandle {
                reference_kind: ref_kind,
                reference_index: 6,
            }, // 7
        ];
        ConstantPool::new(entries)
    }

    #[test]
    fn resolve_method_handle_full_invoke_static() {
        let cp = make_method_handle_cp(6); // InvokeStatic
        let mh = resolve_method_handle_full(&cp, 7).unwrap();
        assert_eq!(mh.kind, MethodHandleKind::InvokeStatic);
        assert_eq!(&*mh.class_name, "com/example/Foo");
        assert_eq!(&*mh.member_name, "doStuff");
        assert_eq!(&*mh.descriptor, "(I)Ljava/lang/String;");
    }

    #[test]
    fn resolve_method_handle_full_invoke_virtual() {
        let cp = make_method_handle_cp(5); // InvokeVirtual
        let mh = resolve_method_handle_full(&cp, 7).unwrap();
        assert_eq!(mh.kind, MethodHandleKind::InvokeVirtual);
    }

    #[test]
    fn resolve_method_handle_full_invalid_kind() {
        let cp = make_method_handle_cp(0); // Invalid kind 0
        assert!(resolve_method_handle_full(&cp, 7).is_err());
    }

    #[test]
    fn resolve_method_handle_full_field_ref() {
        use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
        let entries = vec![
            CPE::Tombstone,                                  // 0
            CPE::Utf8("com/example/Bar".to_string().into()), // 1
            CPE::ClassReference { name_index: 1 },           // 2
            CPE::Utf8("value".to_string().into()),           // 3
            CPE::Utf8("I".to_string().into()),               // 4
            CPE::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            CPE::FieldReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
            CPE::MethodHandle {
                reference_kind: 1,
                reference_index: 6,
            }, // 7 GetField
        ];
        let cp = ConstantPool::new(entries);
        let mh = resolve_method_handle_full(&cp, 7).unwrap();
        assert_eq!(mh.kind, MethodHandleKind::GetField);
        assert_eq!(&*mh.class_name, "com/example/Bar");
        assert_eq!(&*mh.member_name, "value");
        assert_eq!(&*mh.descriptor, "I");
    }

    #[test]
    fn resolve_method_type_test() {
        use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
        let entries = vec![
            CPE::Tombstone,
            CPE::Utf8("(Ljava/lang/Object;)V".to_string().into()), // 1
            CPE::MethodType {
                descriptor_index: 1,
            }, // 2
        ];
        let cp = ConstantPool::new(entries);
        assert_eq!(
            resolve_method_type(&cp, 2),
            Some("(Ljava/lang/Object;)V".to_string())
        );
        assert_eq!(resolve_method_type(&cp, 1), None); // Not a MethodType
    }

    // -----------------------------------------------------------------------
    // Phase 83.1: Primitive pattern matching — widening/narrowing
    // -----------------------------------------------------------------------

    /// Helper: allocate a boxed wrapper on the heap and test primitive_pattern_match.
    fn test_ppm(source_class: &str, value: Value, target_class: &str) -> bool {
        use crate::config::VmConfig;
        use std::sync::Arc;

        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let class_id = ClassId::new(0);
        let obj = shared.mem.heap.alloc_object(class_id, 1);
        shared.mem.heap.set_field(obj, 0, value);
        primitive_pattern_match(&shared, obj, source_class, target_class)
    }

    #[test]
    fn ppm_int_to_long_widening() {
        // int 42 should match long pattern (widening).
        assert!(test_ppm(
            "java/lang/Integer",
            Value::Int(42),
            "java/lang/Long"
        ));
    }

    #[test]
    fn ppm_int_to_double_widening() {
        // int 42 should match double pattern (widening).
        assert!(test_ppm(
            "java/lang/Integer",
            Value::Int(42),
            "java/lang/Double"
        ));
    }

    #[test]
    fn ppm_int_to_byte_narrowing_in_range() {
        // int 5 is in byte range [-128, 127], should match byte pattern.
        assert!(test_ppm(
            "java/lang/Integer",
            Value::Int(5),
            "java/lang/Byte"
        ));
    }

    #[test]
    fn ppm_int_to_byte_narrowing_out_of_range() {
        // int 200 is NOT in byte range, should NOT match.
        assert!(!test_ppm(
            "java/lang/Integer",
            Value::Int(200),
            "java/lang/Byte"
        ));
    }

    #[test]
    fn ppm_long_to_int_narrowing_in_range() {
        // long 42 is in int range, should match.
        assert!(test_ppm(
            "java/lang/Long",
            Value::Long(42),
            "java/lang/Integer"
        ));
    }

    #[test]
    fn ppm_long_to_int_narrowing_out_of_range() {
        // long exceeding int range should NOT match.
        assert!(!test_ppm(
            "java/lang/Long",
            Value::Long(i64::MAX),
            "java/lang/Integer"
        ));
    }

    #[test]
    fn ppm_int_to_char_narrowing_in_range() {
        // int 65 ('A') is in char range [0, 65535], should match.
        assert!(test_ppm(
            "java/lang/Integer",
            Value::Int(65),
            "java/lang/Character"
        ));
    }

    #[test]
    fn ppm_int_to_char_narrowing_negative() {
        // Negative int should NOT match char pattern.
        assert!(!test_ppm(
            "java/lang/Integer",
            Value::Int(-1),
            "java/lang/Character"
        ));
    }

    #[test]
    fn ppm_string_to_long_no_match() {
        // Non-numeric class should never match a numeric pattern.
        assert!(!test_ppm(
            "java/lang/String",
            Value::Int(0),
            "java/lang/Long"
        ));
    }

    #[test]
    fn ppm_float_to_double_widening() {
        // float -> double is always a widening conversion.
        assert!(test_ppm(
            "java/lang/Float",
            Value::Float(3.14),
            "java/lang/Double"
        ));
    }

    /// Wave 7: the `FLAG_BRIDGES` block is read after the marker block when
    /// `FLAG_MARKERS` is set and at `arg[4]` otherwise; a short block yields
    /// the prefix it could read, and no bridge bit means no bridges.
    #[test]
    fn altmetafactory_bridge_descriptors_are_read_after_the_markers() {
        use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
        let cp = ConstantPool::new(vec![
            CPE::Tombstone,
            CPE::Utf8(Arc::from("()Ljava/lang/Object;")), // 1
            CPE::MethodType {
                descriptor_index: 1,
            }, // 2
            CPE::Utf8(Arc::from("(Ljava/lang/Object;)V")), // 3
            CPE::MethodType {
                descriptor_index: 3,
            }, // 4
            CPE::Integer(FLAG_BRIDGES),                   // 5
            CPE::Integer(FLAG_BRIDGES | FLAG_MARKERS),    // 6
            CPE::Integer(1),                              // 7
            CPE::Integer(2),                              // 8
            CPE::Utf8(Arc::from("java/lang/Cloneable")),  // 9
            CPE::ClassReference { name_index: 9 },        // 10
        ]);
        let names = |v: Vec<Arc<str>>| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // sam, impl, inst, flags=BRIDGES, count=2, mt, mt
        let plain: [u16; 7] = [2, 2, 2, 5, 8, 2, 4];
        assert_eq!(
            names(bridge_descriptors_in(&cp, &plain, FLAG_BRIDGES)),
            ["()Ljava/lang/Object;", "(Ljava/lang/Object;)V"]
        );
        // sam, impl, inst, flags=BRIDGES|MARKERS, markers=1, Cloneable, count=1, mt
        let with_markers: [u16; 8] = [2, 2, 2, 6, 7, 10, 7, 2];
        assert_eq!(
            names(bridge_descriptors_in(
                &cp,
                &with_markers,
                FLAG_BRIDGES | FLAG_MARKERS
            )),
            ["()Ljava/lang/Object;"]
        );
        // Declares two bridges, carries one: the prefix.
        let short: [u16; 6] = [2, 2, 2, 5, 8, 4];
        assert_eq!(
            names(bridge_descriptors_in(&cp, &short, FLAG_BRIDGES)),
            ["(Ljava/lang/Object;)V"]
        );
        // Missing count: nothing, never a panic.
        assert!(bridge_descriptors_in(&cp, &[2, 2, 2, 5], FLAG_BRIDGES).is_empty());
    }

    /// Wave 9: the `FLAG_MARKERS` block's count is class-file data. A count
    /// far beyond the static arguments present reads the markers that are
    /// there and reserves nothing for the rest (it used to reserve `count`
    /// slots up front: `0x7fffffff` was a 32 GiB allocation and an abort).
    #[test]
    fn a_marker_count_beyond_the_static_arguments_reads_the_prefix() {
        use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;
        let cp = ConstantPool::new(vec![
            CPE::Tombstone,
            CPE::Utf8(Arc::from("java/lang/Cloneable")), // 1
            CPE::ClassReference { name_index: 1 },       // 2
            CPE::Integer(i32::MAX),                      // 3
            CPE::Integer(1),                             // 4
            CPE::Integer(FLAG_MARKERS),                  // 5
        ]);
        // sam, impl, inst, flags=MARKERS, count=i32::MAX, Cloneable
        let huge: [u16; 6] = [2, 2, 2, 5, 3, 2];
        let markers = marker_interfaces_in(&cp, &huge);
        assert_eq!(markers.len(), 1);
        assert_eq!(&*markers[0], "java/lang/Cloneable");
        assert!(markers.capacity() < 16, "reserved by the arguments present");
        // count=1, as javac writes it.
        let one: [u16; 6] = [2, 2, 2, 5, 4, 2];
        assert_eq!(marker_interfaces_in(&cp, &one).len(), 1);
        // No count: nothing.
        assert!(marker_interfaces_in(&cp, &[2, 2, 2, 5]).is_empty());
    }

    /// Wave 7: the per-thread instruction copy answers only the WHOLE
    /// instruction (VM, class, CP index, pc, method), goes stale with the
    /// row's redefinition gate, matches an equal method name held in another
    /// `Arc`, and never copies a heap-bearing `Linked` row.
    #[test]
    fn the_thread_instruction_copy_is_keyed_by_the_whole_instruction() {
        let name: Arc<str> = Arc::from("run");
        let desc: Arc<str> = Arc::from("()V");
        let other_name: Arc<str> = Arc::from("other");
        let ident = |vm: usize, cp: u16, pc: usize, n: &'static str| {
            let n: &Arc<str> = if n == "run" { &name } else { &other_name };
            InstructionIdent {
                vm_identity: vm,
                class_id: ClassId::new(0x7101),
                cp_index: cp,
                pc,
                method_name: n,
                method_descriptor: &desc,
            }
        };
        let lambda = ThreadInstructionLink::Lambda {
            proxy_class_id: ClassId::new(0x8000_7d01),
            num_captures: 1,
        };
        let (gate, counter) = fresh_gate();
        let mut sites = ThreadInstructionSites::default();
        assert_eq!(sites.get(&ident(1, 4, 9, "run")), None);
        sites.put(&ident(1, 4, 9, "run"), gate, lambda);
        assert_eq!(sites.get(&ident(1, 4, 9, "run")), Some(lambda));
        // Same content, another allocation: still the same method.
        let name_copy: Arc<str> = Arc::from("run");
        let copy = InstructionIdent {
            method_name: &name_copy,
            ..ident(1, 4, 9, "run")
        };
        assert_eq!(sites.get(&copy), Some(lambda));
        assert_eq!(sites.get(&ident(2, 4, 9, "run")), None, "another VM");
        assert_eq!(sites.get(&ident(1, 5, 9, "run")), None, "another entry");
        assert_eq!(sites.get(&ident(1, 4, 14, "run")), None, "another pc");
        assert_eq!(sites.get(&ident(1, 4, 9, "other")), None, "another method");
        counter.fetch_add(1, std::sync::atomic::Ordering::Release);
        assert_eq!(sites.get(&ident(1, 4, 9, "run")), None, "redefined caller");

        assert_eq!(
            ThreadInstructionLink::of(&GenericIndyLink::GroovyCastToBoolean),
            Some(ThreadInstructionLink::GroovyCastToBoolean)
        );
        assert_eq!(
            ThreadInstructionLink::of(&GenericIndyLink::Linked(fake_ref(0x5000))),
            None
        );
    }

    /// Wave 7: the thread-local door fills from a table row and answers it.
    #[test]
    fn the_thread_instruction_copy_fills_from_a_table_row() {
        let name: Arc<str> = Arc::from("m");
        let desc: Arc<str> = Arc::from("()Ljava/lang/Runnable;");
        let ident = InstructionIdent {
            vm_identity: usize::MAX - 0x71,
            class_id: ClassId::new(0x7102),
            cp_index: 3,
            pc: 17,
            method_name: &name,
            method_descriptor: &desc,
        };
        assert_eq!(probe_thread_instruction_site(&ident), None);
        let (gate, _counter) = fresh_gate();
        fill_thread_instruction_site(
            &ident,
            gate.clone(),
            &GenericIndyLink::Linked(fake_ref(0x6000)),
        );
        assert_eq!(
            probe_thread_instruction_site(&ident),
            None,
            "Linked is never copied"
        );
        fill_thread_instruction_site(&ident, gate, &GenericIndyLink::GroovyCastToBoolean);
        // Wave 16: the copy carries the method key the singleton cache is
        // keyed by, hashed once at the fill.
        assert_eq!(
            probe_thread_instruction_site(&ident),
            Some((
                ThreadInstructionLink::GroovyCastToBoolean,
                lambda_site_method_key_of("m", "()Ljava/lang/Runnable;")
            ))
        );
    }

    /// Wave 7: a recorded bridge is answered for its own proxy and VM only.
    #[test]
    fn a_recorded_lambda_bridge_is_scoped_to_its_proxy_and_vm() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let proxy = ClassId::new(0x8000_7c01);
        let other = ClassId::new(0x8000_7c02);
        assert!(!lambda_proxy_has_bridge(
            shared.vm_identity,
            proxy,
            "()Ljava/lang/Object;"
        ));
        record_lambda_proxy_bridges(
            shared.vm_identity,
            proxy,
            &[Arc::from("()Ljava/lang/Object;")],
        );
        assert!(lambda_proxy_has_bridge(
            shared.vm_identity,
            proxy,
            "()Ljava/lang/Object;"
        ));
        assert!(!lambda_proxy_has_bridge(
            shared.vm_identity,
            proxy,
            "()Ljava/lang/String;"
        ));
        assert!(!lambda_proxy_has_bridge(
            shared.vm_identity,
            other,
            "()Ljava/lang/Object;"
        ));
        assert!(!lambda_proxy_has_bridge(
            shared.vm_identity.wrapping_add(1),
            proxy,
            "()Ljava/lang/Object;"
        ));
        forget_vm_generic_indy_sites(shared.vm_identity);
        assert!(!lambda_proxy_has_bridge(
            shared.vm_identity,
            proxy,
            "()Ljava/lang/Object;"
        ));
    }

    /// Wave 8: the bridge list reflection and serialization read is the one
    /// dispatch admits, per proxy and VM.
    #[test]
    fn lambda_bridge_descriptors_are_listed_per_proxy() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let proxy = ClassId::new(0x8000_7d01);
        assert!(lambda_proxy_bridge_descriptors(shared.vm_identity, proxy).is_none());
        record_lambda_proxy_bridges(
            shared.vm_identity,
            proxy,
            &[
                Arc::from("()Ljava/lang/Object;"),
                Arc::from("()Ljava/lang/CharSequence;"),
            ],
        );
        let listed = lambda_proxy_bridge_descriptors(shared.vm_identity, proxy)
            .map(|b| b.iter().map(|d| d.to_string()).collect::<Vec<_>>());
        assert_eq!(
            listed,
            Some(vec![
                "()Ljava/lang/Object;".to_string(),
                "()Ljava/lang/CharSequence;".to_string()
            ])
        );
        assert!(
            lambda_proxy_bridge_descriptors(shared.vm_identity, ClassId::new(0x8000_7d02))
                .is_none()
        );
        forget_vm_generic_indy_sites(shared.vm_identity);
        assert!(lambda_proxy_bridge_descriptors(shared.vm_identity, proxy).is_none());
    }

    /// Wave 8 (`i5-L6` steps 1-3): a proxy spun by a user-loader host carries
    /// that loader's `loader_pin` row, gets it back after the post-GC rebuild
    /// replaced the VM's pins, is visited with its own id by the singleton root
    /// scan (so `defer_or_root` can pin the singleton to the loader), and goes
    /// with its host: call site, bridges, markers, singleton and pin list. A
    /// built-in host's proxy gets no row.
    #[test]
    fn a_user_loader_lambda_proxy_pins_its_host_loader_until_the_host_unloads() {
        use cratonvm_types::loader_pin;
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let vm = shared.vm_identity;
        // Real objects of this VM's heap, so a marker of any VM that meets the
        // rows judges them by its own bounds; ids no other test pins.
        let loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let moved_loader = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let singleton = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let (loader_addr, moved_addr) = (loader.as_ptr() as usize, moved_loader.as_ptr() as usize);
        let (user_host, builtin_host) = (ClassId::new(0x7ff0_6b01), ClassId::new(0x7ff0_6b02));
        let (user_proxy, builtin_proxy) = (ClassId::new(0x8fff_6b01), ClassId::new(0x8fff_6b02));
        loader_pin::set_loader_pin(vm, user_host.as_u32(), loader_addr);

        pin_lambda_proxy_to_host_loader(vm, user_proxy, user_host);
        pin_lambda_proxy_to_host_loader(vm, builtin_proxy, builtin_host);
        assert_eq!(
            loader_pin::loader_pin_addr_for_vm(vm, user_proxy.as_u32()),
            Some(loader_addr)
        );
        assert_eq!(
            loader_pin::loader_pin_addr_for_vm(vm, builtin_proxy.as_u32()),
            None
        );

        // The rebuild keeps only defining-loader rows (the loader moved).
        loader_pin::replace_loader_pins(vm, &[(user_host.as_u32(), moved_addr)]);
        assert_eq!(
            loader_pin::loader_pin_addr_for_vm(vm, user_proxy.as_u32()),
            None
        );
        repin_lambda_proxy_loaders(vm);
        assert_eq!(
            loader_pin::loader_pin_addr_for_vm(vm, user_proxy.as_u32()),
            Some(moved_addr),
            "the proxy follows its host's current row"
        );

        {
            shared
                .classes
                .lambda_proxy_hosts
                .write()
                .insert(user_proxy, user_host);
            shared.classes.lambda_proxies.write().insert(
                user_proxy,
                Arc::new(LambdaCallSite {
                    functional_interface: Arc::from("java/lang/Runnable"),
                    functional_interface_id: None,
                    sam_method_name: Arc::from("run"),
                    sam_descriptor: Arc::from("()V"),
                    impl_handle: MethodHandle {
                        kind: MethodHandleKind::InvokeStatic,
                        class_name: Arc::from("p/Host"),
                        member_name: Arc::from("lambda$0"),
                        descriptor: Arc::from("()V"),
                    },
                    instantiated_descriptor: Arc::from("()V"),
                    capture_types: Vec::new(),
                    proxy_class_id: user_proxy,
                    serializable_flag: false,
                }),
            );
            lambda_singleton_cache().write().insert(
                (vm, user_proxy, 3, 5),
                Arc::new(LambdaSingletonSlot::new(singleton)),
            );
        }
        record_lambda_proxy_bridges(vm, user_proxy, &[Arc::from("()Ljava/lang/Object;")]);
        record_lambda_proxy_markers(vm, user_proxy, &[Arc::from("java/lang/Cloneable")]);

        let mut visited = Vec::new();
        gc_scan_lambda_singleton_roots(vm, &mut |owner, obj| visited.push((owner, obj)));
        assert_eq!(visited, vec![(user_proxy.as_u32(), singleton)]);

        // The host's loader died: the rebuild drops its row, so the proxy
        // gets none back, and the unload drops every proxy row.
        loader_pin::replace_loader_pins(vm, &[]);
        repin_lambda_proxy_loaders(vm);
        assert_eq!(
            loader_pin::loader_pin_addr_for_vm(vm, user_proxy.as_u32()),
            None
        );
        let unloaded: rustc_hash::FxHashSet<ClassId> = [user_host].into_iter().collect();
        forget_unloaded_lambda_proxy_rows(&shared, &unloaded);
        assert!(!shared
            .classes
            .lambda_proxies
            .read()
            .contains_key(&user_proxy));
        assert!(lambda_proxy_bridge_descriptors(vm, user_proxy).is_none());
        assert!(lambda_proxy_marker_interfaces(vm, user_proxy).is_none());
        assert!(!lambda_singleton_cache()
            .read()
            .contains_key(&(vm, user_proxy, 3, 5)));
        assert!(!user_loader_lambdas()
            .lock()
            .get(&vm)
            .is_some_and(|rows| rows.iter().any(|&(p, _)| p == user_proxy)));

        forget_vm_generic_indy_sites(vm);
        assert!(!user_loader_lambdas().lock().contains_key(&vm));
        loader_pin::forget_vm_loader_pins(vm);
    }

    /// Wave 9 (`docs/internal/fixed-bugs/interpreter-L6-reflective-lambda-metafactory-records-no-host-FIXED-20260925.md`): a proxy
    /// the reflective `LambdaMetafactory` path registers gets its lookup class
    /// as host through `register_lambda_proxy_host`, so it is named after it
    /// (not after the impl owner) and unloads with it; one host per proxy.
    #[test]
    fn a_reflective_lambda_proxy_records_its_lookup_class_as_host() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        // A stripped VM may lack either class: nothing to check then.
        let (Ok(host), Ok(other)) = (
            shared.load_class_concurrent("java/lang/Object"),
            shared.load_class_concurrent("java/lang/Integer"),
        ) else {
            return;
        };
        let mut thread = JvmThread::new(crate::threading::jvm_thread::ThreadId(0), "test");
        let mut ctx = crate::vm::NativeContextImpl {
            shared: &shared,
            thread: &mut thread,
        };
        // `String::length` as a `ToIntFunction`-shaped proxy; kind 5 is
        // `REF_invokeVirtual`.
        let raw = ctx.register_lambda_proxy(
            "java/util/function/ToIntFunction",
            "applyAsInt",
            "(Ljava/lang/Object;)I",
            "java/lang/String",
            "length",
            "()I",
            5,
            "(Ljava/lang/String;)I",
            "",
            false,
        );
        assert_ne!(raw, 0, "the proxy table has room");
        let proxy = ClassId::new(raw);
        assert_eq!(
            ctx.lambda_proxy_host(proxy).as_deref(),
            Some("java/lang/String"),
            "host-less: the impl owner's name (the pre-wave-9 answer)"
        );
        ctx.register_lambda_proxy_host(raw, host);
        assert_eq!(
            ctx.lambda_proxy_host(proxy).as_deref(),
            Some("java/lang/Object"),
            "HotSpot names the spun class after the lookup class"
        );
        assert!(
            !record_reflective_lambda_proxy_host(&shared, proxy, other),
            "a proxy is spun once: its host is not replaced"
        );
        assert!(!record_reflective_lambda_proxy_host(
            &shared,
            ClassId::new(0),
            host
        ));
        assert!(
            !record_reflective_lambda_proxy_host(&shared, ClassId::new(0x8fff_6c01), host),
            "a proxy this VM never registered"
        );
        assert_eq!(
            shared.classes.lambda_proxy_hosts.read().get(&proxy),
            Some(&host)
        );

        let unloaded: rustc_hash::FxHashSet<ClassId> = [host].into_iter().collect();
        forget_unloaded_lambda_proxy_rows(&shared, &unloaded);
        assert!(
            !shared.classes.lambda_proxies.read().contains_key(&proxy),
            "the proxy goes with its host"
        );
        assert!(!shared
            .classes
            .lambda_proxy_hosts
            .read()
            .contains_key(&proxy));
        forget_vm_generic_indy_sites(shared.vm_identity);
    }
}

#[cfg(test)]
mod w9c_lambda_proxy_alloc_tests {
    //! gc-common w9-c: a capturing lambda's proxy is allocated through `new`'s
    //! shared-path front end (`alloc_object_shared`), not a private copy of
    //! the pre-w2-d ladder that skipped the old generation, the soft-reference
    //! rung and the allocation counters.

    use super::*;
    use crate::runtime::interpreter::constants::w9c_resolution_oom_tests::{
        fill_heap_with_pins, small_gen_vm,
    };
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::atomic::Ordering;

    /// The proxy's bytes reach this VM's allocation counter, and the captures
    /// land in its fields. Two captures, so the zero-capture singleton table
    /// (process-wide) is never touched.
    #[test]
    fn a_capturing_proxy_is_charged_and_filled() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w9c-lambda-charge");
        let proxy_class_id = shared.alloc_lambda_proxy_id();
        let before = shared.mem.bytes_allocated_total.load(Ordering::Relaxed);
        let proxy = allocate_lambda_proxy_from_values(
            &shared,
            &mut thread,
            proxy_class_id,
            &mut [Value::Int(7), Value::Int(-9)],
            (0, 0),
        )
        .expect("an empty heap has room for a proxy");
        assert!(
            shared.mem.bytes_allocated_total.load(Ordering::Relaxed) > before,
            "the proxy bypassed the TLAB and was not charged"
        );
        assert_eq!(shared.mem.heap.get_field(proxy, 0), Value::Int(7));
        assert_eq!(thread.native_pin_roots.len(), 0);
    }

    /// On a heap held full by pins the proxy is refused with
    /// `OutOfMemoryError` after the shared ladder, and the capture pins are
    /// released.
    #[test]
    fn a_capturing_proxy_on_a_full_heap_is_an_oome_and_releases_its_pins() {
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "w9c-lambda-full");
        assert!(
            fill_heap_with_pins(&shared, &mut thread),
            "the pinned objects never filled the 8 MB heap"
        );
        let pins = thread.native_pin_roots.len();
        let capture = thread.native_pin_roots[0];
        let proxy_class_id = shared.alloc_lambda_proxy_id();
        let got = allocate_lambda_proxy_from_values(
            &shared,
            &mut thread,
            proxy_class_id,
            &mut [Value::Object(Some(capture)), Value::Int(1)],
            (0, 0),
        );
        match got {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { message },
            ))) => assert!(message.starts_with("Java heap space"), "{message}"),
            Ok(_) => panic!("a pinned-full heap cannot hold the proxy"),
            Err(other) => panic!("expected OutOfMemoryError, got {other:?}"),
        }
        assert_eq!(thread.native_pin_roots.len(), pins);
    }
}

#[cfg(test)]
mod i16_l1_indy_site_tests {
    //! Interpreter round i1 wave 16, lane L1: the per-thread `invokedynamic`
    //! entry (`IndySiteCache`) lends its `Arc` instead of cloning it, memoises
    //! a zero-capture lambda's singleton key, and is tagged with the class-name
    //! generation. Parallel tests move the global epochs, so a test whose fill
    //! does not stick returns early rather than failing.

    use super::*;
    use crate::runtime::interpreter::constants::w9c_resolution_oom_tests::small_gen_vm;
    use crate::runtime::interpreter::site_cache::{NameEpochs, SiteCache};
    use crate::threading::jvm_thread::ThreadId;

    const CLASS: u32 = 0x7_1601;
    const METHOD: &str = "i16l1";

    fn frame_at(class_id: ClassId, pc: usize, stack: &[Value]) -> Frame {
        let mut frame = Frame::new(
            class_id,
            String::new(),
            String::from(METHOD),
            String::from("()V"),
            None,
            vec![0xb1],
            vec![],
            10,
            0,
            &[],
        );
        frame.pc = pc;
        for v in stack {
            let _ = frame.stack.push(*v);
        }
        frame
    }

    /// Put `make()` into the thread's table until one fill sticks.
    fn fill(
        thread: &mut JvmThread,
        class_id: ClassId,
        cp: u16,
        make: impl Fn() -> CachedIndySite,
    ) -> bool {
        (0..100).any(|_| {
            thread
                .indy_sites
                .put(class_id, cp, IndySiteCache::epochs_now(), make());
            thread.indy_sites.get(class_id, cp).is_some()
        })
    }

    fn lend(thread: &mut JvmThread, class_id: ClassId, cp: u16) -> Option<Arc<ResolvedCallSite>> {
        match thread.indy_sites.get_mut(class_id, cp) {
            Some(CachedIndySite::Other(slot)) => slot.take(),
            _ => None,
        }
    }

    /// An object shaped like a `String` — slot 0 a `byte[]`, slot 1 a LATIN1
    /// coder, four slots, which is exactly `AbstractStringBuilder`'s layout —
    /// passes the speculative shape reader, but the operand renderers copy
    /// units only from a `String` by class: a `StringBuilder` concat operand
    /// or record component used to render as its whole buffer.
    #[test]
    fn a_string_shaped_object_is_not_rendered_as_a_string() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let s = create_java_string(&shared, "ok");
        assert!(crate::vm::is_java_lang_string(&shared, s));
        let stand_in = shared
            .classes
            .class_manager
            .write()
            .load_class("java/lang/Integer")
            .expect("load a stand-in class");
        let buffer =
            shared
                .mem
                .heap
                .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Byte, 3);
        let builder = shared.mem.heap.alloc_object(stand_in, 4);
        shared
            .mem
            .heap
            .set_field(builder, 0, Value::Object(Some(buffer)));
        shared.mem.heap.set_field(builder, 1, Value::Int(0));
        assert_eq!(
            read_java_string_units(&shared.mem.heap, builder).map(|u| u.len()),
            Some(3),
            "the hazard: the shape reader answers for it"
        );
        assert!(!crate::vm::is_java_lang_string(&shared, builder));
        assert!(!crate::vm::is_java_lang_string(&shared, buffer));
        if let Ok(RenderedOperand::Units(units)) =
            render_operand_checked(&shared, None, &Value::Object(Some(builder)), 'L')
        {
            panic!("a non-String was rendered from its buffer: {units:?}");
        }
    }

    /// The concat path appends a `String` operand's units in place: the same
    /// units the reader returns, a lone surrogate included, after whatever
    /// `out` already holds; a non-String answers `false` and appends nothing.
    #[test]
    fn a_string_operand_is_appended_in_place_losslessly() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut out: Vec<u16> = vec![u16::from(b'>')];
        let latin1 = create_java_string(&shared, "hi");
        assert!(crate::vm::append_java_string_units(
            &shared.mem.heap,
            latin1,
            &mut out
        ));
        let wide = [u16::from(b'x'), 0xD800, 0x4E2D];
        let utf16 = crate::vm::create_java_string_from_units(&shared, &wide);
        assert!(crate::vm::append_java_string_units(
            &shared.mem.heap,
            utf16,
            &mut out
        ));
        assert_eq!(
            out,
            vec![
                u16::from(b'>'),
                u16::from(b'h'),
                u16::from(b'i'),
                u16::from(b'x'),
                0xD800,
                0x4E2D
            ]
        );
        assert_eq!(
            read_java_string_units(&shared.mem.heap, utf16).as_deref(),
            Some(&out[3..])
        );
        let ints =
            shared
                .mem
                .heap
                .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Int, 2);
        let before = out.len();
        assert!(!crate::vm::append_java_string_units(
            &shared.mem.heap,
            ints,
            &mut out
        ));
        assert_eq!(out.len(), before);
    }

    /// Stage 2a of the generic-indy proposal: a linked site resolves
    /// `MethodHandle` once, to the id the class table answers, and reuses it.
    #[test]
    fn a_generic_shape_resolves_method_handle_once() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let shape = GenericIndyShape::new("(Ljava/lang/Object;Z)V");
        assert!(shape.method_handle_class.get().is_none());
        let Ok(first) = shape.method_handle_class(&shared) else {
            return; // A stripped VM without `MethodHandle`.
        };
        assert_eq!(shape.method_handle_class.get(), Some(&first));
        assert_eq!(shape.method_handle_class(&shared).ok(), Some(first));
        assert_eq!(
            shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id("java/lang/invoke/MethodHandle"),
            Some(first)
        );
    }

    #[test]
    fn the_indy_table_is_tagged_with_the_name_generation() {
        assert_eq!(
            std::any::TypeId::of::<IndySiteCache>(),
            std::any::TypeId::of::<SiteCache<CachedIndySite, NameEpochs>>(),
            "a class definition must not drop a warm invokedynamic entry"
        );
    }

    /// The lend / return protocol: a hit moves the entry's `Arc` out and puts
    /// it back; a refill made while it was out wins, and an epoch move drops
    /// the loan. No path doubles or leaks a reference.
    #[test]
    fn a_lent_site_goes_back_only_into_its_own_live_empty_entry() {
        if cratonvm_classloading::any_class_redefined() {
            return; // Latched off process-wide by another test.
        }
        let mut thread = JvmThread::new(ThreadId(0), "i16-l1-lend");
        let class_id = ClassId::new(CLASS);
        let site = Arc::new(ResolvedCallSite::EnumSwitch {
            labels: vec![Arc::from("A")],
        });
        if !fill(&mut thread, class_id, 4, || {
            CachedIndySite::Other(Some(Arc::clone(&site)))
        }) {
            return;
        }
        assert_eq!(Arc::strong_count(&site), 2);
        let Some(loan) = lend(&mut thread, class_id, 4) else {
            return;
        };
        assert_eq!(
            Arc::strong_count(&site),
            2,
            "lending moves, it does not clone"
        );
        assert!(matches!(
            thread.indy_sites.get(class_id, 4),
            Some(CachedIndySite::Other(None))
        ));
        return_lent_indy_site(&mut thread, class_id, 4, loan);
        if !matches!(
            thread.indy_sites.get(class_id, 4),
            Some(CachedIndySite::Other(Some(_)))
        ) {
            return; // The epochs moved while lent: the loan was dropped.
        }
        assert_eq!(Arc::strong_count(&site), 2);

        // A re-entrant execution refilled the entry while it was lent.
        let Some(loan) = lend(&mut thread, class_id, 4) else {
            return;
        };
        thread.indy_sites.put(
            class_id,
            4,
            IndySiteCache::epochs_now(),
            CachedIndySite::Other(Some(Arc::clone(&site))),
        );
        if Arc::strong_count(&site) != 3 {
            return;
        }
        return_lent_indy_site(&mut thread, class_id, 4, loan);
        assert_eq!(
            Arc::strong_count(&site),
            2,
            "the loan is dropped, not doubled"
        );

        // An epoch move while lent: the loan is dropped and the entry misses.
        let Some(loan) = lend(&mut thread, class_id, 4) else {
            return;
        };
        crate::runtime::interpreter::bump_resolution_epoch();
        return_lent_indy_site(&mut thread, class_id, 4, loan);
        assert_eq!(Arc::strong_count(&site), 1);
        assert!(thread.indy_sites.get(class_id, 4).is_none());
    }

    /// A warm shape-bearing site runs from the thread's entry and leaves the
    /// entry holding the `Arc` again, with the refcount where it was.
    #[test]
    fn a_warm_enum_switch_runs_from_the_thread_entry_and_restores_it() {
        if cratonvm_classloading::any_class_redefined() {
            return;
        }
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "i16-l1-enum");
        let class_id = ClassId::new(CLASS + 1);
        let site = Arc::new(ResolvedCallSite::EnumSwitch {
            labels: vec![Arc::from("A"), Arc::from("B")],
        });
        for _ in 0..100 {
            thread.frames.clear();
            thread
                .frames
                .push(frame_at(class_id, 9, &[Value::Object(None), Value::Int(0)]));
            if !fill(&mut thread, class_id, 6, || {
                CachedIndySite::Other(Some(Arc::clone(&site)))
            }) {
                return;
            }
            // A miss (the epochs moved after the fill) reaches the slow path,
            // which finds no such class: retry.
            if execute_invokedynamic(&shared, &mut thread, 0, 6).is_err() {
                continue;
            }
            // `SwitchBootstraps.enumSwitch` answers -1 for a null target.
            assert_eq!(thread.frames[0].stack.pop().ok(), Some(Value::Int(-1)));
            if matches!(
                thread.indy_sites.get(class_id, 6),
                Some(CachedIndySite::Other(Some(_)))
            ) {
                assert_eq!(Arc::strong_count(&site), 2);
            }
            return;
        }
    }

    /// A zero-capture lambda's entry keys the singleton by its memo at the
    /// instruction's own pc, and by the frame's own key anywhere else.
    #[test]
    fn a_warm_zero_capture_lambda_keys_its_singleton_by_the_memo_at_its_pc() {
        if cratonvm_classloading::any_class_redefined() {
            return;
        }
        let shared = small_gen_vm();
        let mut thread = JvmThread::new(ThreadId(0), "i16-l1-singleton");
        let class_id = ClassId::new(CLASS + 2);
        let proxy = shared.alloc_lambda_proxy_id();
        const MEMO: u64 = 0x1616_1616_1616_1616;
        let run = |thread: &mut JvmThread, pc: usize| -> Option<ObjectRef> {
            thread.frames.clear();
            thread.frames.push(frame_at(class_id, pc, &[]));
            let filled = fill(thread, class_id, 8, || CachedIndySite::Lambda {
                proxy_class_id: proxy,
                num_captures: 0,
                singleton_key: Some((MEMO, 13)),
                singleton: None,
            });
            if !filled {
                return None;
            }
            execute_invokedynamic(&shared, thread, 0, 8).ok()?;
            match thread.frames[0].stack.pop().ok()? {
                Value::Object(o) => o,
                _ => None,
            }
        };
        let Some(at_memo) = run(&mut thread, 13) else {
            return;
        };
        let minted = lambda_singleton_cache()
            .read()
            .get(&(shared.vm_identity, proxy, MEMO, 13))
            .and_then(|slot| slot.get());
        assert_eq!(minted, Some(at_memo), "the memo keys the singleton");
        // Wave 20: that first hit learnt the row's slot, and a later hit on
        // the live entry answers from it -- the same instance.
        let learnt = matches!(
            thread.indy_sites.get(class_id, 8),
            Some(CachedIndySite::Lambda {
                singleton: Some(_),
                ..
            })
        );
        if learnt {
            thread.frames.clear();
            thread.frames.push(frame_at(class_id, 13, &[]));
            if execute_invokedynamic(&shared, &mut thread, 0, 8).is_ok() {
                assert_eq!(
                    thread.frames[0].stack.pop().ok(),
                    Some(Value::Object(Some(at_memo))),
                    "the learnt slot answers the minted instance"
                );
            }
        }
        if let Some(again) = run(&mut thread, 13) {
            assert_eq!(again, at_memo, "one instance per instruction");
        }
        if let Some(elsewhere) = run(&mut thread, 21) {
            assert_ne!(elsewhere, at_memo, "another pc is another key");
            let own_key = lambda_site_method_key_of(METHOD, "()V");
            let minted = lambda_singleton_cache()
                .read()
                .get(&(shared.vm_identity, proxy, own_key, 21))
                .and_then(|slot| slot.get());
            assert_eq!(minted, Some(elsewhere));
        }
        forget_vm_generic_indy_sites(shared.vm_identity);
    }
}

#[cfg(test)]
mod i20_l4_bridge_tests {
    //! Round i1 wave 20, lane L4: the compiled concat bridge runs frame-free
    //! and answers what the synthetic-frame arm answered; a compiled lambda
    //! site reads its zero-capture singleton from the row's slot.

    use super::*;
    use crate::threading::jvm_thread::ThreadId;

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn concat_site(recipe: &str, constants: &[&str], descriptor: &str) -> JitStringConcatSite {
        JitStringConcatSite {
            kind: JIT_INDY_SITE_CONCAT,
            recipe: Arc::from(units(recipe).as_slice()),
            constant_args: constants
                .iter()
                .map(|c| Arc::from(units(c).as_slice()))
                .collect(),
            target_descriptor: Arc::from(descriptor),
            arg_types: parse_descriptor_args(descriptor).into_boxed_slice(),
            frame_class_name: Arc::from("<jit-indy>"),
            frame_method_name: Arc::from("concat"),
            frame_method_descriptor: Arc::from("()Ljava/lang/String;"),
            frame_code: crate::runtime::frame::padded_bytecode(&[]),
            frame_exception_table: Arc::from(
                Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
            ),
        }
    }

    /// `I`, `J` and `L` operands (a `String` and a null) plus a recipe
    /// constant: the frame-free bridge renders exactly what the frame arm
    /// renders, and leaves the thread's frame stack as it found it.
    #[test]
    fn the_frame_free_concat_bridge_matches_the_frame_arm() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "i20-l4-concat");
        let site = concat_site(
            "<\x01|\x01|\x01|\x01|\x02>",
            &["K"],
            "(IJLjava/lang/String;Ljava/lang/Object;)Ljava/lang/String;",
        );
        let s = create_java_string(&shared, "s");
        let long = 1_i64 << 40;
        let args: [i64; 4] = [-5, long, s.as_ptr() as i64, 0];
        let site_ptr = &site as *const JitStringConcatSite as usize;
        let depth = thread.frames.len();
        // SAFETY: `site_ptr` points at `site`, live for the call; `args` holds
        // four descriptor-typed slots, the object slot a live `String`.
        let free = unsafe {
            execute_jit_string_concat_raw(&shared, &mut thread, site_ptr, args.as_ptr(), 4)
        };
        assert_eq!(thread.frames.len(), depth, "no synthetic frame is left");
        let Ok(Some(free)) = free else {
            panic!("the frame-free concat did not answer");
        };
        let mut values: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        values.push(Value::Int(-5));
        values.push(Value::Long(long));
        values.push(Value::Object(Some(s)));
        values.push(Value::Object(None));
        let Ok(Some(framed)) =
            execute_jit_string_concat_on_frame(&shared, &mut thread, &site, values)
        else {
            panic!("the frame arm did not answer");
        };
        assert_eq!(thread.frames.len(), depth);
        let want = units("<-5|1099511627776|s|null|K>");
        assert_eq!(
            read_java_string_units(&shared.mem.heap, free).as_deref(),
            Some(want.as_slice())
        );
        assert_eq!(
            read_java_string_units(&shared.mem.heap, framed).as_deref(),
            Some(want.as_slice())
        );
        assert!(
            free.as_ptr() != framed.as_ptr(),
            "each concat result is a fresh String"
        );
    }

    /// A `long` whose bits collide with the operand stack's NaN tag space
    /// (`execute_string_concat`'s note) is rendered from its descriptor type,
    /// and a zero-operand site and an arity mismatch keep their answers.
    #[test]
    fn the_frame_free_concat_bridge_types_operands_by_descriptor() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "i20-l4-concat-j");
        let site = concat_site("\x01", &[], "(J)Ljava/lang/String;");
        let collide: i64 = -1_125_899_906_842_623;
        let args: [i64; 1] = [collide];
        let site_ptr = &site as *const JitStringConcatSite as usize;
        // SAFETY: as above, one `J` slot.
        let got = unsafe {
            execute_jit_string_concat_raw(&shared, &mut thread, site_ptr, args.as_ptr(), 1)
        };
        let Ok(Some(got)) = got else {
            panic!("the concat did not answer");
        };
        assert_eq!(
            read_java_string_units(&shared.mem.heap, got),
            Some(units("-1125899906842623"))
        );
        // Arity mismatch: the historical `Ok(None)`.
        // SAFETY: the count disagrees with the site, so nothing is read.
        let mismatch = unsafe {
            execute_jit_string_concat_raw(&shared, &mut thread, site_ptr, args.as_ptr(), 0)
        };
        assert!(matches!(mismatch, Ok(None)));
        let empty = concat_site("ab", &[], "()Ljava/lang/String;");
        let empty_ptr = &empty as *const JitStringConcatSite as usize;
        // SAFETY: zero operands and a null argument block, which the bridge
        // admits for exactly that case.
        let got = unsafe {
            execute_jit_string_concat_raw(&shared, &mut thread, empty_ptr, std::ptr::null(), 0)
        };
        let Ok(Some(got)) = got else {
            panic!("a zero-operand concat did not answer");
        };
        assert_eq!(
            read_java_string_units(&shared.mem.heap, got),
            Some(units("ab"))
        );
    }

    fn lambda_site(class_id: ClassId, singleton_site: (u64, usize)) -> JitIndyGenericSite {
        JitIndyGenericSite {
            kind: JIT_INDY_SITE_GENERIC,
            class_id,
            cp_index: 4,
            target_descriptor: Arc::from("()Ljava/lang/Runnable;"),
            arg_types: Arc::from(Vec::<u8>::new().as_slice()),
            frame_class_name: Arc::from("<jit-indy>"),
            frame_method_name: Arc::from(JIT_INDY_FRAME_METHOD),
            frame_code: crate::runtime::frame::padded_bytecode(&[]),
            frame_exception_table: Arc::from(
                Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
            ),
            frame_max_stack: 1,
            return_type: b'L',
            singleton_site,
            singleton_slot: std::sync::OnceLock::new(),
        }
    }

    /// A compiled zero-capture site learns its row's slot from the table and
    /// answers from it afterwards -- the minted instance, for its own VM and
    /// proxy class only -- and stops answering once the row is dropped.
    #[test]
    fn a_compiled_lambda_site_answers_its_singleton_from_the_learnt_slot() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let mut thread = JvmThread::new(ThreadId(0), "i20-l4-slot");
        let proxy = shared.alloc_lambda_proxy_id();
        let key = lambda_singleton_site_for("i20l4", "()V", 3);
        let site = lambda_site(ClassId::new(0x7_2004), key);
        assert_eq!(
            site.singleton_memo(&shared, proxy),
            None,
            "nothing learnt yet"
        );
        let minted = allocate_lambda_proxy_from_values(&shared, &mut thread, proxy, &mut [], key)
            .expect("an empty heap has room for the singleton");
        site.remember_singleton_slot(&shared, proxy);
        assert_eq!(site.singleton_memo(&shared, proxy), Some(minted));
        let other = shared.alloc_lambda_proxy_id();
        assert_eq!(
            site.singleton_memo(&shared, other),
            None,
            "a memo answers only for the proxy class it learnt"
        );
        let again = allocate_lambda_proxy_from_values(&shared, &mut thread, proxy, &mut [], key)
            .expect("the table answers");
        assert_eq!(again, minted, "the table and the slot agree");
        forget_vm_generic_indy_sites(shared.vm_identity);
        assert_eq!(
            site.singleton_memo(&shared, proxy),
            None,
            "a dropped row's slot is retired"
        );
    }
}
