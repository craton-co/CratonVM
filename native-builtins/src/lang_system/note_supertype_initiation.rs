#[allow(unused_imports)]
use super::*;

/// Record the user loader `loader_ns` as an initiating loader of the
/// supertype `internal` it resolved for a define: the class `mirror` names
/// (its `loadClass` answer), else the flat store's class of that name (the
/// one the define links). `note_initiating_load` checks the loader
/// constraints first (HotSpot's `check_constraints` in `load_instance_class`;
/// a violation is the `LinkageError` it throws) and records nothing for the
/// loader's own class. Allocates only when it refuses.
#[cold]
#[inline(never)]
pub fn note_supertype_initiation(
    ctx: &mut dyn NativeContext,
    loader_ns: u32,
    internal: &str,
    mirror: Option<ObjectRef>,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let class_id = match mirror {
        Some(mirror) => ctx.class_id_from_mirror(mirror),
        None => ctx.class_id_by_name(internal),
    };
    match class_id {
        Some(class_id) => ctx.note_initiating_load(loader_ns, internal, class_id),
        None => Ok(()),
    }
}

/// Whether `throwable` is a `java.lang.ClassCircularityError`.
pub fn is_class_circularity_error(ctx: &dyn NativeContext, throwable: ObjectRef) -> bool {
    crate::classloader::object_extends(ctx, throwable, "java/lang/ClassCircularityError")
}

thread_local! {
    /// The `(loader pin, loader, internal name)` of every class whose direct
    /// supertypes this thread is linking through its defining loader right
    /// now, innermost last.
    ///
    /// JVMS §5.3.5 steps 3-4: a class that is its own supertype is a
    /// `ClassCircularityError`. A user loader meets the cycle as a re-entered
    /// define: defining `p.A` loads `p.B` through the loader, whose define
    /// loads `p.A` again, and `p.A` is not yet defined, so the loader's
    /// `findClass` defines it a second time. HotSpot's placeholder table
    /// (`SystemDictionary::resolve_super_or_fail`) sees `(loader, p.A)` already
    /// resolving its super on this thread and throws; this is that table. The
    /// loader is compared through the pin its `defineClass` native holds for
    /// the whole link, so a collection in between does not hide it.
    ///
    /// Per thread and per call, not per VM: each entry lives exactly as long
    /// as one `link_supertypes_through_defining_loader` call on this thread.
    pub static SUPERTYPE_LINKS_IN_PROGRESS: std::cell::RefCell<Vec<(usize, ObjectRef, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `CRATONVM_DEFINE_INTERFACES_BEFORE_SUPER` (JIT round 23 wave 10, lane L1;
/// HotSpot alignment, default ON, `=0` asks the superclass first as before):
/// the defining loader's `loadClass` is asked for a defined class's direct
/// supertypes in HotSpot's order. `ClassFileParser::parse_interfaces` resolves
/// each interface (`resolve_super_or_fail`) while it parses, and the
/// superclass is resolved after (`post_process_parsed_stream`). Measured on
/// Temurin 25.0.3 with a logging user loader: `Two extends AbstractList
/// implements Runnable, Closeable` asks `Runnable`, `Closeable`,
/// `AbstractList`; a `Proxy`/`Runnable` proxy asks `Runnable`, `Proxy`. Before,
/// both modes asked the superclass first. Read per define.
pub fn define_interfaces_before_super_enabled() -> bool {
    true
}

/// The order in which the define natives ask the loader for the pre-pass's
/// `count` supertypes, as indices into the pre-pass's own list (the
/// superclass at 0 when `has_super`, then the interfaces in class-file order):
/// the interfaces first, then the superclass, under
/// [`define_interfaces_before_super_enabled`]; the list order otherwise.
pub fn supertype_ask_order(count: usize, has_super: bool) -> Vec<usize> {
    if has_super && count > 1 && define_interfaces_before_super_enabled() {
        (1..count).chain(std::iter::once(0)).collect()
    } else {
        (0..count).collect()
    }
}

/// `CRATONVM_DEFINE_ASKS_FOR_OBJECT` (verifier-link-loads page, item 3;
/// HotSpot alignment, default ON, `=0` never asks as before): a user loader's
/// define of a class whose superclass is `java/lang/Object` asks that loader's
/// `loadClass("java.lang.Object")`, where HotSpot's `resolve_super_or_fail`
/// does -- last, as the superclass is asked last, and once per loader: the
/// supertype dictionary (`CRATONVM_DEFINE_SUPERTYPE_DICTIONARY`, which this
/// requires) files the answer, as HotSpot's dictionary does, so later defines
/// do not ask again. Measured with `VLoad.java` on
/// HotSpot 25.0.4.1: `DEFINE loadClass java.lang.Object` on the first define
/// of a fresh loader, none for the classes it defines after. Read per define.
pub fn define_asks_for_object_enabled() -> bool {
    true
}

/// Whether a user loader's define of `class_file` asks the loader for
/// `java/lang/Object` ([`define_asks_for_object_enabled`]): its superclass is
/// `Object` and the supertype dictionary is on to file the answer.
pub fn define_asks_for_object(
    ctx: &dyn NativeContext,
    class_file: &cratonvm_reader::ClassFile,
) -> bool {
    class_file.super_class.as_deref() == Some("java/lang/Object")
        && ctx.define_supertype_dictionary()
        && define_asks_for_object_enabled()
}

/// Ask the user loader `loader_ns` (pinned at `loader_pin`) for
/// `java/lang/Object` unless its dictionary already holds it
/// ([`define_asks_for_object`]). The answer is filed in the dictionary and,
/// with `record_ns`, as an initiating load (HotSpot's `update_dictionary`:
/// `L.findLoadedClass("java.lang.Object")` answers afterwards).
///
/// `Ok(None)`: answered (or nothing to ask). `Ok(Some(cause))`: the loader
/// did not answer `Object` -- `cause` is what it threw, if anything -- which
/// HotSpot reports as `NoClassDefFoundError: java/lang/Object`; the caller
/// decides (the best-effort route ignores it, the isolated route fails).
pub fn ask_defining_loader_for_object(
    ctx: &mut dyn NativeContext,
    loader_pin: usize,
    loader_obj: ObjectRef,
    loader_ns: u32,
    record_ns: Option<u32>,
) -> Result<Option<Option<ObjectRef>>, cratonvm_types::error::MethodCallFailed> {
    const OBJECT: &str = "java/lang/Object";
    if crate::classloader::supertype_link_known(ctx, loader_ns, OBJECT) {
        return Ok(None);
    }
    let name_str = ctx.create_string("java.lang.Object");
    let p_name = ctx.pin_native_root(name_str);
    let loader = ctx.read_native_pin(loader_pin, loader_obj);
    let name_str = ctx.read_native_pin(p_name, name_str);
    let result = crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, name_str);
    ctx.unpin_native_roots(p_name);
    match result {
        Ok(Some(Value::Object(Some(mirror)))) => {
            let answer = crate::lang_class::mirror_class_id(&*ctx, mirror);
            if answer.and_then(|id| ctx.class_name_of_id(id)).as_deref() != Some(OBJECT) {
                return Ok(Some(None));
            }
            crate::classloader::note_supertype_link(ctx, loader_ns, OBJECT, answer);
            if let Some(ns) = record_ns {
                note_supertype_initiation(ctx, ns, OBJECT, Some(mirror))?;
            }
            Ok(None)
        }
        Ok(_) => Ok(Some(None)),
        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e)) => Ok(Some(Some(e))),
        Err(other) => Err(other),
    }
}


/// The classes a defining loader's `loadClass` answered for the direct
/// supertypes of the class it defines (JIT round 20 lane superAcc, page
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// HotSpot links exactly those classes (`SystemDictionary::resolve_super_or_fail`
/// -> `L.loadClass(name)`). The define backend links supertypes by NAME: the
/// loader's own definition, then a class one of its recorded user ancestors
/// defined, then a loader-blind lookup -- which, for a loader that delegates
/// outside its parent chain (a sibling, a JBoss module, an OSGi bundle) while
/// another copy of the name exists, links a class the loader never named. The
/// pre-pass records each answer by identity and the define is handed them as
/// `superclass_id_override` / `interface_id_overrides`.
///
/// Handed over only when some answer was defined by a USER loader: a built-in
/// loader's answer is unique by name and is what the by-name link finds, so
/// the common define (JDK supertypes) keeps its path unchanged. Interfaces go
/// all or nothing (the overrides are positional): a supertype the pre-pass
/// skipped without an identity (`supertype_link_known`) leaves them to the
/// by-name link.
#[derive(Default)]
pub struct SupertypeAnswers {
    /// Per pre-pass supertype, in its order (the superclass first when it is
    /// not `java/lang/Object`, then the interfaces).
    pub ids: Vec<Option<cratonvm_types::ClassId>>,
    /// Is `ids[0]` the superclass?
    pub has_super: bool,
    /// The class file's interface count (the overrides' required length).
    pub interface_count: usize,
    /// Did a user-defined loader define one of the answers?
    pub user_answer: bool,
    /// The first supertype whose `loadClass` threw a
    /// `ClassNotFoundException`: `(internal name, cause class, cause
    /// message)` (JIT round 21 wave 26, lane nesting;
    /// [`supertype_miss_with_cause`]).
    pub miss_cause: Option<(String, String, Option<String>)>,
    /// Keep the loader's OWN `ClassNotFoundException` object for the cause
    /// (JIT round 22 wave 1, lane review;
    /// [`supertype_miss_cause_original_enabled`]). Set only by the two
    /// `defineClass` natives, which release [`Self::miss_root`] on every exit
    /// ([`Self::release_miss_root`]); other users never root anything.
    pub keep_miss_object: bool,
    /// The global root (`NativeContext::add_global_root`) holding that
    /// object, or `0`.
    pub miss_root: usize,
}

impl SupertypeAnswers {
    pub fn begin(&mut self, class_file: &cratonvm_reader::ClassFile, supertypes: usize) {
        self.ids = vec![None; supertypes];
        self.has_super = class_file
            .super_class
            .as_deref()
            .is_some_and(|s| !s.is_empty() && s != "java/lang/Object");
        self.interface_count = class_file.interfaces.len();
        self.user_answer = false;
        self.miss_cause = None;
    }

    /// The loader's `loadClass` threw `thrown` for the supertype `internal`.
    /// Only a `ClassNotFoundException` is kept, and only the first
    /// (`supertype_miss_with_cause`).
    pub fn note_miss(&mut self, ctx: &mut dyn NativeContext, internal: &str, thrown: ObjectRef) {
        if self.miss_cause.is_some() {
            return;
        }
        if !crate::classloader::object_extends(&*ctx, thrown, "java/lang/ClassNotFoundException") {
            return;
        }
        let Some(class) = ctx.class_name_of_id(ctx.class_id_of_object(thrown)) else {
            return;
        };
        let message = match ctx.get_field_by_name(thrown, "detailMessage") {
            Value::Object(Some(text)) => ctx.read_string(text),
            _ => None,
        };
        self.miss_cause = Some((internal.to_string(), class, message));
        // JIT round 22 wave 1, lane review: keep the thrown object itself
        // (HotSpot's `THROW_MSG_CAUSE` hands the loader's own exception, with
        // its `findClass` stack trace, to the `NoClassDefFoundError`). A global
        // root, because the native pin stack is truncated by the pre-pass's
        // own unpins before the define fails.
        if self.keep_miss_object && supertype_miss_cause_original_enabled() {
            self.release_miss_root(ctx);
            self.miss_root = ctx.add_global_root(thrown);
        }
    }

    /// Release [`Self::miss_root`] (idempotent).
    pub fn release_miss_root(&mut self, ctx: &mut dyn NativeContext) {
        if self.miss_root != 0 {
            let _ = ctx.remove_global_root(self.miss_root);
            self.miss_root = 0;
        }
    }

    /// The loader's own `ClassNotFoundException`, when kept
    /// ([`Self::keep_miss_object`]) and still rooted.
    pub fn miss_object(&self, ctx: &dyn NativeContext) -> Option<ObjectRef> {
        if self.miss_root == 0 {
            return None;
        }
        ctx.resolve_global_root(self.miss_root)
    }
}

/// `CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE_ORIGINAL` -- correctness, both
/// modes, default ON, `=0` rebuilds the cause from its class and message as
/// round 21 wave 26 did (JIT round 22 wave 1, lane review; page
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// the define's `NoClassDefFoundError` carries the defining loader's own
/// `ClassNotFoundException` object (identity and stack trace), as HotSpot's.
/// Read once per missed supertype.
pub fn supertype_miss_cause_original_enabled() -> bool {
    true
}



/// The define's `NoClassDefFoundError` for a supertype the defining loader
/// could not load, with that loader's `ClassNotFoundException` as its cause
/// (JIT round 21 wave 26, lane nesting;
/// `CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE`).
///
/// HotSpot: `resolve_super_or_fail` -> `resolve_or_fail` ->
/// `handle_resolution_exception` turns a pending `ClassNotFoundException`
/// into `NoClassDefFoundError: <internal name>` caused by it
/// (`THROW_MSG_CAUSE`). The pre-pass here swallows the loader's throw and
/// leaves the error to `define_class_full`, which names the supertype but has
/// no cause. Measured on HotSpot 25 (`R21Fz9ReviewEmUser1`'s $B, whose
/// EnclosingMethod class's superclass is missing): `getCause()` is the
/// `ClassNotFoundException` with the loader's message; CratonVM answered no
/// cause. Since JIT round 22 wave 1 (lane review,
/// `CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE_ORIGINAL`) the cause is the loader's
/// own object, kept under a global root by the define natives; with that
/// switch off, or with no root, it is a new throwable of the recorded class
/// and message (as the resolution-error table rebuilds it). Every other
/// failure is handed back unchanged.
pub fn supertype_miss_with_cause(
    ctx: &mut dyn NativeContext,
    failed: MethodCallFailed,
    answers: &SupertypeAnswers,
) -> MethodCallFailed {
    let Some((internal, cause_class, cause_message)) = &answers.miss_cause else {
        return failed;
    };
    let class_name = match &failed {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
            LinkageError::NoClassDefFoundError { class_name },
        )) if class_name.replace('.', "/") == *internal => class_name.clone(),
        _ => return failed,
    };
    // JIT round 22 wave 1, lane review: the loader's own object when it was
    // kept (`CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE_ORIGINAL`), else rebuilt.
    let cause = match answers.miss_object(&*ctx) {
        Some(original) => original,
        None => match cause_message {
            Some(message) => match crate::phases_early::throw_jca_exc(ctx, cause_class, message) {
                MethodCallFailed::ExceptionThrown(cause) => cause,
                _ => return failed,
            },
            None => match ctx.new_object_initialized(cause_class, "()V", &[]) {
                Ok(Some(Value::Object(Some(cause)))) => cause,
                _ => return failed,
            },
        },
    };
    let cause_pin = ctx.pin_native_root(cause);
    let error =
        crate::phases_early::throw_jca_exc(ctx, "java/lang/NoClassDefFoundError", &class_name);
    let cause = ctx.read_native_pin(cause_pin, cause);
    ctx.unpin_native_roots(cause_pin);
    match error {
        MethodCallFailed::ExceptionThrown(made)
            if ctx.class_name_of_id(ctx.class_id_of_object(made)).as_deref()
                == Some("java/lang/NoClassDefFoundError") =>
        {
            ctx.set_field_by_name(made, "cause", Value::Object(Some(cause)));
            MethodCallFailed::ExceptionThrown(made)
        }
        _ => failed,
    }
}

impl SupertypeAnswers {
    /// A supertype the pre-pass did not ask the loader for: the class it
    /// already holds for it, when known.
    pub fn note_present(&mut self, slot: usize, id: Option<cratonvm_types::ClassId>) {
        if let Some(cell) = self.ids.get_mut(slot) {
            *cell = id;
        }
    }

    /// A supertype the pre-pass did not ask the loader for because its
    /// dictionary row (`classloader::supertype_link_answer`) holds the class
    /// the loader answered before: linked by identity, as the answer of an
    /// ask is (interpreter round i4 wave 96, lane L5; `L5W86ModuleLayers`).
    pub fn note_known_answer(
        &mut self,
        ctx: &dyn NativeContext,
        slot: usize,
        id: cratonvm_types::ClassId,
    ) {
        if let Some(cell) = self.ids.get_mut(slot) {
            *cell = Some(id);
        }
        if ctx.loader_id_of_class(id) >= 3 {
            self.user_answer = true;
        }
    }

    /// The loader answered `mirror` for `internal`. HotSpot's
    /// `load_instance_class_impl` takes the answer only when its name is the
    /// requested one, else the supertype resolution fails with
    /// `NoClassDefFoundError: <internal name>`; that error is returned here.
    pub fn note_answer(
        &mut self,
        ctx: &dyn NativeContext,
        slot: usize,
        internal: &str,
        mirror: ObjectRef,
    ) -> Result<(), cratonvm_types::error::MethodCallFailed> {
        let Some(id) = crate::lang_class::mirror_class_id(ctx, mirror) else {
            return Ok(());
        };
        let Some(answered) = ctx.class_name_arc_of_id(id) else {
            return Ok(());
        };
        let same_name = answered.len() == internal.len()
            && answered
                .bytes()
                .zip(internal.bytes())
                .all(|(a, b)| a == b || (a == b'.' && b == b'/'));
        if !same_name {
            return Err(LinkageError::NoClassDefFoundError {
                class_name: internal.to_string(),
            }
            .into());
        }
        if let Some(cell) = self.ids.get_mut(slot) {
            *cell = Some(id);
        }
        if ctx.loader_id_of_class(id) >= 3 {
            self.user_answer = true;
        }
        Ok(())
    }

    /// The define's `(superclass_id_override, interface_id_overrides)`.
    pub fn overrides(
        &self,
    ) -> (
        Option<cratonvm_types::ClassId>,
        Option<Vec<cratonvm_types::ClassId>>,
    ) {
        if !self.user_answer {
            return (None, None);
        }
        let (superclass, interfaces) = match (self.has_super, self.ids.split_first()) {
            (true, Some((first, rest))) => (*first, rest),
            _ => (None, &self.ids[..]),
        };
        let interfaces = if !interfaces.is_empty() && interfaces.len() == self.interface_count {
            interfaces
                .iter()
                .copied()
                .collect::<Option<Vec<cratonvm_types::ClassId>>>()
        } else {
            None
        };
        (superclass, interfaces)
    }
}

/// What the define natives' supertype pre-pass hands the define under
/// `CRATONVM_DEFINE_ONE_PARSE` (interpreter round i3 wave 72, lane L5): the
/// shared copy of the class file's bytes it parsed, and that parse. The
/// define caches the same copy as the class's file, so the lazy attributes
/// and the cached bytes keep sharing one allocation as when it parses itself.
/// The vm's `define_class_full` downcasts to exactly this type.
pub type PreparsedDefine = (std::sync::Arc<[u8]>, cratonvm_reader::ClassFile);

/// The pre-pass's parse of `bytes`: [`cratonvm_reader::read_class`] spelled
/// out (`read_class` is `read_class_arc(Arc::from(bytes))`), keeping the copy
/// for [`PreparsedDefine`]. Same answer and same cost as `read_class`.
pub fn parse_for_supertype_link(
    bytes: &[u8],
) -> Result<PreparsedDefine, cratonvm_reader::ClassReaderError> {
    // `CRATONVM_DBG_DEFINE_CENSUS`: the `prepass-parse` row (wave 93, L5).
    let parse_clock = cratonvm_classloading::define_census::phase_start();
    let source: std::sync::Arc<[u8]> = std::sync::Arc::from(bytes);
    let class_file = cratonvm_reader::read_class_arc(std::sync::Arc::clone(&source));
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::PrepassParse,
        parse_clock,
    );
    Ok((source, class_file?))
}

/// The supertype pre-pass's parse as [`cratonvm_native_api::DefineClassFull::preparsed`]
/// (`CRATONVM_DEFINE_ONE_PARSE`): the define takes it instead of parsing the
/// same bytes again. `None` parses.
pub fn preparsed_for_define(
    parse: Option<PreparsedDefine>,
) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> {
    parse.map(|parsed| {
        std::sync::Arc::new(parsed) as std::sync::Arc<dyn std::any::Any + Send + Sync>
    })
}

/// [`link_supertypes_through_defining_loader`] for the class `name` (internal
/// form; empty when the caller passed no name), refusing a define of `name`
/// that this thread is already linking the supertypes of for the same loader:
/// a circular hierarchy (see [`SUPERTYPE_LINKS_IN_PROGRESS`]). `Ok(Some(..))`
/// is the pre-pass's parse of `bytes`, kept only when `keep_parse`
/// (`CRATONVM_DEFINE_ONE_PARSE`; interpreter round i3 wave 72, lane L5).
pub fn link_supertypes_refusing_circularity(
    ctx: &mut dyn NativeContext,
    loader_pin: usize,
    loader_obj: ObjectRef,
    name: &str,
    bytes: &[u8],
    keep_parse: bool,
    answers: &mut SupertypeAnswers,
) -> Result<Option<PreparsedDefine>, cratonvm_types::error::MethodCallFailed> {
    if name.is_empty() {
        return link_supertypes_through_defining_loader(
            ctx, loader_obj, name, bytes, keep_parse, answers,
        );
    }
    let circular = SUPERTYPE_LINKS_IN_PROGRESS.with(|links| {
        links.borrow().iter().any(|(pin, obj, linking)| {
            linking == name && ctx.read_native_pin(*pin, *obj).as_ptr() == loader_obj.as_ptr()
        })
    });
    if circular {
        return Err(LinkageError::ClassCircularityError {
            class_name: name.to_string(),        }
        .into());
    }
    SUPERTYPE_LINKS_IN_PROGRESS.with(|links| {
        links
            .borrow_mut()
            .push((loader_pin, loader_obj, name.to_string()))
    });
    let result =
        link_supertypes_through_defining_loader(ctx, loader_obj, name, bytes, keep_parse, answers);
    SUPERTYPE_LINKS_IN_PROGRESS.with(|links| {
        links.borrow_mut().pop();
    });
    result
}

/// Resolve direct hierarchy edges through an isolated URL loader before its
/// class is handed to the backend linker. Unlike the generic best-effort
/// helper above, a miss is authoritative: a platform-parented loader cannot
/// fall back to the process application classpath.
pub fn preload_isolated_loader_supertypes(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    bytes: &[u8],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let class_file = match cratonvm_reader::read_class(bytes) {
        Ok(class_file) => class_file,
        Err(_) => return Ok(()),
    };
    let mut names: Vec<String> = Vec::new();
    if let Some(super_name) = &class_file.super_class {
        if !super_name.is_empty() && &**super_name != "java/lang/Object" {
            names.push(super_name.to_string());
        }
    }
    names.extend(
        class_file
            .interfaces
            .iter()
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string()),
    );
    let loader_pin = ctx.pin_native_root(loader_obj);
    let mut loader = loader_obj;
    for internal_name in names {
        loader = ctx.read_native_pin(loader_pin, loader);
        let dotted_name = ctx.create_string(&internal_name.replace('/', "."));
        let name_pin = ctx.pin_native_root(dotted_name);
        loader = ctx.read_native_pin(loader_pin, loader);
        let dotted_name = ctx.read_native_pin(name_pin, dotted_name);
        // Under the loader's monitor when it is not parallel-capable
        // (`--real-jdk`), as a VM-initiated load (wave 29, lane L5).
        let result = crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, dotted_name);
        ctx.unpin_native_roots(name_pin);
        match result {
            Ok(Some(Value::Object(Some(_)))) => {}
            Err(error) => {
                ctx.unpin_native_roots(loader_pin);
                return Err(error);
            }
            _ => {
                let exception = crate::jboss_module_loader::alloc_single_message_exception(
                    ctx,
                    "java/lang/ClassNotFoundException",
                    1,
                    &internal_name,
                );
                ctx.unpin_native_roots(loader_pin);
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exception?,
                ));
            }
        }
    }
    ctx.unpin_native_roots(loader_pin);
    Ok(())
}

/// Link a class's direct supertypes through the loader defining it, before the
/// backend links them from the flat class store.
///
/// Under `--real-jdk` an ISOLATED `URLClassLoader` (null or platform parent)
/// has the final word on them, as HotSpot's linker gives it: a supertype the
/// loader refuses fails the definition with `NoClassDefFoundError: <name>`,
/// caused by the loader's own exception. Any other loader, and every loader
/// under `--compatible`, keeps the best-effort preload
/// ([`preload_supertypes_via_loader`]), whose misses fall back to the flat
/// store.
///
/// The best-effort preload was all the `defineClass` natives had. It was
/// harmless while the VM defined isolated loaders' classes itself
/// (`ucl_try_define_local_class`, which calls
/// [`preload_isolated_loader_supertypes`]). Once the `URLClassLoader`
/// retirement handed that job to the loader's real `findClass`, Spring Boot's
/// `ModifiedClassPathClassLoader` with `jackson-core` excluded DEFINED
/// `ObjectMapper` over the application's `ObjectCodec`, where HotSpot throws.
/// See
/// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
pub fn link_supertypes_through_defining_loader(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    name: &str,
    bytes: &[u8],
    keep_parse: bool,
    answers: &mut SupertypeAnswers,
) -> Result<Option<PreparsedDefine>, cratonvm_types::error::MethodCallFailed> {
    // `CRATONVM_DEFINE_PREPASS_OBJECT_ONLY` (interpreter round i4 wave 93,
    // lane L5; default off): for a user-defined loader, parse first. A class
    // that names no supertype but `java/lang/Object` has nothing for either
    // route below to ask the loader, and both answer the same for it -- the
    // parse (kept with `keep_parse`) and no cross-loader answers -- so the
    // isolation probe (`url_classloader_isolated_from_app`: superclass-name
    // walks, the loader's URL stash and `parent` by name) is skipped. Any
    // other class hands this parse to the route it takes, so it is never
    // parsed twice. A loader that is not user-defined keeps the old order
    // (the preload route returns before parsing for it).
    let mut parsed: Option<PreparsedDefine> = None;
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DEFINE_PREPASS_OBJECT_ONLY")
        && crate::classloader::is_user_defined_loader(ctx, loader_obj)
    {
        let (source, class_file) = match parse_for_supertype_link(bytes) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(None), // malformed bytes: `define_class_full` reports them
        };
        let object_only = class_file
            .super_class
            .as_ref()
            .is_none_or(|s| s.is_empty() || &**s == "java/lang/Object")
            && class_file.interfaces.iter().all(|i| i.is_empty());
        if object_only && !define_asks_for_object(&*ctx, &class_file) {
            // No supertype to ask about: `answers` stays empty, as the
            // preload route leaves it for such a class.
            return Ok(keep_parse.then_some((source, class_file)));
        }
        parsed = Some((source, class_file));
    }
    let isolation_clock = cratonvm_classloading::define_census::phase_start();
    let isolated = ctx.jdk_url_class_loaders()
        && crate::classloader::url_classloader_isolated_from_app(ctx, loader_obj);
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::PrepassIsolation,
        isolation_clock,
    );
    if !isolated {
        return preload_supertypes_via_loader(ctx, loader_obj, bytes, keep_parse, parsed, answers);
    }
    let parse = match parsed {
        Some(parsed) => Ok(parsed),
        None => parse_for_supertype_link(bytes),
    };
    let (source, class_file) = match parse {
        Ok(parsed) => parsed,
        Err(_) => return Ok(None), // malformed bytes: `define_class_full` reports them
    };
    // HotSpot refuses these flags (and a wrong name) while it parses, before
    // it resolves a supertype: no `loadClass` for them; `define_class_full`
    // raises the refusal (interpreter round i4 wave 84, lane L5). Judged in
    // the define's own scope, `-Xverify:none` included (t1 wave 13, lane
    // I24W): a class that define accepts gets its supertype `loadClass`es.
    let relaxation = ctx.define_format_relaxation();
    if cratonvm_classloading::define_refused_while_parsing(&class_file, name, relaxation) {
        return Ok(keep_parse.then_some((source, class_file)));
    }
    let mut names: Vec<String> = Vec::new();
    if let Some(super_name) = &class_file.super_class {
        if !super_name.is_empty() && &**super_name != "java/lang/Object" {
            names.push(super_name.to_string());
        }
    }
    names.extend(
        class_file
            .interfaces
            .iter()
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string()),
    );
    let loader_pin = ctx.pin_native_root(loader_obj);
    // `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY` (interpreter round i3 wave 72,
    // lane L5): HotSpot's `resolve_super_or_fail` asks the loader's
    // `loadClass` only for a name its dictionary does not hold yet -- one the
    // loader neither defined nor answered before -- so a supertype shared by
    // many of the loader's classes is asked for once, not once per define.
    let dictionary_ns = if ctx.define_supertype_dictionary() {
        let loader = ctx.read_native_pin(loader_pin, loader_obj);
        Some(crate::classloader::loader_namespace_id(ctx, loader))
    } else {
        None
    };
    // `CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION` (wave 79, L5): see
    // `supertype_initiation_record_ns`.
    let record_ns = {
        let loader = ctx.read_native_pin(loader_pin, loader_obj);
        supertype_initiation_record_ns(ctx, loader)
    };
    answers.begin(&class_file, names.len());
    // JIT round 23 wave 10 (`CRATONVM_DEFINE_INTERFACES_BEFORE_SUPER`): ask in
    // HotSpot's order; `slot` keeps the pre-pass's own order for `answers`.
    let has_super = class_file
        .super_class
        .as_deref()
        .is_some_and(|s| !s.is_empty() && s != "java/lang/Object");
    for slot in supertype_ask_order(names.len(), has_super) {
        let internal_name = names[slot].clone();
        if let Some(ns) = dictionary_ns {
            let own = ctx.class_id_defined_by_loader_exact(&internal_name, ns);
            if own.is_some() {
                answers.note_present(slot, own);
                continue;
            }
            if crate::classloader::supertype_link_known(ctx, ns, &internal_name) {
                // Linked by the identity the row kept (wave 96, lane L5).
                match crate::classloader::supertype_link_answer(ctx, ns, &internal_name) {
                    Some(id) => answers.note_known_answer(&*ctx, slot, id),
                    None => answers.note_present(slot, None),
                }
                continue;
            }
        }
        let dotted_name = ctx.create_string(&internal_name.replace('/', "."));
        let loader = ctx.read_native_pin(loader_pin, loader_obj);
        // Under the loader's monitor when it is not parallel-capable
        // (`--real-jdk`), as a VM-initiated load (wave 29, lane L5).
        let result = crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, dotted_name);
        let cause = match result {
            // JIT round 20 lane superAcc: the answer is the supertype the
            // define links; an answer of another name is HotSpot's
            // `NoClassDefFoundError` (`load_instance_class_impl` returns null).
            Ok(Some(Value::Object(Some(mirror))))
                if answers
                    .note_answer(&*ctx, slot, &internal_name, mirror)
                    .is_err() =>
            {
                None
            }
            Ok(Some(Value::Object(Some(mirror)))) => {
                if let Some(ns) = dictionary_ns {
                    let answer = crate::lang_class::mirror_class_id(&*ctx, mirror);
                    crate::classloader::note_supertype_link(ctx, ns, &internal_name, answer);
                }
                if let Some(ns) = record_ns {
                    if let Err(e) = note_supertype_initiation(ctx, ns, &internal_name, Some(mirror))
                    {
                        ctx.unpin_native_roots(loader_pin);
                        return Err(e);
                    }
                }
                continue;
            }
            // A circular hierarchy leaves as itself (see
            // `preload_supertypes_via_loader`), not as a missing supertype.
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exception))
                if is_class_circularity_error(ctx, exception) =>
            {
                ctx.unpin_native_roots(loader_pin);
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exception,
                ));
            }
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exception)) => {
                Some(exception)
            }
            Err(other) => {
                ctx.unpin_native_roots(loader_pin);
                return Err(other);
            }
            Ok(_) => None,
        };
        let cause_pin = cause.map(|c| (ctx.pin_native_root(c), c));
        let error = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/NoClassDefFoundError",
            1,
            &internal_name,
        );
        // Typed so the audit starts `error` here: it is used below with no GC
        // point in between (w16-e triage; the flat scanner read the return
        // type's `error::` path as an argument named `error`).
        let error: ObjectRef = match error {
            Ok(error) => error,
            Err(e) => {
                ctx.unpin_native_roots(loader_pin);
                return Err(e);
            }
        };
        if let Some((pin, cause)) = cause_pin {
            let cause = ctx.read_native_pin(pin, cause);
            ctx.set_field_by_name(error, "cause", Value::Object(Some(cause)));
        }
        ctx.unpin_native_roots(loader_pin);
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            error,
        ));
    }
    // `CRATONVM_DEFINE_ASKS_FOR_OBJECT`: an `Object` superclass, last. The
    // loader has the final word here too: a refusal is HotSpot's
    // `NoClassDefFoundError: java/lang/Object`, caused by what it threw.
    if let (Some(ns), true) = (dictionary_ns, define_asks_for_object(&*ctx, &class_file)) {
        let refused =
            match ask_defining_loader_for_object(ctx, loader_pin, loader_obj, ns, record_ns) {
                Ok(refused) => refused,
                Err(e) => {
                    ctx.unpin_native_roots(loader_pin);
                    return Err(e);
                }
            };
        if let Some(cause) = refused {
            let cause_pin = cause.map(|c| (ctx.pin_native_root(c), c));
            let error = crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/NoClassDefFoundError",
                1,
                "java/lang/Object",
            );
            let error: ObjectRef = match error {
                Ok(error) => error,
                Err(e) => {
                    ctx.unpin_native_roots(loader_pin);
                    return Err(e);
                }
            };
            if let Some((pin, cause)) = cause_pin {
                let cause = ctx.read_native_pin(pin, cause);
                ctx.set_field_by_name(error, "cause", Value::Object(Some(cause)));
            }
            ctx.unpin_native_roots(loader_pin);
            return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                error,
            ));
        }
    }
    ctx.unpin_native_roots(loader_pin);
    Ok(keep_parse.then_some((source, class_file)))
}

/// What a backend "already defined" failure means for the `defineClass` call
/// that provoked it.
///
/// The backend probe is keyed on `(loader_id, name)` where `loader_id` is a
/// synthetic NAMESPACE number
/// (`class_manager.rs::define_class_shared_with_options`), so the error alone
/// cannot tell the two cases below apart. Only the DEFINING LOADER OBJECT can.
pub enum DuplicateDefine {
    /// Not an "already defined" failure — the caller keeps its own error path.
    NotDuplicate,
    /// **This exact loader object has already defined this name.** JVMS §5.3.5
    /// forbids it and HotSpot raises `java.lang.LinkageError`.
    SameLoaderObject,
    /// A class of this name exists and this loader did NOT define it — a
    /// namespace collision inside CratonVM's flat store, or a delegation gap
    /// that re-reached `findClass`. Serve the existing mirror.
    ServeExisting(ObjectRef),
}

/// Decide which of the two an "already defined" backend error is.
///
/// # Why this exists, and why it is not simply "delete the tolerance"
///
/// Until 2026-08-11 every "already defined" error served the existing mirror,
/// so `defineClass` NEVER raised the `LinkageError` HotSpot raises for a
/// duplicate definition — measured in both modes with a `ClassLoader` subclass
/// calling `defineClass(name, bytes, 0, len)` twice.
///
/// The tolerance is not decoration, though, and the case it covers is real:
/// during a Tomcat webapp stop/start loop the ~14th `WebappClassLoader` in a
/// process began colliding with an earlier, already-finished one over
/// `org/apache/catalina/loader/JdbcLeakPrevention`, and surfacing the backend
/// error there broke the container lifecycle. On HotSpot those are two
/// DIFFERENT loaders, each entitled to its own copy; what collided was
/// CratonVM's namespace numbering, not the loaders.
///
/// So the discriminator is loader-OBJECT identity, and the two shapes separate
/// cleanly:
///
/// | shape | HotSpot | here |
/// |---|---|---|
/// | same loader object defines a name twice | `LinkageError` | `SameLoaderObject` |
/// | two distinct loaders, one namespace | both succeed | `ServeExisting` |
///
/// # The bias, stated
///
/// `SameLoaderObject` is only ever returned on a POSITIVE identification. The
/// defining-loader record is an `ObjectRef` and a moving collection can leave a
/// stale pointer, so "not recorded" and "recorded elsewhere" both fall to
/// `ServeExisting`. A missed `LinkageError` is what this VM did yesterday; a
/// spurious one is a new way to break a workload that was working.
/// Is a backend define error a duplicate definition of `internal_name`?
/// Either spelling counts (see `classify_duplicate_define`).
pub fn is_duplicate_define_message(internal_name: &str, msg: &str) -> bool {
    !internal_name.is_empty()
        && (msg.contains("already defined") || msg.contains("DuplicateClassDefinition"))
}

/// Is the running `ClassLoader.defineClass1` reached from
/// `BuiltinClassLoader.defineClass`, through nothing but the `ClassLoader` /
/// `SecureClassLoader` define chain?
pub fn called_from_builtin_loader_define(ctx: &dyn NativeContext) -> bool {
    cratonvm_native_api::find_in_caller_frames(ctx, |id| {
        match ctx.class_name_of_id(id).as_deref() {
            Some("jdk/internal/loader/BuiltinClassLoader") => std::ops::ControlFlow::Break(true),
            Some("java/lang/ClassLoader") | Some("java/security/SecureClassLoader") => {
                std::ops::ControlFlow::Continue(())
            }
            _ => std::ops::ControlFlow::Break(false),
        }
    })
    .unwrap_or(false)
}

pub fn classify_duplicate_define(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    internal_name: &str,
    loader_id: u32,
    msg: &str,
) -> DuplicateDefine {
    // Both spellings: the backend has raised `DuplicateClassDefinition`
    // (whose `Debug` text `define_class_full` hands back) since 2026-08-28;
    // "already defined" is the older ICCE wording other producers still use.
    // Matching only the old words made every arm below dead code (gc-common
    // w11-e).
    if !is_duplicate_define_message(internal_name, msg) {
        return DuplicateDefine::NotDuplicate;
    }
    // `CRATONVM_DBG_DUPDEF=1` -- name every "already defined" backend error and
    // the verdict it got. This exists because the two verdicts are otherwise
    // indistinguishable from outside: a workload that never reaches
    // `ServeExisting` proves nothing about the tolerance still working, and one
    // that never reaches `SameLoaderObject` proves nothing about the refusal.
    // Both arms print, so a probe can assert it exercised the arm it claims to.
    let dbg = None::<std::ffi::OsString>.is_some();
    // zt t16/df6 (define page D6): the name is held by THIS loader object's
    // own define whose `addClass` upcall is still in flight (a define that
    // raced past `wait_for_unbound_define`). That class is in no by-name
    // index until its upcall returns, so the questions below cannot see it;
    // its defining-loader row names the loader. HotSpot's second definer
    // would have waited and then met the duplicate definition (or defined,
    // had the first failed); the duplicate definition is the answer the
    // define gave before the late binding existed.
    if let Some(unbound) = ctx.unbound_define_of(internal_name, loader_id) {
        let tables = crate::classloader::loader_tables(ctx);
        if crate::classloader::defining_loader_for(tables, unbound.as_u32())
            .is_some_and(|def| def.as_ptr() == loader_obj.as_ptr())
        {
            if dbg {
                eprintln!(
                    "[DUPDEF] SameLoaderObject(unbound) {internal_name} loader_id={loader_id}"
                );
            }
            return DuplicateDefine::SameLoaderObject;
        }
    }
    // THE one question that may raise: did this exact object define it?
    if crate::classloader::class_defined_by_this_loader_object(ctx, loader_obj, internal_name)
        .is_some()
    {
        if dbg {
            eprintln!("[DUPDEF] SameLoaderObject {internal_name} loader_id={loader_id}");
        }
        return DuplicateDefine::SameLoaderObject;
    }
    // Interpreter round i5 wave 101, lane L5: the BUILT-IN application or
    // platform loader re-defining a name it already defined
    // (`L5W99ModuleVersionClauses` `dup`: `Lookup.defineClass` of `app.Box`'s
    // own bytes, `app.Box` in module `app` of loader 'app'; HotSpot's
    // `LinkageError: loader 'app' attempted duplicate class definition ...`).
    // A built-in loader's classes carry no defining-loader row, so the object
    // question above never sees them, and the tolerance arms below served the
    // existing class. Positive identification only, as above: the existing
    // class must be one the VM KNOWS this built-in loader defined.
    let builtin_ordinal = if crate::classloader::is_application_class_loader(ctx, loader_obj) {
        Some(cratonvm_types::ClassLoaderId::NATIVE_APPLICATION)
    } else if crate::classloader::is_platform_class_loader(ctx, loader_obj) {
        Some(cratonvm_types::ClassLoaderId::NATIVE_EXTENSION)
    } else {
        None
    };
    if let Some(ordinal) = builtin_ordinal {
        let existing = crate::classloader::find_loaded_class_for_loader(ctx, loader_obj, internal_name)
            .and_then(|mirror| ctx.class_id_from_mirror(mirror));
        if let Some(cid) = existing {
            if ctx.loader_id_of_class_definite(cid) == Some(ordinal as i32)
                && crate::classloader::defining_loader_for(
                    crate::classloader::loader_tables(ctx),
                    cid.as_u32(),
                )
                .is_none()
            {
                // `BuiltinClassLoader.loadClassOrNull` defines under the
                // per-name `getClassLoadingLock`, so on HotSpot a define it
                // makes of a name the loader already holds cannot happen. Here
                // the VM resolves a supertype of another class natively, with
                // no Java lock, so a Java-level define can lose that race
                // (Groovy's parallel `loadDgmInfo` vs `groovy.lang.Closure`).
                // Serve the winner; only a define that does NOT come from the
                // built-in loader's own `defineClass` (`Lookup.defineClass`)
                // is a genuine duplicate.
                if called_from_builtin_loader_define(ctx) {
                    if dbg {
                        eprintln!(
                            "[DUPDEF] ServeExisting(built-in loader race) {internal_name} loader_id={loader_id}"
                        );
                    }
                    return DuplicateDefine::ServeExisting(ctx.get_class_mirror(cid));
                }
                if dbg {
                    eprintln!(
                        "[DUPDEF] SameLoaderObject(built-in loader) {internal_name} loader_id={loader_id}"
                    );
                }
                return DuplicateDefine::SameLoaderObject;
            }
        }
    }
    // gc-common w11-e: the name collides with a class in this namespace whose
    // recorded defining loader was collected. That can only be a
    // built-in-namespace class (user namespaces unload with their loader),
    // and its orphan marker hides it from every live loader, so none of the
    // probes below can see it and the define would fail with `LinkageError`.
    // HotSpot would define a fresh copy. CratonVM cannot hold two classes
    // of one name in one namespace, so the new loader adopts the existing
    // class. `register_defining_loader` clears the orphan marker, so the
    // adopter (and its children) can see it again.
    if let Some(class_id) = ctx.class_id_defined_by_loader_exact(internal_name, loader_id) {
        if crate::classloader::is_defining_loader_orphaned(
            crate::classloader::loader_tables(ctx),
            class_id.as_u32(),
        ) {
            if dbg {
                eprintln!(
                    "[DUPDEF] ServeExisting(adopted orphan) {internal_name} loader_id={loader_id}"
                );
            }
            crate::classloader::register_defining_loader(
                crate::classloader::loader_tables(ctx),
                class_id.as_u32(),
                loader_obj,
            );
            return DuplicateDefine::ServeExisting(ctx.get_class_mirror(class_id));
        }
    }
    // A namespace hit whose recorded defining loader IS this object is the same
    // fact reached the other way round — the record can be keyed by namespace
    // before the object association is made.
    if loader_id != 0 {
        if let Some(class_id) = ctx.class_id_defined_by_loader_exact(internal_name, loader_id) {
            let same =
                crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), class_id.as_u32())
                    .is_some_and(|def| def.as_ptr() == loader_obj.as_ptr());
            if same {
                if dbg {
                    eprintln!(
                        "[DUPDEF] SameLoaderObject(via namespace) {internal_name} loader_id={loader_id}"
                    );
                }
                return DuplicateDefine::SameLoaderObject;
            }
            // A namespace hit this loader did not define: the collision shape.
            if dbg {
                eprintln!(
                    "[DUPDEF] ServeExisting(namespace collision) {internal_name} loader_id={loader_id}"
                );
            }
            crate::classloader::register_defining_loader(
                crate::classloader::loader_tables(ctx),
                class_id.as_u32(),
                loader_obj,
            );
            return DuplicateDefine::ServeExisting(ctx.get_class_mirror(class_id));
        }
    }
    match crate::classloader::find_loaded_class_for_loader(ctx, loader_obj, internal_name) {
        Some(mirror) => {
            if dbg {
                eprintln!("[DUPDEF] ServeExisting(visible) {internal_name} loader_id={loader_id}");
            }
            DuplicateDefine::ServeExisting(mirror)
        }
        None => {
            if dbg {
                eprintln!("[DUPDEF] NotDuplicate {internal_name} loader_id={loader_id}");
            }
            DuplicateDefine::NotDuplicate
        }
    }
}

/// `CRATONVM_DEFINE_NAMELESS_DUPLICATE_TEXT` (zt t15/x12,
/// `zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 6; default ON, `=0` kill;
/// every collector, both modes): a `defineClass1` / `defineClass2` with a null
/// name whose bytes this very loader object has already defined raises
/// [`duplicate_define_error`] for the class the bytes name (`this_class`), as
/// the named path does: HotSpot 25 names the class from the bytes, so its
/// `LinkageError` reads `loader X attempted duplicate class definition for
/// a.b.C. (a.b.C is in unnamed module of loader X ...)` either way. Before,
/// the nameless arm could not classify (no caller name) and fell to
/// `define_class_linkage_error("")`, without the parenthetical. Only the exact
/// loader-object answer (`class_defined_by_this_loader_object`) decides,
/// which touches no table; every other verdict keeps the old refusal. `None`:
/// not such a duplicate, unreadable bytes, or the switch off.
pub fn nameless_duplicate_define_error(
    ctx: &mut dyn NativeContext,
    loader_pin: (usize, ObjectRef),
    bytes: &[u8],
    msg: &str,
) -> Option<MethodCallFailed> {
    if !(msg.contains("DuplicateClassDefinition") || msg.contains("already defined"))
    {
        return None;
    }
    let internal_name = crate::classloader::extract_this_class_name(bytes)?;
    let loader_obj = ctx.read_native_pin(loader_pin.0, loader_pin.1);
    crate::classloader::class_defined_by_this_loader_object(ctx, loader_obj, &internal_name)?;
    // Re-read, as the named arm does after its classification.
    let loader_obj = ctx.read_native_pin(loader_pin.0, loader_pin.1);
    Some(duplicate_define_error(ctx, loader_obj, &internal_name))
}


/// zt t25/df11b (define page §9d): a define in the `Extension` namespace (the
/// platform loader object, [`define_loader_namespace`]) of a loaded runtime-image
/// class of a platform module (`classloader::platform_image_class_loaded`):
/// HotSpot's duplicate-definition `LinkageError`, named after the loader, where
/// CratonVM defined a second class of the name (the image class carries the boot
/// tag, so the class manager's `Extension` duplicate probe misses it). `None`
/// for every other define.
pub fn platform_image_duplicate_refusal(
    ctx: &mut dyn NativeContext,
    loader: Option<&Value>,
    loader_id: u32,
    internal_name: &str,
) -> Option<MethodCallFailed> {
    if loader_id != cratonvm_types::ClassLoaderId::NATIVE_EXTENSION {
        return None;
    }
    let Some(Value::Object(Some(loader_obj))) = loader.copied() else {
        return None;
    };
    // The module-table read can run Java on a package's first ask.
    let pin = ctx.pin_native_root(loader_obj);
    let image = crate::classloader::platform_image_class_loaded(ctx, internal_name);
    let loader_obj = ctx.read_native_pin(pin, loader_obj);
    ctx.unpin_native_roots(pin);
    image?;
    Some(duplicate_define_error(ctx, loader_obj, internal_name))
}

/// The `LinkageError` a duplicate definition raises, named after the loader the
/// way HotSpot names it.
pub fn duplicate_define_error(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    internal_name: &str,
) -> MethodCallFailed {
    // `--real-jdk`: HotSpot's whole text, from the loader's `nameAndId` and
    // its parent's (interpreter round i1 wave 38, lane L5; probe
    // `L5W38DuplicateDefineMessage`), and `--compatible` with
    // `CRATONVM_COMPAT_MODULE_MESSAGES` (wave 49, lane L6). Built before the
    // allocation, which can move `loader_obj`.
    if ctx.messages_name_user_loaders() {
        if let Some(message) =
            crate::classloader_real::duplicate_definition_message(ctx, loader_obj, internal_name)
        {
            return match crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/LinkageError",
                1,
                &message,
            ) {
                Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
                Err(e) => e,
            };
        }
    }
    // HotSpot prints `<loader class name> @<identity hash>`. The class name is
    // the part that identifies the loader to a reader; the hash is why no test
    // may assert the whole string.
    let loader = ctx
        .class_name_of_id(ctx.class_id_of_object(loader_obj))
        .map(|n| n.replace('/', "."))
        .unwrap_or_else(|| "<unknown>".to_string());
    LinkageError::DuplicateClassDefinition {
        class_name: internal_name.to_string(),
        loader,
    }
    .into()
}

pub fn native_classloader_define_class1(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: loader(0), name(1), bytes(2), offset(3), length(4), pd(5), source(6)
    // `CRATONVM_DBG_DEFINE_CENSUS` phase clocks (interpreter round i4 wave 88,
    // lane L5).
    let entry_clock = cratonvm_classloading::define_census::phase_start();
    // `native-total`: this whole body, every exit (wave 95, lane L5).
    let _census_total = cratonvm_classloading::define_census::phase_guard(
        cratonvm_classloading::define_census::DefinePhase::NativeTotal,
        entry_clock,
    );
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => {
            let n = ctx.read_string(*s).unwrap_or_default();
            // JDK uses dot-separated names; convert to slash-separated
            n.replace('.', "/")
        }
        _ => String::new(),
    };
    if let Ok(filter) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        if !filter.is_empty() && name.contains(filter.as_str()) {
            eprintln!("[DBG_DEFINE_STACK] defineClass1({name})");
            for entry in ctx.capture_stack_trace() {
                eprintln!("    at {}.{}", entry.class_name, entry.method_name);
            }
        }
    }

    // `--real-jdk`: HotSpot's exceptions for the arguments and the header
    // (interpreter round i1 wave 42, lane L5; probe `L5W42DefineClassErrors`),
    // and `--compatible` with `CRATONVM_COMPAT_MODULE_MESSAGES` (wave 49, lane
    // L6). Only message texts and exception types hang off this.
    let jdk_only = ctx.messages_name_user_loaders();
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::NullPointerException {
                // `JNU_ThrowNullPointerException(env, 0)`: a null message.
                message: (!jdk_only).then(|| "defineClass1: byte[] is null".to_string()),
            }
            .into());
        }
    };

    if jdk_only {
        let raw = |idx: usize| match args.get(idx) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        if let Some(err) =
            jdk_only_define_region_error(raw(3), raw(4), ctx.array_length(byte_array))
        {
            return Err(err);
        }
    }
    let offset = read_define_class_nonnegative_int(args, 3)?;
    let length = read_define_class_nonnegative_int(args, 4)?;
    let bytes = read_byte_array_define_class_slice(ctx, byte_array, offset, length)?;
    // zt t16/pl7: `JVM_DefineClassWithSource` counts the define and its
    // bytes before parsing (`sun.cls.defineAppClasses` / `appClassBytes`).
    let counted = ctx.note_jvm_define_class(bytes.len());
    // zt t16/pl7: and times it (`sun.cls.defineAppClassTime` / `.self`;
    // `CRATONVM_PERF_CLASS_TIMERS`, default ON, inert under `=0`) to this
    // native's end, as `jvm_define_class_common`'s `PerfClassTraceTime`.
    let _define_trace = cratonvm_types::perf_class_trace::ClassTraceTime::begin(
        counted.then(|| ctx.class_trace_counters()).flatten(),
        cratonvm_types::perf_class_trace::ClassTraceEvent::DefineClass,
    );
    if jdk_only {
        if let Some(err) = jdk_only_classfile_header_error(ctx, &name, &bytes) {
            return Err(err);
        }
    }
    validate_classfile_header(&name, "defineClass1", &bytes)?;
    // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 93, lane L5):
    // `native-entry` split into `entry-args` (to here: the name, the byte
    // copy, the header checks) and `entry-loader` (from here: the boot-append
    // probe, the loader's namespace, the protection domain's URL, the pins).
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::EntryArgs,
        entry_clock,
    );
    let entry_loader_clock = cratonvm_classloading::define_census::phase_start();

    // Bootstrap-appended-jar classes (Instrumentation.appendToBootstrapClassLoaderSearch,
    // e.g. Mockito's MockMethodDispatcher) belong to the BOOTSTRAP loader. On
    // HotSpot a user loader's parent delegation reaches the appended boot
    // search before its own findClass runs, so findClass never defines a
    // per-loader copy. CratonVM's real-JDK platform-loader delegation misses
    // the appended jar (its BuiltinClassLoader path never consults the flat
    // store), the JDK loadClass bytecode falls through to findClass, and the
    // duplicate copy's <clinit> then fails Mockito's null-loader assertion.
    // Serve the already-defined bootstrap copy instead — same observable
    // outcome as HotSpot's delegation order.
    if ctx.is_bootstrap_appended_class(&name) {
        if let Some(cid) = ctx.class_id_by_name(&name) {
            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
        }
    }

    // Real JDK ClassLoader instances have no VM-owned integer field. Derive a
    // namespace solely through the stable loader-identity map; reading a raw
    // instance slot here can mistake an unrelated JDK field for a loader id.
    let (loader_id, user_loader_define) = define_loader_namespace(ctx, args.first(), &name);
    // zt t25/df11b: the platform loader's define of a loaded image class of its modules.
    if let Some(err) = platform_image_duplicate_refusal(ctx, args.first(), loader_id, &name) {
        return Err(err);
    }
    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(5) {
        Some(Value::Object(Some(pd))) => {
            crate::classloader::extract_pd_code_source_url_for_define(ctx, *pd)        }
        _ => None,
    };

    // JVMS В§5.3.5 вЂ” resolve direct supertypes through the DEFINING loader before
    // linking. `define_class_full` resolves the superclass/interfaces only via the
    // global classpath; a user loader whose classes are invisible there (Tomcat's
    // `WebappClassLoader` serving `/WEB-INF/lib` jars) would otherwise fail to
    // define a class whose super lives in the same jar (JSTL `JstlCoreTLV` в†’
    // `JstlBaseTLV`). No-op for built-in/app-loader defines.
    // GC: `preload_supertypes_via_loader` LOADS CLASSES, and `define_class_full`
    // / `get_class_mirror` below allocate, so the loader read out of `args` is a
    // pre-call address at every later use. `args` is the snapshot
    // `safe_native_call_impl` took before this native was entered; a collection
    // that happens INSIDE the callback does not rewrite it. Pin once and re-read
    // at each use. Three sibling entry points carry the identical shape — this
    // is one of them. See `internal/audits/wide-tranche-triage-20260907.md`.
    let loader_pin = match args.first() {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    // gc-common w16-e: the ProtectionDomain is pinned BEFORE the supertype
    // link, which runs the loader's Java `loadClass`. It used to be read out of
    // `args` after that call, so under `--real-jdk` a collection inside
    // `loadClass` left the mirror's `protectionDomain` a pre-move address.
    let pd_pin = pin_protection_domain_arg(ctx, args, 5);
    // The rest runs as one closure so every arm, the `?`s included, releases
    // the pins above before the native returns (interpreter round i2 wave 62,
    // lane L4: the `[pin-balance] LEAK` census named the `defineClass0`
    // sibling; this one and `defineClass2` had the same shape).
    let pin_floor = loader_pin.map(|(p, _)| p).or(pd_pin.map(|(p, _)| p));
    let mut answers = SupertypeAnswers {
        keep_miss_object: true,
        ..SupertypeAnswers::default()
    };
    let result = (|| -> MethodCallResult {
        // `CRATONVM_DEFINE_ONE_PARSE` (interpreter round i3 wave 72, lane L5):
        // the pre-pass's parse of `bytes`, handed to the define below.
        let keep_parse = ctx.define_one_parse();
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::EntryLoader,
            entry_loader_clock,
        );
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::NativeEntry,
            entry_clock,
        );
        let prepass_clock = cratonvm_classloading::define_census::phase_start();
        // JIT round 20 lane superAcc: the defining loader's `loadClass`
        // answers, linked by identity (see `SupertypeAnswers`). Round 22
        // wave 1 (lane review): declared before the closure, so its miss root
        // is released on every exit below.
        let preparsed = match loader_pin {
            Some((p, o)) => {
                let loader_obj = ctx.read_native_pin(p, o);
                link_supertypes_refusing_circularity(
                    ctx,
                    p,
                    loader_obj,
                    &name,
                    &bytes,
                    keep_parse,
                    &mut answers,
                )?            }
            None => None,
        };
        let (superclass_id_override, interface_id_overrides) = answers.overrides();
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::SupertypePrepass,
            prepass_clock,
        );

        // `CRATONVM_DBG_DEFINE=1` + `CRATONVM_DBG_DUPCLASS_FILTER=<substring>` --
        // the JAVA callers of this `defineClass`. The class-manager side of the
        // same switch prints the name and the loader; what it cannot print is
        // WHICH Java code decided to define it, and for a duplicate-identity
        // defect that is the whole question -- a loader that defines a class its
        // parent already has is either an isolating loader doing its job or a
        // delegation that failed, and only the caller chain separates the two.
        if cratonvm_types::flags().loader.dbg_define
            && cratonvm_types::flags()
                .loader
                .dbg_dupclass_filter
                .as_deref()
                .is_some_and(|f| name.contains(f))
        {
            let callers: Vec<String> = ctx
                .frame_class_ids()
                .into_iter()
                .rev()
                .take(12)
                .filter_map(|cid| ctx.class_name_of_id(cid))
                .collect();
            eprintln!(
            "[DEFINE-DBG-JAVA] defineClass name={name} loader_id={loader_id} callers={callers:?}"
        );
        }

        // A NULL loader is the BOOT loader: JNI `DefineClass(env, name, NULL, ..)`
        // (`jni.rs::jni_define_class_through_loader` passes it here) and
        // `--real-jdk`'s `JavaLangAccess.defineClass(null, ..)`. HotSpot's
        // `jni_DefineClass` / `JVM_DefineClassWithSource` define in the boot
        // loader, where `java.*` is legal; `loader_id` `0` is the APPLICATION
        // namespace. Java's `ClassLoader.defineClass` always passes its `this`.
        // JIT round 15 wave 6, lane loader6 (exact patch page
        // `r15w6-loader6-define-class1-null-loader-patch-FIXED-20260930.md`).
        // zs s7/nd6: the mirror object first, so a heap that cannot hold it
        // fails this define before anything is published (every reference
        // still used below is pinned).
        // zt t6/nd4: timed as its own census row (`mirror-reserve`).
        // zt t16/df6 (define page D6): a define of this name whose `addClass`
        // upcall is in flight on another thread is waited for, as HotSpot's
        // placeholder makes a second definer wait. Only pinned references
        // are live here.
        let bind_late = loader_pin.is_some()
            && crate::classloader::define_bind_after_add_class_enabled(ctx, loader_id);
        if bind_late {
            // zt t30/dc: the `bind-wait` census row.
            let wait_clock = cratonvm_classloading::define_census::phase_start();
            crate::classloader::wait_for_unbound_define(ctx, &name, &bytes, loader_id)?;
            cratonvm_classloading::define_census::phase_end(
                cratonvm_classloading::define_census::DefinePhase::BindWait,
                wait_clock,
            );
        }
        let reserve_clock = cratonvm_classloading::define_census::phase_start();
        // zt t7/cl: on ZGC, room in the loader's `classes` list for the
        // `addClass` upcall below, before anything is published (after the
        // supertype pre-pass, whose `loadClass` can define into this loader).
        let loader_now = loader_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        crate::classloader::presize_for_add_class(ctx, loader_now, &bytes, &name)?;
        let reservation = reserve_defined_class_mirror(ctx, &define_name_hint(&name, &bytes));
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::MirrorReserve,
            reserve_clock,
        );
        // zt t9/df: malformed bytes keep their `ClassFormatError` on a spent heap.
        let reservation =
            crate::classloader::refusal_yields_to_format_error(ctx, reservation, &bytes, &name)?;
        let null_loader_boot = matches!(args.first(), Some(Value::Object(None)));
        let opts = cratonvm_native_api::DefineClassFull {
            code_source_url: pd_url,
            // The load-time transformers' `protectionDomain` (both modes since
            // interpreter round i1 wave 47).
            protection_domain: pd_pin.map(|(p, o)| ctx.read_native_pin(p, o)),
            bootstrap_namespace: null_loader_boot,
            privileged_define: null_loader_boot,
            preparsed: preparsed_for_define(preparsed),
            superclass_id_override,
            interface_id_overrides,
            user_loader_define,
            defining_loader: loader_pin.map(|(p, o)| ctx.read_native_pin(p, o)),
            // zt t16/df6 (define page §1, corner 2): the name is bound after
            // the `addClass` upcall below (`settle_unbound_define`).
            bind_name_after_add_class: bind_late,
            ..Default::default()
        };
        // zt t18/df7 (x13 item 21d): kept for one define after a withdrawn rival,
        // without the pre-pass parse (a second holder of its `Arc` would make
        // the define below parse the bytes again).
        // zt t30/dc: the `bind-retry-clone` census row (bind arm only).
        let clone_clock = cratonvm_classloading::define_census::phase_start().filter(|_| bind_late);
        let retry_opts = bind_late.then(|| cratonvm_native_api::DefineClassFull {
            preparsed: None,
            ..opts.clone()
        });
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::BindRetryClone,
            clone_clock,
        );
        let defined = ctx.define_class_full(&name, &bytes, loader_id, opts);
        let reserved = take_mirror_reservation(ctx, reservation);
        // zt t18/df7 (x13 review item 21d): a define that raced past the wait
        // above can meet another define of this name that is still unbound in
        // this loader (the class manager's duplicate check counts it). Wait for
        // it to settle and, when it was withdrawn (its upcall threw), define
        // once more, as HotSpot's second definer waits on the placeholder and
        // then defines its own bytes. Only pinned references are live here.
        let (defined, reserved) = match (defined, retry_opts) {
            (Err(msg), Some(mut again)) => {
                let duplicate =
                    msg.contains("already defined") || msg.contains("DuplicateClassDefinition");
                if crate::classloader::redefine_after_unbound_define_settles(
                    ctx, &name, &bytes, loader_id, duplicate,
                )? {
                    again.protection_domain = pd_pin.map(|(p, o)| ctx.read_native_pin(p, o));
                    let reservation =
                        reserve_defined_class_mirror(ctx, &define_name_hint(&name, &bytes));
                    let reservation = crate::classloader::refusal_yields_to_format_error(
                        ctx,
                        reservation,
                        &bytes,
                        &name,
                    )?;
                    let defined = ctx.define_class_full(&name, &bytes, loader_id, again);
                    (defined, take_mirror_reservation(ctx, reservation))
                } else {
                    (Err(msg), reserved)
                }
            }
            (defined, _) => (defined, reserved),
        };
        match defined {
            // zt t18/df7 (x13 review items 21b, 21c): the steps from the
            // define to the answer run with this thread marked as the unbound
            // define's owner, and a panic among them withdraws the name.
            Ok(class_id) => crate::classloader::hold_unbound_define_across(
                ctx,
                class_id,
                bind_late,
                |ctx: &mut dyn NativeContext| -> MethodCallResult {
                    let mirror_clock = cratonvm_classloading::define_census::phase_start();
                    // MUST run before `get_class_mirror` below: `class_mirror_impl`
                    // consults `defining_loader_for(cid)` at mirror-creation time —
                    // the only time it is ever consulted for this mirror, since the
                    // mirror is cached from then on — to decide whether to record a
                    // `mirror_pin` row. Registering after minting the mirror means
                    // that row is never recorded, and `roots.rs` step 6 falls back to
                    // unconditionally rooting the mirror forever, permanently
                    // defeating unloading for every class this loader defines. See
                    // the identical fix and fuller explanation in
                    // `classloader::define_class_via_full`.
                    // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 95,
                    // lane L5): `native-mirror` split into `mirror-loader-record`,
                    // `mirror-create` (itself split by `class_mirror_impl` under
                    // the charged scope) and `mirror-native-fields`.
                    let record_clock = cratonvm_classloading::define_census::phase_start();
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        // zt t16/df6: out of the loader's by-name index until the
                        // name is bound (`settle_unbound_define`).
                        if bind_late {
                            crate::classloader::register_defining_loader_unbound(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        } else {
                            crate::classloader::register_defining_loader(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        }
                    }
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorLoaderRecord,
                        record_clock,
                    );
                    let create_clock = cratonvm_classloading::define_census::phase_start();
                    let census_scope = cratonvm_classloading::define_census::enter_charged_scope(
                        create_clock.is_some(),
                    );
                    let mirror = defined_class_mirror_reserved(ctx, class_id, reserved);
                    cratonvm_classloading::define_census::restore_charged_scope(census_scope);
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorCreate,
                        create_clock,
                    );
                    let fields_clock = cratonvm_classloading::define_census::phase_start();
                    // The JDK's Class.getClassLoader bytecode reads this instance
                    // field directly. Keep it aligned with the VM's loader registry.
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        ctx.set_field_by_name(
                            mirror,
                            "classLoader",
                            Value::Object(Some(loader_obj)),
                        );
                    }
                    install_protection_domain(ctx, mirror, pd_pin);
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorNativeFields,
                        fields_clock,
                    );
                    // zt t7/cl: HotSpot's `define_instance_class` records the class
                    // in its loader's `classes` list (`ClassLoader.addClass`); the
                    // upcall's failure is the define's. zt t9/df: a failed upcall
                    // leaves an orphan a retry of these bytes adopts. zt t16/df6:
                    // with a late binding, the upcall's outcome binds or withdraws
                    // the name first, and a withdrawn class is no orphan.
                    let mirror = match loader_pin {
                        Some((p, o)) => {
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t30/dc: `mirror-add-class` / `-bind-settle`
                            // / `-add-tail`, the rest of `native-mirror`.
                            let add_clock = cratonvm_classloading::define_census::phase_start();
                            let added = crate::classloader::add_defined_class_to_loader(
                                ctx, loader_obj, mirror,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddClass,
                                add_clock,
                            );
                            let settle_clock = cratonvm_classloading::define_census::phase_start();
                            let (added, withdrawn) = crate::classloader::settle_unbound_define(
                                ctx, added, class_id, bind_late,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorBindSettle,
                                settle_clock,
                            );
                            let tail_clock = cratonvm_classloading::define_census::phase_start();
                            if !withdrawn {
                                crate::classloader::note_add_class_orphan_on_failure(
                                    ctx, &added, class_id, &bytes,
                                );
                            }
                            crate::classloader::trace_add_class_door(
                                ctx,
                                "defineClass1",
                                class_id,
                                added.is_ok(),
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddTail,
                                tail_clock,
                            );
                            added?
                        }
                        None => mirror,
                    };
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::NativeMirror,
                        mirror_clock,
                    );
                    Ok(Some(Value::Object(Some(mirror))))
                },
            ),
            Err(msg) => {
                // A refusal naming a user loader by id gets its HotSpot text
                // (i4 wave 84, lane L5; `name_user_loaders_in`).
                let msg = crate::classloader_real::name_user_loaders_in(ctx, &msg);
                if let Some((p, o)) = loader_pin {
                    let loader_obj = ctx.read_native_pin(p, o);
                    match classify_duplicate_define(ctx, loader_obj, &name, loader_id, &msg) {
                        DuplicateDefine::SameLoaderObject => {
                            // zt t9/df: the retry of a define whose `addClass`
                            // upcall failed adopts the class it left behind.
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t14/df5: the retry's domain (corner 3).
                            let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                            if let Some(adopted) = crate::classloader::adopt_add_class_orphan(
                                ctx,
                                loader_obj,
                                &name,
                                loader_id,
                                &bytes,
                                Some(domain),
                            ) {
                                return adopted.map(|m| Some(Value::Object(Some(m))));
                            }
                            let loader_obj = ctx.read_native_pin(p, o);
                            return Err(duplicate_define_error(ctx, loader_obj, &name));
                        }
                        DuplicateDefine::ServeExisting(mirror) => {
                            return Ok(Some(Value::Object(Some(mirror))));
                        }
                        DuplicateDefine::NotDuplicate => {}
                    }
                    // zt t11/df3: a NAMELESS retry of a define whose `addClass`
                    // upcall failed adopts the class it left behind too (the
                    // classification above needs a caller's name).
                    if name.is_empty() {
                        let loader_obj = ctx.read_native_pin(p, o);
                        let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                        if let Some(adopted) = crate::classloader::adopt_nameless_add_class_orphan(
                            ctx,
                            loader_obj,
                            loader_id,
                            &bytes,
                            &msg,
                            Some(domain),
                        ) {
                            return adopted.map(|m| Some(Value::Object(Some(m))));
                        }
                        // zt t15/x12 (x10 page item 6): no orphan to adopt --
                        // HotSpot's duplicate-definition text, parenthetical
                        // included, when this loader object defined the class.
                        if let Some(err) =
                            nameless_duplicate_define_error(ctx, (p, o), &bytes, &msg)
                        {
                            return Err(err);
                        }
                    }
                    // `--real-jdk`, a null name (a named `java.*` define is refused
                    // by `preDefineClass` in Java first): HotSpot's
                    // `ClassFileParser` wording, which names the loader and the
                    // package (wave 42, lane L5; `L5W42DefineClassErrors`).
                    if jdk_only && name.is_empty() && msg.contains("Prohibited package name") {
                        let loader_obj = ctx.read_native_pin(p, o);
                        if let Some(message) = crate::classloader_real::prohibited_package_message(
                            ctx, loader_obj, &bytes,
                        ) {
                            return Err(
                                match crate::jboss_module_loader::alloc_single_message_exception(
                                    ctx,
                                    "java/lang/SecurityException",
                                    1,
                                    &message,
                                ) {
                                    Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
                                    Err(e) => e,
                                },
                            );
                        }
                    }
                }
                if jdk_only {
                    if let Some(err) = jdk_only_truncated_class_file(ctx, &msg) {
                        return Err(err);
                    }
                }
                tracing::warn!("ClassLoader.defineClass1({name}) failed: {msg}");
                // JIT round 21 wave 26, lane nesting: a supertype the loader
                // could not find is caused by its ClassNotFoundException.
                let failed = define_class_linkage_error(&name, "defineClass1", msg);
                Err(supertype_miss_with_cause(ctx, failed, &answers))
            }
        }
    })();
    answers.release_miss_root(ctx);
    if let Some(floor) = pin_floor {
        ctx.unpin_native_roots(floor);
    }
    result
}

/// `ClassLoader.defineClass2(ClassLoader, String, ByteBuffer, int, int, ProtectionDomain, String) -> Class`
pub fn native_classloader_define_class2(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let entry_clock = cratonvm_classloading::define_census::phase_start();
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => {
            let n = ctx.read_string(*s).unwrap_or_default();
            n.replace('.', "/")
        }
        _ => String::new(),
    };

    let byte_buffer = match args.get(2) {
        Some(Value::Object(Some(bb))) => *bb,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("defineClass2: ByteBuffer is null".to_string()),
            }
            .into());
        }
    };

    let offset = read_define_class_nonnegative_int(args, 3)?;
    let length = read_define_class_nonnegative_int(args, 4)?;
    let bytes = read_byte_buffer_define_class_slice(ctx, byte_buffer, offset, length, &name)?;
    // zt t16/pl7: `JVM_DefineClassWithSource` counts the define and its
    // bytes before parsing (`sun.cls.defineAppClasses` / `appClassBytes`).
    let counted = ctx.note_jvm_define_class(bytes.len());
    // zt t16/pl7: and times it (`sun.cls.defineAppClassTime` / `.self`;
    // `CRATONVM_PERF_CLASS_TIMERS`, default ON, inert under `=0`) to this
    // native's end, as `jvm_define_class_common`'s `PerfClassTraceTime`.
    let _define_trace = cratonvm_types::perf_class_trace::ClassTraceTime::begin(
        counted.then(|| ctx.class_trace_counters()).flatten(),
        cratonvm_types::perf_class_trace::ClassTraceEvent::DefineClass,
    );
    // `--real-jdk`: HotSpot's header errors (wave 42, lane L5); `--compatible`
    // too under `CRATONVM_COMPAT_MODULE_MESSAGES` (wave 49, lane L6).
    if ctx.messages_name_user_loaders() {
        if let Some(err) = jdk_only_classfile_header_error(ctx, &name, &bytes) {
            return Err(err);
        }
    }
    validate_classfile_header(&name, "defineClass2", &bytes)?;

    let (loader_id, user_loader_define) = define_loader_namespace(ctx, args.first(), &name);
    // zt t25/df11b: the platform loader's define of a loaded image class of its modules.
    if let Some(err) = platform_image_duplicate_refusal(ctx, args.first(), loader_id, &name) {
        return Err(err);
    }

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(5) {
        Some(Value::Object(Some(pd))) => {
            crate::classloader::extract_pd_code_source_url_for_define(ctx, *pd)        }
        _ => None,
    };

    // GC: `preload_supertypes_via_loader` LOADS CLASSES, and `define_class_full`
    // / `get_class_mirror` below allocate, so the loader read out of `args` is a
    // pre-call address at every later use. `args` is the snapshot
    // `safe_native_call_impl` took before this native was entered; a collection
    // that happens INSIDE the callback does not rewrite it. Pin once and re-read
    // at each use. Three sibling entry points carry the identical shape — this
    // is one of them. See `internal/audits/wide-tranche-triage-20260907.md`.
    let loader_pin = match args.first() {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    // gc-common w16-e: the ProtectionDomain is pinned BEFORE the supertype
    // link, which runs the loader's Java `loadClass`. It used to be read out of
    // `args` after that call, so under `--real-jdk` a collection inside
    // `loadClass` left the mirror's `protectionDomain` a pre-move address.
    let pd_pin = pin_protection_domain_arg(ctx, args, 5);
    // One closure, so every arm releases the pins (see `defineClass1`).
    let pin_floor = loader_pin.map(|(p, _)| p).or(pd_pin.map(|(p, _)| p));
    let mut answers = SupertypeAnswers {
        keep_miss_object: true,
        ..SupertypeAnswers::default()
    };
    let result = (|| -> MethodCallResult {
        // `CRATONVM_DEFINE_ONE_PARSE`, as in `defineClass1`.
        let keep_parse = ctx.define_one_parse();
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::NativeEntry,
            entry_clock,
        );
        let prepass_clock = cratonvm_classloading::define_census::phase_start();
        // JIT round 20 lane superAcc: the defining loader's `loadClass`
        // answers, linked by identity (see `SupertypeAnswers`). Round 22
        // wave 1 (lane review): declared before the closure, so its miss root
        // is released on every exit below.
        let preparsed = match loader_pin {
            Some((p, o)) => {
                let loader_obj = ctx.read_native_pin(p, o);
                link_supertypes_refusing_circularity(
                    ctx,
                    p,
                    loader_obj,
                    &name,
                    &bytes,
                    keep_parse,
                    &mut answers,
                )?            }
            None => None,
        };
        let (superclass_id_override, interface_id_overrides) = answers.overrides();
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::SupertypePrepass,
            prepass_clock,
        );

        // zt t16/df6: the late name binding and its wait, as in `defineClass1`.
        let bind_late = loader_pin.is_some()
            && crate::classloader::define_bind_after_add_class_enabled(ctx, loader_id);
        if bind_late {
            // zt t30/dc: the `bind-wait` census row.
            let wait_clock = cratonvm_classloading::define_census::phase_start();
            crate::classloader::wait_for_unbound_define(ctx, &name, &bytes, loader_id)?;
            cratonvm_classloading::define_census::phase_end(
                cratonvm_classloading::define_census::DefinePhase::BindWait,
                wait_clock,
            );
        }
        // zt t7/cl: the `classes` presize, as in `defineClass1`.
        let loader_now = loader_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        crate::classloader::presize_for_add_class(ctx, loader_now, &bytes, &name)?;
        // zs s7/nd6: the mirror object first, as in `defineClass1` (zt t9/df:
        // malformed bytes keep their `ClassFormatError`, as there).
        let reservation = reserve_defined_class_mirror(ctx, &define_name_hint(&name, &bytes));
        let reservation =
            crate::classloader::refusal_yields_to_format_error(ctx, reservation, &bytes, &name)?;
        let opts = cratonvm_native_api::DefineClassFull {
            code_source_url: pd_url,
            // The load-time transformers' `protectionDomain` (both modes since
            // interpreter round i1 wave 47).
            protection_domain: pd_pin.map(|(p, o)| ctx.read_native_pin(p, o)),
            preparsed: preparsed_for_define(preparsed),
            superclass_id_override,
            interface_id_overrides,
            user_loader_define,
            defining_loader: loader_pin.map(|(p, o)| ctx.read_native_pin(p, o)),
            bind_name_after_add_class: bind_late,
            ..Default::default()
        };
        // zt t18/df7 (x13 item 21d): kept for one define after a withdrawn rival,
        // without the pre-pass parse (a second holder of its `Arc` would make
        // the define below parse the bytes again).
        // zt t30/dc: the `bind-retry-clone` census row (bind arm only).
        let clone_clock = cratonvm_classloading::define_census::phase_start().filter(|_| bind_late);
        let retry_opts = bind_late.then(|| cratonvm_native_api::DefineClassFull {
            preparsed: None,
            ..opts.clone()
        });
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::BindRetryClone,
            clone_clock,
        );
        let defined = ctx.define_class_full(&name, &bytes, loader_id, opts);
        let reserved = take_mirror_reservation(ctx, reservation);
        // zt t18/df7 (x13 review item 21d): a define that raced past the wait
        // above can meet another define of this name that is still unbound in
        // this loader (the class manager's duplicate check counts it). Wait for
        // it to settle and, when it was withdrawn (its upcall threw), define
        // once more, as HotSpot's second definer waits on the placeholder and
        // then defines its own bytes. Only pinned references are live here.
        let (defined, reserved) = match (defined, retry_opts) {
            (Err(msg), Some(mut again)) => {
                let duplicate =
                    msg.contains("already defined") || msg.contains("DuplicateClassDefinition");
                if crate::classloader::redefine_after_unbound_define_settles(
                    ctx, &name, &bytes, loader_id, duplicate,
                )? {
                    again.protection_domain = pd_pin.map(|(p, o)| ctx.read_native_pin(p, o));
                    let reservation =
                        reserve_defined_class_mirror(ctx, &define_name_hint(&name, &bytes));
                    let reservation = crate::classloader::refusal_yields_to_format_error(
                        ctx,
                        reservation,
                        &bytes,
                        &name,
                    )?;
                    let defined = ctx.define_class_full(&name, &bytes, loader_id, again);
                    (defined, take_mirror_reservation(ctx, reservation))
                } else {
                    (Err(msg), reserved)
                }
            }
            (defined, _) => (defined, reserved),
        };
        match defined {
            // zt t18/df7 (x13 review items 21b, 21c): the steps from the
            // define to the answer run with this thread marked as the unbound
            // define's owner, and a panic among them withdraws the name.
            Ok(class_id) => crate::classloader::hold_unbound_define_across(
                ctx,
                class_id,
                bind_late,
                |ctx: &mut dyn NativeContext| -> MethodCallResult {
                    let mirror_clock = cratonvm_classloading::define_census::phase_start();
                    // MUST run before `get_class_mirror` below — see the identical
                    // fix and fuller explanation in `classloader::define_class_via_full`
                    // and the `defineClass1` sibling above.
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        if bind_late {
                            crate::classloader::register_defining_loader_unbound(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        } else {
                            crate::classloader::register_defining_loader(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        }
                    }
                    let create_clock = cratonvm_classloading::define_census::phase_start();
                    // zt t1/nd: the charged scope `defineClass1` opens, so
                    // `class_mirror_impl` splits this define's `mirror-create`
                    // too (it was attributed whole under `defineClass2`).
                    let census_scope = cratonvm_classloading::define_census::enter_charged_scope(
                        create_clock.is_some(),
                    );
                    let mirror = defined_class_mirror_reserved(ctx, class_id, reserved);
                    cratonvm_classloading::define_census::restore_charged_scope(census_scope);
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorCreate,
                        create_clock,
                    );
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        ctx.set_field_by_name(
                            mirror,
                            "classLoader",
                            Value::Object(Some(loader_obj)),
                        );
                    }
                    install_protection_domain(ctx, mirror, pd_pin);
                    // zt t7/cl: `ClassLoader.addClass`, as in `defineClass1` (zt
                    // t9/df: a failed upcall leaves an orphan, as there).
                    let mirror = match loader_pin {
                        Some((p, o)) => {
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t30/dc: `mirror-add-class` / `-bind-settle`
                            // / `-add-tail`, the rest of `native-mirror`.
                            let add_clock = cratonvm_classloading::define_census::phase_start();
                            let added = crate::classloader::add_defined_class_to_loader(
                                ctx, loader_obj, mirror,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddClass,
                                add_clock,
                            );
                            let settle_clock = cratonvm_classloading::define_census::phase_start();
                            let (added, withdrawn) = crate::classloader::settle_unbound_define(
                                ctx, added, class_id, bind_late,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorBindSettle,
                                settle_clock,
                            );
                            let tail_clock = cratonvm_classloading::define_census::phase_start();
                            if !withdrawn {
                                crate::classloader::note_add_class_orphan_on_failure(
                                    ctx, &added, class_id, &bytes,
                                );
                            }
                            crate::classloader::trace_add_class_door(
                                ctx,
                                "defineClass2",
                                class_id,
                                added.is_ok(),
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddTail,
                                tail_clock,
                            );
                            added?
                        }
                        None => mirror,
                    };
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::NativeMirror,
                        mirror_clock,
                    );
                    Ok(Some(Value::Object(Some(mirror))))
                },
            ),
            Err(msg) => {
                // As in `defineClass1` (i4 wave 84, lane L5).
                let msg = crate::classloader_real::name_user_loaders_in(ctx, &msg);
                if let Some((p, o)) = loader_pin {
                    let loader_obj = ctx.read_native_pin(p, o);
                    match classify_duplicate_define(ctx, loader_obj, &name, loader_id, &msg) {
                        DuplicateDefine::SameLoaderObject => {
                            // zt t9/df: as in `defineClass1`.
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t14/df5: the retry's domain (corner 3).
                            let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                            if let Some(adopted) = crate::classloader::adopt_add_class_orphan(
                                ctx,
                                loader_obj,
                                &name,
                                loader_id,
                                &bytes,
                                Some(domain),
                            ) {
                                return adopted.map(|m| Some(Value::Object(Some(m))));
                            }
                            let loader_obj = ctx.read_native_pin(p, o);
                            return Err(duplicate_define_error(ctx, loader_obj, &name));
                        }
                        DuplicateDefine::ServeExisting(mirror) => {
                            return Ok(Some(Value::Object(Some(mirror))));
                        }
                        DuplicateDefine::NotDuplicate => {}
                    }
                    // zt t11/df3: as in `defineClass1` (a nameless retry adopts).
                    if name.is_empty() {
                        let loader_obj = ctx.read_native_pin(p, o);
                        let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                        if let Some(adopted) = crate::classloader::adopt_nameless_add_class_orphan(
                            ctx,
                            loader_obj,
                            loader_id,
                            &bytes,
                            &msg,
                            Some(domain),
                        ) {
                            return adopted.map(|m| Some(Value::Object(Some(m))));
                        }
                        // zt t15/x12 (x10 page item 6): no orphan to adopt --
                        // HotSpot's duplicate-definition text, parenthetical
                        // included, when this loader object defined the class.
                        if let Some(err) =
                            nameless_duplicate_define_error(ctx, (p, o), &bytes, &msg)
                        {
                            return Err(err);
                        }
                    }
                    // A DIRECT buffer with a null name reaches this native
                    // (`ClassLoader.defineClass(String, ByteBuffer, PD)` sends
                    // a heap buffer to `defineClass1`): the same HotSpot
                    // `ClassFileParser` wording as `defineClass1`'s arm, which
                    // this native lacked (interpreter round i5 wave 106, lane
                    // L5; probe `L5W106DefineDirectBufferProhibited`).
                    if ctx.messages_name_user_loaders()
                        && name.is_empty()
                        && msg.contains("Prohibited package name")
                    {
                        let loader_obj = ctx.read_native_pin(p, o);
                        if let Some(message) = crate::classloader_real::prohibited_package_message(
                            ctx, loader_obj, &bytes,
                        ) {
                            return Err(
                                match crate::jboss_module_loader::alloc_single_message_exception(
                                    ctx,
                                    "java/lang/SecurityException",
                                    1,
                                    &message,
                                ) {
                                    Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
                                    Err(e) => e,
                                },
                            );
                        }
                    }
                }
                if ctx.messages_name_user_loaders() {
                    if let Some(err) = jdk_only_truncated_class_file(ctx, &msg) {
                        return Err(err);
                    }
                }
                tracing::warn!("ClassLoader.defineClass2({name}) failed: {msg}");
                // JIT round 21 wave 26, lane nesting: a supertype the loader
                // could not find is caused by its ClassNotFoundException.
                let failed = define_class_linkage_error(&name, "defineClass2", msg);
                Err(supertype_miss_with_cause(ctx, failed, &answers))
            }
        }
    })();
    answers.release_miss_root(ctx);
    if let Some(floor) = pin_floor {
        ctx.unpin_native_roots(floor);
    }
    result
}

/// `ClassLoader.defineClass0(ClassLoader, Class, String, byte[], int, int, ProtectionDomain, boolean, int, Object) в†’ Class`
///
/// JDK 21+ variant of defineClass with additional flags. WP2.3:
/// shares the same backend via `define_class_full`. The `flags`
/// argument is decoded with the JDK's `MethodHandleNatives.Constants`
/// values (0x1 = NESTMATE, 0x2 = HIDDEN, 0x4 = STRONG_LOADER_LINK: a hidden
/// class without it unloads with its mirror, gc-common w18-d — see
/// `docs/feature-designs/class-resolution-and-caching.md`)
/// and translated into options.
pub fn native_classloader_define_class0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: loader(0), lookup(1), name(2), bytes(3), offset(4),
    //       length(5), pd(6), init(7), flags(8), classData(9)
    let entry_clock = cratonvm_classloading::define_census::phase_start();
    let name = match args.get(2) {
        Some(Value::Object(Some(s))) => {
            let n = ctx.read_string(*s).unwrap_or_default();
            n.replace('.', "/")
        }
        _ => String::new(),
    };

    let byte_array = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("defineClass0: byte[] is null".to_string()),
            }
            .into());
        }
    };

    let offset = read_define_class_nonnegative_int(args, 4)?;
    let length = read_define_class_nonnegative_int(args, 5)?;
    let bytes = read_byte_array_define_class_slice(ctx, byte_array, offset, length)?;
    // HotSpot's `JVM_LookupDefineClass` parses in `ClassFileParser`: its
    // header text, with `defineClass1`'s gate (interpreter round i4 wave 95,
    // lane L5; `i42-L5-proposal-one-classfileparser-shaped-error-for-every-define-door`).
    if ctx.messages_name_user_loaders() {
        if let Some(err) = jdk_only_classfile_header_error(ctx, &name, &bytes) {
            return Err(err);
        }
    }
    validate_classfile_header(&name, "defineClass0", &bytes)?;

    let (loader_id, user_loader_define) = define_loader_namespace(ctx, args.first(), &name);
    // zt t25/df11b: the platform loader's define of a loaded image class of its modules.
    if let Some(err) = platform_image_duplicate_refusal(ctx, args.first(), loader_id, &name) {
        return Err(err);
    }

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(6) {
        Some(Value::Object(Some(pd))) => {
            crate::classloader::extract_pd_code_source_url_for_define(ctx, *pd)        }
        _ => None,
    };

    // `init` (boolean) at arg 7: run <clinit> after define.
    let initialize = matches!(args.get(7), Some(Value::Int(v)) if *v != 0);
    let init_after_class_data = initialize && matches!(args.get(9), Some(Value::Object(Some(_))));
    // `flags` (int) at arg 8. JDK 25 `MethodHandleNatives.Constants`:
    //   NESTMATE_CLASS = 0x01, HIDDEN_CLASS = 0x02, STRONG_LOADER_LINK = 0x04,
    //   ACCESS_VM_ANNOTATIONS = 0x08.
    // This block previously read bit 0 as HIDDEN (that is NESTMATE) and bit 2
    // as NESTMATE (that is STRONG), so a non-nestmate `defineHiddenClass`
    // (flags 0x02) decoded as `hidden = false` and collided on a duplicate
    // define. `classloader.rs:4048-4057` has carried the correct constants all
    // along — see DEFINE_CLASS0_FLAG_* there.
    let flags = match args.get(8) {
        Some(Value::Int(f)) => *f,
        _ => 0,
    };
    let nestmate = (flags & 0x1) != 0;
    let hidden = (flags & 0x2) != 0;
    // gc-common w18-d: STRONG_LOADER_LINK clear on a hidden class = it unloads
    // with its mirror (registered below, once the native is done with it).
    let strong_loader_link = (flags & 0x4) != 0;
    // zt t21/df9: a HIDDEN class of the platform loader (the lambdas and
    // species of the platform modules' classes) keeps the application
    // namespace it had before `define_loader_namespace` gave the platform
    // loader its own: its name is unique, so no define can collide on it.
    let loader_id = if hidden
        && !user_loader_define
        && loader_id == cratonvm_types::ClassLoaderId::NATIVE_EXTENSION
    {
        0
    } else {
        loader_id
    };
    // zt t9/df (`zgc-zt7cl-define-path-and-class-name-residuals-SUPERSEDED-20261019.md`
    // item 4): a non-hidden `initialize` define into a loader object runs
    // `<clinit>` after the `addClass` upcall below, as HotSpot's
    // `JVM_LookupDefineClass` initializes only after `define_instance_class`
    // returned. It ran inside the define, before the upcall, unless class
    // data was attached. `CRATONVM_ZGC_DEFINE_INIT_AFTER_ADD_CLASS=0` restores
    // that order.
    let init_after_define = init_after_class_data
        || (initialize
            && !hidden
            && matches!(args.first(), Some(Value::Object(Some(_))))
            && crate::classloader::define_init_after_add_class_enabled(ctx));

    // If hidden, mangle the name uniquely.
    let (effective_name, override_name) = if hidden {
        let id = crate::classloader::HIDDEN_CLASS_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mangled = format!("{name}/0x{id:016x}");
        (mangled.clone(), Some(mangled))
    } else {
        (name.clone(), None)
    };

    // Resolve nest-host name when NESTMATE is set: the lookup class
    // at arg 1 supplies the nest — its OWN nest host when it is itself a
    // nested class (JEP 371; HotSpot's `JVM_LookupDefineClass` takes
    // `lookup_class->nest_host()`), as `classloader.rs::cl_define_class0`
    // already did. It used the lookup class's name, so a NESTMATE hidden class
    // defined through a Lookup on `Outer$Inner` reported `Outer$Inner` from
    // `getNestHost()` (interpreter round i1 wave 27, lane L5; probe
    // `tools/probes/interp/L5/L5W27NestmateEdges.java`).
    let nest_host_class_name = if nestmate {
        match args.get(1) {
            Some(Value::Object(Some(lookup_mirror))) => {
                crate::lang_class::mirror_class_id(ctx, *lookup_mirror).and_then(|cid| {
                    ctx.nest_host_name(cid)
                        .or_else(|| ctx.class_name_of_id(cid))                })            }
            _ => None,
        }
    } else {
        None
    };

    // `--real-jdk`: `Lookup.defineClass` on a lookup class of the bootstrap or
    // platform loader reaches here with that loader (`null`, or the platform
    // loader object). HotSpot's `JVM_LookupDefineClass` defines the class in
    // the lookup class's loader, where a `java.*` name is legal, and the JDK has
    // already confined the name to the lookup class's package
    // (`Lookup.makeClassDefiner`) and required PACKAGE access to it. CratonVM
    // maps both loaders to `Application` here (the `0` sentinel), whose H5
    // guard refused every such define as a "prohibited package": JaCoCo's
    // agent defines `java.lang.$JaCoCo` through a private lookup on `Object`
    // once `Instrumentation.redefineModule` has opened `java.lang` to it
    // (interpreter round i1 wave 29, lane L5; probe
    // `L5/L5W29ChildFirstAgentLoader`, `inject` row). The privileged define is the
    // `Unsafe.defineClass` exemption, and keeps its guard against forging or
    // shadowing a class the bootstrap loader already has. Hidden classes were
    // exempt already. Both modes since wave 47 (lane L6): `--compatible`'s own
    // `Lookup.defineClass` door (`lookup_define.rs` `lk_define_class_b`) makes
    // the same bootstrap define, and this door agrees with it.
    let bootstrap_lookup_define = !hidden
        && match args.first() {
            Some(Value::Object(None)) => true,
            Some(Value::Object(Some(loader_obj))) => {
                crate::classloader::is_platform_loader_object(ctx, *loader_obj)            }
            _ => false,
        };
    if bootstrap_lookup_define && cratonvm_types::flags().loader.dbg_define {
        eprintln!("[DEFINE-DBG] defineClass0 privileged {name} (bootstrap/platform lookup)");
    }
    // gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 7): a NESTMATE
    // define's lookup class itself, the `-Xlog:class+load` source HotSpot
    // prints (`nest_host_class_name` is its nest host).
    let lookup_class_name = if nest_host_class_name.is_some() {
        match args.get(1) {
            Some(Value::Object(Some(lookup_mirror))) => {
                crate::lang_class::mirror_class_id(ctx, *lookup_mirror)
                    .and_then(|cid| ctx.class_name_of_id(cid))            }
            _ => None,
        }
    } else {
        None
    };
    let mut opts = cratonvm_native_api::DefineClassFull {
        override_name,
        hidden,
        code_source_url: pd_url,
        lookup_class_name,
        // With class data, `<clinit>` runs AFTER it is attached (below): a
        // spun `LambdaForm$MH` / lambda class's `<clinit>` loads its constants
        // with `MethodHandles.classData`, which read null when the define ran
        // the initializer first (HotSpot stores the data before it initialises).
        initialize: initialize && !init_after_define,
        nest_host_class_name,
        non_strong_hidden: !strong_loader_link,
        privileged_define: bootstrap_lookup_define,
        // The bootstrap loader's lookup (a null loader) defines in the
        // bootstrap namespace, as `JVM_LookupDefineClass` does: before wave 37
        // the `0` sentinel put it in the application loader's, where
        // `Class.forName(name, false, null)` could not find it (interpreter
        // round i1 wave 37, lane L5; probe `L5/L5W37BootstrapLookupDefine`).
        // The platform loader's lookup keeps the application namespace (its
        // mirror names the platform loader already).
        bootstrap_namespace: bootstrap_lookup_define
            && matches!(args.first(), Some(Value::Object(None))),
        user_loader_define,
        ..Default::default()
    };
    // `CRATONVM_LOOKUP_DEFINE_JDK_BODY` (default off; interpreter round i6
    // wave 120, lane L5, `i44-L5-proposal-lookup-define-runs-the-jdks-own-checks`
    // step 2): with the `Lookup` define natives unregistered, every
    // `Lookup.defineClass` / `defineHiddenClass` reaches this native through
    // the JDK's `ClassDefiner`, so it carries what `lk_define_class_b` did.
    // The supertypes are already resolved through the defining loader below
    // (the JDK passes `lookupClass.getClassLoader()`; JIT round 20 lane
    // superAcc, by identity); what was missing is loader-faithful linking, so
    // a generated proxy links against the exact interface its generator named.
    if cratonvm_types::flags().loader.lookup_define_jdk_body
        && matches!(args.get(1), Some(Value::Object(Some(_))))
    {
        opts.force_loader_faithful_linking = true;
    }
    // A HIDDEN class of a bootstrap-tagged lookup class goes to the bootstrap
    // namespace too, in both modes: its lookup class's loader and runtime
    // package, as HotSpot defines it (interpreter round i1 wave 44, lane L5;
    // `classloader::hidden_define_in_bootstrap_namespace`).
    if hidden {
        let lookup_mirror = match args.get(1) {
            Some(Value::Object(Some(o))) => Some(*o),
            _ => None,
        };
        let loader = args.first().copied().unwrap_or(Value::Object(None));
        if crate::classloader::hidden_define_in_bootstrap_namespace(ctx, lookup_mirror, loader) {
            opts.bootstrap_namespace = true;
            opts.privileged_define = true;
        }
    }
    let non_strong_hidden = opts.unloads_with_its_mirror();
    // GC: `preload_supertypes_via_loader` LOADS CLASSES, and `define_class_full`
    // / `get_class_mirror` below allocate, so the loader read out of `args` is a
    // pre-call address at every later use. `args` is the snapshot
    // `safe_native_call_impl` took before this native was entered; a collection
    // that happens INSIDE the callback does not rewrite it. Pin once and re-read
    // at each use. Three sibling entry points carry the identical shape — this
    // is one of them. See `internal/audits/wide-tranche-triage-20260907.md`.
    let loader_pin = match args.first() {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    let pd_pin = pin_protection_domain_arg(ctx, args, 6);
    // `classData` (arg 9), pinned for the same reason. gc-common w10-f: this
    // backend used to drop it outright, so `Class.getClassData()` (the field
    // `MethodHandles.classData` reads) was null for every hidden class
    // defined through here.
    let class_data_pin = match args.get(9) {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    // The domain `Lookup.ClassDefiner` passed (the lookup class's,
    // `null` for a bootstrap lookup class) is handed to the load-time
    // transformers, as `defineClass1` hands its own; it was pinned and
    // installed in the mirror but not passed (interpreter round i1 wave 29, lane
    // L5; `interpreter-L6-transformers-are-handed-a-null-protection-domain-FIXED-20261011` item 2).
    // A hidden class is never offered to them. zs s8/nd7: read inside the
    // closure below, after the supertype pre-pass (it loads classes: a GC
    // point) and the mirror reservation, not here.
    // One closure, so every arm releases the three pins above before the
    // native returns (interpreter round i2 wave 62, lane L4; the
    // `[pin-balance] LEAK` census named this native under `Lookup.bind`).
    let pin_floor = [loader_pin, pd_pin, class_data_pin]
        .into_iter()
        .flatten()
        .map(|(p, _)| p)
        .min();
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::NativeEntry,
        entry_clock,
    );
    let result = (|| -> MethodCallResult {
        // JIT round 20 lane superAcc (correctness, both modes): a user loader's
        // `Lookup.defineClass` / `defineHiddenClass` resolves the class's direct
        // supertypes through that loader, as `defineClass1` does and HotSpot's
        // `resolve_super_or_fail` does for every define: without the pre-pass a
        // supertype the loader had not loaded yet (a package-private base its
        // `findClass` serves) was looked up by name in the flat store and the
        // define failed with `NoClassDefFoundError` (probe `R20SuperAccModule`,
        // `hidden-pkgpriv`). The answers are linked by identity
        // (`SupertypeAnswers`); a built-in loader returns at once.
        let mut opts = opts;
        if let Some((p, o)) = loader_pin {
            let loader_obj = ctx.read_native_pin(p, o);
            let mut answers = SupertypeAnswers::default();
            link_supertypes_refusing_circularity(
                ctx,
                p,
                loader_obj,
                &name,
                &bytes,
                false,
                &mut answers,
            )?;
            let (superclass_id_override, interface_id_overrides) = answers.overrides();
            if superclass_id_override.is_some() {
                opts.superclass_id_override = superclass_id_override;
            }
            if interface_id_overrides.is_some() {
                opts.interface_id_overrides = interface_id_overrides;
            }
        }
        // zt t16/df6: the late name binding and its wait, as in
        // `defineClass1`, for a non-hidden class (the class manager also
        // binds at once when this define initializes).
        let bind_late = !hidden
            && loader_pin.is_some()
            && crate::classloader::define_bind_after_add_class_enabled(ctx, loader_id);
        if bind_late {
            // zt t30/dc: the `bind-wait` census row.
            let wait_clock = cratonvm_classloading::define_census::phase_start();
            crate::classloader::wait_for_unbound_define(ctx, &effective_name, &bytes, loader_id)?;
            cratonvm_classloading::define_census::phase_end(
                cratonvm_classloading::define_census::DefinePhase::BindWait,
                wait_clock,
            );
            opts.bind_name_after_add_class = true;
        }
        // zt t7/cl: a non-hidden class's `classes` presize, as in
        // `defineClass1` (a hidden class is never added to the list). It runs
        // Java (a GC point), so it comes before the domain is read below.
        if !hidden {
            let loader_now = loader_pin.map(|(p, o)| ctx.read_native_pin(p, o));
            crate::classloader::presize_for_add_class(ctx, loader_now, &bytes, "")?;
        }
        // The domain through its pin, current after the pre-pass above.
        if !hidden {
            opts.protection_domain = pd_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        }
        // zs s8/nd7: the mirror object first, as `defineClass1` does, so a heap
        // that cannot hold it fails this define before anything is published
        // (every reference used below is pinned; the domain in `opts` is
        // re-read across the reservation). A hidden class pools no name.
        // zt t9/df: malformed bytes keep their `ClassFormatError`.
        let reservation = reserve_mirror_for_define(ctx, &effective_name, &mut opts);
        let reservation =
            crate::classloader::refusal_yields_to_format_error(ctx, reservation, &bytes, "")?;
        // zt t18/df7 (x13 item 21d): kept for one define after a withdrawn rival,
        // without the pre-pass parse (a second holder of its `Arc` would make
        // the define below parse the bytes again).
        // zt t30/dc: the `bind-retry-clone` census row (bind arm only).
        let clone_clock = cratonvm_classloading::define_census::phase_start().filter(|_| bind_late);
        let retry_opts = bind_late.then(|| cratonvm_native_api::DefineClassFull {
            preparsed: None,
            ..opts.clone()
        });
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::BindRetryClone,
            clone_clock,
        );
        let defined = ctx.define_class_full(&effective_name, &bytes, loader_id, opts);
        let reserved = take_mirror_reservation(ctx, reservation);
        // zt t18/df7 (x13 review item 21d): a define that raced past the wait
        // above can meet another define of this name that is still unbound in
        // this loader (the class manager's duplicate check counts it). Wait for
        // it to settle and, when it was withdrawn (its upcall threw), define
        // once more, as HotSpot's second definer waits on the placeholder and
        // then defines its own bytes. Only pinned references are live here.
        let (defined, reserved) = match (defined, retry_opts) {
            (Err(msg), Some(mut again)) => {
                let duplicate =
                    msg.contains("already defined") || msg.contains("DuplicateClassDefinition");
                if crate::classloader::redefine_after_unbound_define_settles(
                    ctx,
                    &effective_name,
                    &bytes,
                    loader_id,
                    duplicate,
                )? {
                    again.protection_domain = pd_pin.map(|(p, o)| ctx.read_native_pin(p, o));
                    let reservation = reserve_mirror_for_define(ctx, &effective_name, &mut again);
                    let reservation = crate::classloader::refusal_yields_to_format_error(
                        ctx,
                        reservation,
                        &bytes,
                        "",
                    )?;
                    let defined = ctx.define_class_full(&effective_name, &bytes, loader_id, again);
                    (defined, take_mirror_reservation(ctx, reservation))
                } else {
                    (Err(msg), reserved)
                }
            }
            (defined, _) => (defined, reserved),
        };
        match defined {
            // zt t18/df7 (x13 review items 21b, 21c): the steps from the
            // define to the answer run with this thread marked as the unbound
            // define's owner, and a panic among them withdraws the name.
            Ok(class_id) => crate::classloader::hold_unbound_define_across(
                ctx,
                class_id,
                bind_late,
                |ctx: &mut dyn NativeContext| -> MethodCallResult {
                    let mirror_clock = cratonvm_classloading::define_census::phase_start();
                    // MUST run before `get_class_mirror` below — see the identical
                    // fix and fuller explanation in `classloader::define_class_via_full`
                    // and the `defineClass1`/`defineClass2` siblings above.
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        if bind_late {
                            crate::classloader::register_defining_loader_unbound(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        } else {
                            crate::classloader::register_defining_loader(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        }
                    }
                    let create_clock = cratonvm_classloading::define_census::phase_start();
                    // zt t1/nd: `defineClass1`'s charged scope (see `defineClass2`).
                    let census_scope = cratonvm_classloading::define_census::enter_charged_scope(
                        create_clock.is_some(),
                    );
                    let mirror = defined_class_mirror_reserved(ctx, class_id, reserved);
                    cratonvm_classloading::define_census::restore_charged_scope(census_scope);
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorCreate,
                        create_clock,
                    );
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        ctx.set_field_by_name(
                            mirror,
                            "classLoader",
                            Value::Object(Some(loader_obj)),
                        );
                    }
                    install_protection_domain(ctx, mirror, pd_pin);
                    if let Some((p, o)) = class_data_pin {
                        let data = ctx.read_native_pin(p, o);
                        crate::classloader::attach_class_data(ctx, mirror, data);
                    }
                    // zt t7/cl: a non-hidden class goes into its loader's `classes`
                    // list before it is initialized (`Lookup.defineClass`:
                    // HotSpot's `JVM_LookupDefineClass` -> `define_instance_class`).
                    // zt t9/df: a failed upcall leaves an orphan, as in
                    // `defineClass1`.
                    let mirror = match loader_pin {
                        Some((p, o)) if !hidden => {
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t30/dc: `mirror-add-class` / `-bind-settle`
                            // / `-add-tail`, the rest of `native-mirror`.
                            let add_clock = cratonvm_classloading::define_census::phase_start();
                            let added = crate::classloader::add_defined_class_to_loader(
                                ctx, loader_obj, mirror,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddClass,
                                add_clock,
                            );
                            let settle_clock = cratonvm_classloading::define_census::phase_start();
                            let (added, withdrawn) = crate::classloader::settle_unbound_define(
                                ctx, added, class_id, bind_late,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorBindSettle,
                                settle_clock,
                            );
                            let tail_clock = cratonvm_classloading::define_census::phase_start();
                            if !withdrawn {
                                crate::classloader::note_add_class_orphan_on_failure(
                                    ctx, &added, class_id, &bytes,
                                );
                            }
                            crate::classloader::trace_add_class_door(
                                ctx,
                                "defineClass0",
                                class_id,
                                added.is_ok(),
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddTail,
                                tail_clock,
                            );
                            added?
                        }
                        _ => mirror,
                    };
                    // The deferred `<clinit>` (see `init_after_class_data` and, zt
                    // t9/df, `init_after_define`). A failure is the JVMS 5.5
                    // `ExceptionInInitializerError` the VM already built. The mirror
                    // is re-read: the initializer can move it.
                    let mirror = if init_after_define {
                        ctx.initialize_class(class_id)?;
                        ctx.get_class_mirror(class_id)
                    } else {
                        mirror
                    };
                    // gc-common w18-d: last, with nothing left to allocate; the mirror
                    // is this native's return value. The defining loader was recorded
                    // and written into `classLoader` above, so the mirror carries the
                    // edge to it.
                    if non_strong_hidden {
                        crate::classloader::register_non_strong_hidden_class(ctx, class_id);
                    }
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::NativeMirror,
                        mirror_clock,
                    );
                    Ok(Some(Value::Object(Some(mirror))))
                },
            ),
            Err(msg) => {
                // `hidden` is excluded on purpose and always was: a hidden class
                // is never registered under its name, so "already defined" cannot
                // be about it, and JVMS §5.3.5's rule does not apply to a class
                // that has no binary name in any loader's namespace.
                //
                // gc-common w9-a: read the loader through the pin, as the
                // `defineClass1` / `defineClass2` siblings do. This arm used the
                // `args` snapshot, which is a pre-call address once
                // `define_class_full` (GC-capable) has run: after a moving
                // collection the duplicate check compared the defining-loader
                // record against a stale address (so the loader's own re-define
                // could pass as `NotDuplicate`), and `duplicate_define_error`
                // read the loader's name off a vacated slot.
                if !hidden {
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        match classify_duplicate_define(
                            ctx,
                            loader_obj,
                            &effective_name,
                            loader_id,
                            &msg,
                        ) {
                            DuplicateDefine::SameLoaderObject => {
                                // zt t9/df: as in `defineClass1`.
                                let loader_obj = ctx.read_native_pin(p, o);
                                // zt t14/df5: the retry's domain (corner 3).
                                let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                                if let Some(adopted) = crate::classloader::adopt_add_class_orphan(
                                    ctx,
                                    loader_obj,
                                    &effective_name,
                                    loader_id,
                                    &bytes,
                                    Some(domain),
                                ) {
                                    // zt t10/x7: the adopted class gets the
                                    // deferred `<clinit>` the success arm runs
                                    // after its upcall (`init_after_define`):
                                    // the orphan's own define failed at the
                                    // upcall, before that initialization, and
                                    // HotSpot's retry (`JVM_LookupDefineClass`)
                                    // initializes the class it defines.
                                    let mirror = adopted?;
                                    if !init_after_define {
                                        return Ok(Some(Value::Object(Some(mirror))));
                                    }
                                    let Some(adopted_id) = ctx.class_id_from_mirror(mirror) else {
                                        return Ok(Some(Value::Object(Some(mirror))));
                                    };
                                    ctx.initialize_class(adopted_id)?;
                                    let mirror = ctx.get_class_mirror(adopted_id);
                                    return Ok(Some(Value::Object(Some(mirror))));
                                }
                                let loader_obj = ctx.read_native_pin(p, o);
                                return Err(duplicate_define_error(
                                    ctx,
                                    loader_obj,
                                    &effective_name,
                                ));
                            }
                            DuplicateDefine::ServeExisting(mirror) => {
                                return Ok(Some(Value::Object(Some(mirror))));
                            }
                            DuplicateDefine::NotDuplicate => {}
                        }
                    }
                }
                tracing::warn!("ClassLoader.defineClass0({effective_name}) failed: {msg}");
                // initialize=true failures: see `exception_in_initializer_error`'s
                // doc comment (JVMS 5.5). Checked before `define_class_linkage_error`,
                // which does not recognise this shape and would otherwise wrap it
                // as a `ClassFormatError` — the sibling of the same defect fixed at
                // the two `lookup_define.rs` call sites and the `classloader.rs`
                // `defineHiddenClass` one.
                if msg.contains("initialize after define failed") {
                    return Err(exception_in_initializer_error(ctx, &effective_name, &msg));
                }
                Err(define_class_linkage_error(
                    &effective_name,
                    "defineClass0",
                    msg,
                ))
            }
        }
    })();
    if let Some(floor) = pin_floor {
        ctx.unpin_native_roots(floor);
    }
    result
}

// ---------------------------------------------------------------------------
// jdk.internal.perf.Perf natives (C27)
//
// The JDK's performance-counter infrastructure uses `Perf.getPerf().createLong(...)`
// to register internal counters. We don't track perf counters, so we return
// benign defaults вЂ” empty/zero-filled direct ByteBuffers for createLong /
// createByteArray (so callers can still write into them), zeros / no-ops for
// the rest. Perf counters in the real JDK only drive diagnostic output; no
// program correctness depends on their values. Since zt t9/pl `attach0(0)`
// hands out the VM's own perf-data image (the jvmstat file's view), so the
// internal `sun.management` counters read what `jstat` reads.
// ---------------------------------------------------------------------------

pub fn native_perf_attach(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // attach(String, int) -> ByteBuffer  вЂ”  return an empty direct buffer.
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(0)],
    )
}

pub fn native_perf_attach0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // private attach0(int) -> ByteBuffer
    //
    // zt t9/pl (`docs/feature-designs/zgc-architecture.md`):
    // HotSpot's `Perf_Attach` answers lvmid 0 (or this process's id) with a
    // direct buffer over the VM's own perf-data memory (`PerfMemory::attach`),
    // which `VMManagementImpl.getInternalCounters` walks for
    // `sun.management.HotspotRuntime` / `HotspotThread` / `HotspotMemory` /
    // `HotspotClassLoading`'s internal counters. A zero-capacity buffer made
    // it set `noPerfData` and answer an empty list for every pattern. The
    // lvmid is the last `int` argument (the receiver, if any, comes first).
    // Without perf data (`-XX:-UsePerfData`, `CRATONVM_PERF_DATA_FILE=0`) or
    // under `CRATONVM_PERF_ATTACH_SELF=0`, the zero-capacity buffer below
    // stays.
    //
    // zt t10/pl2 (`zgc-zt9pl-perf-counter-residuals-SUPERSEDED-20261019.md` item 5):
    // another VM's id maps that VM's published perf data read-only, as
    // HotSpot's `PerfMemory::attach` does for a foreign id (jvmstat's local
    // monitoring: `MonitoredHost.getMonitoredVm`, `VirtualMachine.list()`'s
    // display names). `Perf.attach` wraps it in a cleaner that calls
    // `detach` with this buffer, which unmaps it. No such VM (or
    // `CRATONVM_PERF_ATTACH_FOREIGN=0`): the zero-capacity buffer, as before.
    //
    // zt t14/pl5 (`zgc-zt12pl4-*` item 5, rows `pl4_fa_missing` / `_missingC`):
    // an id with no perf data at all throws HotSpot's
    // `IllegalArgumentException` (`PerfMemory::attach`: `Could not map vmid
    // to user name`) instead (`CRATONVM_PERF_ATTACH_MISSING_THROWS`, default
    // ON, `=0` kill); data that exists but cannot be mapped keeps the
    // zero-capacity buffer.
    //
    // zt pl4 item 5: on POSIX, HotSpot's own lookup and exceptions
    // (`NativeContext::attach_foreign_perf_data_hotspot`; another user's
    // directory, a container's `/proc/<pid>/root/tmp`, an unmappable file).
    let own = perf_attach_targets_this_vm(args);
    let hotspot = if own {
        None
    } else {
        perf_attach_lvmid(args).and_then(|lvmid| ctx.attach_foreign_perf_data_hotspot(lvmid))
    };
    let used_hotspot = hotspot.is_some();
    let view = match hotspot {
        Some(Ok(view)) => view,
        Some(Err(failure)) => {
            use cratonvm_native_api::ForeignPerfAttachFailure as F;
            return Err(match failure {
                F::IllegalArgument(message) => RuntimeError::IllegalArgumentException { message },
                F::Io(message) => RuntimeError::IOException { message },
                F::OutOfMemory(message) => RuntimeError::OutOfMemoryError { message },
            }
            .into());
        }
        None if own => ctx.attach_own_perf_data(),
        None => perf_attach_lvmid(args).and_then(|lvmid| ctx.attach_foreign_perf_data(lvmid)),
    };
    if view.is_none() && !own && !used_hotspot {
        let missing = perf_attach_lvmid(args).and_then(|id| ctx.foreign_perf_data_missing(id));
        if let Some(message) = missing {
            return Err(RuntimeError::IllegalArgumentException {
                message: message.to_string(),
            }
            .into());
        }
    }
    if let Some((address, length)) = view {
        match i32::try_from(length) {
            Ok(capacity) => {
                let made = ctx.new_object_initialized(
                    "java/nio/DirectByteBuffer",
                    "(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)V",
                    &[
                        Value::Long(address as i64), // Cast: the view's address
                        Value::Int(capacity),
                        Value::Object(None),
                        Value::Object(None),
                    ],
                );
                // A foreign view no buffer holds is released at once.
                if made.is_err() && !own {
                    ctx.detach_perf_data(address);
                }
                return made;
            }
            Err(_) if !own => ctx.detach_perf_data(address),
            Err(_) => {}
        }
    }
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(0)],
    )
}

/// zt t9/pl: whether `Perf.attach0`'s lvmid (the last `int` argument) names
/// this VM: 0, or this process's id, as HotSpot's `PerfMemory::attach`
/// decides.
pub fn perf_attach_targets_this_vm(args: &[Value]) -> bool {
    let lvmid = perf_attach_lvmid(args);
    lvmid == Some(0) || (lvmid.is_some() && lvmid == i32::try_from(std::process::id()).ok())
}

/// zt t10/pl2: `Perf.attach0`'s lvmid, the last `int` argument.
pub fn perf_attach_lvmid(args: &[Value]) -> Option<i32> {
    args.iter().rev().find_map(|v| match v {
        Value::Int(i) => Some(*i),
        _ => None,
    })
}

/// zt t10/pl2: `Perf.detach(ByteBuffer)`, the cleaner `Perf.attach` registers
/// for an id other than 0: unmaps a foreign view `attach0` mapped
/// (`NativeSystemAccess::detach_perf_data`, which leaves this VM's own perf
/// data and any other buffer alone, as HotSpot's `PerfMemory::detach` leaves
/// its own memory).
pub fn native_perf_detach(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let buffer = args.iter().rev().find_map(|v| match v {
        Value::Object(Some(obj)) => Some(*obj),
        _ => None,
    });
    if let Some(buffer) = buffer {
        if let Value::Long(address) = ctx.get_field_by_name(buffer, "address") {
            if address != 0 {
                ctx.detach_perf_data(address as usize); // Cast: the buffer's address
            }
        }
    }
    Ok(None)
}

pub fn native_perf_create_long(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // createLong(String name, int variability, int units, long value) -> ByteBuffer
    // Return an 8-byte writable direct buffer so the counter slot is usable.
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
}

pub fn native_perf_create_byte_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // W7-86. `createByteArray` is `public native ByteBuffer createByteArray(
    // String, int, int, byte[], int)` — an INSTANCE method (`javap -p --module
    // java.base jdk.internal.perf.Perf`, Adoptium 25.0.3.9) — so `args[0]` is
    // the `Perf` receiver and `maxLength`, the fifth PARAMETER, sits at
    // `args[5]`. The comment this replaces listed the parameters in their
    // STATIC positions and then indexed by them: `args[4]` is the `byte[]`
    // value, a `Value::Object`, which missed the `Value::Int` arm and left
    // `max_length` at its 0 default — so every perf byte-array counter got a
    // ZERO-length direct buffer instead of the size the caller asked for.
    //
    // Not observable from ordinary Java: `jdk.internal.perf.Perf` is exported
    // to nobody and `Perf.getPerf()` is itself gated, so there is no probe for
    // this row and none is pretended. What is measured is the registration:
    // `--dump-native-registry` on a default run shows it owning its slot, and
    // the real method is `acc_native` with no code, so nothing else answers it.
    // Compatible mode (`--fast-boot`, default).
    let max_length = match args.get(5) {
        Some(Value::Int(v)) if *v >= 0 => *v,
        _ => 0,
    };
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(max_length)],
    )
}

pub fn native_perf_high_res_counter(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // zt t21/pl10 (`CRATONVM_PERF_HRT_VM_ORIGIN`, default ON, `=0` kill):
    // HotSpot's `Perf.highResCounter()` is `os::elapsed_counter()`, ticks since
    // the VM began (`os::init`), the same origin as the perf counters'
    // `sun.gc.collector.<n>.lastEntryTime` / `lastExitTime`, so
    // `highResCounter() - lastExitTime` is the time since that pause. Here:
    // ticks since this VM's start (`vm_start_instant`, the origin
    // `perf_data_file` stamps the pause times from), at least 1 (the boot runs
    // for milliseconds before any Java code can ask, so the sentinel idiom
    // below is safe). A context with no start (mocks): the lazy origin below.
    {
        if let Some(origin) = ctx.vm_start_instant() {
            let elapsed = u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let ticks = cratonvm_types::perf_ticks::ticks_from_nanos(elapsed).max(1);
            return Ok(Some(Value::Long(ticks)));
        }
    }
    // highResCounter() -> long nanos-since-start, biased by a fixed non-zero
    // origin. This had exactly the lazy-`OnceLock` shape `System.nanoTime`
    // had, and therefore exactly the same first-reading-is-0 defect. It feeds
    // the same "is my timer armed yet?" comparisons, and `highResFrequency`
    // below reports nanosecond ticks, so callers scale this value as a
    // duration. See `MONOTONIC_ORIGIN_NANOS`.
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    // zt t21/pl10 (`CRATONVM_PERF_HRT_QPC_FREQUENCY`): the elapsed time in
    // ticks of `highResFrequency` (HotSpot's `os::elapsed_counter`: the
    // performance counter on Windows, nanoseconds elsewhere), so a caller
    // that divides by the frequency gets seconds. The origin stays the fixed
    // non-zero offset (in ticks: only its non-zero-ness matters).
    let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let ticks = MONOTONIC_ORIGIN_NANOS
        .saturating_add(cratonvm_types::perf_ticks::ticks_from_nanos(elapsed));
    Ok(Some(Value::Long(ticks)))
}

pub fn native_perf_high_res_frequency(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // highResFrequency() -> ticks per second: HotSpot's
    // `os::elapsed_frequency()`, the performance counter's frequency on
    // Windows (zt t21/pl10, `CRATONVM_PERF_HRT_QPC_FREQUENCY`; 1e9 under its
    // `=0` kill), 1_000_000_000 elsewhere. The same figure as the perf image's
    // `sun.os.hrt.frequency`.
    Ok(Some(Value::Long(
        cratonvm_types::perf_ticks::hrt_frequency(),
    )))
}


/// `Thread.sleep(J)V` plus the `jdk.ThreadSleep` the JDK's own `Thread.sleep`
/// commits (SBR5 lane jfr; the native shadows that bytecode in `--compatible`).
/// Zero cost without a recording: one relaxed load.
pub fn native_thread_sleep_jfr(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if !crate::hooks::jfr_any_recording() {
        return native_thread_sleep(ctx, args);
    }
    let millis = match args.first() {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    let root = crate::hooks::jfr_thread_sleep_begin(ctx, millis.saturating_mul(1_000_000));
    let result = native_thread_sleep(ctx, args);
    crate::hooks::jfr_thread_sleep_end(ctx, root);
    result
}

/// `Thread.sleep(JI)V` with the same event.
pub fn native_thread_sleep_millis_nanos_jfr(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if !crate::hooks::jfr_any_recording() {
        return native_thread_sleep_millis_nanos(ctx, args);
    }
    let millis = match args.first() {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    let nanos = match args.get(1) {
        Some(Value::Int(n)) => i64::from(*n),
        _ => 0,
    };
    let root = crate::hooks::jfr_thread_sleep_begin(
        ctx,
        millis.saturating_mul(1_000_000).saturating_add(nanos),
    );
    let result = native_thread_sleep_millis_nanos(ctx, args);
    crate::hooks::jfr_thread_sleep_end(ctx, root);
    result
}

