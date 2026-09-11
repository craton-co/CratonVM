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

// `NativeContext` trait brought into scope for `ctx.get_class_mirror(...)` on
// `NativeContextImpl` in the generic-invokedynamic bootstrap path.
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeInvokeAccess};

use crate::classloading::resolution::{
    LambdaCallSite, MethodHandle, MethodHandleKind, RecordMethodKind, ResolvedCallSite, SwitchLabel,
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
/// The block that follows the markers is `FLAG_BRIDGES` (0x4): a `bridgeCount`
/// then that many `MethodType`s. Markers come FIRST, so the marker block's
/// extent is computable without looking at the bridge bit — and CratonVM's SAM
/// dispatch is descriptor-driven, so it needs no spun bridge methods.
const FLAG_MARKERS: i32 = 2;

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

/// Groovy call-site name for a coercion (`cast:(Object)Z`, `cast:(Object)I`, …).
const GROOVY_CAST: &str = "cast";

/// Groovy's runtime type-coercion helper (implements "Groovy truth").
const GROOVY_DTT: &str = "org/codehaus/groovy/runtime/typehandling/DefaultTypeTransformation";

/// Data extracted from the constant pool under a read lock, owned so we can
/// drop the lock before proceeding with string creation (which needs a write lock).
/// `StringConcatFactory` recipe tags, as UTF-16 code units.
///
/// The recipe is walked unit-by-unit rather than `char`-by-`char` so a lone
/// surrogate in a folded literal survives; these are the two markers it looks
/// for. Values per `java.lang.invoke.StringConcatFactory`.
const TAG_ARG_UNIT: u16 = 0x0001;
const TAG_CONST_UNIT: u16 = 0x0002;

struct IndyInfo {
    bsm_class: String,
    bsm_method: String,
    target_name: String,
    target_descriptor: String,
    /// The recipe (first bootstrap argument, if StringConcatFactory) as UTF-16
    /// code units. Units rather than `String` because a recipe may embed a lone
    /// surrogate from a folded string literal, which a Rust `str` cannot hold --
    /// see `resolve_concat_constant_units`.
    recipe: Vec<u16>,
    /// Additional constants from bootstrap arguments (TAG_CONST placeholders),
    /// as UTF-16 code units, for the same reason as `recipe`.
    constant_args: Vec<Vec<u16>>,
    /// Raw CP indices for bootstrap arguments (needed for LambdaMetafactory).
    bootstrap_arg_indices: Vec<u16>,
}

/// A compiled `invokedynamic` bridge site is a BRIDGE of one of two kinds, and
/// this is the tag that says which.
///
/// The codegen carries exactly one `usize` per indy site (the fifth element of
/// `indy_info`) and calls exactly one entry (`cratonvm_jit::INDY_BRIDGE_FN`),
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
    })) as usize)
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
pub fn make_jit_indy_bridge_site_from_parts(
    pool: &ConstantPool,
    bootstraps: &[BootstrapMethod],
    cp_index: u16,
    class_id: ClassId,
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
        OBJECT_METHODS => handle.member_name.as_ref() == BOOTSTRAP,
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
    Some(Box::into_raw(Box::new(JitIndyGenericSite {
        kind: JIT_INDY_SITE_GENERIC,
        class_id,
        cp_index,
        target_descriptor: Arc::from(descriptor),
        arg_types: Arc::from(arg_types.as_slice()),
        frame_class_name: Arc::from("<jit-indy>"),
        frame_method_name: Arc::from("bridge"),
        frame_code: crate::runtime::frame::padded_bytecode(&[]),
        frame_exception_table: Arc::from(
            Vec::<cratonvm_reader::attribute::ExceptionTableEntry>::new().into_boxed_slice(),
        ),
        frame_max_stack,
        return_type: ret,
    })) as usize)
}

/// Execute the narrow compiled-code concat bridge. Raw slots are descriptor
/// typed before they enter the normal interpreter concat implementation, so a
/// category-2 value never gets reclassified from its bits alone.
pub unsafe fn execute_jit_string_concat_raw(
    shared: &SharedVm,
    thread: &mut JvmThread,
    site_ptr: usize,
    args_ptr: *const i64,
    arg_count: usize,
) -> Option<ObjectRef> {
    let site = (site_ptr as *const JitStringConcatSite).as_ref()?;
    let arg_types = parse_descriptor_args(&site.target_descriptor);
    if arg_types.len() != arg_count || (arg_count != 0 && args_ptr.is_null()) {
        return None;
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
    let mut values = Vec::with_capacity(arg_count);
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
    let frame_idx = thread.frames.len();
    thread.frames.push(Frame::new(
        crate::classloading::ClassId::new(0),
        "<jit-indy>".to_owned(),
        "concat".to_owned(),
        "()Ljava/lang/String;".to_owned(),
        None,
        vec![],
        vec![],
        (arg_count as u16).saturating_add(1),
        0,
        &[],
    ));
    for value in values {
        if thread.frames[frame_idx].stack.push(value).is_err() {
            thread.frames.pop();
            return None;
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
    let result = executed
        .ok()
        .and_then(|_| thread.frames[frame_idx].stack.pop().ok())
        .and_then(|v| match v {
            Value::Object(Some(obj)) => Some(obj),
            _ => None,
        });
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
    let mut values = Vec::with_capacity(arg_count);
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
    // `site_pc = 0` matches what the synthetic frame's `pc` already was, so the
    // zero-capture singleton key is byte-identical to today's.
    // `CRATONVM_JIT_INDY_LAMBDA_FAST=0` restores the frame path so the two arms
    // can be priced in one binary.
    if jit_indy_lambda_fast_enabled() && site.return_type == b'L' {
        let cached = {
            let cache = shared.classes.resolution_cache.read();
            match cache.get_call_site(site.class_id, site.cp_index) {
                Some(ResolvedCallSite::Lambda(lcs)) => Some(lcs.clone()),
                _ => None,
            }
        };
        if let Some(lcs) = cached {
            if lcs.capture_types.len() == arg_count {
                let proxy = allocate_lambda_proxy_from_values(
                    shared,
                    thread,
                    lcs.proxy_class_id,
                    values,
                    0,
                )?;
                return Ok((proxy.as_ptr() as i64, Some(proxy)));
            }
            // Capture-count disagreement: fall through to the frame path,
            // which raises the internal error rather than guessing. `values`
            // was moved above only inside the taken branch.
            return Err(VmError::Internal {
                message: format!(
                    "jit indy bridge: lambda site expects {} captures, call sequence pushed {}",
                    lcs.capture_types.len(),
                    arg_count
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
    let executed = execute_invokedynamic(shared, thread, frame_idx, site.cp_index);
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
    // with every resolver queued behind the writers. The clone is cheap:
    // `ResolvedCallSite`'s strings are `Arc<str>` (refcount bumps).
    let cached_site = {
        let cache = shared.classes.resolution_cache.read();
        cache.get_call_site(current_class_id, cp_index).cloned()
    };
    if let Some(site) = cached_site {
        return execute_cached_call_site(shared, thread, frame_idx, &site);
    }

    // --- Slow path: bootstrap the call site ---
    // Extract all needed data under the class_manager read lock, then drop it.
    // This avoids deadlocking when create_java_string needs a write lock.
    let info = {
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

        // 5. Extract recipe and constant args (if StringConcatFactory)
        let recipe: Vec<u16> = if let Some(&arg_index) = bsm.bootstrap_arguments.first() {
            resolve_concat_constant_units(&class.constant_pool, arg_index).unwrap_or_default()
        } else {
            Vec::new()
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
        let constant_args: Vec<Vec<u16>> = bsm
            .bootstrap_arguments
            .iter()
            .skip(1) // skip recipe
            .map(|&idx| {
                resolve_concat_constant_units(&class.constant_pool, idx).unwrap_or_default()
            })
            .collect();

        let bootstrap_arg_indices = bsm.bootstrap_arguments.clone();

        IndyInfo {
            bsm_class,
            bsm_method,
            target_name: target_name.to_string(),
            target_descriptor: target_descriptor.to_string(),
            recipe,
            constant_args,
            bootstrap_arg_indices,
        }
    }; // cm read lock dropped here

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
            .put_call_site(current_class_id, cp_index, site);

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
            .put_call_site(current_class_id, cp_index, site);

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
        // identical, and the bridge block genuinely is advisory here (SAM dispatch is
        // descriptor-driven) — but the FLAG_SERIALIZABLE bit and the MARKER INTERFACE
        // list are NOT: they change the spun proxy's interface list, so
        // `instanceof`/`checkcast`/`Class.isInstance` against a marker must succeed.
        // `bootstrap_lambda` reads both.
        bootstrap_lambda(shared, thread, frame_idx, cp_index, &info)
    } else if info.bsm_class == SWITCH_BOOTSTRAPS && info.bsm_method == TYPE_SWITCH {
        bootstrap_type_switch(shared, thread, frame_idx, cp_index, &info)
    } else if info.bsm_class == SWITCH_BOOTSTRAPS && info.bsm_method == ENUM_SWITCH {
        bootstrap_enum_switch(shared, thread, frame_idx, cp_index, &info)
    } else if info.bsm_class == OBJECT_METHODS && info.bsm_method == BOOTSTRAP {
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
        groovy_cast_to_boolean(shared, thread, frame_idx, &info)
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
    MType(String),
}

/// Resolve a single bootstrap static-argument CP entry to a [`StaticArg`].
fn resolve_static_arg_kind(cp: &ConstantPool, index: u16) -> Result<StaticArg, MethodCallFailed> {
    let entry = cp.get(index).ok_or_else(|| VmError::Internal {
        message: format!("invokedynamic generic: bootstrap static arg cp#{index} missing"),
    })?;
    Ok(match entry {
        ConstantPoolEntry::Integer(i) => StaticArg::Int(*i),
        ConstantPoolEntry::Long(l) => StaticArg::Long(*l),
        ConstantPoolEntry::Float(f) => StaticArg::Float(*f),
        ConstantPoolEntry::Double(d) => StaticArg::Double(*d),
        ConstantPoolEntry::StringReference { .. } => {
            StaticArg::Str(resolve_string_constant(cp, index).unwrap_or_default())
        }
        ConstantPoolEntry::ClassReference { .. } => {
            let name = cp.get_class_name(index).ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic generic: bad Class static arg cp#{index}"),
            })?;
            StaticArg::Class(name.to_string())
        }
        ConstantPoolEntry::MethodType { .. } => {
            let desc = resolve_method_type(cp, index).ok_or_else(|| VmError::Internal {
                message: format!("invokedynamic generic: bad MethodType static arg cp#{index}"),
            })?;
            StaticArg::MType(desc)
        }
        other => {
            return Err(VmError::Internal {
                message: format!(
                    "invokedynamic generic: unsupported bootstrap static arg kind at cp#{index} ({other:?})"
                ),
            }
            .into());
        }
    })
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
    info: &IndyInfo,
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

/// Generic invokedynamic linkage: execute an arbitrary bootstrap method to
/// obtain a `CallSite`, then invoke its target `MethodHandle`.
///
/// Correctness-first (no call-site caching yet): the bootstrap runs on every
/// execution of the instruction. That is slower than a real JVM (which links
/// once and caches the `CallSite`) but is semantically correct — each call goes
/// through the freshly-produced target. The hardcoded JDK factories above keep
/// their cached fast paths; only previously-unsupported bootstraps reach here.
fn bootstrap_generic(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
    current_class_id: ClassId,
) -> Result<(), MethodCallFailed> {
    // --- Re-resolve the BSM (with descriptor) + static args under the lock. ---
    let (bsm_class, bsm_method, bsm_desc, static_args) = {
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
        let mut sargs = Vec::with_capacity(bsm.bootstrap_arguments.len());
        for &idx in &bsm.bootstrap_arguments {
            sargs.push(resolve_static_arg_kind(&class.constant_pool, idx)?);
        }
        (
            h.class_name.to_string(),
            h.member_name.to_string(),
            h.descriptor.to_string(),
            sargs,
        )
    };

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

    // 2. The call-site name.
    let name_ref = create_java_string(shared, &info.target_name);
    bsm_pins.push((1, thread.native_pin_roots.len()));
    thread.native_pin_roots.push(name_ref);
    bsm_args.push(Value::Object(Some(name_ref)));

    // 3. The call-site MethodType.
    let mt = {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        cratonvm_native_builtins::lang_invoke::build_method_type_from_descriptor(
            &mut ctx,
            &info.target_descriptor,
        )
    }
    .map_err(|err| VmError::Internal {
        message: format!(
            "invokedynamic generic: MethodType refused for {}: {err:?}",
            info.target_descriptor
        ),
    })?
    .ok_or_else(|| VmError::Internal {
        message: format!(
            "invokedynamic generic: cannot build MethodType from {}",
            info.target_descriptor
        ),
    })?;
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
            StaticArg::Str(s) => {
                let r = create_java_string(shared, s);
                bsm_pins.push((pos, thread.native_pin_roots.len()));
                thread.native_pin_roots.push(r);
                Value::Object(Some(r))
            }
            StaticArg::Class(name) => {
                let cid = shared.load_class_concurrent(name)?;
                let m = {
                    let mut ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    ctx.get_class_mirror(cid)
                };
                bsm_pins.push((pos, thread.native_pin_roots.len()));
                thread.native_pin_roots.push(m);
                Value::Object(Some(m))
            }
            StaticArg::MType(desc) => {
                let m = {
                    let mut ctx = NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_invoke::build_method_type_from_descriptor(
                        &mut ctx, desc,
                    )
                }
                .map_err(|err| VmError::Internal {
                    message: format!(
                        "invokedynamic generic: MethodType refused for {desc}: {err:?}"
                    ),
                })?
                .ok_or_else(|| VmError::Internal {
                    message: format!("invokedynamic generic: bad MethodType static arg {desc}"),
                })?;
                bsm_pins.push((pos, thread.native_pin_roots.len()));
                thread.native_pin_roots.push(m);
                Value::Object(Some(m))
            }
        };
        bsm_args.push(v);
    }

    // Re-read object args from their (possibly forwarded) pin slots.
    for &(pos, slot) in &bsm_pins {
        if let Some(o) = thread.native_pin_roots.get(slot).copied() {
            bsm_args[pos] = Value::Object(Some(o));
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
    // match the declared descriptor before invoking.
    if let Some((decl_param_count, is_last_object_array)) =
        descriptor_param_count_and_last_is_object_array(&bsm_desc)
    {
        if is_last_object_array && decl_param_count > 0 && bsm_args.len() > decl_param_count {
            let leading = decl_param_count - 1;
            let tail: Vec<Value> = bsm_args[leading..].to_vec();
            // GC-safety: the `valueOf` boxing calls below run arbitrary Java
            // (class-load + <clinit>) and can trigger a moving young GC;
            // pin the array AND every still-unprocessed object-typed tail
            // value, re-reading each from its pin slot right before use
            // (mirrors `bsm_pins` above, same function).
            let arr = {
                let mut ctx = NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                ctx.new_array(cratonvm_types::ArrayElementType::Reference, tail.len())
            };
            let base_pin = thread.native_pin_roots.len();
            thread.native_pin_roots.push(arr); // base_pin -> the array itself
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
                let boxed: Value = match current_v {
                    Value::Int(x) => crate::vm::invoke_shared(
                        shared,
                        thread,
                        "java/lang/Integer",
                        "valueOf",
                        "(I)Ljava/lang/Integer;",
                        &[Value::Int(x)],
                    )?
                    .unwrap_or(Value::Object(None)),
                    Value::Long(x) => crate::vm::invoke_shared(
                        shared,
                        thread,
                        "java/lang/Long",
                        "valueOf",
                        "(J)Ljava/lang/Long;",
                        &[Value::Long(x)],
                    )?
                    .unwrap_or(Value::Object(None)),
                    Value::Float(x) => crate::vm::invoke_shared(
                        shared,
                        thread,
                        "java/lang/Float",
                        "valueOf",
                        "(F)Ljava/lang/Float;",
                        &[Value::Float(x)],
                    )?
                    .unwrap_or(Value::Object(None)),
                    Value::Double(x) => crate::vm::invoke_shared(
                        shared,
                        thread,
                        "java/lang/Double",
                        "valueOf",
                        "(D)Ljava/lang/Double;",
                        &[Value::Double(x)],
                    )?
                    .unwrap_or(Value::Object(None)),
                    // Already a reference (String/Class/MethodType/null static
                    // arg, or a value with no tail_pins slot) -- passes through.
                    other => other,
                };
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
    let callsite_val = crate::vm::invoke_shared(
        shared,
        thread,
        &bsm_class,
        &bsm_method,
        &bsm_desc,
        &bsm_args,
    )?;
    thread.native_pin_roots.truncate(pin_base); // bootstrap args no longer needed
    if dbg {
        eprintln!("[indy-generic] bootstrap result = {callsite_val:?}");
    }

    let callsite = match callsite_val {
        Some(Value::Object(Some(cs))) => cs,
        _ => {
            return Err(VmError::Internal {
                message: format!(
                "invokedynamic generic: bootstrap {bsm_class}.{bsm_method} returned a non-CallSite"
            ),
            }
            .into())
        }
    };
    // Pin the CallSite across getTarget().
    thread.native_pin_roots.push(callsite);
    let callsite = thread.native_pin_roots[pin_base];

    // --- callSite.getTarget() → target MethodHandle. ---
    let target_val = crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/invoke/CallSite",
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        &[Value::Object(Some(callsite))],
    )?;
    thread.native_pin_roots.truncate(pin_base); // callsite no longer needed
    let target_mh = match target_val {
        Some(Value::Object(Some(mh))) => mh,
        _ => {
            return Err(VmError::Internal {
                message: format!(
                    "invokedynamic generic: {bsm_class}.{bsm_method} CallSite has a null target"
                ),
            }
            .into())
        }
    };
    // Pin the target across the dynamic-arg pop + invocation.
    let mh_slot = thread.native_pin_roots.len();
    thread.native_pin_roots.push(target_mh);

    // --- Pop the dynamic call arguments (descriptor-typed) and pin objects. ---
    let arg_types = parse_descriptor_args(&info.target_descriptor);
    if dbg {
        eprintln!(
            "[indy-generic] about to pop {} dyn args; stack depth before pop = {}",
            arg_types.len(),
            thread.frames[frame_idx].stack.len(),
        );
    }
    let mut dyn_args: Vec<Value> = Vec::with_capacity(arg_types.len());
    for i in 0..arg_types.len() {
        let cv = thread.frames[frame_idx].stack.pop_compact();
        let desc_byte = arg_types
            .get(arg_types.len() - 1 - i)
            .copied()
            .unwrap_or('L') as u8;
        dyn_args.push(cv.decode_by_descriptor(desc_byte));
    }
    dyn_args.reverse();
    let mut dyn_pins: Vec<(usize, usize)> = Vec::new();
    for (i, v) in dyn_args.iter().enumerate() {
        if let Value::Object(Some(o)) = v {
            dyn_pins.push((i, thread.native_pin_roots.len()));
            thread.native_pin_roots.push(*o);
        }
    }

    // --- Invoke the target: MethodHandle.invoke(args...). The signature-
    //     polymorphic native reads the real descriptor off the handle and
    //     adapts each spread argument, so we pass the dynamic args directly. ---
    for &(i, slot) in &dyn_pins {
        if let Some(o) = thread.native_pin_roots.get(slot).copied() {
            dyn_args[i] = Value::Object(Some(o));
        }
    }
    let mut invoke_args: Vec<Value> = Vec::with_capacity(dyn_args.len() + 1);
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
                let boxed = crate::vm::invoke_shared(
                    shared,
                    thread,
                    "java/lang/Boolean",
                    "valueOf",
                    "(Z)Ljava/lang/Boolean;",
                    &[Value::Int(v)],
                )?
                .unwrap_or(Value::Object(None));
                if let Value::Object(Some(o)) = boxed {
                    thread.native_pin_roots.push(o);
                }
                dyn_args[i] = boxed;
            }
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
    let result = crate::vm::invoke_shared(
        shared,
        thread,
        "java/lang/invoke/MethodHandle",
        "invoke",
        // MethodHandle.invoke is signature-polymorphic. Preserve the actual
        // invokedynamic descriptor so primitive call-site arguments (notably
        // Groovy's trailing `Z, Z` constructor flags) are adapted as their
        // declared primitive types instead of being erased and boxed as
        // Integer through an artificial Object[] signature.
        &info.target_descriptor,
        &invoke_args,
    )?;
    thread.native_pin_roots.truncate(pin_base);
    if dbg {
        eprintln!("[indy-generic] target MH invoke result = {result:?}");
    }

    // --- Push the result, coerced to the call-site return type. ---
    let ret_byte = info
        .target_descriptor
        .rfind(')')
        .and_then(|i| info.target_descriptor.as_bytes().get(i + 1).copied())
        .unwrap_or(b'V');
    if ret_byte != b'V' {
        let val = result.unwrap_or(Value::Object(None));
        let coerced = crate::vm::coerce_value_against_ret_char(val, ret_byte, shared);
        thread.frames[frame_idx].stack.push(coerced)?;
    }
    Ok(())
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
        } => execute_record_object_method(
            shared,
            thread,
            frame_idx,
            *method,
            component_names,
            field_indices,
            field_descriptors,
        ),
    }
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
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // Parse bootstrap arguments from constant pool
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

        let sam_erased = resolve_method_type(&class.constant_pool, info.bootstrap_arg_indices[0])
            .ok_or_else(|| VmError::Internal {
            message: "LambdaMetafactory: invalid SAM erased MethodType".to_string(),
        })?;

        let impl_mh =
            resolve_method_handle_full(&class.constant_pool, info.bootstrap_arg_indices[1])?;

        let instantiated = resolve_method_type(&class.constant_pool, info.bootstrap_arg_indices[2])
            .ok_or_else(|| VmError::Internal {
                message: "LambdaMetafactory: invalid instantiated MethodType".to_string(),
            })?;

        (sam_erased, impl_mh, instantiated, class.loader_id)
    };

    // Parse the factory descriptor to determine:
    //   - capture types (parameters of the invokedynamic)
    //   - functional interface (return type)
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
    let marker_interfaces: Vec<Arc<str>> = if (alt_flags & FLAG_MARKERS) != 0 {
        read_marker_interfaces(shared, current_class_id, &info.bootstrap_arg_indices)
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
        record_lambda_proxy_markers(shared.vm_identity, proxy_class_id, &marker_interfaces);
    }
    shared.classes.resolution_cache.write().put_call_site(
        current_class_id,
        cp_index,
        ResolvedCallSite::Lambda(call_site),
    );

    // Now execute: pop captured values, allocate proxy object, push it
    allocate_lambda_proxy(shared, thread, frame_idx, proxy_class_id, &capture_types)
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
    let count = match arg_indices.get(4).and_then(|i| class.constant_pool.get(*i)) {
        Some(ConstantPoolEntry::Integer(n)) if *n > 0 => *n as usize,
        _ => return Vec::new(),
    };
    let mut out: Vec<Arc<str>> = Vec::with_capacity(count);
    for k in 0..count {
        let Some(idx) = arg_indices.get(5 + k) else {
            break;
        };
        if let Some(name) = class.constant_pool.get_class_name_arc(*idx) {
            out.push(name);
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
// Entries hold plain interned names, never `ObjectRef`s, so unlike the
// singleton cache this table needs no GC root scan or post-compaction remap.
// It is populated only by intersection-cast lambdas, which are rare; the
// `ANY_LAMBDA_MARKERS` gate keeps the (hot) `instanceof`-on-a-lambda path from
// taking the lock at all in the overwhelmingly common empty case.
// ---------------------------------------------------------------------------

/// `true` once any proxy in this process has recorded a marker interface.
/// Read before the mutex on every marker query; see the module note above.
static ANY_LAMBDA_MARKERS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[allow(clippy::type_complexity)]
static LAMBDA_PROXY_MARKERS: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[Arc<str>]>>>,
> = std::sync::OnceLock::new();

#[allow(clippy::type_complexity)]
fn lambda_proxy_markers(
) -> &'static parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), Arc<[Arc<str>]>>> {
    LAMBDA_PROXY_MARKERS.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Record the `altMetafactory` marker interfaces of a freshly-registered lambda
/// proxy. A no-op for the (usual) empty list.
///
/// `pub` because the REFLECTIVE metafactory path reaches this from outside the
/// `vm` crate: `native-builtins`' `LambdaMetafactory.altMetafactory` shim reads
/// the packed `Object[]` and hands the names back through
/// `NativeContext::register_lambda_proxy_markers`, whose `vm` implementation
/// calls this.
pub fn record_lambda_proxy_markers(
    vm_identity: usize,
    proxy_class_id: ClassId,
    markers: &[Arc<str>],
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
) -> Option<Arc<[Arc<str>]>> {
    if !ANY_LAMBDA_MARKERS.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    lambda_proxy_markers()
        .lock()
        .get(&(vm_identity, proxy_class_id))
        .cloned()
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
    for marker in markers.iter() {
        if &**marker == target_name {
            return true;
        }
        if let Ok(marker_id) = shared.load_class_concurrent(marker) {
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
        &lcs.capture_types,
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
static LAMBDA_SINGLETON_CACHE: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<(usize, ClassId, usize), ObjectRef>>,
> = std::sync::OnceLock::new();

fn lambda_singleton_cache(
) -> &'static parking_lot::Mutex<std::collections::HashMap<(usize, ClassId, usize), ObjectRef>> {
    LAMBDA_SINGLETON_CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// GC root scan hook — called from `vm/src/memory/roots.rs`. Reports the
/// cached zero-capture lambda singletons for the active VM so the GC keeps
/// them live.
pub fn gc_scan_lambda_singleton_roots(vm_identity: usize, out: &mut Vec<ObjectRef>) {
    let cache = lambda_singleton_cache().lock();
    for (&(vid, _, _), obj_ref) in cache.iter() {
        if vid == vm_identity {
            out.push(*obj_ref);
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
    let mut cache = lambda_singleton_cache().lock();
    for (&(vid, _, _), obj_ref) in cache.iter_mut() {
        if vid != vm_identity {
            continue;
        }
        let old_addr = obj_ref.as_ptr() as usize;
        if let Some(&new_addr) = pointer_map.get(&old_addr) {
            debug_assert!(new_addr != 0, "GC pointer map contains null address");
            *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
}

/// Pop captured values from the stack, allocate a lambda proxy object, push it.
fn allocate_lambda_proxy(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    proxy_class_id: ClassId,
    capture_types: &[char],
) -> Result<(), MethodCallFailed> {
    let num_captures = capture_types.len();
    let site_pc = thread.frames[frame_idx].pc;

    // Pop captured values (pushed left-to-right, pop right-to-left)
    let mut captures: Vec<Value> = Vec::with_capacity(num_captures);
    for _ in 0..num_captures {
        captures.push(thread.frames[frame_idx].stack.pop()?);
    }
    captures.reverse();

    let proxy_ref =
        allocate_lambda_proxy_from_values(shared, thread, proxy_class_id, captures, site_pc)?;
    thread.frames[frame_idx]
        .stack
        .push(Value::Object(Some(proxy_ref)))?;
    Ok(())
}

/// The frame-free half of [`allocate_lambda_proxy`]: everything from the
/// zero-capture singleton probe to the field stores, given the captures as
/// values rather than as operand-stack entries.
///
/// Split out for the compiled `invokedynamic` bridge, which has the captures in
/// a spill buffer and had to build (and then discard) a whole interpreter frame
/// just to hand them to this code — see `execute_jit_indy_generic_raw`'s fast
/// path. `site_pc` is the singleton cache's per-INSTRUCTION key component; the
/// bridge passes `0`, which is exactly what its synthetic frame's `pc` was.
fn allocate_lambda_proxy_from_values(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy_class_id: ClassId,
    mut captures: Vec<Value>,
    site_pc: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let num_captures = captures.len();

    // Fast path: a zero-capture call site whose singleton was already
    // minted on a prior invocation just returns the cached instance —
    // matches real HotSpot's cached-INSTANCE-field optimization for
    // non-capturing lambdas (see LAMBDA_SINGLETON_CACHE above).
    // Per-INSTRUCTION identity: see LAMBDA_SINGLETON_CACHE's doc comment.
    if num_captures == 0 {
        if let Some(cached) = lambda_singleton_cache()
            .lock()
            .get(&(shared.vm_identity, proxy_class_id, site_pc))
            .copied()
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
                            "[deadref-singleton] #{n} {reason} the zero-capture lambda                              singleton cache returned 0x{:x} for cid={:#x} site_pc={site_pc} —                              the cached instance names no live object, so the cache was not                              remapped for some collection that moved it.",
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
            // Hoisted out of the `eprintln!` below: clippy's
            // `format_in_format_args` is right that a `format!` in another
            // format call allocates a String only to copy it, and this one
            // is on a diagnostic path that a `-D warnings` clippy step --
            // which runs BEFORE `cargo test --workspace` in ci.yml -- was
            // failing the whole job for.
            let detail = format!(
                "in_native_pins={} frames={}
{}",
                thread
                    .native_pin_roots
                    .iter()
                    .any(|r| r.as_ptr() as usize == dead),
                thread.frames.len(),
                stack.join(
                    "
"
                ),
            );
            eprintln!(
                "[deadref-capture] {reason} capture[{i}] = 0x{:x} was ALREADY dead when the                  lambda proxy popped it off the operand stack (cid={:#x}, {} captures) — the                  pin below cannot help, the value was wrong before this call.                  moved_away_to={:?} (needs CRATONVM_DBG_VACATED_FRAMES; Some means the                  referent was RELOCATED and a rewrite was missed, None means it was never                  relocated — reclaimed while referenced, or never a valid reference)
{}",
                o.as_ptr() as usize,
                proxy_class_id.as_u32(),
                num_captures,
                cratonvm_gc::gc_quiescence::moved_away_to(o.as_ptr() as usize),
                detail,
            );
        }
    }

    let pin_base = thread.native_pin_roots.len();
    let mut handles: Vec<Option<usize>> = Vec::with_capacity(captures.len());
    for c in captures.iter() {
        if let Value::Object(Some(o)) = c {
            handles.push(Some(thread.native_pin_roots.len()));
            thread.native_pin_roots.push(*o);
        } else {
            handles.push(None);
        }
    }

    // Allocate a proxy object on the heap with fields for captured values.
    // Use try_alloc + GC retry to avoid aborting on young-gen exhaustion.
    let proxy_ref = match shared
        .mem
        .heap
        .try_alloc_object(proxy_class_id, num_captures)
    {
        Some(obj) => obj,
        None => {
            thread.tlab.retire();
            super::interpreter::maybe_gc_forced_pub_at(shared, thread, "invokedynamic");
            match shared
                .mem
                .heap
                .try_alloc_object(proxy_class_id, num_captures)
                .ok_or_else(|| {
                    MethodCallFailed::InternalError(crate::error::VmError::Runtime(
                        crate::error::RuntimeError::OutOfMemoryError {
                            message: format!(
                                "Java heap space (lambda proxy with {} captures)",
                                num_captures,
                            ),
                        },
                    ))
                }) {
                Ok(obj) => obj,
                Err(e) => {
                    thread.native_pin_roots.truncate(pin_base);
                    return Err(e);
                }
            }
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
                        let _ = write!(stk, "
[WATCH-ALLOC]     at {}.{}", f.class_name(), f.method_name());
                    }
                }
                eprintln!(
                    "[WATCH-ALLOC] lambda proxy cid={:#x} captures={num_captures}                      site_pc={site_pc} -> {addr:#x} tid={} frames={}{}",
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
    if num_captures == 0 {
        lambda_singleton_cache()
            .lock()
            .insert((shared.vm_identity, proxy_class_id, site_pc), proxy_ref);
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
    let arg_types = parse_descriptor_args(target_descriptor);

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
    let mut arg_values: Vec<Value> = Vec::with_capacity(arg_types.len());
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
    let mut arg_pin: Vec<Option<usize>> = Vec::with_capacity(arg_values.len());
    for v in &arg_values {
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
    let mut result: Vec<u16> = Vec::new();
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
                // died. Every other shape (primitives, and objects reached via
                // `toString()`) is produced as Rust text and cannot carry an
                // unpaired surrogate in the first place, so encoding those to
                // units here loses nothing.
                let units = match arg_val {
                    Value::Object(Some(obj)) => read_java_string_units(&shared.mem.heap, obj),
                    _ => None,
                };
                match units {
                    Some(u) => result.extend_from_slice(&u),
                    // A boxed `Character` can itself BE an unpaired surrogate,
                    // and `value_to_string` renders one through a Rust `char`,
                    // which cannot hold it -- `"" + Character.valueOf('\u{d800}')`
                    // came out as `?`, not even U+FFFD. It is one unit by
                    // definition, so emit it directly.
                    None if boxed_char_unit(shared, &arg_val).is_some() => {
                        result.push(boxed_char_unit(shared, &arg_val).unwrap_or(0));
                    }
                    None => {
                        let s = value_to_string(shared, Some(thread), &arg_val, arg_type);
                        result.extend(s.encode_utf16());
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
    // plain Rust data. Release the temporary GC pins (no early `?` returns
    // occurred between `pin_base` and here, so a single truncate restores the
    // root set exactly).
    thread.native_pin_roots.truncate(pin_base);

    // `StringConcatFactory` (the `"a" + b` bytecode shape) produces a brand
    // new String per the JVM spec — it must NOT be interned, otherwise `==`
    // wrongly reports identity with an equal literal.
    // `"a" + b` on a full heap must raise a catchable OutOfMemoryError, not
    // abort the VM -- see `interpreter::create_string_or_oom`.
    let str_ref =
        crate::runtime::interpreter::create_string_from_units_or_oom(shared, thread, &result)?;
    thread.frames[frame_idx]
        .stack
        .push(Value::Object(Some(str_ref)))?;

    Ok(())
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

fn parse_descriptor_args(descriptor: &str) -> Vec<char> {
    let mut args = Vec::new();
    let bytes = descriptor.as_bytes();
    let mut i = 0;

    if i < bytes.len() && bytes[i] == b'(' {
        i += 1;
    }

    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'B' | b'C' | b'I' | b'S' | b'Z' => {
                args.push(bytes[i] as char);
                i += 1;
            }
            b'J' => {
                args.push('J');
                i += 1;
            }
            b'F' => {
                args.push('F');
                i += 1;
            }
            b'D' => {
                args.push('D');
                i += 1;
            }
            b'L' => {
                args.push('L');
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                args.push('L');
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

    args
}

/// Convert a JVM Value to its string representation for string concatenation.
///
/// When `thread` is provided, objects that are not strings or primitive wrappers
/// will have their `toString()` called via virtual dispatch. This produces
/// correct output for ArrayList, HashMap, user classes, etc.
fn value_to_string(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    value: &Value,
    type_char: char,
) -> String {
    match value {
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
            // Try to read as a Java String first
            if let Some(s) = read_java_string(&shared.mem.heap, *obj_ref) {
                return s;
            }

            // Check for wrapper types (1-field objects with a primitive value).
            // Arrays must NOT take this path: num_slots is the array LENGTH,
            // so a length-1 array would masquerade as a wrapper and packed
            // primitive arrays would read a garbage Value slot.
            let is_array =
                shared.mem.heap.kind_of(*obj_ref) == crate::memory::heap::ObjectKind::Array;
            let nf = shared.mem.heap.get_header(*obj_ref).num_slots() as usize;
            if nf == 1 && !is_array {
                match shared.mem.heap.get_field(*obj_ref, 0) {
                    Value::Int(v) => {
                        let class_id = shared.mem.heap.class_id_of(*obj_ref);
                        let name = shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(class_id)
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        if name.contains("Boolean") {
                            return if v != 0 {
                                "true".to_string()
                            } else {
                                "false".to_string()
                            };
                        } else if name.contains("Character") {
                            return char::from_u32(v as u32).unwrap_or('?').to_string();
                        }
                        return v.to_string();
                    }
                    Value::Long(v) => return v.to_string(),
                    Value::Float(v) => return format_float(v),
                    Value::Double(v) => return format_double(v),
                    _ => {}
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
                let obj_class_id = shared.mem.heap.class_id_of(*obj_ref);
                let is_path = ctx
                    .class_id_by_name("java/nio/file/Path")
                    .is_some_and(|path_cid| {
                        shared
                            .classes
                            .class_manager
                            .read()
                            .is_subclass_of(obj_class_id, path_cid)
                    });
                if is_path {
                    return cratonvm_native_builtins::phases_late::p57_path_display_string(
                        &mut ctx, *obj_ref,
                    );
                }

                match ctx.invoke_virtual(*obj_ref, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(str_ref)))) => {
                        return ctx
                            .read_string(str_ref)
                            .unwrap_or_else(|| "null".to_string());
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
    }
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
                    if let Some(ConstantPoolEntry::NameAndType { name_index, .. }) =
                        class.constant_pool.get(*name_and_type_index)
                    {
                        let desc = class
                            .constant_pool
                            .get_utf8(*name_index)
                            .unwrap_or("")
                            .to_string();
                        raw.push(RawSwitchLabel::PrimitiveClass(desc));
                    }
                }
                _ => {
                    tracing::warn!(
                        "Unrecognized constant pool entry type in switch label resolution (index {})",
                        arg_idx
                    );
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
                let cid = shared.load_class_concurrent(&name)?;
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
        .put_call_site(current_class_id, cp_index, site);

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
    let start_index = match thread.frames[frame_idx].stack.pop()? {
        Value::Int(i) => i.max(0) as usize,
        _ => 0,
    };
    let target = thread.frames[frame_idx].stack.pop()?;

    let result = match target {
        // Per the `SwitchBootstraps.typeSwitch` contract, a null target returns
        // -1. javac compiles `case null` as the -1 arm of the generated
        // table/lookupswitch (and, when there is no `case null`, emits a null
        // check that throws before the bootstrap is reached), so returning -1
        // here is what the generated bytecode expects.
        Value::Object(None) => -1,
        Value::Object(Some(obj_ref)) => {
            let obj_class_id = shared.mem.heap.class_id_of(obj_ref);
            // Read the object's class name once for boxed-type matching.
            let obj_class_name = shared
                .classes
                .class_manager
                .read()
                .get_class(obj_class_id)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            type_switch_match(
                shared,
                obj_ref,
                obj_class_id,
                &obj_class_name,
                labels,
                start_index,
            )
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

/// Match an object reference against switch labels. No locks acquired.
fn type_switch_match(
    shared: &SharedVm,
    obj_ref: ObjectRef,
    obj_class_id: ClassId,
    obj_class_name: &str,
    labels: &[SwitchLabel],
    start_index: usize,
) -> i32 {
    for (i, label) in labels.iter().enumerate().skip(start_index) {
        let matched = match label {
            SwitchLabel::Type {
                class_id,
                class_name: _,
            } => {
                // JEP 441: type patterns use instanceof semantics (subclass check).
                // A Long does NOT match `case Integer i` — only exact type or
                // supertype matches are valid.
                shared
                    .classes
                    .class_manager
                    .read()
                    .is_subclass_of(obj_class_id, *class_id)
            }
            SwitchLabel::Int(expected) => {
                unbox_int(shared, obj_ref, obj_class_name) == Some(*expected)
            }
            SwitchLabel::Long(expected) => {
                unbox_long(shared, obj_ref, obj_class_name) == Some(*expected)
            }
            SwitchLabel::Float(expected) => unbox_float(shared, obj_ref, obj_class_name)
                .is_some_and(|v| v.to_bits() == expected.to_bits()),
            SwitchLabel::Double(expected) => unbox_double(shared, obj_ref, obj_class_name)
                .is_some_and(|v| v.to_bits() == expected.to_bits()),
            SwitchLabel::Str(expected) => {
                read_java_string(&shared.mem.heap, obj_ref).as_deref() == Some(&**expected)
            }
            SwitchLabel::PrimitiveClass(desc) => {
                // JEP 507: primitive type pattern matches boxed wrapper types.
                match &**desc {
                    "I" | "Z" | "B" | "S" | "C" => matches!(
                        obj_class_name,
                        "java/lang/Integer"
                            | "java/lang/Boolean"
                            | "java/lang/Byte"
                            | "java/lang/Short"
                            | "java/lang/Character"
                    ),
                    "J" => obj_class_name == "java/lang/Long",
                    "F" => obj_class_name == "java/lang/Float",
                    "D" => obj_class_name == "java/lang/Double",
                    _ => false,
                }
            }
        };
        if matched {
            return i as i32;
        }
    }
    // No label matched: the `typeSwitch` contract returns labels.length (the
    // default arm), not -1 (which is reserved for a null target).
    labels.len() as i32
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
fn unbox_int(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<i32> {
    match class_name {
        "java/lang/Integer"
        | "java/lang/Byte"
        | "java/lang/Short"
        | "java/lang/Character"
        | "java/lang/Boolean" => match shared.mem.heap.get_field(obj, 0) {
            Value::Int(v) => Some(v),
            _ => None,
        },
        _ => None,
    }
}

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
    let (component_names, field_indices, field_descriptors) = {
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

        // Field indices: record components are stored in order starting from the
        // first field offset. We resolve the record class to get its field layout.
        let record_class_name = if !info.bootstrap_arg_indices.is_empty() {
            class
                .constant_pool
                .get_class_name(info.bootstrap_arg_indices[0])
                .map(|s| s.to_string())
        } else {
            None
        };

        // Drop read lock before acquiring write lock for class loading
        drop(cm);

        // Determine the field indices for each component.
        let field_indices: Vec<usize> = if let Some(ref rec_name) = record_class_name {
            let rec_cid = shared.load_class_concurrent(rec_name)?;
            let cm = shared.classes.class_manager.read();
            if let Some(rec_class) = cm.get_class(rec_cid) {
                let first_field = rec_class.first_field_index;
                (0..component_names.len())
                    .map(|i| first_field + i)
                    .collect()
            } else {
                (0..component_names.len()).collect()
            }
        } else {
            (0..component_names.len()).collect()
        };

        (component_names, field_indices, field_descriptors)
    };

    // Cache the call site.
    let site = ResolvedCallSite::RecordObjectMethod {
        method,
        component_names: component_names.clone(),
        field_indices: field_indices.clone(),
        field_descriptors: field_descriptors.clone(),
    };
    shared
        .classes
        .resolution_cache
        .write()
        .put_call_site(current_class_id, cp_index, site);

    execute_record_object_method(
        shared,
        thread,
        frame_idx,
        method,
        &component_names,
        &field_indices,
        &field_descriptors,
    )
}

/// Resolve a MethodHandle CP entry to extract the field descriptor.
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
fn execute_record_object_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    method: RecordMethodKind,
    component_names: &[Arc<str>],
    field_indices: &[usize],
    field_descriptors: &[Arc<str>],
) -> Result<(), MethodCallFailed> {
    match method {
        RecordMethodKind::Equals => {
            let other = thread.frames[frame_idx].stack.pop()?;
            let this = thread.frames[frame_idx].stack.pop()?;
            let result = match (this, other) {
                (Value::Object(Some(a)), Value::Object(Some(b))) => {
                    let mut ctx = NativeContextImpl { shared, thread };
                    i32::from(cratonvm_native_builtins::intrinsics::record::record_equals(
                        &mut ctx, a, b,
                    )?)
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
                    cratonvm_native_builtins::intrinsics::record::record_hash_code(&mut ctx, obj)?
                }
                _ => 0,
            };
            thread.frames[frame_idx].stack.push(Value::Int(hash))?;
        }
        RecordMethodKind::ToString => {
            let this = thread.frames[frame_idx].stack.pop()?;
            let s = match this {
                Value::Object(Some(obj)) => {
                    let cid = shared.mem.heap.class_id_of(obj);
                    let class_name = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    // Use simple name (after last '/' and after '$' for inner classes)
                    let simple = class_name
                        .rsplit('/')
                        .next()
                        .unwrap_or(&class_name)
                        .rsplit('$')
                        .next()
                        .unwrap_or(&class_name)
                        .to_string();

                    // Reference components must render via their VIRTUAL
                    // toString (the JDK's generated record toString formats
                    // each component with String.valueOf) — the previous
                    // identity fallback printed `object@hash` for any non-String
                    // reference component (e.g. a List/Map/nested record),
                    // breaking record toString everywhere. The toString invoke
                    // can trigger a moving GC, so the record ref is pinned and
                    // re-read per component (mirrors the Equals/HashCode arms).
                    use cratonvm_native_api::NativeContext as _;
                    let mut ctx = NativeContextImpl { shared, thread };
                    let obj_pin = ctx.pin_native_root(obj);
                    let mut result = format!("{simple}[");
                    let mut err: Option<MethodCallFailed> = None;
                    for (i, name) in component_names.iter().enumerate() {
                        if i > 0 {
                            result.push_str(", ");
                        }
                        let fi = field_indices.get(i).copied().unwrap_or(i);
                        let desc = field_descriptors.get(i).map(|s| &**s).unwrap_or("I");
                        let cur = ctx.read_native_pin(obj_pin, obj);
                        let v = ctx.get_field(cur, fi);
                        result.push_str(name);
                        result.push('=');
                        match value_to_string_deep(&mut ctx, &v, desc) {
                            Ok(vs) => result.push_str(&vs),
                            Err(e) => {
                                err = Some(e);
                                break;
                            }
                        }
                    }
                    ctx.unpin_native_roots(obj_pin);
                    if let Some(e) = err {
                        return Err(e);
                    }
                    result.push(']');
                    result
                }
                _ => "null".to_string(),
            };
            let str_ref = create_java_string(shared, &s);
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(str_ref)))?;
        }
    }
    Ok(())
}

/// Render one record component like the JDK's generated record `toString`:
/// primitives via their textual form, references via the VIRTUAL `toString`
/// (`String.valueOf`, i.e. `null` → "null", else `component.toString()`). The
/// String content fast path avoids a Java invoke for the common case.
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

fn value_to_string_deep(
    ctx: &mut NativeContextImpl<'_>,
    v: &Value,
    descriptor: &str,
) -> Result<String, MethodCallFailed> {
    match v {
        Value::Object(Some(obj)) => {
            // String fast path — read the chars directly.
            if let Some(s) = read_java_string(&ctx.shared.mem.heap, *obj) {
                return Ok(s);
            }
            use cratonvm_native_api::NativeContext as _;
            match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => {
                    Ok(read_java_string(&ctx.shared.mem.heap, s)
                        .unwrap_or_else(|| "null".to_string()))
                }
                // toString returned null (legal) → JDK prints "null".
                _ => Ok("null".to_string()),
            }
        }
        // Primitives and the null reference: descriptor-aware textual form.
        _ => Ok(format_field_value(ctx.shared, v, descriptor)),
    }
}

/// Format a field value for record toString.
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
        Value::Float(f) => format!("{f}"),
        Value::Double(d) => format!("{d}"),
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
/// Bootstrap arguments are string constants representing enum constant names.
/// The resulting call site takes `(Enum target, int startIndex)` and returns
/// the index of the matching enum constant name.
fn bootstrap_enum_switch(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let current_class_id = thread.frames[frame_idx].class_id;

    // Resolve bootstrap arguments — all are string constants (enum constant names).
    let labels = {
        let cm = shared.classes.class_manager.read();
        let class = cm
            .get_class(current_class_id)
            .ok_or_else(|| VmError::Internal {
                message: format!("enumSwitch: class {current_class_id} not found"),
            })?;

        let mut labels: Vec<Arc<str>> = Vec::with_capacity(info.bootstrap_arg_indices.len());
        for &arg_idx in &info.bootstrap_arg_indices {
            let s = resolve_string_constant(&class.constant_pool, arg_idx).unwrap_or_default();
            labels.push(Arc::from(s));
        }
        labels
    };

    // Cache the call site.
    let site = ResolvedCallSite::EnumSwitch {
        labels: labels.clone(),
    };
    shared
        .classes
        .resolution_cache
        .write()
        .put_call_site(current_class_id, cp_index, site);

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
    let start_index = match thread.frames[frame_idx].stack.pop()? {
        Value::Int(i) => i.max(0) as usize,
        _ => 0,
    };
    let target = thread.frames[frame_idx].stack.pop()?;

    let result = match target {
        // Per the `SwitchBootstraps.enumSwitch` contract, a null target returns
        // -1 (the `case null` arm); a non-null target that matches no label
        // returns labels.length (the default arm).
        Value::Object(None) => -1,
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
}
