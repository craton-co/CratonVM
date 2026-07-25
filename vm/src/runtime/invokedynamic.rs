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

use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

// `NativeContext` trait brought into scope for `ctx.get_class_mirror(...)` on
// `NativeContextImpl` in the generic-invokedynamic bootstrap path.
use cratonvm_native_api::NativeContext;

use crate::classloading::resolution::{
    LambdaCallSite, MethodHandle, MethodHandleKind, RecordMethodKind, ResolvedCallSite, SwitchLabel,
};
use crate::classloading::ClassId;
use crate::error::{MethodCallFailed, RuntimeError, VmError};
use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::{
    create_java_string, create_java_string_uninterned, read_java_string, NativeContextImpl,
    SharedVm,
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

/// Groovy call-site name for a coercion (`cast:(Object)Z`, `cast:(Object)I`, …).
const GROOVY_CAST: &str = "cast";

/// Groovy's runtime type-coercion helper (implements "Groovy truth").
const GROOVY_DTT: &str = "org/codehaus/groovy/runtime/typehandling/DefaultTypeTransformation";

/// Data extracted from the constant pool under a read lock, owned so we can
/// drop the lock before proceeding with string creation (which needs a write lock).
struct IndyInfo {
    bsm_class: String,
    bsm_method: String,
    target_name: String,
    target_descriptor: String,
    /// The recipe string (first bootstrap argument, if StringConcatFactory).
    recipe: String,
    /// Additional constant strings from bootstrap arguments (for \u0002 placeholders).
    constant_args: Vec<String>,
    /// Raw CP indices for bootstrap arguments (needed for LambdaMetafactory).
    bootstrap_arg_indices: Vec<u16>,
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
        let recipe = if let Some(&arg_index) = bsm.bootstrap_arguments.first() {
            resolve_string_constant(&class.constant_pool, arg_index).unwrap_or_default()
        } else {
            String::new()
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
        let constant_args: Vec<String> = bsm
            .bootstrap_arguments
            .iter()
            .skip(1) // skip recipe
            .map(|&idx| resolve_concat_constant(&class.constant_pool, idx).unwrap_or_default())
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

    if std::env::var_os("CRATONVM_DBG_INDY_ALL").is_some() {
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
            recipe: Arc::from(info.recipe.clone()),
            constant_args: info
                .constant_args
                .iter()
                .map(|s| Arc::from(s.as_str()))
                .collect(),
            target_descriptor: Arc::from(info.target_descriptor.clone()),
        };
        shared
            .classes
            .resolution_cache
            .write()
            .put_call_site(current_class_id, cp_index, site);

        execute_string_concat(shared, thread, frame_idx, &info)
    } else if info.bsm_class == STRING_CONCAT_FACTORY && info.bsm_method == MAKE_CONCAT {
        // makeConcat has no recipe — all arguments are simply concatenated in order.
        // Synthesize a recipe of all \u{0001} placeholders so the existing concat
        // logic works unchanged.
        let arg_types = parse_descriptor_args(&info.target_descriptor);
        let synthetic_recipe: String = std::iter::repeat('\u{0001}')
            .take(arg_types.len())
            .collect();
        let patched_info = IndyInfo {
            bsm_class: info.bsm_class.clone(),
            bsm_method: info.bsm_method.clone(),
            target_name: info.target_name.clone(),
            target_descriptor: info.target_descriptor.clone(),
            recipe: synthetic_recipe.clone(),
            constant_args: vec![],
            bootstrap_arg_indices: info.bootstrap_arg_indices.clone(),
        };

        let site = ResolvedCallSite::StringConcat {
            recipe: Arc::from(synthetic_recipe),
            constant_args: vec![],
            target_descriptor: Arc::from(info.target_descriptor.clone()),
        };
        shared
            .classes
            .resolution_cache
            .write()
            .put_call_site(current_class_id, cp_index, site);

        execute_string_concat(shared, thread, frame_idx, &patched_info)
    } else if info.bsm_class == LAMBDA_METAFACTORY
        && (info.bsm_method == METAFACTORY || info.bsm_method == ALT_METAFACTORY)
    {
        // altMetafactory has additional bootstrap arguments (flags, marker interfaces,
        // bridges) beyond the 3 standard ones, but the core lambda proxy creation is
        // identical — extra args are advisory and not needed for dispatch.
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
            match shared.heap.get_field(o, 0) {
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
    let dbg = std::env::var_os("CRATONVM_DBG_INDY_GENERIC").is_some();
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
        } => {
            let info = IndyInfo {
                bsm_class: String::new(),
                bsm_method: String::new(),
                target_name: String::new(),
                target_descriptor: target_descriptor.to_string(),
                recipe: recipe.to_string(),
                constant_args: constant_args.iter().map(|s| s.to_string()).collect(),
                bootstrap_arg_indices: vec![],
            };
            execute_string_concat(shared, thread, frame_idx, &info)
        }
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

    if std::env::var_os("CRATONVM_DBG_LAMBDA_DISPATCH").is_some() {
        eprintln!(
            "[DBG_LAMBDA] bootstrap host={:?} loader={:?} iface={} resolved_id={:?}",
            current_class_id, host_loader, functional_interface, functional_interface_id
        );
    }
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
    };

    // Register the lambda proxy and cache the call site
    let registered = {
        let mut proxies = shared.classes.lambda_proxies.write();
        if proxies.len() < crate::vm::MAX_LAMBDA_PROXIES {
            proxies.insert(proxy_class_id, call_site.clone());
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
    }
    shared.classes.resolution_cache.write().put_call_site(
        current_class_id,
        cp_index,
        ResolvedCallSite::Lambda(call_site),
    );

    // Now execute: pop captured values, allocate proxy object, push it
    allocate_lambda_proxy(shared, thread, frame_idx, proxy_class_id, &capture_types)
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
static LAMBDA_SINGLETON_CACHE: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), ObjectRef>>,
> = std::sync::OnceLock::new();

fn lambda_singleton_cache(
) -> &'static parking_lot::Mutex<std::collections::HashMap<(usize, ClassId), ObjectRef>> {
    LAMBDA_SINGLETON_CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// GC root scan hook — called from `vm/src/memory/roots.rs`. Reports the
/// cached zero-capture lambda singletons for the active VM so the GC keeps
/// them live.
pub fn gc_scan_lambda_singleton_roots(vm_identity: usize, out: &mut Vec<ObjectRef>) {
    let cache = lambda_singleton_cache().lock();
    for (&(vid, _), obj_ref) in cache.iter() {
        if vid == vm_identity {
            out.push(*obj_ref);
        }
    }
}

/// GC post-compaction hook — called from `vm/src/memory/gc.rs`. Remaps
/// every cached singleton for the active VM through the GC's pointer map.
pub fn gc_update_lambda_singleton_refs(
    vm_identity: usize,
    pointer_map: &std::collections::HashMap<usize, usize>,
) {
    if pointer_map.is_empty() {
        return;
    }
    let mut cache = lambda_singleton_cache().lock();
    for (&(vid, _), obj_ref) in cache.iter_mut() {
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

    // Fast path: a zero-capture call site whose singleton was already
    // minted on a prior invocation just returns the cached instance —
    // matches real HotSpot's cached-INSTANCE-field optimization for
    // non-capturing lambdas (see LAMBDA_SINGLETON_CACHE above).
    if num_captures == 0 {
        if let Some(cached) = lambda_singleton_cache()
            .lock()
            .get(&(shared.vm_identity, proxy_class_id))
            .copied()
        {
            thread.frames[frame_idx]
                .stack
                .push(Value::Object(Some(cached)))?;
            return Ok(());
        }
    }

    // Pop captured values (pushed left-to-right, pop right-to-left)
    let mut captures: Vec<Value> = Vec::with_capacity(num_captures);
    for _ in 0..num_captures {
        captures.push(thread.frames[frame_idx].stack.pop()?);
    }
    captures.reverse();

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
    let proxy_ref = match shared.heap.try_alloc_object(proxy_class_id, num_captures) {
        Some(obj) => obj,
        None => {
            thread.tlab.retire();
            super::interpreter::maybe_gc_forced_pub(shared, thread);
            match shared
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
    // Refresh any captured object references from their pins — the retry
    // path above may have relocated them during GC.
    for (j, h) in handles.iter().enumerate() {
        if let Some(h) = *h {
            captures[j] = Value::Object(Some(thread.native_pin_roots[h]));
        }
    }
    thread.native_pin_roots.truncate(pin_base);

    for (i, val) in captures.iter().enumerate() {
        shared.heap.set_field(proxy_ref, i, *val);
    }

    // Zero-capture call sites mint their singleton exactly once; every
    // later invocation hits the fast path above instead.
    if num_captures == 0 {
        lambda_singleton_cache()
            .lock()
            .insert((shared.vm_identity, proxy_class_id), proxy_ref);
    }

    // Push the proxy object onto the stack
    thread.frames[frame_idx]
        .stack
        .push(Value::Object(Some(proxy_ref)))?;

    Ok(())
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
fn execute_string_concat(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    info: &IndyInfo,
) -> Result<(), MethodCallFailed> {
    let arg_types = parse_descriptor_args(&info.target_descriptor);

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

    // Walk the recipe and build the result string
    let mut result = String::new();
    let mut arg_idx = 0;
    let mut const_idx = 0;

    for ch in info.recipe.chars() {
        if ch == '\u{0001}' {
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
                let s = value_to_string(shared, Some(thread), &arg_val, arg_type);
                result.push_str(&s);
                arg_idx += 1;
            }
        } else if ch == '\u{0002}' {
            // Constant placeholder (from bootstrap_arguments[1..])
            if let Some(s) = info.constant_args.get(const_idx) {
                result.push_str(s);
            }
            const_idx += 1;
        } else {
            result.push(ch);
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
    let str_ref = create_java_string_uninterned(shared, &result);
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
            if let Some(s) = read_java_string(&shared.heap, *obj_ref) {
                return s;
            }

            // Check for wrapper types (1-field objects with a primitive value).
            // Arrays must NOT take this path: num_slots is the array LENGTH,
            // so a length-1 array would masquerade as a wrapper and packed
            // primitive arrays would read a garbage Value slot.
            let is_array = shared.heap.kind_of(*obj_ref) == crate::memory::heap::ObjectKind::Array;
            let nf = shared.heap.get_header(*obj_ref).num_slots() as usize;
            if nf == 1 && !is_array {
                match shared.heap.get_field(*obj_ref, 0) {
                    Value::Int(v) => {
                        let class_id = shared.heap.class_id_of(*obj_ref);
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
                // `docs/internal/springboot/path-tostring-dead-dispatch-breaks-inprocess-javac-FIXED.md`,
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
                let obj_class_id = shared.heap.class_id_of(*obj_ref);
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
                let class_id = shared.heap.class_id_of(*obj_ref);
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| "?".to_string())
            };
            let dotted = class_name.replace('/', ".");
            let hash = shared.heap.identity_hash_code(*obj_ref);
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
            let obj_class_id = shared.heap.class_id_of(obj_ref);
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
                read_java_string(&shared.heap, obj_ref).as_deref() == Some(&**expected)
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
        | "java/lang/Boolean" => match shared.heap.get_field(obj_ref, 0) {
            Value::Int(v) => NumericValue::Int(v),
            _ => return false,
        },
        "java/lang/Long" => match shared.heap.get_field(obj_ref, 0) {
            Value::Long(v) => NumericValue::Long(v),
            _ => return false,
        },
        "java/lang/Float" => match shared.heap.get_field(obj_ref, 0) {
            Value::Float(v) => NumericValue::Float(v),
            _ => return false,
        },
        "java/lang/Double" => match shared.heap.get_field(obj_ref, 0) {
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
        | "java/lang/Boolean" => match shared.heap.get_field(obj, 0) {
            Value::Int(v) => Some(v),
            _ => None,
        },
        _ => None,
    }
}

fn unbox_long(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<i64> {
    if class_name == "java/lang/Long" {
        match shared.heap.get_field(obj, 0) {
            Value::Long(v) => Some(v),
            _ => None,
        }
    } else {
        None
    }
}

fn unbox_float(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<f32> {
    if class_name == "java/lang/Float" {
        match shared.heap.get_field(obj, 0) {
            Value::Float(v) => Some(v),
            _ => None,
        }
    } else {
        None
    }
}

fn unbox_double(shared: &SharedVm, obj: ObjectRef, class_name: &str) -> Option<f64> {
    if class_name == "java/lang/Double" {
        match shared.heap.get_field(obj, 0) {
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

            let result = match (&this, &other) {
                (Value::Object(Some(a)), Value::Object(Some(b))) if a == b => Ok(1),
                (Value::Object(Some(a)), Value::Object(Some(b))) => {
                    // Must be same class
                    let a_cid = shared.heap.class_id_of(*a);
                    let b_cid = shared.heap.class_id_of(*b);
                    if a_cid != b_cid {
                        Ok(0)
                    } else {
                        // Compare each component field. Reference components
                        // must compare via their VIRTUAL equals (the JDK's
                        // generated record equals calls Objects.equals per
                        // component) — identity comparison broke e.g. JUnit 6's
                        // record CompositeKey(namespace, key) in
                        // NamespacedHierarchicalStore, so every store lookup
                        // with an equal-but-distinct namespace missed. The
                        // invokes can trigger a moving GC, so the record refs
                        // are pinned and re-read per component.
                        use cratonvm_native_api::NativeContext as _;
                        let mut ctx = NativeContextImpl { shared, thread };
                        let a_pin = ctx.pin_native_root(*a);
                        let b_pin = ctx.pin_native_root(*b);
                        let mut equal = Ok(1);
                        for &fi in field_indices {
                            let aa = ctx.read_native_pin(a_pin, *a);
                            let bb = ctx.read_native_pin(b_pin, *b);
                            let va = ctx.get_field(aa, fi);
                            let vb = ctx.get_field(bb, fi);
                            match values_equal_deep(&mut ctx, &va, &vb) {
                                Ok(true) => continue,
                                Ok(false) => {
                                    equal = Ok(0);
                                    break;
                                }
                                Err(e) => {
                                    equal = Err(e);
                                    break;
                                }
                            }
                        }
                        ctx.unpin_native_roots(a_pin);
                        ctx.unpin_native_roots(b_pin);
                        equal
                    }
                }
                // this.equals(null) → false
                _ => Ok(0),
            };
            let result = result?;
            thread.frames[frame_idx].stack.push(Value::Int(result))?;
        }
        RecordMethodKind::HashCode => {
            let this = thread.frames[frame_idx].stack.pop()?;
            let hash = match this {
                Value::Object(Some(obj)) => {
                    // Reference components must hash via their VIRTUAL
                    // hashCode (the identity fallback made record hashes
                    // unstable across equal instances — see the Equals arm).
                    use cratonvm_native_api::NativeContext as _;
                    let mut ctx = NativeContextImpl { shared, thread };
                    let obj_pin = ctx.pin_native_root(obj);
                    let mut h: Result<i32, MethodCallFailed> = Ok(0);
                    for &fi in field_indices {
                        let cur = ctx.read_native_pin(obj_pin, obj);
                        let v = ctx.get_field(cur, fi);
                        match value_hash_deep(&mut ctx, &v) {
                            Ok(vh) => {
                                h = Ok(h.unwrap().wrapping_mul(31).wrapping_add(vh));
                            }
                            Err(e) => {
                                h = Err(e);
                                break;
                            }
                        }
                    }
                    ctx.unpin_native_roots(obj_pin);
                    h?
                }
                _ => 0,
            };
            thread.frames[frame_idx].stack.push(Value::Int(hash))?;
        }
        RecordMethodKind::ToString => {
            let this = thread.frames[frame_idx].stack.pop()?;
            let s = match this {
                Value::Object(Some(obj)) => {
                    let cid = shared.heap.class_id_of(obj);
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

/// Compare two record component values like the JDK's generated record
/// `equals` does: primitives by value (`Float.equals`/`Double.equals` bit
/// semantics), references via `Objects.equals` — i.e. the component's
/// VIRTUAL `equals`. The String content fast path avoids a Java invoke for
/// the overwhelmingly common case.
fn values_equal_deep(
    ctx: &mut NativeContextImpl<'_>,
    a: &Value,
    b: &Value,
) -> Result<bool, MethodCallFailed> {
    match (a, b) {
        (Value::Object(Some(x)), Value::Object(Some(y))) => {
            if x == y {
                return Ok(true);
            }
            let x_cid = ctx.shared.heap.class_id_of(*x);
            let x_name = ctx
                .shared
                .classes
                .class_manager
                .read()
                .get_class(x_cid)
                .map(|c| c.name.clone());
            if x_name.as_deref() == Some("java/lang/String") {
                let xs = read_java_string(&ctx.shared.heap, *x);
                let ys = read_java_string(&ctx.shared.heap, *y);
                return Ok(xs == ys);
            }
            use cratonvm_native_api::NativeContext as _;
            match ctx.invoke_virtual(
                *x,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(*y))],
            )? {
                Some(Value::Int(v)) => Ok(v != 0),
                _ => Ok(false),
            }
        }
        _ => Ok(values_equal(ctx.shared, a, b)),
    }
}

/// Hash one record component like the JDK's generated record `hashCode`:
/// primitives by their wrapper hash, references via the VIRTUAL `hashCode`.
fn value_hash_deep(ctx: &mut NativeContextImpl<'_>, v: &Value) -> Result<i32, MethodCallFailed> {
    match v {
        Value::Object(Some(obj)) => {
            let cid = ctx.shared.heap.class_id_of(*obj);
            let name = ctx
                .shared
                .classes
                .class_manager
                .read()
                .get_class(cid)
                .map(|c| c.name.clone());
            if name.as_deref() == Some("java/lang/String") {
                return Ok(value_hash(ctx.shared, v));
            }
            use cratonvm_native_api::NativeContext as _;
            match ctx.invoke_virtual(*obj, "hashCode", "()I", &[])? {
                Some(Value::Int(h)) => Ok(h),
                _ => Ok(0),
            }
        }
        _ => Ok(value_hash(ctx.shared, v)),
    }
}

/// Render one record component like the JDK's generated record `toString`:
/// primitives via their textual form, references via the VIRTUAL `toString`
/// (`String.valueOf`, i.e. `null` → "null", else `component.toString()`). The
/// String content fast path avoids a Java invoke for the common case.
fn value_to_string_deep(
    ctx: &mut NativeContextImpl<'_>,
    v: &Value,
    descriptor: &str,
) -> Result<String, MethodCallFailed> {
    match v {
        Value::Object(Some(obj)) => {
            // String fast path — read the chars directly.
            if let Some(s) = read_java_string(&ctx.shared.heap, *obj) {
                return Ok(s);
            }
            use cratonvm_native_api::NativeContext as _;
            match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => {
                    Ok(read_java_string(&ctx.shared.heap, s).unwrap_or_else(|| "null".to_string()))
                }
                // toString returned null (legal) → JDK prints "null".
                _ => Ok("null".to_string()),
            }
        }
        // Primitives and the null reference: descriptor-aware textual form.
        _ => Ok(format_field_value(ctx.shared, v, descriptor)),
    }
}

/// Compare two JVM values for equality (used by record equals).
fn values_equal(shared: &SharedVm, a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => {
            // Float.equals semantics: NaN == NaN, +0 != -0
            x.to_bits() == y.to_bits()
        }
        (Value::Double(x), Value::Double(y)) => {
            // Double.equals semantics
            x.to_bits() == y.to_bits()
        }
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Object(Some(x)), Value::Object(Some(y))) => {
            if x == y {
                return true;
            }
            // For String objects, compare by content
            let x_cid = shared.heap.class_id_of(*x);
            let x_name = shared
                .classes
                .class_manager
                .read()
                .get_class(x_cid)
                .map(|c| c.name.clone());
            if x_name.as_deref() == Some("java/lang/String") {
                let xs = read_java_string(&shared.heap, *x);
                let ys = read_java_string(&shared.heap, *y);
                return xs == ys;
            }
            // For other objects, reference equality
            false
        }
        _ => false,
    }
}

/// Hash a JVM value (used by record hashCode).
fn value_hash(shared: &SharedVm, v: &Value) -> i32 {
    match v {
        Value::Int(n) => *n,
        Value::Long(n) => (*n ^ (*n >> 32)) as i32,
        Value::Float(f) => f.to_bits() as i32,
        Value::Double(d) => {
            let bits = d.to_bits();
            (bits ^ (bits >> 32)) as i32
        }
        Value::Object(Some(obj)) => {
            // For strings, hash the content
            let cid = shared.heap.class_id_of(*obj);
            let name = shared
                .classes
                .class_manager
                .read()
                .get_class(cid)
                .map(|c| c.name.clone());
            if name.as_deref() == Some("java/lang/String") {
                if let Some(s) = read_java_string(&shared.heap, *obj) {
                    return s
                        .bytes()
                        .fold(0i32, |h, b| h.wrapping_mul(31).wrapping_add(b as i32));
                }
            }
            obj.as_ptr() as i32
        }
        Value::Object(None) => 0,
        _ => 0,
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
            if let Some(s) = read_java_string(&shared.heap, *obj) {
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
            let name = match shared.heap.get_field(obj_ref, 0) {
                Value::Object(Some(name_ref)) => read_java_string(&shared.heap, name_ref),
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
        let obj = shared.heap.alloc_object(class_id, 1);
        shared.heap.set_field(obj, 0, value);
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
