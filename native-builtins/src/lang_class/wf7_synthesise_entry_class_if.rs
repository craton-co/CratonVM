#[allow(unused_imports)]
use super::*;

/// WF7 вЂ” if `internal_name` looks like a WildFly/Keycloak entry-class FQCN
/// (slash-separated) and the class isn't already defined, build a minimal
/// stub with a no-op `main` and register it via
/// `NativeContext::define_class_from_bytes`. Returns the class mirror on
/// success, or `None` if the name doesn't match, the class is already
/// defined, or the define call fails. Callers should fall through to the
/// regular `Class.forName` resolution when this returns `None`.
pub fn wf7_synthesise_entry_class_if_missing(
    ctx: &mut dyn NativeContext,
    internal_name: &str,
) -> Option<ObjectRef> {
    // Real-bytecode audit: this WF7 synthetic class fallback is disabled
    // by default. It defined a 191-byte synthetic class with a no-op
    // `main([Ljava/lang/String;)V` whenever `Class.forName` for a
    // WildFly/Keycloak entry-class name missed. That synthetic class
    // would then be invoked as the entry-point вЂ” short-circuiting any
    // real boot. The guard above ("if class already loaded, return None")
    // limits scope, but the gate-off is the safer default.
    if !crate::nbflags().use_wildfly_synth_bytecode {
        let _ = internal_name;
        return None;
    }
    // Only synthesise for names we explicitly recognise.
    if !WF7_ENTRY_FRAGMENTS
        .iter()
        .any(|f| internal_name.contains(f))
    {
        return None;
    }
    // If the class is already loaded, defer to the real one вЂ” synthesis is
    // a fallback, never a replacement.
    if ctx.class_id_by_name(internal_name).is_some() {
        return None;
    }
    let bytecode = wf7_build_minimal_main_class(internal_name);
    let cid = ctx.define_class_from_bytes(internal_name, &bytecode)?;
    Some(ctx.get_class_mirror(cid))
}

/// Security hardening вЂ” reject `Class.forName` names that look like a
/// class-load / path-traversal injection rather than a legitimate binary
/// class name.
///
/// The `Class.forName(Module, String)` overload (`native_class_for_name_module`)
/// already screens its input for control bytes and path separators; the
/// primary `Class.forName(String)` and `Class.forName(String,boolean,ClassLoader)`
/// overloads historically did not, so a hostile caller could smuggle NUL
/// bytes, control characters, or `/`, `\`, `..` path segments straight into
/// the loader and probe the host filesystem. This helper centralizes the
/// check so both paths enforce the same rules.
///
/// A binary class name (JLS В§13.1 / JVMS В§4.2.1) never contains:
///   * a NUL, any other control byte (`< 0x20`), or DEL (`0x7F`);
///   * a forward slash `/` or backslash `\` in its *dotted* form (the dotted
///     form uses `.` as the package separator вЂ” a literal `/`/`\` is a path
///     character, not a package separator);
///   * an empty package segment, i.e. a `..` run (which in dotted form means
///     a zero-length identifier and is the canonical filesystem
///     parent-directory traversal token).
/// Empty names are likewise rejected.
///
/// On rejection we return `ClassNotFoundException(dotted_name)` вЂ” the same
/// exception the primary overloads raise for any other unresolved name, so
/// a rejected probe is indistinguishable from an ordinary miss and leaks no
/// information about why it failed.
/// The slashed name `Class.forName` refuses when `dotted_name` (no `/`, not
/// an array descriptor) is not a valid JVM class name once its dots are
/// slashes: `check_classname.c` `VerifyClassname` / `skip_over_fieldname`
/// refuse a `[` or `;` inside the name, a leading `/` (a leading `.`) and two
/// consecutive `/` (`..`). `Class.c` then throws `ClassNotFoundException` with
/// the FIXED (slashed) text. `None` for a name that passes: the loader's
/// `ClassNotFoundException` names it dotted. A name containing `/` keeps its
/// original spelling (`VerifyFixClassname`), handled by the caller.
pub fn for_name_invalid_jvm_name(dotted_name: &str) -> Option<String> {
    if dotted_name.is_empty() || dotted_name.contains('/') || dotted_name.starts_with('[') {
        return None;
    }
    let fixed = dotted_name.replace('.', "/");
    let invalid = fixed.starts_with('/')
        || fixed.contains("//")
        || fixed.contains('[')
        || fixed.contains(';');
    invalid.then_some(fixed)
}

pub fn validate_for_name_dotted(dotted_name: &str) -> Result<(), MethodCallFailed> {
    let reject = dotted_name.is_empty()
        || dotted_name.bytes().any(|b| b < 0x20 || b == 0x7F)
        || dotted_name.contains('/')
        || dotted_name.contains('\\')
        || dotted_name.contains("..");
    if reject {
        return Err(
            cratonvm_types::error::RuntimeError::ClassNotFoundException {
                class_name: dotted_name.to_string(),
            }
            .into(),
        );
    }
    Ok(())
}

/// The name HotSpot's `Class.forName` puts in a `ClassNotFoundException`.
///
/// JVMS 5.3.3: an array class is created *by the VM from its element type* --
/// no class file for the array itself is ever consulted -- so `Class.forName`
/// strips the `[`s itself and the only name a loader, and therefore a
/// `ClassNotFoundException`, ever sees is the ELEMENT's. Measured on JDK 25
/// (jdk-25.0.3.9-hotspot):
///
/// ```text
/// Class.forName("[Lp.X;")  -> ClassNotFoundException msg="p.X"  cause=null
/// Class.forName("[[Lp.X;") -> ClassNotFoundException msg="p.X"  cause=null
/// ```
///
/// `ClassLoader.loadClass("[Lp.X;")` by contrast keeps the descriptor -- it
/// never resolves an array form at all, not even one whose element exists.
/// That asymmetry is exactly why the correction belongs here, at
/// `Class.forName`, and not in the loader: the loader's own wording is right
/// for the loader.
///
/// `array_descriptor_element_class` answers `None` for everything that is not
/// a reference-array descriptor (`[I`, a plain class name, a malformed `[Lp/X`
/// or `[L;`), so every non-array case keeps today's name character for
/// character.
///
/// See docs/feature-designs/native-dispatch-architecture.md
pub fn for_name_cnfe_name(dotted_name: &str) -> String {
    cratonvm_classloading::array_descriptor_element_class(dotted_name)
        .unwrap_or(dotted_name)
        .to_string()
}

/// Re-mint a `ClassNotFoundException` that a loader raised for an array
/// descriptor so it names the ELEMENT, per `for_name_cnfe_name`.
///
/// Deliberately narrow on both axes:
///
/// * Only `java/lang/ClassNotFoundException` is rewritten, by exact class
///   name. A `NoClassDefFoundError` is left alone on purpose -- when
///   `[Lp/X;`'s element `p/X` EXISTS but `p/X`'s own supertype is missing,
///   `load_class_visible_to`'s `DependencyMissing` arm mints an NCDFE naming
///   that supertype, which is what `Class.forName` should report and what the
///   L16 guard in `classloader_real.rs` was deliberately not widened past.
/// * Only a reference-array descriptor is rewritten; anything else is handed
///   straight back.
///
/// j27 wave 4 (lane TRACEROWS, TR2W2-6): the loader's own throwable is
/// rethrown with its `detailMessage` renamed, not replaced by a fresh
/// `RuntimeError::ClassNotFoundException`. HotSpot's `Class.forName` array
/// miss throws the loader's `ClassNotFoundException` for the element, whose
/// trace lists the loader frames (`BuiltinClassLoader.loadClass` ...); the
/// fresh one had none of them (and the loader's had already taken the
/// served-loader-frames hint). Its cause is the loader's (`null` for the
/// built-in loaders, as on HotSpot).
pub fn for_name_rename_array_cnfe(
    ctx: &mut dyn NativeContext,
    dotted_name: &str,
    failed: MethodCallFailed,
) -> MethodCallFailed {
    let element = match cratonvm_classloading::array_descriptor_element_class(dotted_name) {
        Some(element) => element.to_string(),
        None => return failed,
    };
    let exc_ref = match failed {
        MethodCallFailed::ExceptionThrown(exc_ref) => exc_ref,
        other => return other,
    };
    let exc_cid = ctx.class_id_of_object(exc_ref);
    if !ctx
        .class_name_of_id(exc_cid)
        .is_some_and(|n| n == "java/lang/ClassNotFoundException")
    {
        return MethodCallFailed::ExceptionThrown(exc_ref);
    }
    // The message allocation may move the throwable: keep it pinned.
    let pin = ctx.pin_native_root(exc_ref);
    let message = ctx.create_string_uninterned(&element);
    let exc_ref = ctx.read_native_pin(pin, exc_ref);
    ctx.unpin_native_roots(pin);
    crate::lang_misc::throwable_field_set(
        ctx,
        exc_ref,
        "detailMessage",
        Value::Object(Some(message)),
    );
    MethodCallFailed::ExceptionThrown(exc_ref)
}

/// DBG: report whether `cid` is a bytecode-enhanced entity (declares a
/// `$$_hibernate_*` member). Trace-only.
pub fn dbg_class_enhanced(ctx: &mut dyn NativeContext, cid: ClassId) -> bool {
    ctx.declared_methods(cid)
        .iter()
        .any(|m| m.name.starts_with("$$_hibernate_"))
}

pub fn dbg_is_entity_name(n: &str) -> bool {
    n.contains("orm/test/cache/") || n.contains("orm.test.cache.")
}

pub fn class_for_name_one_arg_caller_loader(
    ctx: &mut dyn NativeContext,
) -> Option<ObjectRef> {
    for caller_cid in ctx.frame_class_ids() {
        if matches!(
            ctx.class_name_arc_of_id(caller_cid).as_deref(),
            Some("java/lang/Class")
        ) {
            continue;
        }
        let caller_mirror = ctx.get_class_mirror(caller_cid);
        return match native_class_get_class_loader(ctx, &[Value::Object(Some(caller_mirror))]) {
            Ok(Some(Value::Object(Some(loader)))) => Some(loader),
            _ => None,
        };
    }
    None
}

/// `Class.forName(<array name>, init, loader)` for a user-defined `loader`
/// under `--real-jdk`: the element class `element` (internal form) is what
/// `loader.loadClass` answers (unless the loader defined or initiated it
/// already, as the non-array arm reads), asked once, under the loader's monitor as the
/// VM asks it (`invoke_load_class_as_the_vm`), and the array is the one
/// keyed by that element's defining loader (JVMS §5.3.3). The loader's
/// throwable is `forName`'s, as it is; a `null` answer, or a class of another
/// name, is `ClassNotFoundException` naming the array (measured on JDK 25),
/// and the answer meets the §5.3.4 initiating-load check as the non-array arm
/// below does. Before, the array arm answered from the global store and never
/// asked the loader, so a child-first loader's `X[]` was the application's.
/// Interpreter round i1 wave 41, lane L5; probe
/// `tools/probes/interp/L5/L5W41ForNameArrayLoader.java`.
pub fn for_name_array_through_loader(
    ctx: &mut dyn NativeContext,
    loader: cratonvm_types::ObjectRef,
    internal_name: &str,
    element: &str,
) -> MethodCallResult {
    // What the loader defined or initiated under the element's name answers
    // without a call, as HotSpot's dictionary does (the non-array arm below
    // reads the same two records).
    // A mirror may be allocated on the way: the loader is read back from its
    // pin before its next use.
    let loader_pin = ctx.pin_native_root(loader);
    let known = crate::classloader::find_loaded_class_for_loader(ctx, loader, element)
        .or_else(|| crate::classloader_real::initiated_class_mirror(ctx, loader, element))
        .and_then(|mirror| ctx.class_id_from_mirror(mirror));
    let loader = ctx.read_native_pin(loader_pin, loader);
    ctx.unpin_native_roots(loader_pin);
    if let Some(element_id) = known {
        let element_loader = ctx.loader_id_of_class(element_id).max(0) as u32;
        if let Some(array_id) = ctx.array_class_id_for_loader(internal_name, element_loader) {
            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(array_id)))));
        }
    }
    let initiating_ns = crate::classloader::peek_loader_namespace_id(ctx, loader)
        .or_else(|| Some(crate::classloader::loader_namespace_id(ctx, loader)))
        .filter(|&ns| ns >= 3);
    // The shared door, publishing the initiating load under a
    // non-parallel-capable loader's monitor (`CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK`;
    // wave 130, lane L7); `published`: the note below was made there.
    let loader_pin = ctx.pin_native_root(loader);
    // JIT round 22 wave 3, lane review (`r13_misc10_create_string_ratchet`):
    // the binary name handed to `loadClass` is computed text, a fresh `String`
    // in HotSpot (`java_lang_String::create_from_symbol`), never pooled.
    let element_name = ctx.create_string_uninterned(&element.replace('/', "."));
    let loader = ctx.read_native_pin(loader_pin, loader);
    let (answer, published) = crate::classloader_real::invoke_load_class_as_the_vm_publishing(
        ctx,
        loader,
        element_name,
        initiating_ns,
    );
    ctx.unpin_native_roots(loader_pin);
    let answer = answer?;
    // `loader` ran Java: it is not used again. The answer's mirror is read
    // into a class id before anything allocates.
    let element_id = match answer {
        Some(Value::Object(Some(mirror))) => ctx
            .class_id_from_mirror(mirror)
            .filter(|&cid| ctx.class_name_of_id(cid).is_some_and(|n| n == element)),
        _ => None,
    };
    let Some(element_id) = element_id else {
        let exc = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassNotFoundException",
            1,
            internal_name,
        );
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    };
    if let (Some(ns), false) = (initiating_ns, published) {
        // Allocates only when it refuses (the `LinkageError`).
        ctx.note_initiating_load(ns, element, element_id)?;
    }
    let element_loader = ctx.loader_id_of_class(element_id).max(0) as u32;
    match ctx.array_class_id_for_loader(internal_name, element_loader) {
        Some(array_id) => Ok(Some(Value::Object(Some(ctx.get_class_mirror(array_id))))),
        None => {
            let exc = crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/ClassNotFoundException",
                1,
                internal_name,
            );
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                exc?,
            ))
        }
    }
}

/// The class named `internal` that a run-time define put in the BOOT loader's
/// namespace (`DefineClassFull::bootstrap_namespace`), for the null-loader
/// `Class.forName`; `None` when there is none, when the name is ambiguous
/// (`class_id_by_name` fails closed), for a hidden class (never found by
/// name), or with `CRATONVM_FORNAME_BOOT_NAMESPACE_DEFINES=0`.
pub fn runtime_boot_namespace_class(ctx: &dyn NativeContext, internal: &str) -> Option<ClassId> {
    let cid = ctx.class_id_by_name(internal)?;
    let boot = ctx.loader_id_of_class(cid)
        == cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP as i32
        && !ctx.is_class_hidden(cid);
    (boot)
    .then_some(cid)
}

/// The BOOTSTRAP namespace's copy of `internal` for the null-loader
/// `Class.forName`, when `internal` is a class the runtime image gives a BOOT
/// module but [`crate::classloader::is_bootstrap_class_name`] refuses
/// (`sun/reflect/misc/Trampoline`: `MethodUtil`, a `SecureClassLoader`,
/// defines its own copy and invokes through it, and the name exceptions keep
/// that copy loader-aware). HotSpot's boot loader serves the image class like
/// any other, so `forName(T, false, null)` answers a boot-defined class, and
/// `forName(T, true, null)` runs that copy's `<clinit>` (whose `Error` is the
/// answer), whatever `MethodUtil` defined before
/// (`docs/feature-designs/zgc-architecture.md`;
/// interpreter round i7 wave 136, lane L5; probe
/// `L3W135TrampolineBootAndMethodUtil.java`).
///
/// The load is the global route, which defines an image class under the
/// bootstrap loader and never answers a user loader's same-named class while
/// the image has bytes for it (`ClassManager::resolve_fast_path_class_id`);
/// the answer is still checked to be bootstrap-defined, and anything else is
/// `None` (the caller's `ClassNotFoundException`, as before). Only a Java
/// exception of the load (a linkage error) is propagated. `None` with
/// `CRATONVM_FORNAME_BOOT_IMAGE_NAMESPACE=0`; asked only on the refusal path.
pub fn boot_image_namespace_class(
    ctx: &mut dyn NativeContext,
    internal: &str,
) -> Result<Option<ClassId>, MethodCallFailed> {
    if internal.starts_with('[')
        || crate::classloader::is_bootstrap_class_name(internal)
        || ctx.runtime_image_defines_class(internal) != Some(true)
        || !true
        || crate::classloader::image_class_outside_boot_modules(ctx, internal)
    {
        return Ok(None);
    }
    let mirror = match ctx.load_class(internal) {
        Ok(Some(Value::Object(Some(mirror)))) => mirror,
        Ok(_) => return Ok(None),
        Err(e @ MethodCallFailed::ExceptionThrown(_)) => return Err(e),
        Err(_) => return Ok(None),
    };
    let Some(cid) = ctx.class_id_from_mirror(mirror) else {
        return Ok(None);
    };
    let boot = ctx.loader_id_of_class(cid)
        == cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP as i32
        && !ctx.is_class_hidden(cid);
    Ok(boot.then_some(cid))
}

/// `Class.forName0` as registered: [`native_class_for_name`], then (pure-bytecode
/// lever only) the package definition HotSpot's `BuiltinClassLoader.defineClass`
/// performs when it defines an image class.
///
/// The VM loads a JDK class straight from the image, never through the built-in
/// loader's Java `defineClass`, so the loader's `packages` map stays empty and
/// the JDK's own `ClassLoader.getDefinedPackage` (a read of that map, with no
/// native in front of it under the lever) answers `null` for a package a class
/// of which is loaded. Default mode answers from a native; this fills the field
/// the bytecode reads, through the JDK's own idempotent `definePackage(Class)`.
pub fn native_class_for_name_defining(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let result = native_class_for_name(ctx, args)?;
    let result = for_name_no_init_package_step(ctx, args, result)?;
    if cratonvm_native_api::unretire::pure_bytecode() {
        if let Some(Value::Object(Some(mirror))) = result {
            define_builtin_loader_package(ctx, mirror);
        }
    }
    Ok(result)
}

/// `Class.forName(n, false, loader)` hands the class out unlinked, and the VM
/// runs the JDK's `defineOrCheckPackage` at link: run it now for an
/// application class-path class, so a sealing violation is this call's
/// `SecurityException`, as HotSpot raises it at the define
/// (`NativeContext::check_class_path_package_unlinked`; interpreter round i4
/// wave 92, lane L5; probe `L5W92ForNameSealedNoInit`). With `initialize`
/// the class was linked by [`native_class_for_name`], which ran the step.
/// Returns `result` (its mirror re-read after the Java call).
pub fn for_name_no_init_package_step(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    result: Option<Value>,
) -> Result<Option<Value>, MethodCallFailed> {
    let initialize =
        args.len() == 1 || matches!(args.get(1), Some(v) if v.as_int().unwrap_or(0) != 0);
    if initialize {
        return Ok(result);
    }
    let Some(Value::Object(Some(mirror))) = result else {
        return Ok(result);
    };
    let Some(cid) = ctx.class_id_from_mirror(mirror) else {
        return Ok(result);
    };
    let pin = ctx.pin_native_root(mirror);
    let checked = ctx.check_class_path_package_unlinked(cid);
    let mirror = ctx.read_native_pin(pin, mirror);
    ctx.unpin_native_roots(pin);
    checked?;
    Ok(Some(Value::Object(Some(mirror))))
}

pub fn define_builtin_loader_package(ctx: &mut dyn NativeContext, mirror: ObjectRef) {
    if !crate::classloader::builtin_loaders_adopted(crate::classloader::loader_tables(&*ctx))
        || mirror_is_array(ctx, mirror)
    {
        return;
    }
    let pin = ctx.pin_native_root(mirror);
    let loader = native_class_get_class_loader(ctx, &[Value::Object(Some(mirror))]);
    let mirror = ctx.read_native_pin(pin, mirror);
    ctx.unpin_native_roots(pin);
    let Ok(Some(Value::Object(Some(loader)))) = loader else {
        return;
    };
    if crate::classloader::is_user_defined_loader(ctx, loader) {
        return;
    }
    let _ = ctx.invoke_virtual_bytecode_only(
        loader,
        "definePackage",
        "(Ljava/lang/Class;)Ljava/lang/Package;",
        &[Value::Object(Some(mirror))],
    );
}

/// `CRATONVM_BUILTIN_FOR_NAME_RECORD` (performance, default ON, `=0` off --
/// WARNING i7-user-flip-20261022;
/// `LoaderFlags::builtin_for_name_record`). Interpreter round i6 wave 133,
/// lane L5d; page
/// `docs/feature-designs/native-dispatch-architecture.md`.
///
/// Per built-in loader (the application and the platform loader), the class
/// it answered a `Class.forName(name, init, loader)` with, by internal name:
/// the row HotSpot's `SystemDictionary::resolve_instance_class_or_null` files
/// in the loader's dictionary after `load_instance_class` (the loader is
/// then an initiating loader of the answer, JVMS §5.3), and finds on every
/// later `forName` without calling the loader's `loadClass` again.
///
/// Only a SUCCESSFUL answer of the requested name is filed: a miss is asked
/// again every time, as on HotSpot (no negative dictionary rows), so the
/// `ClassNotFoundException` and its text are the loader's own. A row is read
/// back only while the VM's class table still names that class
/// ([`builtin_for_name_recorded`]), so a class replaced under its name (a
/// `--compatible` stand-in upgraded to the real class, a tombstone) is asked
/// again. A class id is never reused. Cleared at [`Self::CAP`] rows per
/// loader (a cleared row costs one more `loadClass`, the switch-off cost).
#[derive(Default)]
pub struct BuiltinForNameRecord {
    /// Index 0: the platform loader; index 1: the application loader
    /// ([`builtin_for_name_kind_of_class`]).
    pub by_loader: [FxHashMap<Box<str>, u32>; 2],
}

impl BuiltinForNameRecord {
    /// Rows kept per loader before that loader's rows are cleared.
    pub const CAP: usize = 1 << 14;

    pub fn get(&self, kind: usize, internal_name: &str) -> Option<u32> {
        self.by_loader.get(kind)?.get(internal_name).copied()
    }

    pub fn insert(&mut self, kind: usize, internal_name: &str, class_id: u32) {
        let Some(rows) = self.by_loader.get_mut(kind) else {
            return;
        };
        if rows.len() >= Self::CAP && !rows.contains_key(internal_name) {
            rows.clear();
        }
        rows.insert(Box::from(internal_name), class_id);
    }
}

/// Which [`BuiltinForNameRecord`] row set a loader of class `class_name`
/// (internal form) uses: `Some(0)` for the JDK's platform loader, `Some(1)`
/// for its application loader, `None` for every other loader -- a
/// `-Djava.system.class.loader` custom system loader, any user-defined
/// loader, a bare `URLClassLoader`. Exactly the two JDK classes: neither can
/// be subclassed outside `jdk.internal.loader.ClassLoaders`, so a loader of
/// either class runs the JDK's `BuiltinClassLoader.loadClass`, whose answer
/// for a name, once given, does not change. Pure.
pub fn builtin_for_name_kind_of_class(class_name: &str) -> Option<usize> {
    match class_name {
        "jdk/internal/loader/ClassLoaders$PlatformClassLoader" => Some(0),
        "jdk/internal/loader/ClassLoaders$AppClassLoader" => Some(1),
        _ => None,
    }
}

/// [`builtin_for_name_kind_of_class`] of `loader`, when
/// `CRATONVM_BUILTIN_FOR_NAME_RECORD` is on and `dotted_name` can be filed
/// (no `/`: the dotted -> internal rewrite would give `a/b` the row of `a.b`;
/// no array descriptor, which `native_class_for_name` answers before any
/// loader). `None` otherwise.
pub fn builtin_for_name_kind(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    dotted_name: &str,
) -> Option<usize> {
    if !cratonvm_types::flags().loader.builtin_for_name_record
        || dotted_name.is_empty()
        || dotted_name.contains('/')
        || dotted_name.starts_with('[')
    {
        return None;
    }
    let class_name = ctx.class_name_arc_of_id(ctx.class_id_of_object(loader))?;
    builtin_for_name_kind_of_class(&class_name)
}

/// The class the built-in loader `kind` answered an earlier `forName` of
/// `internal_name` with, while the VM's class table still names that class
/// under it (`class_id_by_name`; an ambiguous name answers nothing and is
/// asked again). Reads no Java and allocates nothing on the heap.
pub fn builtin_for_name_recorded(
    ctx: &mut dyn NativeContext,
    kind: usize,
    internal_name: &str,
) -> Option<ClassId> {
    let cid = crate::classloader::builtin_for_name_record(&*ctx)?
        .read()
        .get(kind, internal_name)?;
    let cid = ClassId::new(cid);
    let still_named = ctx.class_id_by_name(internal_name) == Some(cid)
        && ctx
            .class_name_arc_of_id(cid)
            .is_some_and(|name| &*name == internal_name)
        && !ctx.is_class_hidden(cid);
    still_named.then_some(cid)
}

/// File the built-in loader `kind`'s answer `cid` for `internal_name`
/// ([`BuiltinForNameRecord`]). The caller has checked that `cid` is a class
/// of that name. The lock is not held across any `ctx` call.
pub fn builtin_for_name_file(
    ctx: &mut dyn NativeContext,
    kind: usize,
    internal_name: &str,
    cid: ClassId,
) {
    if let Some(record) = crate::classloader::builtin_for_name_record(&*ctx) {
        record.write().insert(kind, internal_name, cid.as_u32());
    }
}


/// Is `loader` an instance of exactly the JDK's application or platform
/// loader class (`ClassLoaders$AppClassLoader` / `$PlatformClassLoader`,
/// neither subclassable outside the JDK)? A custom system loader
/// (`-Djava.system.class.loader`) is another class and answers `false`.
pub fn for_name_loader_is_builtin_jdk_class(ctx: &dyn NativeContext, loader: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(loader))
        .is_some_and(|name| {
            matches!(
                &*name,
                "jdk/internal/loader/ClassLoaders$AppClassLoader"
                    | "jdk/internal/loader/ClassLoaders$PlatformClassLoader"
            )
        })
}

/// JIT round 21 lane reflect3 (`CRATONVM_FORNAME_BUILTIN_LOADED_FIRST`, perf,
/// default ON, `=0` off; page
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// `Class.forName(name, init, loader)` under `--real-jdk` with the JDK's
/// application or platform loader, answered from that loader's own
/// `findLoadedClass0` native ([`crate::native_classloader_find_loaded_class`])
/// when it holds a class of the name.
///
/// HotSpot's `forName0` looks the name up in the loader's dictionary first
/// (`SystemDictionary::resolve_instance_class_or_null`) and calls
/// `loadClass` only on a miss. This VM called `loader.loadClass(name)` every
/// time; the JDK body (`BuiltinClassLoader.loadClass` -> `loadClassOrNull`,
/// whose reviewed `Intrinsic` hands an image class to the bytecode through
/// `invoke_virtual_bytecode_only`, i.e. the interpreter) runs
/// `findLoadedClass(cn)` -> `findLoadedClass0` FIRST and returns a hit as it
/// is (with `resolve == false`, as `loadClass(String)` passes). So a hit here
/// is that body's answer, without the per-call interpreted
/// `loadClassOrNull` / `findLoadedClass` frames (570001 each over
/// `R21Reflect2Rows`). [`crate::classloader_real::package_step_for_handed_out_class`]
/// is applied as `bcl_load_class_or_null` applies it to the body's answer.
///
/// Only a name `checkName` accepts (the body's `findLoadedClass` answers null
/// for the others before its native runs) and only a mirror whose class has
/// exactly that name; a miss, an error or anything else returns `None` and
/// the caller runs `loadClass` as before. The loader and the name are pinned
/// across the native (its lazy module define can collect) and returned
/// re-read.
pub fn for_name_builtin_loaded_first(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    name_obj: ObjectRef,
    dotted_name: &str,
    internal_name: &str,
) -> (Option<MethodCallResult>, ObjectRef, ObjectRef) {
    if !crate::nbflags().forname_builtin_loaded_first
        || !ctx.is_jdk_only()
        || dotted_name.is_empty()
        || !crate::classloader_real::find_loaded_class_name_is_legal(dotted_name)
        || !for_name_loader_is_builtin_jdk_class(&*ctx, loader)
    {
        return (None, loader, name_obj);
    }
    let loader_pin = ctx.pin_native_root(loader);
    let name_pin = ctx.pin_native_root(name_obj);
    let found = crate::native_classloader_find_loaded_class(
        ctx,
        &[Value::Object(Some(loader)), Value::Object(Some(name_obj))],
    );
    let loader = ctx.read_native_pin(loader_pin, loader);
    let name_obj = ctx.read_native_pin(name_pin, name_obj);
    ctx.unpin_native_roots(loader_pin);
    let Ok(Some(Value::Object(Some(mirror)))) = found else {
        return (None, loader, name_obj);
    };
    let named = ctx
        .class_id_from_mirror(mirror)
        .and_then(|cid| ctx.class_name_arc_of_id(cid))
        .is_some_and(|name| &*name == internal_name);
    if !named {
        return (None, loader, name_obj);
    }
    let answer = crate::classloader_real::package_step_for_handed_out_class(
        ctx,
        Ok(Some(Value::Object(Some(mirror)))),
    );
    (Some(answer), loader, name_obj)
}

pub fn native_class_for_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                // HotSpot's is a bare NPE with no message.
                message: None,
            }
            .into());
        }
    };
    let dotted_name = ctx.read_string(name_obj).unwrap_or_default();
    // `Class.c` `forName0`: a name that is not a valid JVM class name after
    // the dot-to-slash fix (`VerifyClassname`) is refused with the FIXED name,
    // `Class.forName("java.lang.String[]")` -> `ClassNotFoundException:
    // java/lang/String[]` (interpreter round i4 wave 98, lane L9;
    // `L9W98JdkApiTexts` row `forName-array-syntax`).
    if let Some(fixed) = for_name_invalid_jvm_name(&dotted_name) {
        return Err(cratonvm_types::error::RuntimeError::ClassNotFoundException {
            class_name: fixed,
        }
        .into());
    }
    // Security hardening вЂ” screen the requested name for control bytes and
    // path-traversal separators before it reaches any class loader. See
    // `validate_for_name_dotted`.
    validate_for_name_dotted(&dotted_name)?;
    let internal_name = dotted_name.replace('.', "/");

    // AN ARRAY DESCRIPTOR is resolved HERE, without consulting any loader.
    //
    // `Class.forName("[I")` and `Class.forName("[Ljava.lang.String;")` are
    // specified to work; `ClassLoader.loadClass("[I")` is specified to throw
    // `ClassNotFoundException`. Both are measured against HotSpot 25.0.3+9 and
    // both probes now assert them (`ClassShadowSweep`, `ClassLoaderShadowSweep`).
    //
    // This VM implemented `forName` by delegating to `loader.loadClass(name)`,
    // which meant the two doors could not disagree -- so adding the binary-name
    // refusal that `loadClass` owes turned three passing `forName` rows red.
    // Handling the descriptor form before the delegation is what lets each door
    // keep its own contract.
    //
    // Two builds were spent learning that the guard could not live in the
    // shared base OR at the `loadClass` entry, because `forName` reaches both.
    // The asymmetry is a property of the two APIs, not of one call site.
    if internal_name.starts_with('[') {
        // `--real-jdk`, an explicit user-defined loader, a reference element
        // outside `java/` (which no user loader may define): the element is
        // the loader's answer, as HotSpot's `SystemDictionary` resolves an
        // array name (JVMS §5.3.3). The lookup below ignores the loader.
        // Interpreter round i1 wave 41, lane L5; probe
        // `tools/probes/interp/L5/L5W41ForNameArrayLoader.java`.
        // An explicit null loader is the bootstrap loader, for an array's
        // element too (JVMS §5.3.3): the non-array arm's rule below, which
        // the array arm used to bypass, so `Class.forName("[Lapp.X;", false,
        // null)` answered the application's array. HotSpot names the array,
        // in internal form (measured on JDK 25). Both modes: a bootstrap-only
        // lookup that sees application classes is a bug in either (wave 41,
        // lane L5; probe `tools/probes/interp/L5/L5W41ForNameArrayBootstrap.java`).
        if matches!(args.get(2), Some(Value::Object(None))) {
            if let Some(element) =
                cratonvm_classloading::array_descriptor_element_class(&internal_name)
            {
                if !crate::classloader::is_bootstrap_class_name(element)
                    && !ctx.is_bootstrap_appended_class(element)
                {
                    let exc = crate::jboss_module_loader::alloc_single_message_exception(
                        ctx,
                        "java/lang/ClassNotFoundException",
                        1,
                        &internal_name,
                    );
                    return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                        exc?,
                    ));
                }
            }
        }
        // Both modes since wave 47 (lane L5): `--compatible` is held to
        // HotSpot too, and answered the array from the global store, never
        // asking the loader (`L5W41ForNameArrayLoader`, every row `app
        // asked=0`).
        {
            if let (Some(Value::Object(Some(loader))), Some(element)) = (
                args.get(2),
                cratonvm_classloading::array_descriptor_element_class(&internal_name)
                    .filter(|e| !e.starts_with("java/")),
            ) {
                let loader = *loader;
                let element = element.to_string();
                if crate::classloader::is_user_defined_loader(ctx, loader) {
                    return for_name_array_through_loader(ctx, loader, &internal_name, &element);
                }
            }
        }
        if let Some(cid) = ctx
            .class_id_by_name(&internal_name)
            .or_else(|| ctx.ensure_class_initialized(&internal_name).ok())
        {
            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
        }
        // THE MESSAGE NAMES THE ELEMENT, NOT THE DESCRIPTOR.
        //
        // MEASURED on JDK 25, and asserted by two vectors that were already in
        // the tree before this arm existed:
        //
        //   Class.forName("[Lp.X;")   ->  CNFE msg="p.X"   cause=null
        //   Class.forName("[[Lp.X;")  ->  CNFE msg="p.X"   cause=null
        //
        // `regression-suite/src/RExceptions.java:382` and
        // `RJdkFailure.java:168` both check exactly this, and both turned red
        // when this arm was added with `&dotted_name` -- the descriptor -- as
        // the message. HotSpot resolves the descriptor down to the element
        // class and reports the resolution that actually failed, which is the
        // name a caller can act on: `[Lp.X;` is not a name anything can be
        // asked for again.
        //
        // Through `for_name_cnfe_name`, which is exactly this rule and is what
        // the two loader arms further down already call. Two lanes fixed this
        // on the same day and the other one re-derived the extraction inline;
        // a third copy of a rule that already has a named home is how the
        // multi-dimension case (`[[Lp.X;`) ends up handled in two places and
        // then in one. The helper delegates to
        // `cratonvm_classloading::array_descriptor_element_class`, which strips
        // every dimension and returns `None` -- so the caller's own name
        // survives -- for `[I` and for malformed spellings.
        //
        // A name that is NOT a well-formed array descriptor (`[Ljava.lang.String`
        // with no `;`, `[[`) is refused by the VM itself, which names it in
        // INTERNAL form (measured on JDK 25: `Class.forName("[Ljava.lang.String")`
        // -> `ClassNotFoundException: [Ljava/lang/String`; probe
        // `tools/probes/interp/L5/L5W44LoaderReview.java`, `forname-bad-array`;
        // interpreter round i1 wave 44, lane L5). `for_name_cnfe_name` answers
        // the caller's dotted spelling for those.
        let message = match cratonvm_classloading::array_descriptor_element_class(&dotted_name) {
            Some(element) => element.to_string(),
            None => internal_name.clone(),
        };
        // Interpreter round i2 wave 53, lane L3
        // (`interpreter-L3-forname-of-an-array-of-a-missing-class-lists-no-loader-frames-FIXED-20261017.md`):
        // HotSpot loads a reference element through the loader, so the miss
        // is the loader's own, with its `loadClass` frames above `forName0`.
        // For a built-in loader that is the served miss a plain
        // `forName("Nope")` raises: build it the same way, so the capture is
        // told which loader missed (`load_class_miss_exception_of`, the
        // served-loader-miss hint). A malformed name (no element) is the VM's
        // own refusal, as before.
        let miss_loader = cratonvm_classloading::array_descriptor_element_class(&internal_name)
            .and_then(|_| match args.get(2) {
                Some(Value::Object(Some(loader))) => Some(*loader),
                None if args.len() == 1 => class_for_name_one_arg_caller_loader(ctx),
                _ => None,
            })
            .filter(|&loader| !crate::classloader::is_user_defined_loader(ctx, loader));
        let exc = match miss_loader {
            Some(loader) => {
                let loader_class = ctx.class_id_of_object(loader);
                crate::classloader_real::load_class_miss_exception_of(ctx, loader_class, &message)
            }
            None => crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/ClassNotFoundException",
                1,
                &message,
            ),
        };
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            exc?,
        ));
    }

    // BUG-06 вЂ” mark this thread as inside a reflective class-existence probe for
    // the duration of resolution. The class loader consults this flag and
    // refuses to fabricate a synthetic enterprise-framework stub (org/jboss/,
    // io/smallrye/, вЂ¦) for a class that is not actually on the classpath, so
    // `Class.forName` / `ClassUtils.isPresent` correctly report it absent (CNFE)
    // instead of a false positive. The guard clears on every return path,
    // including the wf7 entry-class synthesis below (which is unaffected: it
    // runs only after `loadClass` itself yields null/CNFE).
    let _probe_guard = cratonvm_types::reflective_probe::ProbeGuard::new();

    // RKC16r23 вЂ” jboss-logging i18n localized-logger lookup short-circuit.
    //
    // jboss-logging's `Messages.getBundle` / `LoggerProviders.doGetMessageLogger`
    // walks a fallback chain of `<FQCN>_$logger_<lang>_<country>`,
    // `<FQCN>_$logger_<lang>`, `<FQCN>_$logger` for every i18n logger lookup.
    // The first two almost never exist (no app ships per-locale generated
    // classes); they're spec'd to throw CNFE so the chain tries the next.
    //
    // Inside CratonVM, each `loadClass` round-trip for a missing class is
    // expensive: ModuleClassLoader's `loadClass` walks the dependency graph,
    // tries every resource-root JAR, fails, throws CNFE. For Keycloak this
    // happens on every WARN/ERROR log call вЂ” turning what should be a no-op
    // cached negative into a tight loop of class-loader walks that visibly
    // dominates boot time.
    //
    // Real OpenJDK has the same CNFE cost in principle but jboss-logging's
    // own `Messages` infrastructure caches the resolved bundle Class per
    // `Class<?>` key, so the chain only walks once per bundle interface. In
    // our run the cache lookup isn't hitting (likely the `Messages` static
    // field is being re-clinit'd or the WeakReference clears) вЂ” and rather
    // than reverse-engineer jboss-logging's internals, we short-circuit the
    // negative answer at the VM boundary: any name matching the
    // `<...>_$logger_<lang>(_<country>)?` suffix where the suffix's
    // language and (optional) country tokens look like locale codes
    // returns CNFE immediately without calling out to loader.loadClass.
    if i18n_logger_locale_suffix(&dotted_name) {
        return Err(
            cratonvm_types::error::RuntimeError::ClassNotFoundException {
                class_name: dotted_name,
            }
            .into(),
        );
    }

    let effective_loader = match args.get(2) {
        Some(Value::Object(Some(loader))) => {
            let initialize = matches!(args.get(1), Some(v) if v.as_int().unwrap_or(0) != 0);
            Some((*loader, initialize))
        }
        None if args.len() == 1 => class_for_name_one_arg_caller_loader(ctx).map(|loader| {
            // Class.forName(String) is caller-sensitive and initializes the
            // resolved class.
            (loader, true)        }),
        _ => None,
    };
    // The three/four-argument Class.forName overload treats an explicit null
    // loader as the bootstrap loader. This is observably different from
    // having no loader routing available: our flat class store must not make
    // application classes visible through a bootstrap-only lookup.
    // Keep this check ahead of every loader/global fallback (including the
    // Spring Boot nested-JAR rescue below). Those fallbacks are appropriate
    // only after a non-null application loader participated in resolution.
    let explicit_bootstrap_loader = matches!(args.get(2), Some(Value::Object(None)));
    // A null loader normally restricts Class.forName to JDK bootstrap classes.
    // Instrumentation.appendToBootstrapClassLoaderSearch deliberately extends
    // that namespace at runtime, however: Mockito injects its
    // MockMethodDispatcher into such a JAR and immediately resolves it via
    // Class.forName(name, false, null).  Rejecting it before the class manager
    // sees the appended search path makes a successfully appended class appear
    // absent.  The recorded set is populated only from the actual appended
    // JAR, so it does not widen bootstrap visibility to application classes.
    // A `java/` name of a PLATFORM or APPLICATION module of the image
    // (`java.sql.Date`, `com.sun.tools.javac.Main`) is not the bootstrap
    // loader's either, though the prefix predicate admits it (interpreter
    // round i2 wave 54, lane L5; `image_class_outside_boot_modules`).
    let outside_boot_modules = explicit_bootstrap_loader
        && crate::classloader::is_bootstrap_class_name(&internal_name)
        && crate::classloader::image_class_outside_boot_modules(ctx, &internal_name);
    if explicit_bootstrap_loader
        && !ctx.is_bootstrap_appended_class(&internal_name)
        && (!crate::classloader::is_bootstrap_class_name(&internal_name) || outside_boot_modules)
    {
        // A class DEFINED into the boot loader at run time (a null-loader
        // `Unsafe.defineClass`, a bootstrap lookup's `Lookup.defineClass`:
        // `bootstrap_namespace` defines, round 15 wave 6 PX8-1/PX8-2) is in
        // HotSpot's boot dictionary, so `forName(name, init, null)` finds it;
        // neither predicate above knows it (it is not in the image and not
        // on an appended search path), so it was refused (w6a:
        // `R15Loader6LookupBootDefine` `forName false`,
        // `R15Loader6UnsafeNullLoader` `forName-boot false`). Round 15 wave 7,
        // lane drainA; switch `CRATONVM_FORNAME_BOOT_NAMESPACE_DEFINES`
        // (default on, read only when such a class exists). Not for a
        // platform / application module's image class: this VM tags every
        // image class it loaded with the boot loader's id, so the rescue
        // would answer `java.sql.Date` once anything had loaded it.
        // An image class of a BOOT module the prefix predicate refuses
        // (`sun/reflect/misc/Trampoline`, which `MethodUtil` also defines in
        // its own loader) is the bootstrap loader's on HotSpot: answer the
        // BOOTSTRAP namespace's copy, never another loader's same-named
        // class (interpreter round i7 wave 136, lane L5;
        // `boot_image_namespace_class`).
        let boot_cid = if outside_boot_modules {
            None
        } else {
            match runtime_boot_namespace_class(ctx, &internal_name) {
                Some(cid) => Some(cid),
                None => boot_image_namespace_class(ctx, &internal_name)?,
            }
        };
        if let Some(cid) = boot_cid {
            let mut mirror = ctx.get_class_mirror(cid);
            if matches!(args.get(1), Some(v) if v.as_int().unwrap_or(0) != 0) {
                // `<clinit>` is a GC point: return the re-read mirror.
                let mirror_pin = ctx.pin_native_root(mirror);
                let init = ctx.initialize_class(cid);
                mirror = ctx.read_native_pin(mirror_pin, mirror);
                ctx.unpin_native_roots(mirror_pin);
                init?;
            }
            return Ok(Some(Value::Object(Some(mirror))));
        }
        // The VM, not a loader's Java, refuses here: HotSpot's message is the
        // INTERNAL name (`p/q/X`, measured on JDK 25; it was the dotted one).
        // Interpreter round i1 wave 41, lane L5; probe
        // `L5W41ForNameArrayBootstrap`, `missing` row.
        return Err(
            cratonvm_types::error::RuntimeError::ClassNotFoundException {
                class_name: internal_name,
            }
            .into(),
        );
    }
    // Interpreter round i7 wave 138, lane L5 (`CRATONVM_BOOT_LOOKUP_EXACT`):
    // a name several loaders define (`sun/reflect/misc/Trampoline`, now that
    // `is_bootstrap_class_name` no longer refuses it, once `MethodUtil` has
    // defined its own copy) answers the BOOTSTRAP loader's own class when it
    // has one; the by-name routes below cannot pick between the copies. A
    // unique or absent name keeps the routes below unchanged.
    if explicit_bootstrap_loader
        && !outside_boot_modules
        && crate::boot_lookup_exact_enabled()
        && matches!(
            ctx.classify_class_name(&internal_name),
            cratonvm_native_api::NameLookup::Ambiguous { .. }
        )
    {
        if let Some(cid) = ctx
            .bootstrap_class_id_exact(&internal_name)
            .filter(|&cid| !ctx.is_class_hidden(cid))
        {
            let mut mirror = ctx.get_class_mirror(cid);
            if matches!(args.get(1), Some(v) if v.as_int().unwrap_or(0) != 0) {
                // `<clinit>` is a GC point: return the re-read mirror.
                let mirror_pin = ctx.pin_native_root(mirror);
                let init = ctx.initialize_class(cid);
                mirror = ctx.read_native_pin(mirror_pin, mirror);
                ctx.unpin_native_roots(mirror_pin);
                init?;
            }
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }
    // The initialize flag belongs to the explicit-null-loader overload too.
    // Loading a bootstrap class with initialize=false must not run clinit.
    let bootstrap_initialize = matches!(args.get(1), Some(v) if v.as_int().unwrap_or(0) != 0);
    if crate::nbflags().forname_trace {
        eprintln!(
            "[FORNAME-TRACE] name={} args.len()={} args={:?} effective_loader_is_some={}",
            dotted_name,
            args.len(),
            args,
            effective_loader.is_some()
        );
        if let Some((loader, _init)) = effective_loader {
            let lcid = ctx.class_id_of_object(loader);
            let lname = ctx.class_name_of_id(lcid).unwrap_or_default();
            eprintln!(
                "[FORNAME-TRACE] loader_class={} loader_obj={:?}",
                lname, loader
            );
        }
    }

    // RKC16N.12 вЂ” when `Class.forName` is invoked with an explicit non-null
    // classloader, route through `loader.loadClass(name)` so module-scoped
    // loaders (notably `org.jboss.modules.ModuleClassLoader`) get their
    // visibility-closure search. Without this, JBoss Modules' boot path
    // (`Module.run` в†’ `Class.forName(mainClass, false, mcl)` at PC 40 of
    // `Module.run(String,String[])`) bypasses the module's resource-roots
    // and the bootstrap classpath has no entry for module-private classes
    // like `org.jboss.as.server.Main` вЂ” so KC16 boot dies with CNFE before
    // reaching `Main.main`.
    //
    // Argument layout per JDK 25 `Class.forName0(String,boolean,ClassLoader,Class)`:
    //   args[0] = name (String, already read above)
    //   args[1] = initialize (boolean)
    //   args[2] = loader (ClassLoader, may be null = bootstrap)
    //   args[3] = caller (Class, ignored by us)
    // HIB-CV-40: the public one-arg overload is caller-sensitive too. When
    // native dispatch reaches us directly with only the name argument, derive
    // the caller's loader from the current Java frames and route through the
    // same loader.loadClass path. A global fallback can otherwise see classes
    // defined only by sibling/child loaders; BeanShell's second interpreter
    // then reuses the first interpreter's generated MyMessenger class and
    // Spring's prototype script casts fail.
    if let Some((loader, initialize)) = effective_loader {
        // Groovy CGLIB-proxy fix (2026-07-19): `Class.forName(name, init,
        // loader)` on real HotSpot consults the JVM's internal "already
        // defined by exactly this loader" table BEFORE ever calling
        // `loader.loadClass(name)` -- it does not rely on the loader's own
        // bytecode to rediscover a class the JVM itself already knows that
        // loader defined. Groovy's `GroovyClassLoader$InnerLoader` breaks
        // that assumption for us: its `loadClass(String)` override
        // unconditionally forwards to the *outer* shared `GroovyClassLoader`
        // (`return delegate.loadClass(name);`), never re-consulting its own
        // namespace. A CGLIB proxy generated via `Lookup.defineClass` against
        // a Groovy-compiled class is defined under the *InnerLoader's* own
        // namespace (correctly), but the immediately-following
        // `Class.forName(proxyName, true, innerLoader)` — cglib's
        // `ReflectUtils.defineClass`'s final "force <clinit>" call, with no
        // surrounding try/catch — dispatched straight to
        // `invoke_virtual(loader, "loadClass", ...)` and so never looked at
        // InnerLoader's own namespace either, instead riding whatever the
        // outer loader's `findClass` happened to resolve. The outer loader's
        // own delegation (`cl_load_class_base_delegation` ->
        // `class_id_by_name_and_loader` -> `find_class_by_name_in_loader`'s
        // parent-chain fallback) lands on the loader-blind, ambiguity-aware
        // `find_class_by_name`, which "worked" only by accident while a
        // single Groovy-compiled class of that generated name existed
        // process-wide -- and correctly refused to guess (returning a miss)
        // the moment a SECOND Groovy compile unit (a second test method, a
        // second `GroovyClassLoader$InnerLoader`) defined its own distinct
        // class under the exact same generated name, surfacing as a genuine
        // `ClassNotFoundException` inside cglib's un-caught final line —
        // `AopConfigException: Could not generate CGLIB subclass ...`
        // (`GroovyAspectTests`/`GroovyAspectIntegrationTests`,
        // `GroovyScriptFactoryTests::proxyTargetClassNotAllowedIfNotGroovy`).
        // Checking the *exact* loader's own namespace first — the same
        // authoritative relation `inherit_lookup_loader` already used to
        // place the class there in the first place — resolves it directly,
        // matching HotSpot's semantics and bypassing the loader's own
        // (here, misleading) `loadClass` bytecode entirely when the answer
        // is already known.
        if crate::classloader::is_user_defined_loader(ctx, loader) {
            // `--real-jdk`: the loader's INITIATING record too — the class it
            // answered an earlier VM-initiated request with (the resolution
            // door's, or a `forName`'s, `note_initiating_load` below). HotSpot's
            // dictionary lookup finds both and does not call `loadClass` again
            // (JVMS §5.3; interpreter round i1 wave 41, lane L5; probe
            // `L5W41ForNameInitiatedRecord`). `None` in the other mode.
            // `loader_dictionary_mirror`: the same look-up the shared door
            // repeats under a non-parallel-capable loader's monitor (wave 130,
            // lane L7).
            if let Some(mirror) =
                crate::classloader_real::loader_dictionary_mirror(ctx, loader, &internal_name)
            {
                let mut mirror = mirror;
                if initialize {
                    if let Some(cid) = ctx.class_id_from_mirror(mirror) {
                        // gen r4w3/rooting: `initialize_class` runs <clinit>
                        // (GC point) and mirrors move; return the re-read one.
                        let mirror_pin = ctx.pin_native_root(mirror);
                        let init = ctx.initialize_class(cid);
                        mirror = ctx.read_native_pin(mirror_pin, mirror);
                        ctx.unpin_native_roots(mirror_pin);
                        init?;
                    }
                }
                return Ok(Some(Value::Object(Some(mirror))));
            }
        }
        // `CRATONVM_BUILTIN_FOR_NAME_RECORD` (performance, default OFF;
        // interpreter round i6 wave 133, lane L5d; page
        // `i133-L2-typevariable-getannotations-is-16-to-19x-hotspot-xint`): the
        // application or platform loader's dictionary row. HotSpot's
        // `SystemDictionary` finds a name the loader already answered a
        // `forName` for and runs no Java; this VM ran the loader's
        // `loadClass` (`BuiltinClassLoader.loadClassOrNull`, the module
        // lookup and the parent delegation) on every call. The row is filed
        // below from the loader's own answer, so a hit is that answer, the
        // initialization and the JDK-name note made as after `loadClass`.
        let builtin_kind = builtin_for_name_kind(ctx, loader, &dotted_name);
        if let Some(cid) =
            builtin_kind.and_then(|kind| builtin_for_name_recorded(ctx, kind, &internal_name))
        {
            if ctx.jdk_names_initiated_armed() {
                let loader_id = if builtin_kind == Some(1) {
                    cratonvm_types::ClassLoaderId::Application
                } else {
                    cratonvm_types::ClassLoaderId::Extension
                };
                ctx.note_jdk_class_initiated(loader_id.to_native_id(), cid);
            }
            let mut mirror = ctx.get_class_mirror(cid);
            if initialize {
                // `<clinit>` is a GC point: return the re-read mirror.
                let mirror_pin = ctx.pin_native_root(mirror);
                let init = ctx.initialize_class(cid);
                mirror = ctx.read_native_pin(mirror_pin, mirror);
                ctx.unpin_native_roots(mirror_pin);
                init?;
            }
            return Ok(Some(Value::Object(Some(mirror))));
        }
        let loader_class_name_debug = {
            let cid = ctx.class_id_of_object(loader);
            ctx.class_name_of_id(cid).unwrap_or_default()
        };
        s111_dbg!(
            "[S111-DBG] Class.forName({}) loader={}",
            dotted_name,
            loader_class_name_debug
        );
        // Spring's DynamicClassLoader deliberately delegates generated classes
        // to its forked parent. In real-JDK mode the inherited ClassLoader
        // bytecode can collapse that lookup to the flat global store before
        // the parent's loader-local namespace participates. Invoke the parent
        // directly for this known delegating loader so Class.forName observes
        // the same class identity as the generated AOT code.
        let lookup_loader = if loader_class_name_debug
            == "org/springframework/core/test/tools/DynamicClassLoader"
        {
            // Only reroute when the parent is the FORKED test loader: in that
            // configuration DynamicClassLoader's constructor defines every
            // generated class into the fork (defineDynamicClass), so the
            // fork's namespace is authoritative. A plain per-compile
            // DynamicClassLoader (parent = app/test loader) defines classes
            // ITSELF — both its lazily-defined generated classes and classes
            // third parties (CGLIB's ReflectUtils.defineClass fallback) push
            // into it — and rerouting to the app parent hid those,
            // CNFE-failing the Class.forName(name, true, loader) that CGLIB
            // issues right after a successful define.
            let parent_is_fork = match ctx.get_field_by_name(loader, "parent") {
                Value::Object(Some(parent)) => {
                    let pid = ctx.class_id_of_object(parent);
                    ctx.class_name_of_id(pid).is_some_and(|n| {
                        n == "org/springframework/core/test/tools/CompileWithForkedClassLoaderClassLoader"
                    })
                }
                _ => false,
            };
            if parent_is_fork {
                match ctx.get_field_by_name(loader, "parent") {
                    Value::Object(Some(parent)) => parent,
                    _ => loader,
                }
            } else {
                loader
            }
        } else {
            loader
        };
        // `loader` initiates this load. HotSpot's `Class.forName` goes through
        // `SystemDictionary`, which checks the JVMS §5.3.4 loader constraints
        // recorded for it (`check_constraints`) and records it as an
        // initiating loader of the answer (§5.3), as the interpreter's
        // resolution door does (`NativeContext::note_initiating_load`). Its
        // namespace id is taken now (assigned if the loader never defined a
        // class, as its first define would; ids come from a counter): the Java
        // call below can move `loader`. A built-in loader's id is below 3 and
        // takes neither. `--real-jdk` only (and `--compatible` with the
        // initiating records on, wave 48). Interpreter round i1 wave 38, lane
        // L5.
        let initiating_ns = if ctx.initiating_records_on() {
            crate::classloader::peek_loader_namespace_id(ctx, loader)
                .or_else(|| Some(crate::classloader::loader_namespace_id(ctx, loader)))
                .filter(|&ns| ns >= 3)
        } else {
            None
        };
        // The loader, for the record of the JDK classes it initiates
        // (`Instrumentation.getInitiatedClasses`): HotSpot files the class
        // `forName` answers in the loader's dictionary whatever the loader --
        // the application and platform loaders too, and in either mode. A
        // user loader under `--real-jdk` is recorded by
        // `note_initiating_load` above. Taken now, as `initiating_ns` is: the
        // call below can move `loader`. Interpreter round i1 wave 47, lane L5.
        let jdk_record_loader = if initiating_ns.is_none() && ctx.jdk_names_initiated_armed() {
            if crate::classloader::is_application_class_loader(&*ctx, loader) {
                Some(cratonvm_types::ClassLoaderId::Application.to_native_id())
            } else if crate::classloader::is_platform_class_loader(&*ctx, loader) {
                Some(cratonvm_types::ClassLoaderId::Extension.to_native_id())
            } else {
                crate::classloader::peek_loader_namespace_id(ctx, loader).filter(|&ns| ns >= 3)
            }
        } else {
            None
        };
        // Under the loader's monitor when it is not parallel-capable, as
        // HotSpot's `SystemDictionary` takes it for `Class.forName`
        // (`--real-jdk`; interpreter round i1 wave 29, lane L5; i25-L5 racing
        // resolution page). Under `CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK`
        // (default ON) the door also re-checks the dictionary under that
        // monitor and publishes this initiating load before releasing it, so
        // racing `forName` calls ask the loader once, as on HotSpot
        // (`invoke_load_class_as_the_vm_publishing`; wave 130, lane L7, probe
        // `L7W130ForNameRace`). `published`: the note below was made there.
        // JIT round 21 lane reflect3 (`CRATONVM_FORNAME_BUILTIN_LOADED_FIRST`):
        // a built-in loader's `findLoadedClass0` hit is the answer its
        // `loadClass` body returns first; see `for_name_builtin_loaded_first`.
        // Such a hit runs no Java and publishes nothing (`published` false).
        let (loaded_first, lookup_loader, name_obj) = if lookup_loader.as_ptr() == loader.as_ptr() {
            for_name_builtin_loaded_first(
                ctx,
                lookup_loader,
                name_obj,
                &dotted_name,
                &internal_name,
            )
        } else {
            (None, lookup_loader, name_obj)
        };
        let (loaded, published) = match loaded_first {
            Some(answer) => (answer, false),
            None => {
                // zt pl4 item 1: the VM's upcall to the loader's `loadClass` is
                // HotSpot's `CLASS_LOAD` event (`sun.cls.appClassLoadTime` / `.self` /
                // `appClassLoadCount`), made only for a name the loader's dictionary
                // does not hold yet (its `findLoadedClass` answer, the same lookup):
                // HotSpot's `SystemDictionary` finds such a name and runs no Java.
                // The lookup can run Java (the module tables' first read): the two
                // references the call below takes are re-read through pins.
                let mut lookup_loader = lookup_loader;
                let mut name_obj = name_obj;
                let class_load = match ctx.class_trace_counters() {
                    Some(counters) => {
                        let loader_pin = ctx.pin_native_root(lookup_loader);
                        let name_pin = ctx.pin_native_root(name_obj);
                        let held = crate::classloader::find_loaded_class_for_loader_java(
                            ctx,
                            lookup_loader,
                            &internal_name,
                        );
                        let loader_now = ctx.read_native_pin(loader_pin, lookup_loader);
                        let held = crate::classloader::find_loaded_answer_for_java(
                            ctx,
                            loader_now,
                            &internal_name,
                            held,
                        )
                        .is_some();
                        lookup_loader = ctx.read_native_pin(loader_pin, lookup_loader);
                        name_obj = ctx.read_native_pin(name_pin, name_obj);
                        ctx.unpin_native_roots(loader_pin);
                        (!held).then(|| {
                            cratonvm_types::perf_class_trace::ClassTraceTime::begin(
                                Some(counters),
                                cratonvm_types::perf_class_trace::ClassTraceEvent::ClassLoad,
                            )
                        })
                    }
                    None => None,
                };
                let answer = crate::classloader_real::invoke_load_class_as_the_vm_publishing(
                    ctx,
                    lookup_loader,
                    name_obj,
                    initiating_ns,
                );
                drop(class_load);
                answer
            }
        };
        match loaded {
            Ok(Some(mut mirror)) => {
                // HotSpot's `SystemDictionary` takes only a class of the
                // requested name from the loader (JVMS §5.3.2): `null`, a
                // primitive mirror or a class of another name is
                // `ClassNotFoundException` naming the INTERNAL name, measured on
                // JDK 25 (interpreter round i1 wave 39, lane L5, `--real-jdk`;
                // both modes since wave 47, `--compatible` being held to
                // HotSpot too; probe `L5W39ForNameWrongAnswer`). Before,
                // `forName` returned the `null` or the other class. The mirror
                // is not used again on the refusal, so the exception's
                // allocation is safe.
                {
                    let answered = match mirror {
                        Value::Object(Some(mirror_ref)) => ctx
                            .class_id_from_mirror(mirror_ref)
                            .and_then(|cid| ctx.class_name_of_id(cid))
                            .is_some_and(|n| n == internal_name),
                        _ => false,
                    };
                    if !answered {
                        let exc = crate::jboss_module_loader::alloc_single_message_exception(
                            ctx,
                            "java/lang/ClassNotFoundException",
                            1,
                            &internal_name,
                        );
                        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                            exc?,
                        ));
                    }
                }
                // The loaded-first Class.forName path can bypass the Java
                // loader body entirely. HotSpot's built-in loader still has
                // its package map populated when it returns a platform image
                // class, so cover that fast path as well as loadClass's cold
                // return door.
                if let Value::Object(Some(mirror_ref)) = mirror {
                    if let Some(cid) = ctx.class_id_from_mirror(mirror_ref) {
                        crate::classloader_real::sync_builtin_loader_package_for_loaded_class(
                            ctx,
                            lookup_loader,
                            cid,
                        )?;
                        mirror = Value::Object(Some(ctx.get_class_mirror(cid)));
                    }
                }
                // `CRATONVM_BUILTIN_FOR_NAME_RECORD`: file the built-in
                // loader's answer (a class of the requested name, checked
                // just above) for the next `forName` (wave 133, lane L5d).
                if let (Some(kind), Value::Object(Some(mirror_ref))) = (builtin_kind, mirror) {
                    if let Some(cid) = ctx.class_id_from_mirror(mirror_ref) {
                        builtin_for_name_file(ctx, kind, &internal_name, cid);
                    }
                }
                if let (Some(ns), Value::Object(Some(mirror_ref)), false) =
                    (initiating_ns, mirror, published)
                {
                    if let Some(cid) = ctx.class_id_from_mirror(mirror_ref) {
                        // Allocates only when it refuses (the `LinkageError`),
                        // and then `mirror` is not used again.
                        ctx.note_initiating_load(ns, &internal_name, cid)?;
                    }
                }
                if let (Some(loader_id), Value::Object(Some(mirror_ref))) =
                    (jdk_record_loader, mirror)
                {
                    if let Some(cid) = ctx.class_id_from_mirror(mirror_ref) {
                        ctx.note_jdk_class_initiated(loader_id, cid);
                    }
                }
                s111_dbg!(
                    "[S111-DBG] loadClass({}) succeeded via invoke_virtual",
                    dotted_name
                );
                if s111_dbg_enabled()
                    && (dotted_name.contains("ConditionalOnMissingBean")
                        || dotted_name
                            .contains("DataSourceAutoConfiguration$PooledDataSourceConfiguration"))
                {
                    if let Value::Object(Some(mirror_ref)) = mirror {
                        let cid = ctx.class_id_from_mirror(mirror_ref);
                        let defining_loader = cid.and_then(|id| {
                            crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), id.as_u32())
                        });
                        eprintln!(
                            "[S111-DBG] loadClass({}) mirror={:?} cid={:?} defining_loader={:?}",
                            dotted_name, mirror_ref, cid, defining_loader
                        );
                    }
                }
                // Root-cause-2 fix (WildFly parallel-extension-add CCE family,
                // wildfly-remoting-classcastexception-parallel-extension-add-FIXED.md): `mirror` -- the `Class` object
                // `invoke_virtual` just handed back from recursively
                // interpreting `loadClass`'s real bytecode -- is a bare Rust
                // local at this point, not yet on any interpreter operand
                // stack and not yet in `native_pending_return` (that only
                // gets set by the OUTERMOST `safe_native_call` wrapper once
                // this whole native call -- `Class.forName` -- finally
                // returns). `ctx.initialize_class(cid)` below runs the
                // class's `<clinit>`, arbitrary GC-capable bytecode, and the
                // pre-clinit `mirror` was being returned unrefreshed
                // afterward -- the classic Family-1 stale-ObjectRef shape.
                // Live-captured via `EnhancedQueueExecutor$ThreadBody`
                // worker threads racing this exact window during
                // `parallel-extension-add`: `Class.asSubclass()` (jboss-
                // logging's `Messages.doGetBundle`, loading a generated
                // `*_$bundle` message-bundle impl class) read back a mirror
                // with every field blank (`class_id_from_mirror`/field-0/
                // field-1 all miss), producing `ClassCastException: class `
                // (`this.toString()`'s name empty) -- reproduced identically
                // under `CRATONVM_DISABLE_JIT=1`, ruling out a JIT-specific
                // mechanism. Pin `mirror_ref` across `initialize_class` and
                // refresh before every later use, including the final
                // return.
                let mut mirror = mirror;
                let mirror_pin = if let Value::Object(Some(mirror_ref)) = mirror {
                    Some(ctx.pin_native_root(mirror_ref))
                } else {
                    None
                };
                // HIB-CV-26 вЂ” honour the `initialize` flag (args[1]).
                // `ClassLoader.loadClass` only loads + links the class; it does
                // NOT run static initialisers. The JDK contract for
                // `Class.forName(name, true, loader)` is that the class IS
                // initialised before the call returns (this is exactly the
                // distinction HHH-7272 relies on: `loadClass` skips `<clinit>`,
                // `Class.forName(.., true, ..)` runs it вЂ” so a JDBC driver's
                // self-registering static block fires). Without this, drivers
                // loaded via the 3-arg overload never register and
                // `DriverManager.getDriver` throws "No suitable driver".
                if initialize {
                    if let Value::Object(Some(mirror_ref)) = mirror {
                        if let Some(cid) = ctx.class_id_from_mirror(mirror_ref) {
                            // `loadClass` has already resolved this exact class through
                            // the requested loader.  Initializing it by name again loses
                            // that loader identity and falls back to the flat application
                            // store, which cannot see a freshly defined CGLIB proxy in a
                            // filtered/user loader.  Initialize the resolved ClassId
                            // directly, as Class.forName0 does on the JVM.
                            //
                            // HIB-CV-26 fix (2026-07-16): `initialize_class`
                            // now returns the real `MethodCallFailed` instead
                            // of a flattened `String`, so a `<clinit>`
                            // exception propagates as-is here — already
                            // wrapped as a catchable
                            // `ExceptionInInitializerError`/
                            // `NoClassDefFoundError` by
                            // `ensure_class_initialized_shared` per JVMS
                            // §5.5. Previously every nested clinit failure
                            // (e.g. `MemorySegment.<clinit>` throwing
                            // `IllegalCallerException` without
                            // `--enable-native-access`) was re-wrapped as an
                            // unrecoverable `VmError::Internal` here, which
                            // aborted the whole VM instead of letting Java
                            // code catch the exception.
                            let init_result = ctx.initialize_class(cid);
                            if let Some(pin) = mirror_pin {
                                mirror = Value::Object(Some(ctx.read_native_pin(pin, mirror_ref)));
                            }
                            init_result?;
                        }
                    }
                }
                if crate::nbflags().iae_trace_ok && dbg_is_entity_name(&internal_name) {
                    if let Value::Object(Some(mr)) = mirror {
                        if let Some(cid) = ctx.class_id_from_mirror(mr) {
                            let enh = dbg_class_enhanced(ctx, cid);
                            eprintln!("FORNAME-RET name={dotted_name} loader={loader_class_name_debug} cid={} enhanced={enh} (via-loader)", cid.as_u32());
                        }
                    }
                }
                if let Some(pin) = mirror_pin {
                    ctx.unpin_native_roots(pin);
                }
                return Ok(Some(mirror));
            }
            // ClassLoader.loadClass returning null is technically illegal
            // (per spec it must throw CNFE) but defensively translate it.
            Ok(None) => {
                s111_dbg!("[S111-DBG] loadClass({}) returned null", dotted_name);
                // WF7 вЂ” see the matching block at the bottom of this
                // function for the rationale: synthesise an entry-class
                // stub when the module loader yields nothing for a name
                // we recognise as a WildFly / Keycloak boot entry.
                if let Some(mirror) = wf7_synthesise_entry_class_if_missing(ctx, &internal_name) {
                    tracing::warn!(
                        target: "wf7",
                        "[wf-shim] Class.forName synthesised stub for {} (loader returned null)",
                        internal_name
                    );
                    return Ok(Some(Value::Object(Some(mirror))));
                }
                // L16 -- for an array descriptor HotSpot's `Class.forName`
                // names the ELEMENT, never the descriptor. See
                // `for_name_cnfe_name`.
                return Err(
                    cratonvm_types::error::RuntimeError::ClassNotFoundException {
                        class_name: for_name_cnfe_name(&dotted_name),
                    }
                    .into(),
                );
            }
            // S111r12 вЂ” NSME on `loader.loadClass` rescue. Spring Boot 2's
            // SB2 launcher path delivers a `LaunchedURLClassLoader`
            // instance whose `class_id_of` returns `java/lang/Comparable`
            // (the loader inherited that stub class_id somewhere in the
            // boot chain). The receiver-driven virtual dispatch then
            // resolves to `Comparable.loadClass` and raises NSME.
            // Fall back to bootstrap-style class loading so the
            // `Class.forName(name, init, loader)` chain still resolves.
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Linkage(
                    cratonvm_types::error::LinkageError::NoSuchMethodError { .. },
                ),
            )) => {
                s111_dbg!(
                    "[S111-DBG] loadClass({}) -> NoSuchMethodError, fallback",
                    dotted_name
                );
                // Fall through to bootstrap-style ensure_class_initialized below.
            }
            // S111r20 вЂ” Spring Boot 2.x LaunchedURLClassLoader.loadClass
            // fails in CratonVM because the JDK bytecode for URLClassPath
            // walks nested-JAR URLs (jar:file:/fat.jar!/BOOT-INF/lib/foo.jar!/)
            // via JarURLConnection + the Spring Boot custom jar: Handler. These
            // lower-level primitives are not wired up in CratonVM's real-JDK
            // mode, so loadClass throws ClassNotFoundException (Java-level) or
            // fails internally. In both cases, fall through to bootstrap-style
            // ensure_class_initialized, which uses CratonVM's built-in classpath
            // scanner (which already extracted the nested JARs from BOOT-INF/lib).
            //
            // We do NOT apply this rescue to module-aware class loaders like
            // org.jboss.modules.ModuleClassLoader вЂ” for those, CNFE is the
            // authoritative answer about module visibility. Detect Spring Boot's
            // LaunchedURLClassLoader by class name prefix.
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc_ref)) => {
                // Only the Spring Boot launcher itself needs the nested-JAR
                // rescue. A general URLClassLoader can deliberately reject a
                // class (notably Spring's FilteredClassLoader); treating that
                // ClassNotFoundException as a global lookup makes conditional
                // auto-configuration observe classes the requested loader hid.
                let loader_class_name = loader_class_name_debug.clone();
                let is_launched_url_cl = loader_class_name
                    == "org/springframework/boot/loader/LaunchedURLClassLoader"
                    || loader_class_name
                        == "org/springframework/boot/loader/launch/LaunchedURLClassLoader";
                if is_launched_url_cl {
                    s111_dbg!(
                        "[S111-DBG] loadClass({}) -> ExceptionThrown for LaunchedURLCL, fallback",
                        dotted_name
                    );
                    // Fall through to ensure_class_initialized below.
                } else {
                    // For module-scoped loaders (JBoss Modules, OSGi, etc.)
                    // the CNFE is authoritative вЂ” propagate it.
                    // WF7 вЂ” but FIRST try entry-class synthesis: jboss-modules'
                    // `ModuleClassLoader.loadClass("org.jboss.as.server.Main")`
                    // fails because the module's resource-roots don't list a
                    // real `Main` (KC16 + WF deliver it via a different path
                    // CratonVM doesn't reproduce). Synthesising a stub here is
                    // the whole point of WF7 вЂ” let the bootstrap finish.
                    if let Some(mirror) = wf7_synthesise_entry_class_if_missing(ctx, &internal_name)
                    {
                        tracing::warn!(
                            target: "wf7",
                            "[wf-shim] Class.forName synthesised stub for {} (module loader CNFE)",
                            internal_name
                        );
                        return Ok(Some(Value::Object(Some(mirror))));
                    }
                    // The loader threw a Java exception вЂ” propagate it as-is.
                    // For a class-load failure it already names the missing
                    // class (CNFE/NCDFE), and for any other failure type the
                    // raw exception is more informative than a synthesized
                    // CNFE(dotted_name). Matches HotSpot's behaviour.
                    s111_dbg!("[S111-DBG] loadClass({}) threw, propagating", dotted_name);
                    // L16 -- one correction before propagating. We handed the
                    // loader an array DESCRIPTOR, so a `ClassNotFoundException`
                    // it raises names the descriptor; HotSpot's `Class.forName`
                    // never shows a loader an array form at all and reports the
                    // element. Everything else, including the
                    // `NoClassDefFoundError` a genuinely-missing dependency
                    // produces, is handed back untouched.
                    return Err(for_name_rename_array_cnfe(
                        ctx,
                        &dotted_name,
                        cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc_ref),
                    ));
                }
            }
            // Propagate internal VM errors without re-wrapping.
            Err(e) => {
                s111_dbg!(
                    "[S111-DBG] loadClass({}) -> InternalError {:?}, propagating",
                    dotted_name,
                    e
                );
                return Err(e);
            }
        }
        s111_dbg!(
            "[S111-DBG] falling through to ensure_class_initialized({})",
            dotted_name
        );
    }

    let global_resolution: Result<(ClassId, ObjectRef), MethodCallFailed> =
        if explicit_bootstrap_loader && !bootstrap_initialize {
            // load_class deliberately stops after load/link; unlike
            // ensure_class_initialized, it preserves forName(false, null).
            match ctx.load_class(&internal_name) {
                Ok(Some(Value::Object(Some(mirror)))) => match ctx.class_id_from_mirror(mirror) {
                    Some(class_id) => Ok((class_id, mirror)),
                    None => Err(
                        cratonvm_types::error::RuntimeError::ClassNotFoundException {
                            class_name: dotted_name.clone(),
                        }
                        .into(),
                    ),
                },
                Ok(_) => Err(
                    cratonvm_types::error::RuntimeError::ClassNotFoundException {
                        class_name: dotted_name.clone(),
                    }
                    .into(),
                ),
                Err(e) => Err(e),
            }
        } else {
            match ctx.ensure_class_initialized(&internal_name) {
                Ok(class_id) => Ok((class_id, ctx.get_class_mirror(class_id))),
                Err(e) => Err(e),
            }
        };

    match global_resolution {
        Ok((class_id, mirror)) => {
            // WP2.10 вЂ” JDK 25 spec: hidden classes (created via
            // Lookup.defineHiddenClass) are NOT discoverable by name.
            // `Class.forName` must throw ClassNotFoundException for them
            // even though they are loaded.
            if ctx.is_class_hidden(class_id) {
                return Err(
                    cratonvm_types::error::RuntimeError::ClassNotFoundException {
                        class_name: dotted_name,
                    }
                    .into(),
                );
            }
            if crate::nbflags().iae_trace_ok && dbg_is_entity_name(&internal_name) {
                let enh = dbg_class_enhanced(ctx, class_id);
                eprintln!(
                    "FORNAME-RET name={dotted_name} cid={} enhanced={enh} (global-fallback)",
                    class_id.as_u32()
                );
            }
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(e) => {
            if let cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc_ref) = &e {
                let exc_cid = ctx.class_id_of_object(*exc_ref);
                let exc_class = ctx.class_name_of_id(exc_cid).unwrap_or_default();
                let msg = match ctx.get_field(*exc_ref, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                s111_dbg!(
                    "[FORNAME-ERR] name={} exc_class={} msg={}",
                    dotted_name,
                    exc_class,
                    msg
                );
            } else {
                s111_dbg!("[FORNAME-ERR] name={} err={:?}", dotted_name, e);
            }
            // WF7 вЂ” last-ditch: if this is a known WildFly/Keycloak entry
            // class, synthesise a minimal stub class with a no-op `main` so
            // jboss-modules' bootstrap (`Class.forName(mainClass, false, mcl)`
            // followed by `getDeclaredMethod("main", String[].class).invoke`)
            // can complete instead of dying on NSME. See WF7 strategy notes
            // at the top of this module for the rationale вЂ” the WF6
            // `getDeclaredMethod` shim never fires because the class
            // *appears* loaded by the time `getDeclaredMethod` runs (it just
            // doesn't have a `main`); by injecting a real class earlier we
            // sidestep that path entirely.
            if let Some(mirror) = wf7_synthesise_entry_class_if_missing(ctx, &internal_name) {
                tracing::warn!(
                    target: "wf7",
                    "[wf-shim] Class.forName synthesised stub for {}",
                    internal_name
                );
                return Ok(Some(Value::Object(Some(mirror))));
            }
            // If a Java exception was raised during load/link/init, it is the
            // authoritative diagnostic вЂ” propagate it unchanged. JVMS lets
            // `Class.forName` surface the underlying `LinkageError` (commonly
            // `NoClassDefFoundError` naming the missing transitive class), and
            // HotSpot does exactly that. The previous behaviour of synthesizing
            // a fresh `ClassNotFoundException(dotted_name)` discarded the
            // actually-missing class name and forced users to guess at the
            // classpath gap.
            if matches!(
                e,
                cratonvm_types::error::MethodCallFailed::ExceptionThrown(_)
            ) {
                // L16 -- same array-descriptor correction as the loader arm
                // above; a no-op for every non-array name and for every
                // throwable that is not a `ClassNotFoundException`.
                return Err(for_name_rename_array_cnfe(ctx, &dotted_name, e));
            }
            // The define's own `LinkageError` (a boot class path class's
            // `ClassFormatError`, a wrong name's `NoClassDefFoundError`) is
            // HotSpot's answer too: `forName(name, false, null)` threw
            // `ClassNotFoundException` for a malformed class on
            // `-Xbootclasspath/a:` (t1 wave 13, lane I24W, probe
            // `T1W13I24WStructuralFormat.java` `boot` rows).
            if matches!(
                e,
                cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::Linkage(_)
                )
            ) {
                return Err(e);
            }
            // No Java exception was raised вЂ” the class file simply could not
            // be located on any source on the classpath. Throw `CNFE(dotted)`.
            Err(
                cratonvm_types::error::RuntimeError::ClassNotFoundException {
                    // L16 -- the element, not the descriptor, for an array
                    // form. See `for_name_cnfe_name`.
                    class_name: for_name_cnfe_name(&dotted_name),
                }
                .into(),
            )
        }
    }
}

pub fn native_class_for_name_3(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Same as forName(String) but ignores initialize flag and classLoader.
    //
    // Security hardening вЂ” screen the requested name here as well, BEFORE any
    // class loader sees it. Delegation to `native_class_for_name` already
    // re-runs `validate_for_name_dotted`, but performing the check up-front
    // keeps the guarantee local to this entry point and resilient to future
    // changes in how the two overloads share code. (`read_string` on a null
    // name yields the empty string, which `validate_for_name_dotted` rejects;
    // a genuinely null `name` argument is still surfaced as the
    // NullPointerException by the delegate below.)
    if let Some(Value::Object(Some(name_obj))) = args.first() {
        let dotted_name = ctx.read_string(*name_obj).unwrap_or_default();
        validate_for_name_dotted(&dotted_name)?;
    }
    let result = native_class_for_name(ctx, args)?;
    // The `forName(n, false, loader)` package step (interpreter round i4
    // wave 92, lane L5): `--compatible`'s door for the three-argument
    // overload is this native, not the JDK body that reaches `forName0`.
    for_name_no_init_package_step(ctx, args, result)
}

// T19_H12_FORNAME_MODULE вЂ” `Class.forName(Module, String)` per JDK 25 spec.
//
// JDK's stock bytecode for this overload resolves `module.getClassLoader()`
// then invokes `cl.loadClass(module, name)`. Our synthetic `java.lang.Module`
// objects (built by `phases_late.rs::ModuleLayer.modules` and
// `jboss_jdkspecific.rs::build_module`) have a 5-slot layout where slot 2
// is `packages: Set<String>` (a `HashSet` populated with JDK package names).
// On a real JDK 25 `java.lang.Module` class file the field at the
// equivalent offset is `loader: ClassLoader`. The mismatch caused JDK
// bytecode to dispatch `loadClass(Module, String)` against the HashSet
// receiver and surface a `NoSuchMethodError: HashSet.loadClass(...)`.
//
// Bypassing the JDK bytecode and resolving directly through the bootstrap
// loader is correct: every Module our VM hands out for platform classes
// (java.base, java.logging, java.management, ...) is in the boot layer
// and would resolve via the bootstrap `BootLoader.loadClass(module, name)`
// branch in real JDK anyway.
//
// Per JEP 261 / Class.forName(Module, String) javadoc:
//   - If `module` is null в†’ NullPointerException
//   - If `name` is null в†’ NullPointerException
//   - If the class is not found in the module в†’ return null (NOT throw CNFE)
//   - Hidden classes are not discoverable
pub fn native_class_for_name_module(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Module, args[1] = String name
    match args.first() {
        Some(Value::Object(Some(_))) => {}
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    let name_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                // HotSpot's is a bare NPE with no message.
                message: None,
            }
            .into());
        }
    };
    let dotted_name = ctx.read_string(name_obj).unwrap_or_default();
    if dotted_name.is_empty() {
        // JDK rejects empty names by returning null per Class.forName(Module, String) spec.
        return Ok(Some(Value::Object(None)));
    }
    // Hardening: reject names that look like a class-load injection.
    // Names with NUL, control bytes, or path separators are never valid
    // class names and would let an attacker probe the filesystem.
    if dotted_name.bytes().any(|b| b < 0x20 || b == 0x7F)
        || dotted_name.contains('/')
        || dotted_name.contains('\\')
    {
        return Ok(Some(Value::Object(None)));
    }
    // A module defined to a user-defined loader (a module of a non-boot
    // `ModuleLayer`; both modes since wave 48, lane L6) is searched as the
    // JDK does,
    // `module.getClassLoader().loadClass(module, name)`: that loader's loaded
    // class or its `findClass(moduleName, name)`, answered only when the
    // class is a member of `module`, and never initialized. The flat lookup
    // below knows no module and initializes (interpreter round i1 wave 43,
    // lane L5, `L5W43UserLayerModule`).
    if let Some(Value::Object(Some(module))) = args.first().copied() {
        if let Value::Object(Some(loader)) = ctx.get_field_by_name(module, "loader") {
            if crate::classloader::is_user_defined_loader(ctx, loader) {
                return ctx.invoke_virtual(
                    loader,
                    "loadClass",
                    "(Ljava/lang/Module;Ljava/lang/String;)Ljava/lang/Class;",
                    &[Value::Object(Some(module)), Value::Object(Some(name_obj))],
                );
            }
        }
    }
    // An ARRAY descriptor is never found through a module-scoped lookup: the JDK
    // body asks the module's loader for a class NAMED `[I`, which no loader
    // defines, so `Class.forName(module, "[I")` answers null (measured against
    // HotSpot 25, `SbrW6Module` row `class.forName.2.module.array`).
    if dotted_name.starts_with('[') {
        return Ok(Some(Value::Object(None)));
    }
    let internal_name = dotted_name.replace('.', "/");
    // The module is compared with the found class's below; the flat lookup
    // can allocate (a moving collection), so it is pinned across it.
    let module_pin = match args.first() {
        Some(Value::Object(Some(module))) => Some((ctx.pin_native_root(*module), *module)),
        _ => None,
    };
    // Loaded, not initialized -- "This method does not ... run the class
    // initializer" (the JDK's `loadClass(Module, String)` /
    // `BootLoader.loadClass(Module, String)`); the flat lookup ran `<clinit>`
    // (wave 43, lane L5, row `forname-noinit`, `--real-jdk`; both modes since
    // wave 47, `--compatible` being held to HotSpot too).
    let found = match ctx.load_class(&internal_name) {
        Ok(Some(Value::Object(Some(mirror)))) => ctx.class_id_from_mirror(mirror).ok_or(()),
        _ => Err(()),
    };
    let result = match found {
        Ok(class_id) => for_name_module_member(ctx, class_id, module_pin),
        // Spec: this overload returns null (NOT throws CNFE) on miss.
        Err(_) => Ok(Some(Value::Object(None))),
    };
    if let Some((pin, _)) = module_pin {
        ctx.unpin_native_roots(pin);
    }
    result
}

/// [`native_class_for_name_module`]'s answer for the class the flat lookup
/// found: the class, or `null` when it is not a member of the module.
pub fn for_name_module_member(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    module_pin: Option<(usize, ObjectRef)>,
) -> MethodCallResult {
    // Hidden classes are not discoverable via Class.forName per JEP 371.
    if ctx.is_class_hidden(class_id) {
        return Ok(Some(Value::Object(None)));
    }
    let mirror = ctx.get_class_mirror(class_id);
    // The module here is a built-in loader's (a user loader's was
    // answered above), and no class a user-defined loader defined is
    // a member of it; the flat lookup can still find one by name (a
    // class of a user `ModuleLayer`, wave 43, lane L5; both modes
    // since wave 48, lane L6).
    if let Ok(Some(Value::Object(Some(loader)))) =
        native_class_get_class_loader(ctx, &[Value::Object(Some(mirror))])
    {
        if crate::classloader::is_user_defined_loader(ctx, loader) {
            return Ok(Some(Value::Object(None)));
        }
    }
    // A class of ANOTHER module is not an answer either: the JDK's
    // `loadClass(Module, String)` returns the class only when
    // `c.getModule() == module` (HotSpot:
    // `Class.forName(<java.sql>, "java.lang.String")` is null). The flat
    // lookup knows no module and found `java.lang.String` (interpreter round
    // i2 wave 54, lane L5; `--compatible`, where this native serves the
    // call; probe `L5W54ForNameModuleMember`).
    if let Some((pin, module)) = module_pin {
        let module = ctx.read_native_pin(pin, module);
        let mirror = ctx.get_class_mirror(class_id);
        match ctx.invoke_virtual(mirror, "getModule", "()Ljava/lang/Module;", &[]) {
            Ok(Some(Value::Object(Some(own)))) => {
                let module = ctx.read_native_pin(pin, module);
                if own != module {
                    return Ok(Some(Value::Object(None)));
                }
            }
            Err(e) => return Err(e),
            _ => {}
        }
    }
    Ok(Some(Value::Object(Some(ctx.get_class_mirror(class_id)))))
}

pub fn native_class_is_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        Some(Value::Object(None)) => return Ok(Some(Value::Int(0))),
        _ => return Ok(Some(Value::Int(0))),
    };
    // The array-aware branch runs BEFORE any ClassId resolution, mirroring
    // `native_class_is_assignable_from` below (which always did). It is a
    // purely name/descriptor-based check that does not need `this`'s ClassId --
    // and `mirror_class_id` MISSES for an array mirror handed out by
    // `Method.getReturnType()` under a custom loader, so resolving it first
    // made `isInstance` bail with `false` while `isAssignableFrom` answered
    // `true` for the very same pair. Spring's `AnnotationTypeMapping.adapt`
    // calls `type.isInstance(value)` and rejected an annotation's enum-array
    // value with the self-contradictory "should be compatible with
    // RequestMethod[] but a RequestMethod[] value was returned"
    // (`test.context.aot.*`). `probes/ForkArr2Probe.java`.
    // S111r17 вЂ” Array-aware isInstance.  Heap-stored `class_id_of` for an
    // array returns the COMPONENT class id (e.g. `java/lang/Class` for a
    // `Class[]` array), NOT the synthetic `[Lcomponent;` array class id.
    // Without this branch, `Class[].isInstance(myClassArray)` reduces to
    // `is_subclass(java/lang/Class, [Ljava/lang/Class;)` which is always
    // false вЂ” the same bug that the interpreter's `instanceof` bytecode
    // already works around in `array_descriptor_of` /
    // `array_is_assignable_to`.  Mirror that logic here so reflection
    // callers (Spring's `TypeMappedAnnotation.adapt`, which throws
    // `IllegalArgumentException` when `type.isInstance(value)` returns
    // false for a wrapped attribute array) see consistent results.
    let target_is_array = ctx.heap_kind_of(target) == cratonvm_types::ObjectKind::Array;
    if target_is_array {
        // JIT round 20 rev13b (correctness, unconditional; fuzz
        // `R20FuzzFAssignable1` inst.25 / cast.25): a LAMBDA PROXY mirror has
        // no array instances (a proxy is a final, non-array hidden class on
        // HotSpot). The array's header holds its COMPONENT's id, so for
        // `Array.newInstance(lambda.getClass(), n)` the id tail below compared
        // the proxy's id with itself and answered `true` (and `Class.cast`
        // passed the array through as the proxy): the proxy's mirror has no
        // store name for the named-class guard below to refuse it by.
        if mirror_class_id(ctx, this)
            .is_some_and(|id| id.as_u32() >= 0x8000_0000 && ctx.lambda_proxy_host(id).is_some())
        {
            return Ok(Some(Value::Int(0)));
        }
        // JIT round 20 wave 15 (lane fuzz14d; correctness, unconditional): an
        // array whose COMPONENT is a lambda proxy (`Array.newInstance(lambda
        // .getClass(), n)`, its clone) is answered by the identity verdict in
        // every mode. The proxy is a virtual class with no store name, so the
        // `--compatible` route below (the descriptor rule by NAMES, then the
        // id rescue's `is_subclass`) refused `Runnable[].class.isInstance(
        // proxies)` / `Class.cast` where HotSpot (and `--real-jdk`, through
        // this same verdict) admits it -- `R20FuzzGLambdaArr1` `types.warm`
        // differed under `--compatible` only, on its interpreted calls.
        let target_component = ctx.class_id_of_object(target);
        if ctx.heap_element_type_of(target) == cratonvm_types::ArrayElementType::Reference
            && target_component.as_u32() >= 0x8000_0000
            && ctx.lambda_proxy_host(target_component).is_some()
        {
            if let Some(verdict) = mirror_class_id(ctx, this)
                .and_then(|id| ctx.array_instance_of_array_class(target, id))
            {
                return Ok(Some(Value::Int(i32::from(verdict))));
            }
        }
        // `--real-jdk`: an array mirror with a reference component is answered
        // by the `instanceof` bytecode's own identity verdict. The descriptor
        // rule below compares NAMES, so an array of another loader's
        // same-named class passed `isInstance` (and `Class.cast`) where
        // `instanceof` refuses it (interpreter round i1 wave 29, lane L4;
        // `L4W29ClassReflectionShapes.java`).
        // `--compatible` too with the `NamespacedArrayTypes` row (wave 50,
        // lane L5).
        if ctx.aligned(cratonvm_types::HotSpotAlignment::NamespacedArrayTypes) {
            if let Some(verdict) = mirror_class_id(ctx, this)
                .and_then(|id| ctx.array_instance_of_array_class(target, id))
            {
                if !verdict {
                    // The descriptor rule (the row-off route) would have
                    // admitted it (wave 51, lane L5: the alignment census).
                    let src_desc = array_descriptor_for(ctx, target);
                    let this_name = mirror_class_name(ctx, this).unwrap_or_default();
                    if array_is_assignable(ctx, &src_desc, &this_name) {
                        ctx.note_alignment_divergence(
                            cratonvm_types::HotSpotAlignment::NamespacedArrayTypes,
                        );
                    }
                }
                return Ok(Some(Value::Int(i32::from(verdict))));
            }
        }
        let src_desc = array_descriptor_for(ctx, target);
        let this_name = mirror_class_name(ctx, this).unwrap_or_default();
        if array_is_assignable(ctx, &src_desc, &this_name) {
            return Ok(Some(Value::Int(1)));
        }
        // A NAMED non-array class (`array_is_assignable` already admitted
        // `Object` / `Cloneable` / `Serializable`) has no array instances. The
        // fall-through below must not decide it: `class_id_of_object` of an
        // `H[]` is `H`'s id, so the tail's `target_class_id == this_class_id`
        // answered `H.class.isInstance(new H[1])` TRUE, and `Method.invoke`,
        // which asks this, dispatched `H`'s method on the array (r11w14 lane
        // vh). Only an unnamed or array mirror keeps the fall-through, and so
        // does a DOTTED spelling of the three supertypes (`mirror_class_name`'s
        // slot-1 fallback reads `Class.name`, which is the binary name).
        let supertype = matches!(
            this_name.replace('.', "/").as_str(),
            "java/lang/Object" | "java/lang/Cloneable" | "java/io/Serializable"
        );
        if !this_name.is_empty() && !this_name.starts_with('[') && !supertype {
            return Ok(Some(Value::Int(0)));
        }
        // An ARRAY mirror the descriptor rule refused keeps one rescue -- a
        // component the name check could not prove -- decided on COMPONENTS.
        // The tail below compares the target's id (its COMPONENT's, for an
        // array) with this mirror's own id (the ARRAY class), so a target one
        // dimension deeper matched: `String[].class.isInstance(new
        // String[1][])` answered true and `Class.cast` let a `String[][]`
        // through as a `String[]` (interpreter round i1 wave 29, lane L4;
        // `L4W29ClassReflectionShapes.java`). A
        // primitive-component array class has no component id: the
        // descriptor rule above was its whole answer.
        if this_name.starts_with('[') {
            let target_component = ctx.class_id_of_object(target);
            let admitted = mirror_class_id(ctx, this)
                .and_then(|id| ctx.array_component_class_id(id))
                .is_some_and(|this_component| {
                    this_component != ClassId::new(0)
                        && (target_component == this_component
                            || ctx.is_subclass(target_component, this_component))
                });
            return Ok(Some(Value::Int(i32::from(admitted))));
        }
        // Fall through to legacy id-based check below (covers some
        // edge cases where the target is an array but the type mirror
        // is a non-array Class вЂ” those reduce to assignable-to-Object
        // via `array_is_assignable` already, so this is just defensive).
    }
    let this_class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    // JIT round 20 wave 19, lane zAssign (`CRATONVM_JIT_CLASS_IS_INSTANCE_SUBTYPE_INLINE`):
    // the receiver's mirror hint for the compiled subtype arm (the VM proves
    // `class_mirrors[this_class_id] == this` before writing one).
    if !target_is_array {
        note_reflect_mirror_id(&*ctx, this, this_class_id);
    }
    let target_class_id = ctx.class_id_of_object(target);

    // PERF (webapp-deploy annotation scan): answer the ordinary "the object's
    // class IS, or extends/implements, this mirror's class" shape from class
    // ids alone, before anything below materialises a class NAME.
    //
    // Every `class_name_of_id` / `mirror_class_name` call takes the class-
    // manager read lock and clones the name into a fresh `String`, and the
    // AnnotationProxy and `Proxy$Instance` probes below each do one on EVERY
    // call just to compare against a fixed literal. Tomcat's BCEL scanner
    // reaches this through `ConstantPool.getConstant(int, Class)`, which runs
    // `castTo.isAssignableFrom(...)` + `castTo.cast(...)` once per constant-
    // pool access of every class in every scanned jar: a `--stack-sample-ms`
    // profile of `TestHostConfigAutomaticDeploymentCopyXML` put 55% of all
    // interpreted time in that one method.
    //
    // This is the FIRST TWO DISJUNCTS OF THIS FUNCTION'S OWN TAIL, evaluated
    // earlier. Every branch between here and there returns `1` or falls
    // through -- none returns `0` -- so hoisting a `true` cannot change an
    // answer. Arrays are excluded: `class_id_of_object` reports an array's
    // COMPONENT class id, so the id comparison is not meaningful for them and
    // they keep taking the descriptor-based path above and the tail below.
    if !target_is_array
        && (target_class_id == this_class_id || ctx.is_subclass(target_class_id, this_class_id))
    {
        return Ok(Some(Value::Int(1)));
    }

    // Annotation proxy special case: our `create_annotation_proxy` allocates
    // objects of class `java/lang/annotation/AnnotationProxy`, not of the
    // actual annotation interface. JDK reflection and Spring's
    // `AttributeMethods.assertAnnotation` do `annotationType.isInstance(ann)`
    // which would otherwise return false (proxy class doesn't extend / implement
    // the user-declared annotation interface), driving Spring into
    // `Assert.instanceCheckFailed` and an unrelated NPE during message
    // formatting. Read `ANN_PROXY_TYPE_MIRROR` (slot 1) and treat the proxy
    // as an instance of the recorded annotation type (and any of its
    // super-interfaces, including `java.lang.annotation.Annotation`).
    //
    // NIO2-17-5 rung 4 (JIT round 19 wave 3, lane funnel3;
    // `CRATONVM_CLASS_FACTS_MEMO_PROXY=1`, default OFF): this test and the
    // `Proxy$Instance` one below answer from the per-thread class-facts memo
    // instead of rendering the target's class name twice on every call that
    // reaches them (the false answer of every `isInstance` / `cast` the id
    // test above did not admit). Off: the two name compares, unchanged.
    let target_rung_facts = crate::class_facts_memo::rung_facts(
        ctx,
        crate::class_facts_memo::Rung::Proxy,
        target_class_id,
    );
    let target_is_annotation_proxy = match target_rung_facts {
        Some(facts) => facts.is_some_and(crate::class_facts_memo::NameFacts::is_annotation_proxy),
        None => {
            ctx.class_name_of_id(target_class_id).as_deref()
                == Some("java/lang/annotation/AnnotationProxy")
        }
    };
    if target_is_annotation_proxy {
        if let Value::Object(Some(type_mirror)) = ctx.get_field(target, ANN_PROXY_TYPE_MIRROR) {
            if let Some(ann_cid) = mirror_class_id(ctx, type_mirror) {
                let proxy_matches =
                    ann_cid == this_class_id || ctx.is_subclass(ann_cid, this_class_id);
                if proxy_matches {
                    return Ok(Some(Value::Int(1)));
                }
                // Always treat proxies as instances of `Annotation`
                // itself, even if the type-mirror class graph doesn't
                // record the implements-edge yet.
                if let Some(this_name) = ctx.class_name_of_id(this_class_id) {
                    if this_name == "java/lang/annotation/Annotation" {
                        return Ok(Some(Value::Int(1)));
                    }
                }
            }
        }
    }

    // Dynamic `java.lang.reflect.Proxy` instances: every proxy lands on the
    // single synthetic `java/lang/reflect/Proxy$Instance` class, so the
    // `is_subclass(Proxy$Instance, <iface>)` check below is always false even
    // when the proxy was created implementing `<iface>`. The implemented
    // interfaces are recorded per-instance in the `interfaces` slot (a
    // `Class[]`). Mirror the interpreter's `proxy_instance_satisfies_target`
    // (checkcast/instanceof admission) here so reflective `Class.isInstance` /
    // `Class.cast` agree. Without this, `Class.cast` on a proxy throws a
    // ClassCastException вЂ” e.g. Gradle's logging `ListenerBroadcast.getSource()`
    // proxy cast to `OutputEventListener` killed every test worker.
    let target_is_proxy_instance = match target_rung_facts {
        Some(facts) => facts.is_some_and(crate::class_facts_memo::NameFacts::is_proxy_instance),
        None => {
            ctx.class_name_of_id(target_class_id).as_deref()
                == Some("java/lang/reflect/Proxy$Instance")
        }
    };
    if target_is_proxy_instance {
        // Object / Serializable are always satisfied per the Proxy spec.
        if let Some(this_name) = ctx.class_name_of_id(this_class_id) {
            if this_name == "java/lang/Object" || this_name == "java/io/Serializable" {
                return Ok(Some(Value::Int(1)));
            }
        }
        const PROXY_FIELD_INTERFACES: usize = 1;
        if let Value::Object(Some(ifaces)) = ctx.get_field(target, PROXY_FIELD_INTERFACES) {
            let n = ctx.array_length(ifaces);
            for i in 0..n {
                let mirror = match ctx.get_array_element(ifaces, i) {
                    Value::Object(Some(m)) => m,
                    _ => continue,
                };
                if let Some(iface_cid) = mirror_class_id(ctx, mirror) {
                    if iface_cid == this_class_id || ctx.is_subclass(iface_cid, this_class_id) {
                        return Ok(Some(Value::Int(1)));
                    }
                }
            }
        }
    }

    // STR619-3 (JIT round 19 proposal wave 8, lane str8;
    // `CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_FALSE`, opt-in): the tail below is
    // `loader_aware_reflect_assignable(.., name) || display-class test ||
    // synthetic_implements_declared(.., name)` (the id disjuncts already ran
    // above for a non-array target), and the first and last can be proved
    // false without rendering the name:
    //
    // * `name` is the class store's name of `this_class_id` whenever the mirror
    //   reverse-maps to that id (both arms below read it so), and its memoised
    //   facts say it is not a `synthetic_implements` target -- whose every rule
    //   is keyed on an exact target of the shared table the fact is built from
    //   (`cratonvm_types::synthetic_targets`, proved over the rules' literals
    //   in vm `typecheck`);
    // * the by-name assignability rule is inert: loader-aware resolution is off
    //   (`reflect_assignable_by_name` answers false first), or the
    //   `NamespacedClassTypes` row is on under `--real-jdk` (false, no census).
    //
    // The display-class disjunct still runs. Off, a fact miss, an array target
    // or the debug trace: the tail below, unchanged.
    if !target_is_array
        && !crate::nbflags().dbg_isinstance
        && crate::class_facts_memo::rung_enabled(crate::class_facts_memo::Rung::IsInstanceFalse)
        && ctx.class_id_from_mirror(this) == Some(this_class_id)
        && (!crate::classloader::loader_aware_resolution()
            || (ctx.is_jdk_only()
                && ctx.aligned(cratonvm_types::HotSpotAlignment::NamespacedClassTypes)))
        && crate::class_facts_memo::name_facts(ctx, this_class_id)
            .is_some_and(|facts| !facts.is_synthetic_target())
    {
        if let Some(disp) = crate::getclass_display_class_id(ctx, target_class_id, target) {
            if disp == this_class_id || ctx.is_subclass(disp, this_class_id) {
                return Ok(Some(Value::Int(1)));
            }
        }
        return Ok(Some(Value::Int(0)));
    }

    // FUNNEL3-19-3 (JIT round 19 proposal wave 6, lane str6;
    // `CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_NAME`, opt-in): the very name
    // `mirror_class_name` renders -- the class store's entry for the mirror's
    // reverse-mapped id when it has one (even an absent one), else its
    // fallbacks -- as an `Arc<str>` clone when the id path answers, so the
    // false answer of an ordinary class no longer allocates a `String` per
    // call. Off: the `String`, unchanged.
    let this_name_arc: Option<std::sync::Arc<str>>;
    let this_name_string: String;
    let this_name_for_assignability: &str =
        if crate::class_facts_memo::rung_enabled(crate::class_facts_memo::Rung::IsInstanceName) {
            this_name_arc = match ctx.class_id_from_mirror(this) {
                Some(cid) => ctx.class_name_arc_of_id(cid),
                None => mirror_class_name(ctx, this).map(std::sync::Arc::from),
            }
            .or_else(|| ctx.class_name_arc_of_id(this_class_id));
            this_name_arc.as_deref().unwrap_or("")
        } else {
            this_name_string = mirror_class_name(ctx, this)
                .or_else(|| ctx.class_name_of_id(this_class_id))
                .unwrap_or_default();
            &this_name_string
        };
    if target_class_id == this_class_id
        || ctx.is_subclass(target_class_id, this_class_id)
        || loader_aware_reflect_assignable(
            ctx,
            target_class_id,
            this_class_id,
            this_name_for_assignability,
        )
    {
        return Ok(Some(Value::Int(1)));
    }
    if crate::nbflags().dbg_isinstance {
        let ifaces = ctx.class_interfaces(target_class_id);
        let iface_names: Vec<String> = ifaces
            .iter()
            .map(|&c| format!("{}:{:?}", c.as_u32(), ctx.class_name_of_id(c)))
            .collect();
        eprintln!(
            "[DBG_ISINSTANCE] target_class_id={} ({:?}) this_class_id={} ({:?}) target_interfaces={iface_names:?}",
            target_class_id.as_u32(),
            ctx.class_name_of_id(target_class_id),
            this_class_id.as_u32(),
            ctx.class_name_of_id(this_class_id),
        );
    }

    // Consistency with `getClass()`: synthetic collection wrappers (the
    // `List.of`/`Map.of`/`Collections.unmodifiable*` families) and other
    // JDK-aliased objects carry an internal craton stamp class as their raw
    // header `class_id`, but `Object.getClass()` reports a cosmetic *display*
    // class (e.g. `java.util.ImmutableCollections$List12`) via
    // `getclass_display_class_id`. The internal stamp is not registered as a
    // subtype of that display class, so the raw-id check above returns false
    // for `List12.class.isInstance(List.of("a","b"))` even though
    // `obj.getClass() == List12.class`. `Class.isInstance` must agree with
    // `getClass()`, so also test the object's display class id. Spring's
    // `GenericConversionService.convert` does `sourceType.getObjectType()
    // .isInstance(source)` and throws "Source to convert from must be an
    // instance of [вЂ¦]" when this disagrees вЂ” breaking every conversion whose
    // source is an immutable collection.
    if let Some(disp) = crate::getclass_display_class_id(ctx, target_class_id, target) {
        if disp == this_class_id || ctx.is_subclass(disp, this_class_id) {
            return Ok(Some(Value::Int(1)));
        }
    }
    // LAST, after every hierarchy answer: the relationships the VM DECLARES for
    // the concrete stand-in classes it mints itself. This is the same table the
    // `instanceof` bytecode consults, reached through the same door the
    // reflective array store uses — see
    // `NativeContext::synthetic_implements_declared` for why reflection asking a
    // narrower question than the opcode is a defect and not a conservatism.
    //
    // Measured on `probes/FfmVectorSegmentProbe.java`: an FFM segment that a
    // `checkcast jdk/internal/foreign/AbstractMemorySegmentImpl` had just
    // admitted answered `isInstance` FALSE for the same class.
    if ctx.synthetic_implements_declared(target_class_id, this_name_for_assignability) {
        return Ok(Some(Value::Int(1)));
    }
    Ok(Some(Value::Int(0)))
}

/// S111r17 вЂ” Build the JVMS array descriptor (e.g. `[Ljava/lang/Class;`,
/// `[I`, `[[Ljava/lang/String;`) for an array heap object.  Mirrors the
/// interpreter's `array_descriptor_of` (vm/src/runtime/interpreter.rs)
/// but lives in NativeContext-land so reflection natives can use it.
/// pub: also used by the Object.toString native (lib.rs) so the
/// default `Name@hash` rendering of arrays matches HotSpot
/// (`[Ljava.lang.Class;@вЂ¦`, not the component class name).
#[inline]
pub fn is_global_resolution_namespace(name: &str) -> bool {
    name.starts_with("java/")
        || name.starts_with("javax/")
        || name.starts_with("jdk/")
        || name.starts_with("sun/")
        || name.starts_with("com/sun/")
}

pub fn loader_aware_reflect_assignable(
    ctx: &dyn NativeContext,
    source_class_id: ClassId,
    target_class_id: ClassId,
    target_class_name: &str,
) -> bool {
    // `--real-jdk`: real class bytes are authoritative, so two classes of one
    // binary name in different loaders are two classes (JVMS 5.3.4) and neither
    // is assignable to the other. This rule exists for the default mode's class
    // store, which can lose an edge (a child loader's implementation whose
    // interface it delegated to the parent); under strict mode the edges are
    // real, and the rule only made `Class.isInstance` / `isAssignableFrom` /
    // `cast` / `asSubclass` answer true where HotSpot answers false. The
    // interpreter's `checkcast` / `instanceof` and the JIT carry the same gate
    // (`docs/feature-designs/native-dispatch-architecture.md`
    // `spring-jdkonly-bytecode-cast-name-rule-jit-class-resolution-FIXED-20260923.md`).
    // `--compatible` too with the `NamespacedClassTypes` row (wave 51, lane L4).
    // Under `--compatible` the refusal is counted in the alignment census when
    // the by-name rule would have admitted it (wave 52, lane L4): the walk is
    // the one the row-off route pays on this same path, so the row costs
    // nothing it did not; `--real-jdk` does not pay it.
    if ctx.aligned(cratonvm_types::HotSpotAlignment::NamespacedClassTypes) {
        if !ctx.is_jdk_only()
            && reflect_assignable_by_name(ctx, source_class_id, target_class_id, target_class_name)
        {
            ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::NamespacedClassTypes);
        }
        return false;
    }
    reflect_assignable_by_name(ctx, source_class_id, target_class_id, target_class_name)
}

/// The `--compatible` by-name rule of [`loader_aware_reflect_assignable`]
/// (the `NamespacedClassTypes` row off): a class, superclass or interface of
/// `source_class_id` carrying the target's NAME, whatever its loader.
pub fn reflect_assignable_by_name(
    ctx: &dyn NativeContext,
    source_class_id: ClassId,
    target_class_id: ClassId,
    target_class_name: &str,
) -> bool {
    if !crate::classloader::loader_aware_resolution()
        || target_class_name.is_empty()
        || target_class_name.starts_with('[')
        || is_global_resolution_namespace(target_class_name)
    {
        return false;
    }

    let Some(target_actual_name) = ctx.class_name_of_id(target_class_id) else {
        return false;
    };
    if target_actual_name != target_class_name {
        return false;
    }

    if ctx.class_name_arc_of_id(source_class_id).as_deref() == Some(target_class_name) {
        return true;
    }

    let mut queue = Vec::new();
    let mut current = Some(source_class_id);
    while let Some(class_id) = current {
        // A class defined by a user loader can extend that loader's copy of a
        // superclass while the reflective Method mirror still carries the
        // global copy.  The ids legitimately differ, but the superclass edge
        // is an exact loader-resolved relation.  Treat the matching binary
        // name as assignable just as the same-name receiver case above does.
        if ctx.class_name_arc_of_id(class_id).as_deref() == Some(target_class_name) {
            return true;
        }
        queue.extend(ctx.class_interfaces(class_id));
        current = ctx.superclass_of(class_id);
    }

    if !ctx.is_interface_class(target_class_id) {
        return false;
    }

    let mut seen = Vec::new();
    while let Some(iface_id) = queue.pop() {
        if seen.contains(&iface_id) {
            continue;
        }
        seen.push(iface_id);
        if ctx.class_name_arc_of_id(iface_id).as_deref() == Some(target_class_name) {
            return true;
        }
        queue.extend(ctx.class_interfaces(iface_id));
    }

    false
}

pub fn array_descriptor_for(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
) -> String {
    use cratonvm_types::ArrayElementType;
    let et = ctx.heap_element_type_of(obj);
    match et {
        ArrayElementType::Boolean => "[Z".to_string(),
        ArrayElementType::Char => "[C".to_string(),
        ArrayElementType::Float => "[F".to_string(),
        ArrayElementType::Double => "[D".to_string(),
        ArrayElementType::Byte => "[B".to_string(),
        ArrayElementType::Short => "[S".to_string(),
        ArrayElementType::Int => "[I".to_string(),
        ArrayElementType::Long => "[J".to_string(),
        ArrayElementType::Reference => {
            let comp_id = ctx.class_id_of_object(obj);
            let comp_name = ctx.class_name_of_id(comp_id).unwrap_or_default();
            if comp_name.is_empty() {
                "[Ljava/lang/Object;".to_string()
            } else if comp_name.starts_with('[') {
                format!("[{}", comp_name)
            } else {
                format!("[L{};", comp_name)
            }
        }
    }
}

/// S111r17 вЂ” Recursive array assignability check, mirroring the
/// interpreter's `array_is_assignable_to`.  `target_name` is a
/// "raw" class name (slash-separated, no `L...;` wrapping for
/// non-array types) вЂ” matches what `mirror_class_name` returns.
pub fn array_is_assignable(ctx: &dyn NativeContext, src_desc: &str, target_name: &str) -> bool {
    if target_name == "java/lang/Object"
        || target_name == "java/io/Serializable"
        || target_name == "java/lang/Cloneable"
    {
        return true;
    }
    if !target_name.starts_with('[') {
        return false;
    }
    if src_desc == target_name {
        return true;
    }
    let src_rest = &src_desc[1..];
    let tgt_rest = &target_name[1..];
    if src_rest.len() == 1 && "ZCBSIJFD".contains(&src_rest[..1]) {
        return src_rest == tgt_rest;
    }
    if tgt_rest.len() == 1 && "ZCBSIJFD".contains(&tgt_rest[..1]) {
        return false;
    }
    let extract = |desc: &str| -> Option<(bool, String)> {
        if desc.starts_with('[') {
            Some((true, desc.to_string()))
        } else if desc.starts_with('L') && desc.ends_with(';') {
            Some((false, desc[1..desc.len() - 1].to_string()))
        } else {
            None
        }
    };
    let (src_is_arr, src_comp) = match extract(src_rest) {
        Some(x) => x,
        None => return false,
    };
    let (tgt_is_arr, tgt_comp) = match extract(tgt_rest) {
        Some(x) => x,
        None => return false,
    };
    if src_is_arr && tgt_is_arr {
        return array_is_assignable(ctx, &src_comp, &tgt_comp);
    }
    if src_is_arr != tgt_is_arr {
        if !src_is_arr {
            return false;
        }
        return tgt_comp == "java/lang/Object"
            || tgt_comp == "java/io/Serializable"
            || tgt_comp == "java/lang/Cloneable";
    }
    // Covariance is directional: `Object[]` is assignable to `T[]` ONLY when
    // `T` is itself `Object`. The previous unconditional `return true` here
    // made `Integer[].isAssignableFrom(Object[])` true (it must be false),
    // which defeated the JDK's most-specific-return-type selection in
    // `Class.getMethod`: comparing a covariant bridge `Object[] getValues()`
    // against the real `Integer[] getValues()`, BOTH directions reported
    // assignable, so neither was deemed more specific and the synthetic bridge
    // could win (BridgeMethodResolverTests.findBridgedMethodWithвЂ¦). The
    // forward direction (`Object[]` IS assignable from a subtype array like
    // `Integer[]`) is handled by the `is_subclass` check below; this mirrors
    // the interpreter's strict (non-lenient) `array_is_assignable_to_impl`.
    if src_comp == "java/lang/Object" {
        return tgt_comp == "java/lang/Object";
    }
    // Every reference component widens to `Object` (JLS 10.10), whatever
    // loader defined it. Without this, a component name two loaders define
    // missed `class_id_by_name` below (an ambiguous name answers `None`), so
    // `Object[].class.isAssignableFrom(X[].class)` was `false` (interpreter
    // round i1 wave 31; `L4W31CrossLoaderArrayStore.java`).
    if tgt_comp == "java/lang/Object" {
        return true;
    }
    let src_id = match ctx.class_id_by_name(&src_comp) {
        Some(id) => id,
        None => return false,
    };
    let tgt_id = match ctx.class_id_by_name(&tgt_comp) {
        Some(id) => id,
        None => return false,
    };
    src_id == tgt_id || ctx.is_subclass(src_id, tgt_id)}

// bytebuddy_probe stack-overflow guard (agent11).
//
// `Class.isAssignableFrom` is a hot path during ByteBuddy / Spring
// reflective bootstrap, and a few of the helpers it transitively touches
// (`array_is_assignable`, `is_subclass`) themselves consult class mirrors
// that may re-enter this function for component-type checks. A pathological
// type graph (or a circular array-component cycle observed under
// ByteBuddy's `JavaDispatcher` proxy validation) can drive recursion deep
// enough to blow the Rust thread stack.
//
// Mirror the depth-guard pattern from `lang_reflect::native_method_invoke_boxed`:
// bail out with `Value::Int(0)` (i.e. "not assignable") if we re-enter this
// function past a conservative ceiling. Returning false-but-Ok is safer than
// throwing here because callers (`Assert.isAssignable` in Spring,
// `JavaDispatcher` in ByteBuddy) treat a thrown error as a fatal class-init
// failure, while a `false` return is a recoverable runtime decision.
thread_local! {
    pub static IS_ASSIGNABLE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub fn native_class_is_assignable_from(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Reentrancy / recursion guard вЂ” see comment on IS_ASSIGNABLE_DEPTH.
    let prev_depth = IS_ASSIGNABLE_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    if prev_depth > 100 {
        IS_ASSIGNABLE_DEPTH.with(|d| d.set(prev_depth));
        // Bail safe: report not-assignable rather than throwing, so callers
        // that are mid-bootstrap don't see this as a hard failure.
        return Ok(Some(Value::Int(0)));
    }
    // zr z11/nd: restored on the unwind too (see `DepthRestore`).
    let _depth_restore = DepthRestore(&IS_ASSIGNABLE_DEPTH, prev_depth);
    // RAII-style depth restore around the original body.
    let result = (|| -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(obj))) => *obj,
            _ => return Ok(Some(Value::Int(0))),
        };
        let other = match args.get(1) {
            Some(Value::Object(Some(obj))) => *obj,
            _ => {
                // HotSpot's native raises it with no message (interpreter
                // round i2 wave 53, lane L3; probe `L3W53ReflectionNullArgs`);
                // its `(Native Method)` frame is the funnel's mark's.
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into());
            }
        };

        // S111r17 вЂ” Array-aware isAssignableFrom.  Same root cause as
        // `native_class_is_instance` above: when `this` represents an array
        // Class (descriptor like `[Lfoo;` recoverable from the mirror's
        // name field) and `other` represents an array Class, the simple
        // `is_subclass` walk doesn't traverse JVM-level array covariance
        // (e.g. `String[] -> Object[]`, `AnnotationProxy[] ->
        // Annotation[]`).  Reuse the same descriptor / assignability
        // helpers we added for `isInstance`.  Spring 5.x's
        // `Assert.isAssignable(supertype, subtype)` is the visible caller
        // вЂ” it throws `IllegalArgumentException` from
        // `assignableCheckFailed` when this returns false, masking the
        // underlying type-system gap.
        // PERF (webapp-deploy annotation scan): same hoist as
        // `native_class_is_instance` above, and the same reason -- the two
        // `mirror_class_name` calls immediately below each take the class-
        // manager read lock and clone the name into a fresh `String`, on a path
        // Tomcat's BCEL scanner walks once per constant-pool access
        // (`ConstantPool.getConstant(int, Class)` -> `castTo.isAssignableFrom`).
        //
        // These are the first two disjuncts of this function's own tail. Every
        // branch between here and there returns `1` or falls through, and the
        // two `mirror_class_id` `None` arms cannot be reached once BOTH ids
        // have resolved -- so hoisting a `true` cannot change an answer.
        // Skipped when the ObservationRegistry trace is armed, so that
        // diagnostic still sees every call.
        if !crate::vmflags().loader.dbg_obsreg {
            if let (Some(this_cid), Some(other_cid)) =
                (mirror_class_id(ctx, this), mirror_class_id(ctx, other))
            {
                if this_cid == other_cid || ctx.is_subclass(other_cid, this_cid) {
                    return Ok(Some(Value::Int(1)));
                }
            }
        }

        let this_name = mirror_class_name(ctx, this).unwrap_or_default();
        let other_name = mirror_class_name(ctx, other).unwrap_or_default();
        if crate::vmflags().loader.dbg_obsreg
            && (this_name.contains("ObservationRegistry")
                || other_name.contains("ObservationRegistry"))
        {
            let this_cid = mirror_class_id(ctx, this);
            let other_cid = mirror_class_id(ctx, other);
            eprintln!(
                "[OBSREG-DBG] isAssignableFrom this={:?}({} cid={:?}) other={:?}({} cid={:?})",
                this, this_name, this_cid, other, other_name, other_cid
            );
        }
        // `--real-jdk`: two array mirrors are judged on their components'
        // identities, as `checkcast` judges them; the descriptor rule below
        // compares NAMES, so an array of another loader's same-named class was
        // reported assignable (interpreter round i1 wave 31,
        // `interpreter-L4-cross-loader-type-checks-and-trace-shapes-FIXED`, item 2).
        // `--compatible` too with the `NamespacedArrayTypes` row (wave 50,
        // lane L5).
        if this_name.starts_with('[')
            && other_name.starts_with('[')
            && ctx.aligned(cratonvm_types::HotSpotAlignment::NamespacedArrayTypes)
        {
            if let Some(verdict) = mirror_class_id(ctx, other)
                .zip(mirror_class_id(ctx, this))
                .and_then(|(src, tgt)| ctx.array_class_assignable_to_array_class(src, tgt))
            {
                if !verdict && array_is_assignable(ctx, &other_name, &this_name) {
                    // The descriptor rule (the row-off route) would have
                    // admitted it (wave 51, lane L5: the alignment census).
                    ctx.note_alignment_divergence(
                        cratonvm_types::HotSpotAlignment::NamespacedArrayTypes,
                    );
                }
                return Ok(Some(Value::Int(i32::from(verdict))));
            }
        }
        if this_name.starts_with('[') || other_name.starts_with('[') {
            // Build descriptors. Non-array Class mirrors get an `L...;`
            // wrap to match the array_is_assignable contract; array
            // mirrors keep their leading `[`.
            let to_desc = |n: &str| -> String {
                if n.starts_with('[') {
                    n.to_string()
                } else if n.is_empty() {
                    "Ljava/lang/Object;".to_string()
                } else {
                    format!("L{};", n)
                }
            };
            let other_desc = to_desc(&other_name);
            let this_desc = to_desc(&this_name);
            // For arrays, dispatch to array_is_assignable (which expects
            // a target that may be an array name, raw class name, or
            // Object/Serializable/Cloneable). Pass other's full descriptor
            // as src and this's name (NOT descriptor) as target.
            if other_desc.starts_with('[') && array_is_assignable(ctx, &other_desc, &this_name) {
                return Ok(Some(Value::Int(1)));
            }
            if this_desc.starts_with('[') && other_desc == this_desc {
                return Ok(Some(Value::Int(1)));
            }
            // Fall through to id-based check below for non-array vs array
            // mismatches (e.g. `String.class.isAssignableFrom(stringArray.class)`
            // в†’ false, handled by the legacy is_subclass which always
            // returns false for these).
        }

        // bytebuddy_probe + WildFly: primitive Class mirrors (`int.class`,
        // `boolean.class`, etc.) don't have a real ClassId вЂ” `mirror_class_id`
        // returns None. Falling through returned 0, breaking
        // `int.class.isAssignableFrom(int.class)`, which ByteBuddy uses to
        // validate proxy return types, and WildFly uses for reflection
        // sanity checks during early module-loader bootstrap.
        //
        // Fix: name-based equality for same-name primitives BEFORE the
        // class-id lookup. Two mirrors with identical primitive names are
        // assignable (they ARE the same primitive type). For reference
        // types we still go through the class-id / subclass path.
        if !this_name.is_empty()
            && this_name == other_name
            && matches!(
                this_name.as_str(),
                "int"
                    | "long"
                    | "boolean"
                    | "byte"
                    | "short"
                    | "char"
                    | "float"
                    | "double"
                    | "void"
            )
        {
            return Ok(Some(Value::Int(1)));
        }

        let this_class_id = match mirror_class_id(ctx, this) {
            Some(id) => id,
            None => {
                // Fallback: same non-empty name в†’ assignable. Handles missing
                // class_id for primitives and synthetic mirrors.
                if !this_name.is_empty() && this_name == other_name {
                    return Ok(Some(Value::Int(1)));
                }
                return Ok(Some(Value::Int(0)));
            }
        };
        let other_class_id = match mirror_class_id(ctx, other) {
            Some(id) => id,
            None => {
                if !this_name.is_empty() && this_name == other_name {
                    return Ok(Some(Value::Int(1)));
                }
                return Ok(Some(Value::Int(0)));
            }
        };
        // The last disjunct is the DECLARED relationships of the concrete
        // stand-in classes the VM mints itself — the same table the `checkcast`
        // and `instanceof` opcodes consult. See
        // `NativeContext::synthetic_implements_declared`, and the matching
        // arm at the tail of `native_class_is_instance`: the two reflective
        // questions have to agree with each other as well as with the bytecode,
        // and a `Class.isAssignableFrom` that refuses what `Class.isInstance`
        // admits is the shape `Class.cast` fails on.
        let result = other_class_id == this_class_id
            || ctx.is_subclass(other_class_id, this_class_id)
            || loader_aware_reflect_assignable(ctx, other_class_id, this_class_id, &this_name)
            || ctx.synthetic_implements_declared(other_class_id, &this_name);
        Ok(Some(Value::Int(if result { 1 } else { 0 })))
    })();
    // Restore depth on every exit path (success or error).
    IS_ASSIGNABLE_DEPTH.with(|d| d.set(prev_depth));
    result
}

/// WALK12-3 (`docs/feature-designs/jit-ir-optimizations.md`; JIT
/// round 20 wave 11, lane pNat): `CRATONVM_CLASS_IS_ASSIGNABLE_FROM_LEAF`
/// (default ON, `=0` registers [`native_class_is_assignable_from`] as before,
/// non-leaf). Read at registration (per registry, no cache).
pub fn class_is_assignable_from_leaf_enabled() -> bool {
    true
}

/// WALK12-3 follow-up (JIT round 20 wave 11, lane pNat; host A/B w11d: the
/// array control row lost 167 -> 215 ns to decline-then-funnel):
/// `CRATONVM_CLASS_IS_ASSIGNABLE_FROM_LEAF_FULL_BODY` (default ON, `=0`
/// registers [`native_class_is_assignable_from_leaf_declining`], which declines
/// those pairs as before). Read at registration (per registry, no cache): it
/// picks the leaf callback.
pub fn class_is_assignable_from_leaf_full_body_enabled() -> bool {
    true
}

/// `Class.isAssignableFrom(Class)` claimed LEAF (WALK12-3,
/// [`class_is_assignable_from_leaf_enabled`]): the resolved-pair answer of
/// [`native_class_is_assignable_from`] without the native-call funnel, and
/// for `false` without that native's tail (two `mirror_class_name` `String`
/// clones under the class-manager lock, the array and primitive arms).
///
/// The resolved-pair rule ([`class_is_assignable_resolved_pair`]) answers
/// only when both mirrors map to a class id through `class_id_from_mirror`
/// (the first arm of both `mirror_class_id` and `mirror_class_name`), so the
/// tail's names and ids are exactly the ones read here; neither name is an
/// array (the two array arms then apply); the names are not one primitive
/// (the primitive arm); and the source is not a lambda proxy (`is_subclass`
/// would reach `lambda_proxy_satisfies`, which may load a class). It reads
/// class-store state under its READ lock and bumps census atomics only --
/// the set the leaf `isInstance` beside it takes (leaf rules 1-4).
///
/// Host A/B (w11d): declining the array pairs cost +29 % on the array control
/// row (the leaf attempt, the decline, then the whole funnel). So a pair the
/// rule leaves -- an array or primitive name, an unresolved mirror -- runs
/// [`native_class_is_assignable_from`] itself ON the leaf path whenever the
/// source is not a lambda proxy: with two non-null mirrors, `dbg_obsreg` off
/// and the depth guard clear, that body is leaf-safe too -- `mirror_class_id`
/// / `mirror_class_name` (reverse-map and class-store READ locks, a
/// `read_string` of the mirror's name slot: no Java allocation),
/// `array_class_assignable_to_array_class` / `array_is_assignable` /
/// `class_id_by_name` / `is_subclass` of named, non-proxy ids (READ locks),
/// `note_alignment_divergence` (an atomic), Rust `String`s only, no throw.
/// The same function, so the same answers. Only a null argument (its NPE keeps
/// the funnel's `(Native Method)` frame), a lambda-proxy source, the depth
/// guard and the `ObservationRegistry` trace still decline first
/// (`NativeContext::decline_leaf_call`, before any side effect) and run on the
/// funnel, where `decline_leaf_call` answers `false`.
pub fn native_class_is_assignable_from_leaf(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    class_is_assignable_from_leaf_impl(ctx, args, true)
}

/// [`native_class_is_assignable_from_leaf`] with
/// `CRATONVM_CLASS_IS_ASSIGNABLE_FROM_LEAF_FULL_BODY=0`: every pair the
/// resolved-pair rule leaves declines to the funnel (the first w11d build).
pub fn native_class_is_assignable_from_leaf_declining(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    class_is_assignable_from_leaf_impl(ctx, args, false)
}

/// The body of both leaf callbacks above; `full_body` is fixed per callback,
/// chosen at registration.
pub fn class_is_assignable_from_leaf_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    full_body: bool,
) -> MethodCallResult {
    if IS_ASSIGNABLE_DEPTH.with(|d| d.get()) <= 100 && !crate::vmflags().loader.dbg_obsreg {
        if let (Some(Value::Object(Some(this))), Some(Value::Object(Some(other)))) =
            (args.first(), args.get(1))
        {
            if let Some(answer) = class_is_assignable_resolved_pair(&*ctx, *this, *other) {
                return Ok(Some(Value::Int(i32::from(answer))));
            }
            // The rule's leftovers (arrays, primitives, unresolved mirrors):
            // the whole native, still on the leaf path, unless the source is a
            // lambda proxy (`CRATONVM_CLASS_IS_ASSIGNABLE_FROM_LEAF_FULL_BODY=0`
            // restores the decline).
            if full_body
                && !mirror_class_id(&*ctx, *other).is_some_and(|cid| cid.as_u32() >= 0x8000_0000)
            {
                return native_class_is_assignable_from(ctx, args);
            }
        }
    }
    if ctx.decline_leaf_call() {
        return Ok(None);
    }
    native_class_is_assignable_from(ctx, args)
}

/// JIT round 20 wave 19, lane zAssign (`CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE`):
/// tell the VM that `mirror` reverse-maps to `cid`, so it may publish the
/// mirror hint the optimizing tier's `Class.isAssignableFrom` /
/// `Class.isInstance` arms read (`NativeClassAccess::note_class_mirror_id`).
/// The hint's kind from the class's name facts
/// ([`crate::class_facts_memo::mirror_hint_kind`]): none for an array or a
/// primitive-named class (their pairs keep this native's array / primitive
/// arms), misses answered otherwise -- also for a `synthetic_implements`
/// target (JIT r21 tc21b, TYPECHECK21-2 of
/// `jit-r21-typecheck-proposals-20261014.md`) unless
/// `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS_ANY_TARGET=0` restores the decline:
/// the compiled arm answers a miss only for a source whose display verdict is
/// `1`, and `synthetic_implements` declines every such source. Asked only
/// when the VM will write a hint. Never changes an answer.
pub fn note_reflect_mirror_id(ctx: &dyn NativeContext, mirror: ObjectRef, cid: ClassId) {
    ctx.note_class_mirror_id(mirror, cid, &|| {
        crate::class_facts_memo::mirror_hint_kind(
            crate::class_facts_memo::name_facts(ctx, cid),
            true,
        )    });}

/// [`native_class_is_assignable_from`]'s answer for `this.isAssignableFrom(other)`
/// when both mirrors resolve through `class_id_from_mirror` and the pair takes
/// none of that native's array / primitive / lambda-proxy arms; `None`
/// otherwise (see [`native_class_is_assignable_from_leaf`]). With both ids
/// resolved, that native's hoist answers `true` for `this == other` or
/// `other <: this`, and its tail's last expression is the four-way
/// disjunction below over the same ids and `this`'s name.
pub fn class_is_assignable_resolved_pair(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    other: ObjectRef,
) -> Option<bool> {
    let this_cid = ctx.class_id_from_mirror(this)?;
    let other_cid = ctx.class_id_from_mirror(other)?;
    // JIT round 20 wave 19, lane zAssign: the compiled arm's mirror hints.
    note_reflect_mirror_id(ctx, this, this_cid);
    note_reflect_mirror_id(ctx, other, other_cid);
    if this_cid == other_cid {
        return Some(true);
    }
    // A lambda proxy's `is_subclass` may load its functional interface.
    if other_cid.as_u32() >= 0x8000_0000 {
        return None;
    }
    if ctx.is_subclass(other_cid, this_cid) {
        return Some(true);
    }
    // JIT round 20 wave 17, lane pRefl (`CRATONVM_CLASS_FACTS_MEMO_ASSIGNABLE_FALSE`,
    // default ON, `=0` restores the name path below; `R20PNatAssignable`
    // `assignable-false-*`): the same decision from the per-thread name-facts
    // memo, without the two class-name reads under the class-manager lock.
    // Equivalence with the name path, arm by arm:
    // * an array name on either side -> `None`, as below;
    // * equal names -> `None` below; the only arm of the full native that reads
    //   equal names first is its primitive arm, so declining whenever the
    //   target's name is a primitive one (a superset) routes every pair that
    //   arm could answer to that native, and any other equal-name pair gets
    //   the very disjunction the native's tail would compute;
    // * `loader_aware_reflect_assignable` is `false` when the by-name rule is
    //   inert (`loader_aware_resolution` off) or `--real-jdk` with the
    //   `NamespacedClassTypes` row (it then returns before reading the name,
    //   and notes no divergence) -- the condition `isInstance`'s
    //   `IsInstanceFalse` rung uses;
    // * `synthetic_implements_declared(_, name)` is `false` for every object
    //   when the target is not a synthetic target (`NameFacts::is_synthetic_target`).
    // A fact miss (no class at an id: the name path's empty name) or any other
    // case keeps the name path.
    if crate::class_facts_memo::rung_enabled(crate::class_facts_memo::Rung::AssignableFalse) {
        if let (Some(this_facts), Some(other_facts)) = (
            crate::class_facts_memo::name_facts(ctx, this_cid),
            crate::class_facts_memo::name_facts(ctx, other_cid),
        ) {
            if this_facts.is_array() || other_facts.is_array() || this_facts.is_primitive_name() {
                return None;
            }
            if !this_facts.is_synthetic_target()
                && (!crate::classloader::loader_aware_resolution()
                    || (ctx.is_jdk_only()
                        && ctx.aligned(cratonvm_types::HotSpotAlignment::NamespacedClassTypes)))
            {
                return Some(false);
            }
        }
    }
    let this_name = ctx.class_name_arc_of_id(this_cid);
    let other_name = ctx.class_name_arc_of_id(other_cid);
    let this_name: &str = this_name.as_deref().unwrap_or("");
    let other_name: &str = other_name.as_deref().unwrap_or("");
    if this_name.starts_with('[') || other_name.starts_with('[') {
        return None;
    }
    if !this_name.is_empty() && this_name == other_name {
        // The tail's primitive arm (and nothing else) reads equal names first.
        return None;
    }
    Some(
        loader_aware_reflect_assignable(ctx, other_cid, this_cid, this_name)
            || ctx.synthetic_implements_declared(other_cid, this_name),
    )
}

pub fn native_class_is_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // JIT round 17 wave 2, lane nio2 (FUNNEL17-3): when the mirror maps to a
    // class id -- the first arm of `mirror_class_name`, and the only one a
    // loaded class takes -- answer from the per-thread, per-VM memo of that
    // id's name facts instead of rendering the name into a `String` under the
    // class-manager lock on every call. `None` (no class at the id) is
    // `mirror_class_name`'s `None` too: `false`, as before. A mirror with no
    // id keeps the old name path (slot 1, then the slot-0 overlay).
    if let Some(cid) = ctx.class_id_from_mirror(this) {
        let result = crate::class_facts_memo::name_facts(&*ctx, cid)
            .is_some_and(crate::class_facts_memo::NameFacts::is_array);
        return Ok(Some(Value::Int(if result { 1 } else { 0 })));
    }
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let result = name.starts_with('[');
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub fn native_class_is_interface(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => return Ok(Some(Value::Int(0))),
    };
    let result = ctx.is_interface_class(class_id);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub fn native_class_is_primitive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // WP4.2, hardened: real-JDK Class layout stores `primitive:boolean` at
    // some fixed instance slot that `get_or_create_primitive_mirror` writes
    // Int(1) into for primitive mirrors — prefer that flag over the
    // name-based check below, because the name slot can be observed as a
    // stale Int after early-boot descriptor-aware coercion runs (the legacy
    // Int(-1) sentinel that `prim_mirror_create` writes into slot 0 leaks
    // into slot 1 read-back under some boot orderings, leaving
    // `mirror_class_name` returning None and primitive `Class.getName()`
    // returning null).
    //
    // The slot used to be hard-coded as `7`, on the unchecked assumption
    // that that's where THIS JDK build's compiled `java/lang/Class` happens
    // to place the `primitive` field — the exact "answering true for a
    // class that is not primitive" hazard flagged in
    // spring-bean-attribute-type-null-flake-RESOLVED-20260806.md.
    // Resolve it the same way the writer does (`resolve_class_mirror_slots`
    // in `vm/src/vm/vm_object.rs`, i.e. by field name against the loaded
    // `java/lang/Class`), so reader and writer agree by construction instead
    // of by coincidence. `java/lang/Class`'s layout is fixed for the process
    // once loaded, so the resolved index is cached — but never a permanent
    // `None`, so a call landing before `java/lang/Class` has resolved to its
    // real (non-stub) form still gets rechecked, rather than falling back to
    // the name-based check for the rest of the run.
    Ok(Some(Value::Int(i32::from(mirror_is_primitive(ctx, this)))))
}

/// `Class.isPrimitive()` for a mirror, callable from Rust.
///
/// Factored out of [`native_class_is_primitive`] so that natives which have to
/// make the same *decision* — rather than answer the same Java call — cannot
/// drift from it. `MethodHandles.Lookup.in` is the first such caller: it must
/// reject a primitive or array target, and a private re-implementation of
/// "is this mirror primitive" is how the slot-vs-name subtlety documented
/// above gets lost.
pub fn mirror_is_primitive(ctx: &dyn NativeContext, mirror: ObjectRef) -> bool {
    if let Some(idx) = primitive_field_slot(ctx) {
        if let Value::Int(flag) = ctx.get_field(mirror, idx) {
            if flag != 0 {
                return true;
            }
        }
    }
    // Fallback: the legacy synthetic-mode layout where the only signal
    // for primitive-ness is the name string.
    if let Some(facts) = mirror_rung_facts(ctx, mirror) {
        return facts.is_some_and(crate::class_facts_memo::NameFacts::is_primitive_name);
    }
    let name = mirror_class_name(ctx, mirror).unwrap_or_default();
    matches!(
        name.as_str(),
        "int" | "long" | "float" | "double" | "boolean" | "char" | "byte" | "short" | "void"
    )
}

/// `Class.isArray()` for a mirror, callable from Rust.
///
/// An array mirror's internal name is its descriptor (`[I`, `[Ljava/lang/String;`),
/// which is the one shape a class name can never otherwise take — JVMS §4.2.1
/// forbids `[` in a binary name. A mirror whose name cannot be read is NOT an
/// array: the caller is deciding whether to REJECT, and an unreadable name is
/// not evidence for rejecting.
pub fn mirror_is_array(ctx: &dyn NativeContext, mirror: ObjectRef) -> bool {
    if let Some(facts) = mirror_rung_facts(ctx, mirror) {
        return facts.is_some_and(crate::class_facts_memo::NameFacts::is_array);
    }
    mirror_class_name(ctx, mirror).is_some_and(|n| n.starts_with('['))
}

/// NIO2-17-5 rung 3 (JIT round 19 wave 3, lane funnel3;
/// `CRATONVM_CLASS_FACTS_MEMO_MIRROR=1`, default OFF): the name facts of a
/// mirror that maps to a class id, from the per-thread class-facts memo.
///
/// That is [`mirror_class_name`]'s first arm, whose name is
/// `class_name_of_id(cid)`, so a fact answers exactly what a compare of that
/// name answers; the inner `None` ("no class at that id") is
/// `mirror_class_name`'s `None` too. The outer `None` -- the rung off, or a
/// mirror with no id -- leaves the caller's name path to decide.
pub fn mirror_rung_facts(
    ctx: &dyn NativeContext,
    mirror: ObjectRef,
) -> Option<Option<crate::class_facts_memo::NameFacts>> {
    if !crate::class_facts_memo::rung_enabled(crate::class_facts_memo::Rung::Mirror) {
        return None;
    }
    let cid = ctx.class_id_from_mirror(mirror)?;
    Some(crate::class_facts_memo::name_facts(ctx, cid))
}

/// `ACC_PUBLIC` on a mirror's class, callable from Rust.
///
/// A mirror whose `ClassId` cannot be resolved answers `false`. That is the
/// conservative direction for the one caller: `Lookup.in` uses this to decide
/// whether UNCONDITIONAL survives, and answering `true` for a class we could
/// not identify would GRANT access, which is the direction every defect this
/// surface has had already leaned.
pub fn mirror_is_public(ctx: &dyn NativeContext, mirror: ObjectRef) -> bool {
    const ACC_PUBLIC: u16 = 0x0001;
    mirror_class_id(ctx, mirror).is_some_and(|cid| ctx.class_access_flags(cid) & ACC_PUBLIC != 0)
}

/// Instance-field index of `java/lang/Class.primitive` (a `boolean`),
/// resolved by name against the loaded class — mirrors
/// `resolve_class_mirror_slots`'s writer-side resolution in
/// `vm/src/vm/vm_object.rs` so `native_class_is_primitive` reads the exact
/// slot `get_or_create_primitive_mirror` wrote, regardless of this JDK
/// build's actual field order. Cached once resolved; a `None` (e.g.
/// `java/lang/Class` not yet loaded as its real, non-stub form) is not cached,
/// so a later call gets a fresh chance to resolve it.
///
/// gc-common w21-d: the memo is per `(thread, VM)`, not one process-wide
/// slot. The layout is fixed for ONE VM's `java/lang/Class`, not for the
/// process: two VMs booted from different images (or one synthetic, one real)
/// can place `primitive` at different indices, and the process-wide atomic
/// latched whichever VM asked first -- the other then read an unrelated slot
/// for `Class.isPrimitive()`. Same shape as `native-collections`'
/// `abstract_list_mod_count_slot`.
pub fn primitive_field_slot(ctx: &dyn NativeContext) -> Option<usize> {
    thread_local! {
        static CACHED: std::cell::Cell<Option<(usize, usize)>> =
            const { std::cell::Cell::new(None) };
    }
    let vm = ctx.vm_identity();
    if vm == 0 {
        // No identity (the trait default: mock and test contexts, which all
        // share it). Never memoized -- real VM identities count from 1.
        return ctx.resolve_field_index("java/lang/Class", "primitive");
    }
    if let Some((cached_vm, idx)) = CACHED.with(|c| c.get()) {
        if cached_vm == vm {
            return Some(idx);
        }
    }
    let idx = ctx.resolve_field_index("java/lang/Class", "primitive")?;
    CACHED.with(|c| c.set(Some((vm, idx))));
    Some(idx)
}

pub fn native_class_get_superclass(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            if dbg_bb_enabled() {
                eprintln!("[bb-dbg] getSuperclass(<null>) -> null");
            }
            return Ok(Some(Value::Object(None)));
        }
    };

    // bytebuddy_probe (agent-bb3) вЂ” Object/array/primitive name short-circuits
    // BEFORE the class_id lookup. ByteBuddy's `TypeDescription.ForLoadedType`
    // hierarchy walk calls `Class.getSuperclass()` repeatedly; if our shim
    // ever returns Object's own mirror as the superclass of Object (a cycle),
    // ByteBuddy throws `IllegalStateException("Failed to resolve super class
    // class java.lang.Object from [class java.lang.Object]")`. The real JDK
    // returns `null` here for: interfaces, primitive types, void, and the
    // `Object` class itself. We must match that EXACTLY, even when the
    // mirror is a synthetic/duplicate one whose reverse-map entry points
    // at a different ClassId-than-canonical-Object instance.
    //
    // bytebuddy_probe (agent-bb4) вЂ” read the name via the STRICT helper
    // so a corrupted reverse-map that aliases a non-Object class to
    // Object's ClassId does NOT make us short-circuit a non-Object class
    // to null. The strict reader trusts the slot-1 String that was set
    // at mirror-allocation time. We only short-circuit when BOTH the
    // strict name AND the fallback name agree this mirror is Object.
    let dbg_bb = dbg_bb_enabled();
    // W3A: javac's hierarchy walks call this per class; the ASCII slot-1 name
    // (the strict reader's first choice) is compared in a stack buffer instead
    // of through a `String` per call. Anything else (non-ASCII, over 64 bytes,
    // empty) takes the original readers below.
    let mut name_buf = [0u8; 64];
    let fast_len = match ctx.get_field(this, 1) {
        Value::Object(Some(name_obj)) => ctx
            .read_ascii_string_into(name_obj, &mut name_buf)
            .filter(|len| *len > 0),
        _ => None,
    };
    let fast_name = fast_len.map(|len| &name_buf[..len]);
    let (strict_name, this_name) = match fast_name {
        // Only the debug trace reads the name on this path.
        Some(_) if !dbg_bb => (String::new(), String::new()),
        Some(bytes) => {
            let s = String::from_utf8_lossy(bytes).into_owned();
            (s.clone(), s)
        }
        None => {
            let strict_name = mirror_class_name_strict(ctx, this).unwrap_or_default();
            let this_name = if !strict_name.is_empty() {
                strict_name.clone()
            } else {
                mirror_class_name(ctx, this).unwrap_or_default()
            };
            (strict_name, this_name)
        }
    };
    let (strict_is_object, name_is_array) = match fast_name {
        Some(bytes) => (
            bytes == b"java/lang/Object" || bytes == b"java.lang.Object",
            bytes[0] == b'[',
        ),
        None => (
            strict_name == "java/lang/Object" || strict_name == "java.lang.Object",
            this_name.starts_with('['),
        ),
    };
    // Slot 1 named the class: the name is neither empty nor Object here.
    let name_empty_or_object = fast_name.is_none()
        && (this_name.is_empty()
            || this_name == "java/lang/Object"
            || this_name == "java.lang.Object");
    if strict_is_object {
        if dbg_bb {
            eprintln!(
                "[bb-dbg] getSuperclass({}) -> null [object-early-strict]",
                this_name
            );
        }
        return Ok(Some(Value::Object(None)));
    }
    // Per JLS 10.8 / `Class.getSuperclass()` spec: arrays report `Object`
    // as their superclass (not the component type's superclass, not null).
    // Handle this explicitly so a synthetic array mirror without a real
    // ClassId still returns the correct answer.
    if name_is_array {
        if let Some(obj_id) = ctx.class_id_by_name("java/lang/Object") {
            let mirror = ctx.get_class_mirror(obj_id);
            if dbg_bb {
                eprintln!(
                    "[bb-dbg] getSuperclass({}) -> java/lang/Object [array]",
                    this_name
                );
            }
            return Ok(Some(Value::Object(Some(mirror))));
        }
        if dbg_bb {
            eprintln!(
                "[bb-dbg] getSuperclass({}) -> null [array no-obj-id]",
                this_name
            );
        }
        return Ok(Some(Value::Object(None)));
    }

    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            if dbg_bb {
                eprintln!(
                    "[bb-dbg] getSuperclass({}) -> null [no-class-id]",
                    this_name
                );
            }
            return Ok(Some(Value::Object(None)));
        }
    };
    // Synthetic lambda proxies (class id >= 0x8000_0000) are not in the class
    // store, so `superclass_of` returns None в†’ a null superclass. But a lambda's
    // concrete runtime class extends Object; real JVMs never report a null
    // superclass for a concrete (non-interface) class, and Gradle's listener
    // type-walk does `listener.getClass().getSuperclass().isInterface()` в†’
    // "Cannot invoke isInterface on null" (SB-14). Return Object.
    // Proxy ids sit at or above 0x8000_0000 (`LAMBDA_PROXY_ID_BASE`): an
    // ordinary id skips the proxy-table lock and the interface-name `String`.
    if class_id.as_u32() >= 0x8000_0000 && ctx.lambda_functional_interface(class_id).is_some() {
        if let Some(obj_id) = ctx.class_id_by_name("java/lang/Object") {
            let mirror = ctx.get_class_mirror(obj_id);
            if dbg_bb {
                eprintln!(
                    "[bb-dbg] getSuperclass({}) -> java/lang/Object [lambda]",
                    this_name
                );
            }
            return Ok(Some(Value::Object(Some(mirror))));
        }
    }
    // bytebuddy_probe (agent-bb3) вЂ” second-line defence: even after name
    // resolution above, if the resolved ClassId IS Object's canonical id,
    // bail out null. This catches the case where `mirror_class_name`
    // returned something empty/unexpected but the reverse-map still points
    // at Object (e.g. a synthetic mirror whose slot-1 name is null but
    // whose `class_id_from_mirror` resolves to Object).
    if let Some(obj_id) = ctx.object_class_id() {
        if class_id == obj_id {
            if dbg_bb {
                eprintln!(
                    "[bb-dbg] getSuperclass({}) -> null [object-by-id]",
                    this_name
                );
            }
            return Ok(Some(Value::Object(None)));
        }
    }
    // Per JLS 8.1.4 / `Class.getSuperclass()` spec: returns `null` if this
    // Class represents an interface, the Object class, a primitive type,
    // or void.
    //
    // G2-fix (NegativeArraySizeException): the class file of an interface
    // stores `super_class = java/lang/Object`, so `superclass_of` returns
    // `Some(Object)`. Without this gate, `Class.privateGetPublicMethods()`
    // (the real JDK Java method that backs `Class.getMethods()`) walks
    // into Object's public methods for an interface and returns 14
    // entries (Object's `toString`/`hashCode`/`getClass`/`notify`/
    // `notifyAll`/`wait`/etc. вЂ” most 0-arg). ByteBuddy's
    // `JavaDispatcher$DynamicClassLoader.invoker()` then computes
    // `parameterTypes.length - 1` and `anewarray Type[-1]`, raising
    // `NegativeArraySizeException` from `JavaDispatcher.<clinit>`.
    //
    // Real JDK 25 returns `null` here for interfaces вЂ” we must match.
    // W4B: one class-store read answers both the interface test and the parent.
    let (is_interface, superclass) = ctx.interface_flag_and_superclass_of(class_id);
    if is_interface {
        if dbg_bb {
            eprintln!("[bb-dbg] getSuperclass({}) -> null [interface]", this_name);
        }
        return Ok(Some(Value::Object(None)));
    }
    match superclass {
        Some(parent_id) => {
            // bytebuddy_probe (agent-bb2) cycle guard вЂ” if the class
            // manager ever reports a class as its OWN superclass (a stale
            // reload, a double-registration, or a legacy
            // `super_class = self_index` constant-pool entry), refuse to
            // propagate the cycle. ByteBuddy's hierarchy walk detects it
            // anyway and throws `IllegalStateException`; returning null
            // here matches what `Class.getSuperclass()` does for Object
            // and lets the walk terminate cleanly.
            if parent_id == class_id {
                if dbg_bb {
                    eprintln!(
                        "[bb-dbg] getSuperclass({}) -> null [self-cycle id={}]",
                        this_name,
                        class_id.as_u32()
                    );
                }
                return Ok(Some(Value::Object(None)));
            }
            // Defensive: if the resolved parent's name is `java/lang/Object`
            // and we ourselves ARE `java/lang/Object` under a different
            // ClassId (canonical-vs-synthetic mirror split), return null.
            // The string compare above already handles the common case;
            // this catches the reverse-map-only path where slot-1 name was
            // empty / corrupted.
            if let Some(parent_name) = name_empty_or_object
                .then(|| ctx.class_name_of_id(parent_id))
                .flatten()
            {
                if parent_name == "java/lang/Object" {
                    if dbg_bb {
                        eprintln!(
                            "[bb-dbg] getSuperclass({}) -> null [parent=Object name-split]",
                            this_name
                        );
                    }
                    return Ok(Some(Value::Object(None)));
                }
            }
            let mirror = ctx.get_class_mirror(parent_id);
            if dbg_bb {
                let parent_name = ctx.class_name_of_id(parent_id).unwrap_or_default();
                eprintln!("[bb-dbg] getSuperclass({}) -> {}", this_name, parent_name);            }
            Ok(Some(Value::Object(Some(mirror))))
        }
        None => {
            if dbg_bb {
                eprintln!("[bb-dbg] getSuperclass({}) -> null [no-super]", this_name);
            }
            Ok(Some(Value::Object(None)))
        }
    }
}

/// W4B: `Class.getSuperclass` for the LEAF path (`NativeMethodRegistry::set_leaf`).
/// An ordinary class whose parent already has a mirror is answered from the
/// class id with no funnel, no name read and no allocation; everything else
/// (arrays, interfaces, `Object`, lambda proxies, a parent without a mirror,
/// the `bb-dbg` trace) declines to [`native_class_get_superclass`] on the
/// full funnel, so its answers are unchanged.
pub fn native_class_get_superclass_leaf(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(parent) = ordinary_superclass_mirror(&*ctx, *this) {
            return Ok(Some(Value::Object(Some(parent))));
        }
    }
    if ctx.decline_leaf_call() {
        return Ok(None);
    }
    native_class_get_superclass(ctx, args)
}

/// Fast `Class.getSuperclass()` for the direct JIT helper and leaf path:
/// - `Some(Some(parent))`: ordinary class whose parent already has a mirror
/// - `Some(None)`: `Object.class` or an interface (returns Java `null`)
/// - `None`: decline to the full native dispatcher
pub fn class_get_superclass_fast(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Option<Option<ObjectRef>> {
    if dbg_bb_enabled() {
        return None;
    }
    let id = ctx.class_id_from_mirror(this)?;
    if id.as_u32() >= 0x8000_0000 {
        return None;
    }
    if ctx.object_class_id() == Some(id) {
        return Some(None);
    }
    if crate::class_facts_memo::name_facts(ctx, id)?.is_array() {
        return None;
    }
    let (is_interface, superclass) = ctx.interface_flag_and_superclass_of(id);
    if is_interface {
        return Some(None);
    }
    let parent = superclass.filter(|p| *p != id)?;
    ctx.cached_class_mirror(parent).map(Some)
}

pub fn ordinary_superclass_mirror(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    class_get_superclass_fast(ctx, this).flatten()
}

pub fn native_class_get_simple_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // JIT round 17 wave 8, lane mh7: the bridge `--compatible` dispatches
    // raises what the JDK body's `getDeclaringClass0` raises.
    // JIT round 21 wave 21, lane innerclasses (page
    // `r21-review-w21-findings-FIXED-20261020.md` item 7): JDK 25 asks
    // `getDeclaringClass0` only where `isTopLevelClass` reaches it
    // ([`name_bridge_asks_declaring_class`]).
    //
    // JIT round 23 wave 1, lane L6 (REVIEW22-2): a class whose checks once
    // passed skips them ([`name_bridge_verdict_probe`]).
    let verdict = name_bridge_verdict_probe(ctx, this, false);
    // The memo skips only the checks below; the inner-classes agreement check
    // further down is never cached, so it re-asks when the memo hit.
    let mut asks_declaring_known = None;
    if !verdict.passed {
        let asks_declaring = name_bridge_asks_declaring_class(ctx, this, false);
        asks_declaring_known = Some(asks_declaring);
        if asks_declaring {
            if let Some(icce) = inner_classes_icce_for_mirror(ctx, this) {
                return Err(icce);
            }
        } else if let Some(failed) =
            name_bridge_enclosing_method_error(ctx, this, false, &SIMPLE_CLASS_NAME_CACHE)
        {
            // Wave 22, lane hidden-resid (item 1 of
            // `r21-review-w22-findings-FIXED-20261020.md`): `hasEnclosingMethodInfo`
            // resolves the EnclosingMethod class first.
            return Err(failed);
        }
        // JIT round 18 wave 11, lane mhresid (MHRESID18-3): and, with no
        // user-defined loader, the outer class's resolution error.
        if asks_declaring {
            if let Some(failed) =
                name_bridge_outer_resolution_error(ctx, this, &SIMPLE_CLASS_NAME_CACHE)
            {
                return Err(failed);
            }
        }
        name_bridge_note_passed(ctx, verdict);
    }
    // Lambda proxies: HotSpot's `getSimpleName()` keeps the hidden-class tail
    // (`$$Lambda/0x..`) and does NOT split on `$` or `/` -- slicing on those
    // gives "0x..", which is bug-06 fam5 #1 and is why this branch exists.
    //
    // It DOES still strip the package, though, and the original spelling of this
    // branch returned `getName()` verbatim and so kept it. That is the ordinary
    // top-level rule -- `getName().substring(lastIndexOf('.') + 1)` -- and the
    // `/0x<id>` tail carries no `.`, so taking the segment after the last one is
    // exactly it. MEASURED on HotSpot 25 (`probes/LambdaSimpleNameProbe.java`),
    // both cases, which is why the old comment read as correct:
    //
    //     host in the default package
    //       getName       = SimpleNameProbe$$Lambda/0x..3f040210
    //       getSimpleName = SimpleNameProbe$$Lambda/0x..3f040210   (no change)
    //     host in a package
    //       getName       = java.util.function.Predicate$$Lambda/0x..3f000ae8
    //       getSimpleName = Predicate$$Lambda/0x..3f000ae8         (package GONE)
    //
    // CratonVM answered the fully-qualified name in both, so anything rendering a
    // lambda by its simple name carried the package. Spring's
    // `DefaultRetryPolicy.toString()` does `predicate.getClass().getSimpleName()`
    // and asserts `predicate=Predicate..Lambda..`
    // (`core.retry.RetryPolicyTests.predicatesCombined`).
    if let Some(class_id) = mirror_class_id(ctx, this) {
        if let Some(lname) = lambda_proxy_class_name(ctx, class_id) {
            // `rsplit` yields the whole string when there is no `.`, which is
            // the default-package case above -- so this is one rule, not two.
            let simple = lname.rsplit('.').next().unwrap_or(&lname);
            let result = ctx.create_string(simple);
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    // Spring's ReflectionTypeReference builds its key from getPackageName()
    // and getSimpleName(). A generated configuration subclass must therefore
    // expose the same public $$SpringCGLIB$$ identity here as it does through
    // getName(), rather than leaking Craton's internal EnhancerByCGLIB name.
    if let Some(class_id) = ctx.class_id_from_mirror(this) {
        if let Some(name) = ctx.class_name_of_id(class_id) {
            if let Some(display_name) =
                spring_configuration_cglib_display_name(ctx, class_id, &name)
            {
                let simple = display_name.rsplit('/').next().unwrap_or(&display_name);
                return Ok(Some(Value::Object(Some(ctx.create_string(simple)))));
            }
        }
    }
    // Cache the simple-name derivation per `ClassId` only when the VM's
    // reverse mirror map owns this mirror. Test-fixture mirrors that
    // encode ClassId only via field-0 are excluded to avoid cross-test
    // pollution on ClassId(0).
    if let Some(class_id) = ctx.class_id_from_mirror(this) {
        if let Some(arc) = cache_get(&SIMPLE_CLASS_NAME_CACHE, ctx.vm_identity(), class_id) {
            let result = ctx.create_string(&arc);
            return Ok(Some(Value::Object(Some(result))));
        }
        // JDK 25's `getSimpleName` asks `isTopLevelClass()` ->
        // `getDeclaringClass0()` first (interpreter round i2 wave 59, lane
        // L3; `CRATONVM_INNER_CLASSES_AGREEMENT`). A refusal is never cached.
        let asks_declaring = asks_declaring_known
            .unwrap_or_else(|| name_bridge_asks_declaring_class(ctx, this, false));
        if let Some(refusal) = asks_declaring
            .then(|| member_class_inner_classes_disagreement(ctx, class_id))
            .flatten()
        {
            return Err(refusal);
        }
        if let Some(name) = ctx.class_name_of_id(class_id) {
            // Reference arrays: the component CLASS's simple name plus one
            // `[]` per dimension, so an array of an anonymous class is `"[]"`
            // rather than the descriptor-derived `"1[]"`.
            if let Some((dims, elem, elem_id)) = array_reference_element(&name)
                .and_then(|(dims, elem)| ctx.class_id_by_name(elem).map(|id| (dims, elem, id)))
            {
                // Wave 19 (item 4): the element's name reads its tables.
                let fill = inner_tables_fill_guard(&*ctx);
                let elem_simple = resolve_simple_name(ctx, elem_id, elem).to_string();
                let simple = cache_insert_inner_tables(
                    &SIMPLE_CLASS_NAME_CACHE,
                    &*ctx,
                    class_id,
                    Arc::from(append_array_suffix(elem_simple, dims)),
                    fill,
                );
                let result = ctx.create_string(&simple);
                return Ok(Some(Value::Object(Some(result))));
            }
            let simple = resolve_simple_name(ctx, class_id, &name);
            let result = ctx.create_string(&simple);
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    // Fallback: original per-call derivation. Used by test fixtures and
    // primitive mirrors that fall outside the reverse map.
    let name = mirror_class_name(ctx, this).unwrap_or_default();
    let simple = if name.starts_with('[') {
        array_descriptor_to_simple_name(&name).unwrap_or_default()
    } else {
        let simple = name.rsplit(&['/', '.'][..]).next().unwrap_or(&name);
        simple.rsplit('$').next().unwrap_or(simple).to_string()
    };
    let result = ctx.create_string(&simple);
    Ok(Some(Value::Object(Some(result))))
}

pub fn native_class_new_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Class.newInstance on null".to_string()),
            }
            .into())
        }
    };
    let class_id = match mirror_class_id(ctx, this) {
        Some(id) => id,
        None => {
            // JO5-5: a primitive mirror has no class and no constructor
            // (`getConstructor0` -> `NoSuchMethodException`).
            if mirror_is_primitive(ctx, this) {
                let name = mirror_class_name(ctx, this).unwrap_or_default();
                return Err(class_new_instance_no_nullary_constructor(ctx, this, &name));
            }
            return Err(cratonvm_types::error::RuntimeError::NotImplemented {
                feature: "Class.newInstance: no class_id".to_string(),
            }
            .into());
        }
    };
    if let Some(refusal) = class_new_instance_refusal(ctx, this, class_id) {
        return Err(refusal);
    }
    // A lambda proxy the refusal admitted (a nestmate of its host, or the
    // checks switched off): its id is synthetic (>= 0x8000_0000) and absent
    // from the class store, so the funnel below (`<clinit>` first, then a
    // `<init>()V` by id) died with `internal error: prepare_class: class
    // 2147483655 not found` in every arm (round 15 wave 7, lane drainA; the
    // w6a run of `R15Jni6LambdaNew`). HotSpot's spun class has nothing to
    // initialize and its private constructor only stores the captures, so a
    // fresh instance is the proxy-shaped allocation the `invokedynamic` site
    // makes (one slot per capture, left at its default: a capturing proxy
    // is reached here only with the checks off).
    if is_lambda_proxy_id(class_id) {
        let slots = lambda_proxy_captured_fields(&*ctx, class_id).len();
        let obj = ctx.alloc_object(class_id, slots);
        return Ok(Some(Value::Object(Some(obj))));
    }
    // Interpreter round i2 wave 58, lane L3: the class is initialized where
    // JDK 25 initializes it (the constructor accessor's factory), its failure
    // listed under those frames.
    if !ctx.is_class_initialized(class_id) {
        class_new_instance_initialize(ctx, class_id)?;
    }
    // GC-safe allocate + `<init>()V` вЂ” pins the new object across the
    // constructor under the moving collector (see `new_object_initialized`).
    // Allocate by the mirror's EXACT class id (not its name) so per-loader
    // class identity is preserved (JVMS В§5.3); the name-based path collapses
    // to the first/global definer of that name.
    match ctx.new_object_initialized_with_class_id(class_id, "()V", &[])? {
        Some(v @ Value::Object(Some(_))) => Ok(Some(v)),
        _ => Err(cratonvm_types::error::RuntimeError::NotImplemented {
            feature: "Class.newInstance: new_object failed".to_string(),
        }
        .into()),
    }
}

/// `Class.newInstance()`'s initialization of `class_id`: JDK 25 reaches the
/// constructor accessor's factory through `ReflectionFactory.newInstance` ->
/// `ReflectAccess.newInstance` -> `Constructor.newInstanceWithCaller`, and the
/// failure lists those frames (measured, JDK 25.0.3). Interpreter round i2
/// wave 58, lane L3.
#[cold]
#[inline(never)]
pub fn class_new_instance_initialize(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const NEW_INSTANCE: &str =
        "(Ljava/lang/reflect/Constructor;[Ljava/lang/Object;Ljava/lang/Class;)Ljava/lang/Object;";
    const WITH_CALLER: &str = "([Ljava/lang/Object;ZLjava/lang/Class;)Ljava/lang/Object;";
    const ACQUIRE: &str = "()Ljdk/internal/reflect/ConstructorAccessor;";
    const RF_NEW_ACCESSOR: &str =
        "(Ljava/lang/reflect/Constructor;)Ljdk/internal/reflect/ConstructorAccessor;";
    const MHAF_NEW_ACCESSOR: &str =
        "(Ljava/lang/reflect/Constructor;)Ljdk/internal/reflect/ConstructorAccessorImpl;";
    let hop = |class, name, descriptor, next: &'static str, next_descriptor: &'static str| {
        ServedRaiseHop {
            class,
            name,
            descriptor,
            site: ServedRaiseHopSite::LastCall {
                name: next,
                descriptor: Some(next_descriptor),
            },
        }
    };
    let rf = "jdk/internal/reflect/ReflectionFactory";
    let mhaf = "jdk/internal/reflect/MethodHandleAccessorFactory";
    let ctor = "java/lang/reflect/Constructor";
    let mut hops = vec![
        hop(
            "java/lang/Class",
            "newInstance",
            "()Ljava/lang/Object;",
            "newInstance",
            NEW_INSTANCE,
        ),
        hop(rf, "newInstance", NEW_INSTANCE, "newInstance", NEW_INSTANCE),
        hop(
            "java/lang/reflect/ReflectAccess",
            "newInstance",
            NEW_INSTANCE,
            "newInstanceWithCaller",
            WITH_CALLER,
        ),
        hop(
            ctor,
            "newInstanceWithCaller",
            WITH_CALLER,
            "acquireConstructorAccessor",
            ACQUIRE,
        ),
        hop(
            ctor,
            "acquireConstructorAccessor",
            ACQUIRE,
            "newConstructorAccessor",
            RF_NEW_ACCESSOR,
        ),
        hop(
            rf,
            "newConstructorAccessor",
            RF_NEW_ACCESSOR,
            "newConstructorAccessor",
            MHAF_NEW_ACCESSOR,
        ),
        hop(
            mhaf,
            "newConstructorAccessor",
            MHAF_NEW_ACCESSOR,
            "ensureClassInitialized",
            "(Ljava/lang/Class;)V",
        ),
    ];
    hops.extend(class_init_chain_tail(mhaf));
    ctx.initialize_class_served(
        class_id,
        ServedRaiseChain {
            throwable_class: "java/lang/ExceptionInInitializerError",
            hops,
        },
    )
}



/// J1 of `r15w4-jdkonly6-review-findings-CLOSED-20260930.md` (round 15 wave 5, lane
/// jdkonly7): is the `Constructor` `ctor`, whose accessor's handle allocates
/// `instance_class`, an ordinary constructor WITH parameters rather than a
/// serialization constructor? `ReflectionFactory.newConstructorForSerialization`
/// hands back a `Constructor` whose `clazz` is an ANCESTOR of the class its
/// handle allocates; a plain constructor's accessor
/// (`Constructor.acquireConstructorAccessor`) allocates `clazz` itself. The
/// serialization arm runs only a nullary `<init>`, so taking it for the plain
/// shape dropped every argument. The plain NULLARY shape keeps the arm: its
/// construction is the same. `CRATONVM_SERIALIZATION_CTOR_SHAPE=0` restores
/// the arm for every accessor (read only for the plain shape with parameters).
pub fn constructor_is_plain_with_parameters(
    ctx: &dyn NativeContext,
    ctor: ObjectRef,
    instance_class: ClassId,
) -> bool {
    let clazz = match ctx.get_field_by_name(ctor, "clazz") {
        Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
        _ => None,
    };
    if clazz != Some(instance_class) {
        return false;
    }
    let parameters = match ctx.get_field_by_name(ctor, "parameterTypes") {
        Value::Object(Some(types)) => ctx.array_length(types),
        _ => 0,
    };
    parameters != 0}


/// Interpreter round i4 wave 98, lane L8: the failure HotSpot's
/// `Unsafe.allocateInstance(instanceClass)` raises inside a serialization
/// constructor's handle when `instanceClass` is abstract or an interface
/// (`InstanceKlass::check_valid_for_instantiation`:
/// `InstantiationException(external_name)`); `None` for an instantiable class.
pub fn serialization_ctor_abstract_refusal(
    ctx: &mut dyn NativeContext,
    inst_cid: ClassId,
) -> Option<MethodCallFailed> {
    let flags = ctx.class_access_flags(inst_cid);
    let refused = cratonvm_types::access_flags::ACC_ABSTRACT
        | cratonvm_types::access_flags::ACC_INTERFACE;
    if flags & refused == 0 {
        return None;
    }
    // `Klass::external_name()`: a hidden class keeps its `/0x<hex>` tail.
    let name = reflect_text_class_name(&*ctx, &ctx.class_name_of_id(inst_cid)?);
    let text = ctx.create_string_uninterned(&name);
    Some(crate::throwable_mint::construct_thrown(
        ctx,
        "java/lang/InstantiationException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(text))],
        || {
            cratonvm_types::error::RuntimeError::IllegalStateException {
                message: format!("InstantiationException: {name}"),
            }
            .into()
        },
    ))
}

/// Interpreter round i4 wave 98, lane L8: what a NULLARY serialization
/// constructor's `DirectConstructorHandleAccessor.newInstance` refuses before
/// it constructs: an argument array that is not empty
/// (`IllegalArgumentException: wrong number of arguments: N expected: 0`),
/// then an abstract class or interface (the handle's allocation,
/// `InvocationTargetException` caused by `InstantiationException`). `None`
/// when the constructor has parameters (`serialization_ctor_with_arguments`
/// owns those) or neither refusal applies.
pub fn serialization_ctor_nullary_refusal(
    ctx: &mut dyn NativeContext,
    ctor: ObjectRef,
    inst_cid: ClassId,
    args_array_pin: Option<(usize, ObjectRef)>,
) -> Option<MethodCallFailed> {
    let parameters = match ctx.get_field_by_name(ctor, "parameterTypes") {
        Value::Object(Some(types)) => ctx.array_length(types),
        _ => 0,
    };
    if parameters != 0 {
        return None;
    }
    let args_array = args_array_pin.map(|(pin, arr)| ctx.read_native_pin(pin, arr));
    let actual = args_array.map_or(0, |arr| ctx.array_length(arr));
    if actual != 0 {
        return Some(raise_reflective_check(
            ctx,
            cratonvm_native_api::ReflectiveCheck::ArgumentCount,
            |_| illegal_arg_exc(format!("wrong number of arguments: {actual} expected: 0")),
        ));
    }
    let failure = serialization_ctor_abstract_refusal(ctx, inst_cid)?;
    Some(wrap_as_invocation_target_exception(ctx, failure))
}



/// K3 of `r15w5-jdkonly7-review-findings-CLOSED-20260930.md` (round 15 wave 6, lane
/// jni6): the serialization constructor over a PARAMETERIZED ancestor
/// constructor (`sun.reflect.ReflectionFactory.newConstructorForSerialization(
/// cl, ctorToCall)` with a `ctorToCall` taking arguments). JDK 25's
/// `JLIA.serializableConstructor(cl, ctorToCall)` handle allocates `cl` and
/// runs `ctorToCall` WITH the arguments; `DirectConstructorHandleAccessor`
/// checks the count first (`wrong number of arguments: N expected: M`),
/// converts the arguments (`argument type mismatch`), and wraps whatever the
/// allocation (class initialization) or the constructor threw in an
/// `InvocationTargetException`. `NativeConstruct::AncestorInit` runs `<init>()V`
/// only, so the arm dropped the arguments (or failed with
/// `InvocationTargetException(NoSuchMethodError)` when the ancestor has no
/// nullary constructor).
///
/// `None` hands the call back to the nullary arm: a `Constructor` with no
/// parameters, a descriptor this cannot read, an ancestor or instance class
/// that does not resolve, or `CRATONVM_SERIALIZATION_CTOR_ARGS=0` (read only
/// for a constructor with parameters). `args_array_pin` is the caller's pin;
/// the caller releases it.
///
/// The object is built through `instantiate(.., AncestorInit(java.lang.Object))`
/// so it is initialized, allocated and REGISTERED as the nullary arm's object
/// is (`Object.<init>` is empty; it runs again at the end of the ancestor's
/// own constructor chain), then the ancestor's constructor runs by class id,
/// `invokespecial`-style.
pub fn serialization_ctor_with_arguments(
    ctx: &mut dyn NativeContext,
    ctor: ObjectRef,
    inst_cid: ClassId,
    args_array_pin: Option<(usize, ObjectRef)>,
) -> Option<MethodCallResult> {
    let parameters = match ctx.get_field_by_name(ctor, "parameterTypes") {
        Value::Object(Some(types)) => ctx.array_length(types),
        _ => 0,
    };
    if parameters == 0
    {
        return None;
    }
    let anc_cid = match ctx.get_field_by_name(ctor, "clazz") {
        Value::Object(Some(anc_mirror)) => ctx.class_id_from_mirror(anc_mirror),
        _ => None,
    }?;
    let anc_name = ctx.class_name_of_id(anc_cid)?;
    let descriptor = constructor_descriptor_for_new_instance(&*ctx, ctor);
    let (param_descs, _) = parse_descriptor_param_and_return(&descriptor);
    if param_descs.len() != parameters {
        return None;
    }
    let object_cid = ctx.class_id_by_name("java/lang/Object")?;

    // `DirectConstructorHandleAccessor.newInstance`: the count, then the
    // conversions, both before the handle allocates.
    let args_array = args_array_pin.map(|(pin, arr)| ctx.read_native_pin(pin, arr));
    let actual = args_array.map_or(0, |arr| ctx.array_length(arr));
    if actual != parameters {
        return Some(Err(raise_reflective_check(
            ctx,
            cratonvm_native_api::ReflectiveCheck::ArgumentCount,
            |_| {
                illegal_arg_exc(format!(
                    "wrong number of arguments: {actual} expected: {parameters}"
                ))
            },
        )));
    }
    for (i, pdesc) in param_descs.iter().enumerate() {
        let arg_val = match args_array {
            Some(arr) => ctx.get_array_element(arr, i),
            None => Value::Object(None),
        };
        if let Err(e) = coerce_arg_strict(
            &*ctx,
            arg_val,
            pdesc,
            "Constructor.newInstance argument",
            Some(anc_cid),
        ) {
            // `Constructor.newInstance`'s accessor keeps the CCE as the IAE's
            // cause (the i1 wave-47 `constructor` flag, added on dev while
            // round 15 wave 6 wrote this call).
            return Some(Err(raise_reflective_coercion_failure(
                ctx, arg_val, pdesc, e, true,
            )));
        }
    }
    // Interpreter round i4 wave 98, lane L8: the handle's allocation refuses
    // an abstract class or interface (`serialization_ctor_abstract_refusal`).
    if let Some(failure) = serialization_ctor_abstract_refusal(ctx, inst_cid) {
        return Some(Err(wrap_as_invocation_target_exception(ctx, failure)));
    }

    let obj = match ctx.instantiate(
        inst_cid,
        cratonvm_native_api::registry::NativeConstruct::AncestorInit(object_cid),
    ) {
        Ok(Some(obj)) => obj,
        Ok(None) => return None,
        Err(failure) => return Some(Err(wrap_as_invocation_target_exception(ctx, failure))),
    };
    let obj_pin = ctx.pin_native_root(obj);
    // The allocation may have moved the arguments: converted again from the
    // array's current address (the conversion is pure and already succeeded).
    let args_array = args_array_pin.map(|(pin, arr)| ctx.read_native_pin(pin, arr));
    let mut init_args: Vec<Value> = Vec::with_capacity(1 + parameters);
    init_args.push(Value::Object(Some(obj)));
    for (i, pdesc) in param_descs.iter().enumerate() {
        let arg_val = match args_array {
            Some(arr) => ctx.get_array_element(arr, i),
            None => Value::Object(None),
        };
        let coerced = coerce_arg_strict(
            &*ctx,
            arg_val,
            pdesc,
            "Constructor.newInstance argument",
            Some(anc_cid),
        )
        .unwrap_or(arg_val);
        init_args.push(coerced);
    }
    let init =
        ctx.invoke_special_by_class_id(anc_cid, &anc_name, "<init>", &descriptor, &init_args);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Some(match init {
        Ok(_) => Ok(Some(Value::Object(Some(obj)))),
        Err(failure) => Err(wrap_as_invocation_target_exception(ctx, failure)),
    })
}

/// The refusal JDK 25's `Class.newInstance()` body makes before it constructs,
/// in its order, or `None` to construct (round 15 wave 4, lane jdkonly6;
/// JO5-5). The body is
///
/// ```text
/// if (this == Class.class) throw new IllegalAccessException("Can not call newInstance() ..");
/// c = getConstructor0(empty, DECLARED)   // NoSuchMethodException ->
///                                        //   InstantiationException(getName()).initCause(e)
/// newInstanceWithCaller(null, true, caller):
///     checkAccess(caller, clazz, clazz, modifiers)   // IllegalAccessException
///     newConstructorAccessor: abstract -> InstantiationException(null)
/// ```
///
/// The native went straight to the construction: an ABSTRACT class with a
/// nullary constructor was allocated and returned, an interface or array
/// failed somewhere inside `<init>` dispatch, and a `private` constructor
/// ran for any caller. The access check is the member-modifier rule
/// (`caller_may_access_member`, nestmates included) after the module
/// `exports` rule, as `Constructor.newInstance` asks them; a caller that
/// cannot be resolved (VM bootstrap) is not refused. The class-accessibility
/// half of `verifyMemberAccess` (a non-public class from another runtime
/// package) is `reflective_ctor_class_is_reachable`, as on the
/// `Constructor.newInstance` path.
pub fn class_new_instance_refusal(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    class_id: ClassId,
) -> Option<MethodCallFailed> {
    use cratonvm_types::access_flags::{ACC_ABSTRACT, ACC_INTERFACE};
    // Ahead of the name: a lambda proxy has no class-store name.
    if is_lambda_proxy_id(class_id) {
        return lambda_proxy_new_instance_refusal(ctx, mirror, class_id);
    }
    let name = ctx.class_name_of_id(class_id)?;
    let flags = ctx.class_access_flags(class_id);
    if name == "java/lang/Class" {
        return true.then(|| {
            cratonvm_types::error::RuntimeError::IllegalAccessException {
                message: "Can not call newInstance() on the Class for java.lang.Class".to_string(),
            }
            .into()
        });
    }
    let ctor_flags = if flags & ACC_INTERFACE != 0 || name.starts_with('[') {
        None
    } else {
        ctx.declared_method_access_flags(class_id, "<init>", "()V")
    };
    let Some(ctor_flags) = ctor_flags else {
        return Some(class_new_instance_no_nullary_constructor(
            ctx, mirror, &name,
        ));
    };
    // `Constructor.modifiers`: the method's flags as HotSpot's reflection
    // records them (`JVM_RECOGNIZED_METHOD_MODIFIERS`).
    let modifiers = i32::from(ctor_flags) & 0x1DFF;
    if let Err(msg) =
        check_reflection_export_access_with_target_id(ctx, &name, Some(class_id), false)
    {
        // JO6-1 / J9: `Reflection.newIllegalAccessException`'s module form.
        let legacy = format!(
            "Class.newInstance: {}: {msg}",
            reflect_text_class_name(&*ctx, &name)
        );
        return Some(reflective_access_refusal(
            ctx,
            Some(class_id),
            modifiers,
            legacy,
        ));
    }
    if !reflective_ctor_class_is_reachable(ctx, class_id) {
        let legacy = format!(
            "Class.newInstance: {}: class is not public and the caller is not in its runtime package",
            reflect_text_class_name(&*ctx, &name)
        );
        return Some(reflective_access_refusal(
            ctx,
            Some(class_id),
            modifiers,
            legacy,
        ));
    }
    if modifiers & ACC_PUBLIC == 0 {
        if let Some(caller) = resolve_caller_class_id(ctx) {
            // A private constructor's nest hosts are loaded first, as
            // `getNestHost` does (JIT round 20 wave 16, lane nest). Nothing
            // here or in `native_class_new_instance` reads `mirror` after this
            // point, so no reference is handed over.
            settle_reflective_nest_hosts(ctx, caller, class_id, modifiers, &mut [], &mut [], &[]);
            if !crate::lang_reflect::caller_may_access_member(
                ctx,
                caller,
                class_id,
                modifiers,
                Some(class_id),
            )
            {
                return Some(
                    cratonvm_types::error::RuntimeError::IllegalAccessException {
                        message: {
                            let text =
                                hotspot_member_access_message(ctx, caller, class_id, modifiers);
                            lambda_proxy_caller_message(ctx, caller, text)
                        },
                    }
                    .into(),
                );
            }
        }
    }
    if flags & ACC_ABSTRACT != 0 {
        return Some(class_new_instance_instantiation_exception(ctx, None));    }
    None
}

/// K4 of `r15w5-jdkonly7-review-findings-CLOSED-20260930.md` (round 15 wave 6, lane
/// jni6): `Class.newInstance()` of a lambda proxy. HotSpot's spun class
/// declares ONE `private` constructor over its captures
/// ([`lambda_proxy_constructor`]), so `getConstructor0(empty, DECLARED)` finds
/// a nullary one only for a non-capturing lambda (otherwise
/// `InstantiationException(getName())` caused by `NoSuchMethodException`),
/// and `checkAccess` then admits only a nestmate of the lambda's host (the
/// proxy is defined `NESTMATE`, so its nest is its host's): any other caller
/// gets `IllegalAccessException`. This constructed for every caller.
/// `CRATONVM_LAMBDA_NEW_INSTANCE_CHECKS=0` restores that; a proxy with no
/// lambda metadata, or whose host or caller cannot be resolved, is still
/// constructed (fail open: every refusal here is new).
pub fn lambda_proxy_new_instance_refusal(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    class_id: ClassId,
) -> Option<MethodCallFailed> {
    let ctor = lambda_proxy_constructor(&*ctx, class_id)?;
    let name = lambda_proxy_class_name(&*ctx, class_id).unwrap_or_default();
    if ctor.descriptor != "()V" {
        return Some(class_new_instance_no_nullary_constructor(
            ctx, mirror, &name,
        ));
    }
    let host = lambda_proxy_host_class(&*ctx, class_id)?;
    let caller = resolve_caller_class_id(ctx)?;
    let modifiers = i32::from(ctor.access_flags) & 0x1DFF;
    // Private access to the lambda's constructor is exactly private access to
    // its host's members: the same nest. The caller's claimed host is loaded
    // first, as `getNestHost` does (JIT round 20 wave 16, lane nest); `mirror`
    // is not read after this point.
    settle_reflective_nest_hosts(ctx, caller, host, modifiers, &mut [], &mut [], &[]);
    if crate::lang_reflect::caller_may_access_member(ctx, caller, host, modifiers, Some(host)) {
        return None;
    }
    // `Reflection.newIllegalAccessException`; the proxy lives in its host's
    // module. `class_to_string_with_module` cannot spell the proxy (no
    // class-store name), so its half is composed here.
    let member_module = match ctx.module_identity_of_class(host).name() {
        Some(module) => format!(" (in module {module})"),
        None => String::new(),
    };
    Some(
        cratonvm_types::error::RuntimeError::IllegalAccessException {
            message: format!(
                "{} cannot access a member of class {name}{member_module} with modifiers \"{}\"",
                class_to_string_with_module(&*ctx, caller),
                java_modifier_to_string(modifiers)
            ),
        }
        .into(),
    )
}


/// `throw new InstantiationException(message)`: the constructed object, the
/// constructor's own throwable when it threw, or a flat error when the class
/// cannot be built at all.
pub fn class_new_instance_instantiation_exception(
    ctx: &mut dyn NativeContext,
    message: Option<ObjectRef>,
) -> MethodCallFailed {
    match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/lang/InstantiationException",
        "(Ljava/lang/String;)V",
        &[Value::Object(message)],
    ) {
        Ok(Some(exc)) => MethodCallFailed::ExceptionThrown(exc),
        Err(thrown) => thrown,
        Ok(None) => cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "InstantiationException: cannot instantiate this type".to_string(),
        }
        .into(),
    }
}

/// `getConstructor0` found no nullary constructor (an interface, an array or
/// primitive class, or a class that declares none): JDK 25 throws
/// `(InstantiationException) new InstantiationException(getName()).initCause(e)`
/// where `e` is `NoSuchMethodException(getName() + ".<init>()")`
/// (`Class.methodToString`). `getName()` is asked of `mirror` itself (a
/// primitive's keyword, a hidden class's suffix); `internal_name` with `/`
/// as `.` answers when it cannot be.
pub fn class_new_instance_no_nullary_constructor(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    internal_name: &str,
) -> MethodCallFailed {
    let binary_name = match ctx.invoke_virtual(mirror, "getName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(name)))) => ctx.read_string(name),
        _ => None,
    }
    .filter(|name| !name.is_empty())
    .unwrap_or_else(|| reflect_text_class_name(&*ctx, internal_name));
    let nsme_text = ctx.create_string_uninterned(&format!("{binary_name}.<init>()"));
    let cause = match cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/lang/NoSuchMethodException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(nsme_text))],
    ) {
        Ok(cause) => cause,
        Err(thrown) => return thrown,
    };
    let cause_pin = cause.map(|c| (ctx.pin_native_root(c), c));
    let message = ctx.create_string_uninterned(&binary_name);
    let built = cratonvm_native_api::throwable_builder::construct_new(
        ctx,
        "java/lang/InstantiationException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message))],
    );
    let exc = match built {
        Ok(Some(exc)) => exc,
        other => {
            if let Some((pin, _)) = cause_pin {
                ctx.unpin_native_roots(pin);
            }
            return match other {
                Err(thrown) => thrown,
                _ => cratonvm_types::error::RuntimeError::IllegalStateException {
                    message: format!("InstantiationException: {binary_name}"),
                }
                .into(),
            };
        }
    };
    let Some((pin, cause)) = cause_pin else {
        return MethodCallFailed::ExceptionThrown(exc);
    };
    let exc_pin = ctx.pin_native_root(exc);
    let cause = ctx.read_native_pin(pin, cause);
    let init = ctx.invoke_virtual(
        exc,
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        &[Value::Object(Some(cause))],
    );
    let exc = ctx.read_native_pin(exc_pin, exc);
    ctx.unpin_native_roots(pin);
    match init {
        Ok(_) => MethodCallFailed::ExceptionThrown(exc),
        Err(thrown) => thrown,
    }
}

/// `Reflection.newIllegalAccessException(currentClass, memberClass,
/// targetClass, modifiers)`'s text when module access holds:
/// `<currentClass> cannot access a member of <memberClass> with modifiers
/// "<Modifier.toString(modifiers)>"` (` with package access` for a member
/// with none of the `public`/`protected`/`private` bits), each class as `Class.toString()`
/// plus ` (in module <name>)` for a named module.
pub fn hotspot_member_access_message(
    ctx: &dyn NativeContext,
    current: ClassId,
    member: ClassId,
    modifiers: i32,
) -> String {
    let access_bits = 0x0001 | 0x0002 | 0x0004;
    // JDK 25 `Reflection.msgSuffix`: a member with none of the three access
    // bits reads ` with package access` (t1 wave 2, lane P; probe
    // `T1PRuntimePackageAccess.java`, `AccReflect` rows). The
    // empty suffix this used to give matched no HotSpot 25 text.
    let suffix = if modifiers & access_bits == 0 {
        " with package access".to_string()
    } else {
        format!(" with modifiers \"{}\"", java_modifier_to_string(modifiers))
    };
    format!(
        "{} cannot access a member of {}{suffix}",
        class_to_string_with_module(ctx, current),
        class_to_string_with_module(ctx, member)
    )
}

/// `Class.toString()` + ` (in module <name>)` when the module is named, as
/// `Reflection.newIllegalAccessException` spells a class.
pub fn class_to_string_with_module(ctx: &dyn NativeContext, class_id: ClassId) -> String {
    // `Class.getName()`'s spelling (interpreter round i4 wave 94, lane L3;
    // `L3W94CallerSensitiveMatrix`, `get-other-reflect` under `--real-jdk`): a
    // hidden class keeps its `/0x<hex>` tail (`Host$$InjectedInvoker/0x..`,
    // printed `Host$$InjectedInvoker.0x..` by the plain rewrite), and a
    // synthetic lambda proxy is `Host$$Lambda/0x..`.
    let name = lambda_proxy_class_name(ctx, class_id).unwrap_or_else(|| {
        dotted_binary_name(&ctx.class_name_of_id(class_id).unwrap_or_default())
    });
    let kind =
        if ctx.class_access_flags(class_id) & cratonvm_types::access_flags::ACC_INTERFACE != 0 {
            "interface "
        } else {
            "class "
        };
    match ctx.module_identity_of_class(class_id).name() {
        Some(module) => format!("{kind}{name} (in module {module})"),
        None => format!("{kind}{name}"),
    }
}

/// `java.lang.reflect.Modifier.toString(int)`, in the JDK's order.
pub fn java_modifier_to_string(modifiers: i32) -> String {
    const WORDS: [(i32, &str); 12] = [
        (0x0001, "public"),
        (0x0004, "protected"),
        (0x0002, "private"),
        (0x0400, "abstract"),
        (0x0008, "static"),
        (0x0010, "final"),
        (0x0080, "transient"),
        (0x0040, "volatile"),
        (0x0020, "synchronized"),
        (0x0100, "native"),
        (0x0800, "strictfp"),
        (0x0200, "interface"),
    ];
    WORDS
        .iter()
        .filter(|(bit, _)| modifiers & bit != 0)
        .map(|(_, word)| *word)
        .collect::<Vec<_>>()
        .join(" ")
}

