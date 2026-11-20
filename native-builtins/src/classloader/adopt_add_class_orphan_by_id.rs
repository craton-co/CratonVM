#[allow(unused_imports)]
use super::*;

/// zt t10/df2 (`zgc-zt9df-define-path-residuals-SUPERSEDED-20261019.md` §1, the proxy
/// cache): [`adopt_add_class_orphan`] for a definer that answers an existing
/// class by its id instead of defining the bytes again -- the `Proxy`
/// bridges, whose `PROXY_CLASS_CACHE` hands back the `$ProxyN` a refused
/// upcall left behind. When `class_id` is an orphan (any digest: the bridge
/// generated the bytes itself), claim the row, make the upcall on `loader`
/// (current at entry) and answer its result; `None` when it is not an orphan
/// (the common case: one lock of a map that is empty unless an upcall
/// failed). A failed upcall puts the row back. A GC point when it adopts.
/// [`add_class_orphan_pending`] asks first without a loader in hand.
pub fn adopt_add_class_orphan_by_id(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
    class_id: cratonvm_types::ClassId,
) -> Option<Result<ObjectRef, MethodCallFailed>> {
    if !define_add_class_retry_enabled(ctx) {
        return None;
    }
    // zt t14/df5: a row that still holds the in-flight mark of a `Proxy`
    // bridge's fresh upcall (`CRATONVM_ZGC_PROXY_UPCALL_SERIALIZED`, default ON) is not
    // an orphan: that upcall is running on another thread (a cache hit whose
    // wait valve ran out reaches here). Claiming it made a second `addClass`
    // of the same class, and the first upcall's clear then found no mark.
    let digest = {
        let mut orphans = loader_tables(ctx).add_class_orphans.lock();
        match orphans.get(&class_id.as_u32()).copied() {
            None | Some(ADD_CLASS_UPCALL_IN_FLIGHT) => return None,
            Some(_) => orphans.remove(&class_id.as_u32())?,
        }
    };
    let loader_pin = ctx.pin_native_root(loader);
    let mirror = ctx.get_class_mirror(class_id);
    let loader = ctx.read_native_pin(loader_pin, loader);
    let added = add_adopted_orphan_to_loader(ctx, loader, mirror);
    ctx.unpin_native_roots(loader_pin);
    if added.is_err() {
        loader_tables(ctx)
            .add_class_orphans
            .lock()
            .insert(class_id.as_u32(), digest);
    }
    if dbg_define_add_class_retry() {
        eprintln!(
            "[define] add-class orphan adopted: by-id class_id={} ok={}",
            class_id.as_u32(),
            added.is_ok()
        );
    }
    Some(added)
}

/// zt t10/df2: is `class_id` an orphan of a failed `addClass` upcall (with
/// `CRATONVM_ZGC_DEFINE_ADD_CLASS_RETRY` on)? Lets a caller skip the work of
/// finding the loader for [`adopt_add_class_orphan_by_id`] on the common path.
/// One lock; allocates nothing. zt t14/df5: the in-flight mark of a fresh
/// `Proxy` upcall ([`mark_add_class_upcall_in_flight`]) is not an orphan
/// (the adoption declines it too).
pub fn add_class_orphan_pending(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    define_add_class_retry_enabled(ctx)
        && loader_tables(ctx)
            .add_class_orphans
            .lock()
            .get(&class_id.as_u32())
            .is_some_and(|&digest| digest != ADD_CLASS_UPCALL_IN_FLIGHT)
}

/// zt t12/df4 (`zgc-zt11df3-define-path-what-is-left-SUPERSEDED-20261019.md` §1, first
/// corner): `CRATONVM_ZGC_DEFINE_ORPHAN_HIDDEN` (default ON on ZGC, `=0`
/// kill). Is `class_id` an orphan that `ClassLoader.findLoadedClass` must not
/// answer? HotSpot's `define_instance_class` makes the `addClass` upcall
/// before the dictionary update, so a define whose upcall failed leaves the
/// name undefined and `findLoadedClass(name)` answers `null` until the retry
/// (row R1-visible: `Zt10Df2OrphanRetry 4 64 400000 visible`, HotSpot
/// `visible=0`); here the class stays published until the retry adopts it
/// ([`adopt_add_class_orphan`]). The three `findLoadedClass` doors filter
/// their answer through `classloader_real::hide_package_refused`, which asks
/// this. The adoption itself looks the class up by its defining-loader row
/// or namespace, not through `findLoadedClass`, so a retry is still adopted.
///
/// Cost: one lock of a map that is empty unless an upcall failed; the switch
/// is read only for a pending orphan. Allocates nothing, runs no Java.
/// g1gen/c7: on every collector, with the retry.
pub fn add_class_orphan_hidden(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    let pending = {
        let orphans = loader_tables(ctx).add_class_orphans.lock();
        !orphans.is_empty() && orphans.contains_key(&class_id.as_u32())
    };
    pending
        && define_add_class_retry_enabled(ctx)
}

/// zt t12/df4 (define page §8, hand-off D8): the digest an orphan-map row
/// holds while a `Proxy` bridge's FRESH `addClass` upcall is in flight
/// (`CRATONVM_ZGC_PROXY_UPCALL_SERIALIZED`, default ON since zt t31). No defining-bytes
/// digest is compared against it by a define door: the `Proxy` bridges
/// generate their own bytes and adopt by id, and a row that still holds this
/// value after the upcall is removed by [`clear_add_class_upcall_in_flight`].
pub const ADD_CLASS_UPCALL_IN_FLIGHT: u64 = u64::MAX;

/// zt t12/df4 (D8): record that `class_id`'s fresh `addClass` upcall is about
/// to run, BEFORE the class is published to the `Proxy` cache, so a racing
/// cache hit waits for its outcome
/// (`reflect_annotations::proxy_add_class_after_define`) instead of handing
/// out a class that is not (yet) in its loader's `classes` list. HotSpot's
/// `ProxyBuilder` cache (`AbstractClassLoaderValue.Memoizer`) makes a racing
/// caller wait for the defining call and, when it threw, rethrows its error.
/// One lock; allocates nothing.
pub fn mark_add_class_upcall_in_flight(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) {
    loader_tables(ctx)
        .add_class_orphans
        .lock()
        .insert(class_id.as_u32(), ADD_CLASS_UPCALL_IN_FLIGHT);
}

/// zt t12/df4 (D8): is `class_id`'s fresh `addClass` upcall in flight
/// ([`mark_add_class_upcall_in_flight`])? One lock; allocates nothing.
pub fn add_class_upcall_in_flight(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    let orphans = loader_tables(ctx).add_class_orphans.lock();
    !orphans.is_empty() && orphans.get(&class_id.as_u32()) == Some(&ADD_CLASS_UPCALL_IN_FLIGHT)
}

/// zt t12/df4 (D8): the fresh upcall finished. A served upcall leaves the
/// in-flight row, which this removes; a refused one has already replaced it
/// with the orphan's row ([`note_add_class_orphan_on_failure`]), which stays
/// for a racing or later cache hit to adopt. One lock; allocates nothing.
pub fn clear_add_class_upcall_in_flight(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) {
    let mut orphans = loader_tables(ctx).add_class_orphans.lock();
    if orphans.get(&class_id.as_u32()) == Some(&ADD_CLASS_UPCALL_IN_FLIGHT) {
        orphans.remove(&class_id.as_u32());
    }
}

/// zt t10/df2: `CRATONVM_DBG_DEFINE_ADD_CLASS_DOOR=1` prints one stderr line
/// per `ClassLoader.addClass` upcall a define door made
/// (`[define] add-class door=<door> class=<name> ok=<b>`), so a row can show
/// WHICH door served it. Asked because `Zt8X5AddClassDefiners` under
/// `--compatible` printed `in=true` with
/// `CRATONVM_ZGC_DEFINE_ADD_CLASS_MORE_DOORS=0` (t9a): the JDK's own
/// `Proxy` / `Lookup.defineClass` bodies ran and reached `defineClass1` /
/// `defineClass0`, not the bridges that switch gates. Changes no behaviour.
pub fn dbg_define_add_class_door() -> bool {
    false
}

/// zt t10/df2: the [`dbg_define_add_class_door`] line for one upcall. `door`
/// names the definer (`defineClass0`, `defineClass1`, `defineClass2`,
/// `unsafe`, `lookup`, `proxy`); `class_id` is the defined class. Allocates
/// nothing.
pub fn trace_add_class_door(
    ctx: &dyn NativeContext,
    door: &str,
    class_id: cratonvm_types::ClassId,
    ok: bool,
) {
    if !dbg_define_add_class_door() {
        return;
    }
    let name = ctx.class_name_of_id(class_id).unwrap_or_default();
    eprintln!("[define] add-class door={door} class={name} ok={ok}");
}

/// zt t9/df: `CRATONVM_ZGC_DEFINE_FORMAT_ERROR_FIRST` (default ON on ZGC,
/// `=0` kill; read per refusal). HotSpot parses the class file before it
/// creates the mirror or calls `addClass`, so malformed bytes are a
/// `ClassFormatError` even on a spent heap. The ZGC define doors grow the
/// `classes` list ([`presize_for_add_class`]) and reserve the mirror before
/// the define parses, so a malformed class on a spent heap was an
/// `OutOfMemoryError` (`zgc-zt7cl-define-path-and-class-name-residuals-SUPERSEDED-20261019.md`
/// item 2). With the switch on, an `OutOfMemoryError` refusal of either step
/// yields to the parse when `bytes` do not parse: the step is skipped
/// (`T::default()`: no presize, no reservation) and the define raises its
/// `ClassFormatError`. Bytes that parse keep the refusal. The parse runs only
/// on a refusal, never on a served define.
///
/// zt t16/df6 (`docs/feature-designs/zgc-architecture.md`
/// §3): `CRATONVM_ZGC_DEFINE_FORMAT_CHECKS_FIRST` (default ON, `=0` kill;
/// read per refusal) widens "do not parse" to the class manager's pre-publish
/// `ClassFileParser` checks (`cratonvm_classloading::define_refused_by_format`:
/// the attribute decode, the constant pool's references and names, class and
/// member modifiers, duplicate interfaces, the class attributes), which
/// HotSpot also makes before it allocates anything: bytes that parse
/// structurally but carry, say, `ACC_INTERFACE` without `ACC_ABSTRACT` were an
/// `OutOfMemoryError` on a spent heap where HotSpot throws `ClassFormatError`.
/// `=0` keeps the structural parse alone (t9).
///
/// zt t20/df8 (define page §3): `requested_name` is the define's requested
/// name (internal form), so that the switch's checks include HotSpot's
/// wrong-name check (`NoClassDefFoundError: <name> (wrong name: <this>)`,
/// raised while `ClassFileParser` parses, as the format checks are). `""`
/// for a door whose name is not the caller's request or whose registered name
/// is not the bytes' own (hidden classes, `Lookup`, `Unsafe`, the synthetic
/// JDK backend): no wrong-name check, as before.
pub fn refusal_yields_to_format_error<T: Default>(
    ctx: &dyn NativeContext,
    result: Result<T, MethodCallFailed>,
    bytes: &[u8],
    requested_name: &str,
) -> Result<T, MethodCallFailed> {
    match result {
        // g1gen/c7: on every collector (it was ZGC only by the zt round's
        // rule; HotSpot's `ClassFormatError` comes before any allocation on
        // each of them).
        Err(e)
            if is_out_of_memory_failure(ctx, &e)
                && define_bytes_refused_by_format(bytes, requested_name) =>
        {
            Ok(T::default())
        }
        other => other,
    }
}

/// zt t16/df6: the format question of [`refusal_yields_to_format_error`]:
/// the class manager's pre-publish checks under
/// `CRATONVM_ZGC_DEFINE_FORMAT_CHECKS_FIRST` (default ON; zt t20/df8: with
/// the wrong-name check when `requested_name` is not empty), the structural
/// parse alone with `=0`.
pub fn define_bytes_refused_by_format(bytes: &[u8], requested_name: &str) -> bool {
    {
        cratonvm_classloading::define_refused_by_format_as(bytes, requested_name)
    }
}

/// Is `failure` an `OutOfMemoryError`: the VM's own refusal
/// (`RuntimeError::OutOfMemoryError`, a native door's), or a thrown
/// `java.lang.OutOfMemoryError` (a Java allocation's, e.g. `ArrayList.grow`)?
pub fn is_out_of_memory_failure(ctx: &dyn NativeContext, failure: &MethodCallFailed) -> bool {
    match failure {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            cratonvm_types::error::RuntimeError::OutOfMemoryError { .. },
        )) => true,
        MethodCallFailed::ExceptionThrown(exc) => {
            let class_id = ctx.class_id_of_object(*exc);
            ctx.class_name_of_id(class_id).as_deref() == Some("java/lang/OutOfMemoryError")
        }
        _ => false,
    }
}

/// zt t7/cl: the ZGC presize of t6/nd4 ([`presize_loader_classes`]) for the
/// real-JDK define doors that now make the `addClass` upcall
/// ([`add_defined_class_to_loader`]): before the mirror reservation and the
/// define, grow `loader.classes` when it is full, so the upcall after the
/// publish allocates nothing and a refused growth is the define's
/// `OutOfMemoryError` with nothing defined. `loader` is current at entry (read
/// through its pin by the caller); `None` (the boot loader) does nothing.
/// ZGC only (`CRATONVM_ZGC_ADDCLASS_PRESIZE`), and only while the upcall
/// itself is on (`CRATONVM_DEFINE_ADD_CLASS`).
///
/// zt t9/df: `bytes` are the class file about to be defined; a refused growth
/// on bytes that do not parse yields to the define's `ClassFormatError`
/// ([`refusal_yields_to_format_error`]). zt t20/df8: `requested_name` is the
/// define's requested name for its wrong-name check (`""`: none).
pub fn presize_for_add_class(
    ctx: &mut dyn NativeContext,
    loader: Option<ObjectRef>,
    bytes: &[u8],
    requested_name: &str,
) -> Result<(), MethodCallFailed> {
    let Some(loader) = loader else {
        return Ok(());
    };
    if !addclass_presize_enabled(ctx)
        || !define_add_class_enabled()
        || !ctx.method_exists(CL_CLASS, "addClass", ADD_CLASS_DESCRIPTOR)
    {
        return Ok(());
    }
    let presized = presize_loader_classes(ctx, loader);
    refusal_yields_to_format_error(ctx, presized, bytes, requested_name)
}

/// zt t6/nd4: `CRATONVM_ZGC_ADDCLASS_PRESIZE` (default ON, `=0` kill); ZGC
/// only (Generational and G1 keep the old order) until g1gen/c7, on every
/// collector since: off ZGC a failed `addClass` upcall was the common case
/// (nothing presized the list), and the retry then met `LinkageError`.
///
/// zt t11/df3 (t10/df2 finding 7): read per define, as every other define
/// switch is. It was memoized in a process-wide `MemoSlot`, so the first VM's
/// answer held for every later VM of the process and a test's
/// `override_thread` could not turn it after the first read. A define is far
/// dearer than the read.
pub fn addclass_presize_enabled(ctx: &dyn NativeContext) -> bool {
    let _ = ctx;
    true
}

/// The JDK class whose `classes` list [`presize_loader_classes`] grows.
pub const ARRAY_LIST_CLASS: &str = "java/util/ArrayList";

/// Does the `ArrayList` `list` have no free slot (`size == elementData.length`),
/// so that its next `add` allocates? `false` for anything that does not read
/// as an `ArrayList` (no `size` int, no `elementData` array): nothing is done
/// to an object of another shape.
///
/// zt t7/x4 (finding 1): `false` too unless `list` is exactly a
/// `java.util.ArrayList` (what `ClassLoader`'s constructor puts there). A
/// subclass may re-declare `size` (a by-name read would answer its shadow,
/// and the presize would write it) or override `add` (user code with side
/// effects, run by the VM for a define the program never asked to run it).
pub fn array_list_is_full(ctx: &dyn NativeContext, list: ObjectRef) -> bool {
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(list));
    if class_name.as_deref() != Some(ARRAY_LIST_CLASS) {
        return false;
    }
    let size = match ctx.get_field_by_name(list, "size") {
        Value::Int(n) if n >= 0 => n as usize,
        _ => return false,
    };
    match ctx.get_field_by_name(list, "elementData") {
        Value::Object(Some(data)) => size >= ctx.array_length(data),
        _ => false,
    }
}

/// zt t6/nd4 (`zgc-zt1nd-define-and-door-residuals-after-the-merge-SUPERSEDED-20261019.md`
/// item 1): make room for one more element in `loader.classes` BEFORE the
/// define, so the `addClass` that [`define_class_via_full`] runs after the
/// define publishes the class allocates nothing.
///
/// HotSpot's `SystemDictionary::define_instance_class` calls
/// `ClassLoader.addClass` before it updates the dictionary, so an
/// `OutOfMemoryError` from the list's growth leaves the class undefined and a
/// retry defines it. Here the growth runs first: `add(null)` (the JDK's own
/// growth policy, so the capacity sequence is the one `addClass` would have
/// produced: 10 for the empty default list, then 1.5x), and the `size` field
/// is put back, which drops the null. The list's contents are unchanged; only
/// `modCount` (an iterator-only counter of a private list) differs.
///
/// `addClass` is `synchronized (classes)`, and a parallel-capable loader can
/// run it on another thread, so the growth runs under the same monitor. The
/// monitor follows the object across a moving collection (the lock word is
/// in the header, which every collector copies verbatim, and an inflated
/// monitor's index entry is re-keyed by `MonitorTable::remap_after_gc`, which
/// ZGC calls with its pointer map: gc/src/zgc.rs `monitors.remap_after_gc`),
/// so exiting through the re-read pin releases the monitor this entered, as
/// an interpreted `monitorexit` after a collection does. The enter is the
/// GC-safe one: the owner may be inside `addClass` and collecting.
///
/// Residual (not closed): another thread's `addClass` between this presize
/// and ours can take the slot; ours then grows after the publish, as before.
///
/// zt t7/x4 (finding 1): the list is `ClassLoader`'s own `classes` field,
/// resolved from `java/lang/ClassLoader` (the slot `addClass` reads), not by
/// name on the loader's class: a by-name read answers the MOST-DERIVED
/// declaration, so a user loader that declares its own `classes` field (an
/// `ArrayList` of anything) had ITS list grown, its `modCount` bumped (a
/// concurrent iterator of it throws `ConcurrentModificationException`) and
/// its monitor taken by the VM.
pub fn presize_loader_classes(
    ctx: &mut dyn NativeContext,
    loader: ObjectRef,
) -> Result<(), MethodCallFailed> {
    // zt t7/cl: through the slot `ClassLoader` declares (a subclass's own
    // `classes` field no longer shadows it).
    let Some(classes) = loader_classes_list(ctx, loader) else {
        return Ok(());
    };
    // Unsynchronized hint: a list with room needs no monitor.
    if !array_list_is_full(ctx, classes) {
        return Ok(());
    }
    let pin = ctx.pin_native_root(classes);
    let entered = ctx.monitor_enter_gc_safe(classes);
    let classes = ctx.read_native_pin(pin, entered);
    let result = if array_list_is_full(ctx, classes) {
        match ctx.invoke_virtual(
            classes,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(None)],
        ) {
            Ok(_) => {
                let classes = ctx.read_native_pin(pin, classes);
                if let Value::Int(n) = ctx.get_field_by_name(classes, "size") {
                    if n > 0 {
                        ctx.set_field_by_name(classes, "size", Value::Int(n - 1));
                    }
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    } else {
        Ok(())
    };
    let classes = ctx.read_native_pin(pin, classes);
    ctx.monitor_exit(classes);
    ctx.unpin_native_roots(pin);
    result
}

/// JDK-internal: `static native Class<?> defineClass1(
///     ClassLoader loader, String name, byte[] b, int off, int len,
///     ProtectionDomain pd, String source);`
pub fn cl_define_class1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    let loader = args.first().copied().unwrap_or(Value::Object(None));
    let name = read_optional_internal_name(ctx, args, 1);
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineClass1: bytes must not be null".into(),
            }
            .into());
        }
    };
    let off = match read_nonneg_int(args, 3) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let len = match read_nonneg_int(args, 4) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let bytes = read_byte_array_slice(ctx, byte_array, off, len).map_err(|_msg| {
        cratonvm_types::error::MethodCallFailed::from(RuntimeError::aioobe_index_only(
            (off as i32).max(0),
        ))
    })?;

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

    // cglib SEGV guard.
    if let Some(v) = cglib_guard_value(ctx, &name, &bytes) {
        return Ok(Some(v));
    }

    // Optional ProtectionDomain at slot 5.
    let mut opts = cratonvm_native_api::DefineClassFull::default();
    if let Some(Value::Object(Some(pd))) = args.get(5) {
        opts.code_source_url = extract_pd_code_source_url(ctx, *pd);
    }
    // Optional `String source` at slot 6 — JDK uses this as the
    // SourceFile attribute hint, surfacing through `Class.getResource(...)`
    // / debugging. We thread it through `override_name = None`, leaving
    // the backend's own SourceFile attribute path intact, but log it
    // when present so debugging/JFR can correlate.
    if let Some(Value::Object(Some(src_obj))) = args.get(6) {
        if let Some(src) = ctx.read_string(*src_obj) {
            tracing::debug!(
                target: "cratonvm_native_builtins::classloader",
                class = %name,
                source = %src,
                "defineClass1 source hint"
            );
        }
    }

    let loader_id = loader_id_for(ctx, loader);
    define_class_via_full(ctx, &name, bytes, loader_id, opts, false, None, loader)
}

/// JDK-internal: `static native Class<?> defineClass2(
///     ClassLoader loader, String name, ByteBuffer bb, int off,
///     int len, ProtectionDomain pd, String source);`
pub fn cl_define_class2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    let loader = args.first().copied().unwrap_or(Value::Object(None));
    let name = read_optional_internal_name(ctx, args, 1);
    let bb = match args.get(2) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineClass2: ByteBuffer must not be null".into(),
            }
            .into());
        }
    };
    let off = match read_nonneg_int(args, 3) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let len = match read_nonneg_int(args, 4) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    // Decoded by `lang_system::read_byte_buffer_define_class_slice` — the SAME
    // decoder this crate's other `defineClass2` registration uses. See the
    // comment on that function: this entry point shadows that registration in
    // synthetic-JDK mode, so a private copy here meant the bounds hardening was
    // silently inert wherever the shadow won.
    let bytes = crate::lang_system::read_byte_buffer_define_class_slice(ctx, bb, off, len, &name)?;

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

    // cglib SEGV guard.
    if let Some(v) = cglib_guard_value(ctx, &name, &bytes) {
        return Ok(Some(v));
    }

    let mut opts = cratonvm_native_api::DefineClassFull::default();
    if let Some(Value::Object(Some(pd))) = args.get(5) {
        opts.code_source_url = extract_pd_code_source_url(ctx, *pd);
    }
    if let Some(Value::Object(Some(src_obj))) = args.get(6) {
        if let Some(src) = ctx.read_string(*src_obj) {
            tracing::debug!(
                target: "cratonvm_native_builtins::classloader",
                class = %name,
                source = %src,
                "defineClass2 source hint"
            );
        }
    }

    let loader_id = loader_id_for(ctx, loader);
    define_class_via_full(ctx, &name, bytes, loader_id, opts, false, None, loader)
}

// JEP 371 / JEP 466 flag bits accepted by `defineClass0`.
pub const DEFINE_CLASS0_FLAG_NESTMATE: i32 = 0x01;
pub const DEFINE_CLASS0_FLAG_HIDDEN: i32 = 0x02;
/// `STRONG_LOADER_LINK` ties a hidden class's lifetime to its defining
/// loader — see `java.lang.invoke.MethodHandles.Lookup.ClassOption.STRONG`.
/// A hidden class defined WITHOUT it unloads with its mirror
/// (`DefineClassFull::non_strong_hidden`, [`register_non_strong_hidden_class`],
/// gc-common w18-d); see
/// `docs/feature-designs/class-resolution-and-caching.md`.
pub const DEFINE_CLASS0_FLAG_STRONG: i32 = 0x04;
/// `ACCESS_VM_ANNOTATIONS` — surface JDK-internal annotations to
/// reflection. We accept the flag but treat it as a no-op.
pub const DEFINE_CLASS0_FLAG_VM_ANNOTATIONS: i32 = 0x08;

/// JDK-internal modern (JDK 17+) entry point:
/// `static native Class<?> defineClass0(
///     ClassLoader loader, Class<?> lookup, String name,
///     byte[] b, int off, int len, ProtectionDomain pd,
///     boolean initialize, int flags, Object classData);`
pub fn cl_define_class0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    let loader = args.first().copied().unwrap_or(Value::Object(None));
    // Slot 1: lookup class (used for nest-host derivation when the
    // NESTMATE flag is set).
    let lookup_mirror = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = read_optional_internal_name(ctx, args, 2);
    let byte_array = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "defineClass0: bytes must not be null".into(),
            }
            .into());
        }
    };
    let off = match read_nonneg_int(args, 4) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let len = match read_nonneg_int(args, 5) {
        Some(v) => v,
        None => {
            return Err(RuntimeError::aioobe_index_only(-1).into());
        }
    };
    let bytes = read_byte_array_slice(ctx, byte_array, off, len).map_err(|_msg| {
        cratonvm_types::error::MethodCallFailed::from(RuntimeError::aioobe_index_only(
            (off as i32).max(0),
        ))
    })?;

    // cglib SEGV guard.
    if let Some(v) = cglib_guard_value(ctx, &name, &bytes) {
        return Ok(Some(v));
    }

    // Slot 6: ProtectionDomain
    let mut opts = cratonvm_native_api::DefineClassFull::default();
    if let Some(Value::Object(Some(pd))) = args.get(6) {
        opts.code_source_url = extract_pd_code_source_url(ctx, *pd);
    }

    // Slot 7: boolean initialize
    let initialize = matches!(args.get(7), Some(Value::Int(v)) if *v != 0);

    // Slot 8: int flags
    let flags = match args.get(8) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if (flags & DEFINE_CLASS0_FLAG_HIDDEN) != 0 {
        opts.hidden = true;
        // Hidden classes are bytecode the JDK trusts (or has already
        // verified at the source level). The backend's verifier still
        // runs by default, but we skip verification when callers
        // explicitly mark this as hidden — matches HotSpot behaviour.
        opts.skip_verification = true;
    }
    if (flags & DEFINE_CLASS0_FLAG_NESTMATE) != 0 {
        // Derive nest-host from the lookup class. If the lookup
        // class's nest_host is itself, that name is used.
        if let Some(lk) = lookup_mirror {
            if let Some(cid) = crate::lang_class::mirror_class_id(ctx, lk) {
                let nest_host = ctx
                    .nest_host_name(cid)
                    .or_else(|| ctx.class_name_of_id(cid));
                if let Some(host) = nest_host {
                    opts.nest_host_class_name = Some(host);
                }
                // gci i11/cl: HotSpot's `-Xlog:class+load` source is the
                // lookup class, not its nest host.
                opts.lookup_class_name = ctx.class_name_of_id(cid);
            }
        }
    }
    // gc-common w18-d: a hidden class without STRONG_LOADER_LINK unloads with
    // its mirror (`define_class_via_full` registers it after the define).
    opts.non_strong_hidden = (flags & DEFINE_CLASS0_FLAG_STRONG) == 0;
    let _vm_anns = (flags & DEFINE_CLASS0_FLAG_VM_ANNOTATIONS) != 0; // advisory

    // Slot 9: Object classData (may be null)
    let class_data = match args.get(9) {
        Some(Value::Object(Some(_))) => Some(args[9]),
        _ => None,
    };

    // A hidden class of a bootstrap-tagged lookup class is defined in the
    // bootstrap namespace, as HotSpot defines it in its lookup class's loader
    // (`hidden_define_in_bootstrap_namespace`; interpreter round i1 wave 44).
    if opts.hidden && hidden_define_in_bootstrap_namespace(ctx, lookup_mirror, loader) {
        opts.bootstrap_namespace = true;
        opts.privileged_define = true;
    }
    let loader_id = loader_id_for(ctx, loader);
    define_class_via_full(
        ctx, &name, bytes, loader_id, opts, initialize, class_data, loader,
    )
}

/// WP2.3-C — register the JDK-internal `defineClass0/1/2` natives on
/// `java.lang.ClassLoader`. The public `defineClass(...)` overloads
/// are pure Java and route through these natives; CGLIB / direct
/// user code typically calls `defineClass1` because that's what the
/// public 4-arg / 5-arg overloads delegate to.
pub fn jla_system_define_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let byte_array = match args.get(4) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: "System$1.defineClass: bytes must not be null".into(),
                }
                .into(),
            );
        }
    };
    let len = ctx.array_length(byte_array) as i32;
    let mapped = vec![
        args.get(1).copied().unwrap_or(Value::Object(None)),
        args.get(2).copied().unwrap_or(Value::Object(None)),
        args.get(3).copied().unwrap_or(Value::Object(None)),
        args.get(4).copied().unwrap_or(Value::Object(None)),
        Value::Int(0),
        Value::Int(len),
        args.get(5).copied().unwrap_or(Value::Object(None)),
        args.get(6).copied().unwrap_or(Value::Int(0)),
        args.get(7).copied().unwrap_or(Value::Int(0)),
        args.get(8).copied().unwrap_or(Value::Object(None)),
    ];
    cl_define_class0(ctx, &mapped)
}

pub fn register_classloader_define_class(r: &mut NativeMethodRegistry) {
    let cl = CL_CLASS;

    // defineClass1(ClassLoader loader, String name, byte[] b, int off, int len,
    //              ProtectionDomain pd, String source) -> Class
    r.register(
        cl,
        "defineClass1",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;[BIILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        cl_define_class1, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // defineClass2(ClassLoader loader, String name, ByteBuffer bb, int off, int len,
    //              ProtectionDomain pd, String source) -> Class
    r.register(
        cl,
        "defineClass2",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/nio/ByteBuffer;IILjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        cl_define_class2, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // defineClass0(ClassLoader loader, Class<?> lookup, String name,
    //              byte[] b, int off, int len, ProtectionDomain pd,
    //              boolean initialize, int flags, Object classData) -> Class
    r.register(
        cl,
        "defineClass0",
        "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BIILjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
        cl_define_class0, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // `defineClass` is a JavaLangAccess method, and that interface's carrier
    // class is not spelled the same on every JDK -- see
    // `shared_secrets_bridge::JLA_CARRIER_CANDIDATES`. Register on each
    // candidate; the ones that are not the carrier on this image have no class
    // for anything to dispatch through.
    //
    // **Spelled inline rather than looped over that const, deliberately.**
    // `native-builtins/tests/registrar_drift.rs` scans this file as TEXT: its
    // `parse_loops` accepts `for x in [ .. ]` over literal elements and skips
    // any `for` whose `in` is not followed by `[`. Written as
    // `for carrier in JLA_CARRIER_CANDIDATES` this registration becomes
    // invisible to it -- measured, on 2026-09-09: total drift pairs 1356 ->
    // 1355 and `the_drift_baseline_has_no_stale_rows` reporting this very call
    // as "no longer drift[ing] ... registered by: <not registered anywhere
    // this scan can see>". The registration had not stopped happening; the
    // gate had stopped seeing it, and taking the regenerated baseline would
    // have retired a live row on a parser's blind spot.
    //
    // `jla_define_class_is_registered_on_every_carrier_candidate` below keeps
    // this list and that const from drifting apart, since the compiler cannot.
    for carrier in ["java/lang/System$1", "java/lang/System$2"] {
        r.register(
            carrier,
            "defineClass",
            "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
            jla_system_define_class, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
}

// ---------------------------------------------------------------------------
// Round-16 (agent 16): defensive `sun.misc.Unsafe.defineClass` shim for the
// cglib proxy-generation path.
//
// Symptom: a SEGV / stack-overflow during cglib's proxy bytecode emit when
// it calls `Unsafe.defineClass(name, bytecode[], off, len, loader, pd)`.
// The crash is in the non-JIT path during bytecode generation — most
// likely a null/short bytecode array being deref'd by the underlying
// `define_class_full` plumbing.
//
// Fix: validate args up-front, then delegate to the same backend used by
// the public `ClassLoader.defineClass(String, byte[], int, int,
// ProtectionDomain)` overload. The defensive check prevents the process
// crash even if cglib doesn't fully work.
//
// Error contract (no silent-wrong-result stubs): instead of returning a
// null Class on failure (which only NPEs later in the caller), we throw
// the exception HotSpot's `Unsafe.defineClass` would:
//   * null bytecode array        → NullPointerException
//   * out-of-bounds off/len      → ArrayIndexOutOfBoundsException
//   * zero/oversize/bad-magic/    → ClassFormatError
//     parse panic/backend reject
//   * backend "not found" reason → NoClassDefFoundError
//
// Signature: `defineClass(String name, byte[] b, int off, int len,
//                         ClassLoader loader, ProtectionDomain pd) -> Class`
// args = [this, name, byte_array, off, len, loader, pd]
//   (this == the Unsafe singleton, ignored)
//
// Note: registered from `register_classloader_natives` because the user
// requested all cglib-related URL/Unsafe defineClass paths live in this
// file. This does NOT shadow the existing `Unsafe.defineAnonymousClass`
// natives in `unsafe_natives.rs` — different method name + descriptor.
// ---------------------------------------------------------------------------

/// Max class file size we accept on the Unsafe.defineClass path. cglib
/// proxies for typical Spring/Hibernate classes are <200 KB; anything
/// over 1 MB is almost certainly a misinterpreted argument (off/len
/// mismatch reading past the array end) and we throw ClassFormatError
/// rather than try to parse it.
pub const UNSAFE_DEFINE_CLASS_MAX_BYTES: usize = 1024 * 1024;

pub fn unsafe_define_class_defensive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::{LinkageError, RuntimeError};
    // arg[0] = this (Unsafe singleton), ignored
    // arg[1] = name : String (may be null — bytecode carries this_class)
    // arg[2] = b : byte[]
    // arg[3] = off : int
    // arg[4] = len : int
    // arg[5] = loader : ClassLoader (may be null — system loader)
    // arg[6] = pd : ProtectionDomain (may be null)

    // Resolve the (possibly null) requested name up-front so it can be
    // attached to thrown exceptions for diagnostics.
    let name_str = match args.get(1) {
        Some(Value::Object(Some(name_obj))) => {
            let dotted = ctx.read_string(*name_obj).unwrap_or_default();
            dotted.replace('.', "/")
        }
        _ => String::new(),
    };

    // arg[2] check: a null bytecode array is a programming error on the
    // caller's side. HotSpot's Unsafe.defineClass NPEs here; throw the
    // same so the Java caller observes the real fault instead of an NPE
    // later on a null Class return.
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            tracing::warn!("Unsafe.defineClass({name_str}): null bytecode array — throwing NPE");
            return Err(RuntimeError::NullPointerException {
                message: Some("Unsafe.defineClass: bytecode array must not be null".into()),
            }
            .into());
        }
    };

    // zt t16/pl7: HotSpot's `Unsafe_DefineClass_impl` counts every call that
    // gets past `Unsafe.defineClass`'s own null check
    // (`sun.cls.unsafeDefineClassCalls`), before it copies the bytes.
    ctx.note_unsafe_define_class_call();

    let array_len = ctx.array_length(byte_array);

    // Defensive offset/length validation.
    let offset = match args.get(3) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => 0,
    };
    let length = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => array_len,
    };

    // Sanity-cap: cglib proxies are small. A zero-length or absurdly large
    // length can't be a valid class file → ClassFormatError (the bytes are
    // structurally malformed), not a silent null.
    if length == 0 || length > UNSAFE_DEFINE_CLASS_MAX_BYTES {
        tracing::warn!(
            "Unsafe.defineClass({name_str}): rejecting bytecode of length {length} \
             (max={UNSAFE_DEFINE_CLASS_MAX_BYTES})"
        );
        return Err(cratonvm_types::error::LinkageError::ClassFormatError {
            class_name: name_str,
            message: format!(
                "Unsafe.defineClass: bytecode length {length} out of range (max {UNSAFE_DEFINE_CLASS_MAX_BYTES})"
            ),
        }
        .into());
    }

    // Bounds: offset+length must fit inside the array. An out-of-bounds
    // slice is an AIOOBE on the caller's side (HotSpot's Unsafe range
    // checks throw before parsing), so surface that rather than a null.
    // checked_add prevents wrap-around on pathological inputs.
    if offset
        .checked_add(length)
        .map_or(true, |end| end > array_len)
    {
        tracing::warn!(
            "Unsafe.defineClass({name_str}): offset/length out of bounds \
             (off={offset}, len={length}, array={array_len}) — throwing AIOOBE"
        );
        return Err(RuntimeError::aioobe_index_only(
            offset.saturating_add(length).min(i32::MAX as usize) as i32,
        )
        .into());
    }

    // Copy bytes defensively. Any out-of-band element read returns 0 byte.
    // Wrap in `catch_unwind` so a panic during the copy (corrupt array
    // header, GC-moved object on the cglib path) returns null instead
    // of SIGABRT.
    let class_bytes_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        read_class_bytes_bulk(ctx, byte_array, offset, length)
    }));
    let class_bytes = match class_bytes_result {
        Ok(b) => b,
        Err(_) => {
            tracing::error!(
                "Unsafe.defineClass({name_str}): panic while reading byte array; \
                 throwing ClassFormatError"
            );
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: name_str,
                message: "Unsafe.defineClass: failed to read bytecode array".into(),
            }
            .into());
        }
    };
    // zt t16/pl7: then `JVM_DefineClass`, with the bytes in hand, counts the
    // define and its bytes (`sun.cls.defineAppClasses` / `appClassBytes`).
    let counted = ctx.note_jvm_define_class(class_bytes.len());
    // zt t16/pl7: and times it (`sun.cls.defineAppClassTime` / `.self`;
    // `CRATONVM_PERF_CLASS_TIMERS`, default ON, inert under `=0`) to this
    // native's end, as `jvm_define_class_common`'s `PerfClassTraceTime`.
    let _define_trace = cratonvm_types::perf_class_trace::ClassTraceTime::begin(
        counted.then(|| ctx.class_trace_counters()).flatten(),
        cratonvm_types::perf_class_trace::ClassTraceEvent::DefineClass,
    );

    // Sanity check magic before handing to backend — `define_class_full`
    // already checks this, but doing it here keeps the warn log clear
    // about WHO rejected the bytecode. Require at least 8 bytes
    // (magic + minor + major) so the backend never reads past EOF.
    // Malformed bytes → ClassFormatError (JVMS 5.3.5), not a silent null,
    // in HotSpot's `ClassFileParser` wording (`defineClass0` ->
    // `JVM_DefineClass`; interpreter round i4 wave 95, lane L5).
    if class_bytes.len() < 8 || class_bytes[0..4] != CLASS_FILE_MAGIC {
        tracing::warn!("Unsafe.defineClass({name_str}): bad magic — throwing ClassFormatError");
        return Err(classfile_parser_header_refusal(
            ctx,
            &name_str,
            &class_bytes,
        ));
    }

    // cglib SEGV guard — this is the hottest path for the cglib_probe
    // reproducer because cglib's `ReflectUtils.defineClass` calls into
    // `sun.misc.Unsafe.defineClass`.
    if let Some(v) = cglib_guard_value(ctx, &name_str, &class_bytes) {
        return Ok(Some(v));
    }

    // Resolve loader id from arg[5]. Null loader → system (id 0).
    let loader_id = match args.get(5) {
        Some(Value::Object(Some(loader_obj))) => get_or_assign_loader_id(ctx, *loader_obj),
        _ => 0,
    };

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(6) {
        Some(Value::Object(Some(pd))) => extract_pd_code_source_url(ctx, *pd),
        _ => None,
    };

    let mut opts = cratonvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        ..Default::default()
    };
    // zs s8/nd7: the mirror object before the define (no reference is used
    // after this point).
    let reservation = crate::lang_system::reserve_mirror_for_define(
        ctx,
        &crate::lang_system::define_name_hint(&name_str, &class_bytes),
        &mut opts,
    )?;
    // catch_unwind: malformed cglib bytes (10-50 KB) can crash the
    // backend parser. Translate panic → ClassFormatError so the Java
    // caller sees a recoverable linkage error instead of a process exit
    // (or a null Class that NPEs later).
    let define_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.define_class_full(&name_str, &class_bytes, loader_id, opts)
    }));
    let reserved = crate::lang_system::take_mirror_reservation(ctx, reservation);
    let define_result = match define_result {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(
                "Unsafe.defineClass({name_str}): panic inside define_class_full; \
                 throwing ClassFormatError"
            );
            return Err(cratonvm_types::error::LinkageError::ClassFormatError {
                class_name: name_str,
                message: "Unsafe.defineClass: panic inside backend (likely malformed bytecode)"
                    .into(),
            }
            .into());
        }
    };
    match define_result {
        Ok(cid) => {
            let mirror = crate::lang_system::defined_class_mirror_reserved(ctx, cid, reserved);
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            // Backend rejected the bytes. A "not found" style failure maps
            // to NoClassDefFoundError; everything else is a malformed-class
            // (ClassFormatError). Either way the caller observes the real
            // fault rather than an NPE on a null Class.
            tracing::warn!("Unsafe.defineClass({name_str}) backend failed: {msg} — throwing");
            let lower = msg.to_ascii_lowercase();
            if lower.contains("not found") || lower.contains("no class def") {
                Err(LinkageError::NoClassDefFoundError {
                    class_name: if name_str.is_empty() {
                        msg.clone()
                    } else {
                        name_str
                    },
                }
                .into())
            } else {
                Err(LinkageError::ClassFormatError {
                    class_name: name_str,
                    message: format!("Unsafe.defineClass: {msg}"),
                }
                .into())
            }
        }
    }
}

pub fn cl_resolve_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // resolveClass(Class) — trigger class preparation and linking
    if let Some(Value::Object(Some(class_mirror))) = args.get(1) {
        if let Some(cid) = crate::lang_class::mirror_class_id(ctx, *class_mirror) {
            let _ = ctx.ensure_class_initialized(&ctx.class_name_of_id(cid).unwrap_or_default());
        }
    }
    Ok(None)
}

pub fn cl_find_loaded_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // JVM spec: findLoadedClass checks if a class has already been loaded by
    // this loader (or delegated to a parent). Does NOT trigger class loading.
    let this = obj_arg(args, 0)?;
    let name_str = match args.get(1) {
        Some(Value::Object(Some(name_obj))) => {
            let dotted = ctx.read_string(*name_obj).unwrap_or_default();
            dotted.replace('.', "/")        }
        _ => return Ok(Some(Value::Object(None))),
    };

    // Loader-scoped lookup (shared with real-JDK mode): a user-defined loader
    // reports a class only if it is in that loader's own namespace or it is the
    // recorded defining loader — NOT a class some other loader (typically the
    // application loader) happens to have loaded. A fresh custom loader thus
    // gets null for an app-loaded class, so its override-first redefinition
    // (Spring's OverridingClassLoader) fires and it becomes the defining loader.
    // Built-in loaders keep the global (no-load) lookup, correct for them.
    // A class HotSpot never defined (its package's sealing refused it) is
    // not found (`hide_package_refused`, wave 95).
    let found = find_loaded_class_for_loader(ctx, this, &name_str);
    Ok(Some(Value::Object(
        crate::classloader_real::hide_package_refused(ctx, found),
    )))
}

pub fn cl_get_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(match classloader_parent(ctx, this) {
        Some(parent) => Value::Object(Some(parent)),
        None => Value::Object(None),
    }))
}

pub fn cl_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // L1: `CL_NAME_REF` is slot 2, which on the real layout is
    // `unnamedModule` — reading it back handed a `Module` to a caller
    // expecting a `String`. The real `name` is slot 1 and is written BY NAME,
    // so ask for it that way on that layout and keep the synthetic slot for
    // ours. (The by-name read is deliberately NOT tried first on our own
    // layout: `resolve_field_index_in_hierarchy` answers the MOST-DERIVED
    // declaration, so a user loader that happens to declare its own `name`
    // field would shadow the loader name — the trap `native_enum_name` fell
    // into.)
    let name_val = if cl_has_synthetic_layout(ctx, this) {
        ctx.get_field(this, CL_NAME_REF)
    } else {
        ctx.get_field_by_name(this, "name")
    };
    if let Value::Object(Some(_)) = name_val {
        Ok(Some(name_val))
    } else {
        // Return loader type as name if no explicit name set
        let lt = loader_type_of(ctx, this).unwrap_or(LOADER_CUSTOM);
        let name_str = match lt {
            LOADER_BOOTSTRAP => "bootstrap",
            LOADER_PLATFORM => "platform",
            LOADER_APP => "app",
            _ => "custom",
        };
        let s = ctx.create_string(name_str);
        Ok(Some(Value::Object(Some(s))))
    }
}

pub fn cl_get_system_class_loader(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let app = get_or_create_app_loader(ctx);
    Ok(Some(Value::Object(Some(app?))))
}

pub fn cl_get_platform_class_loader(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let platform = get_or_create_platform_loader(ctx);
    Ok(Some(Value::Object(Some(platform?))))
}

/// Public re-export of the `getResource` (singular) native so
/// `register_essential_natives` can install it in real-JDK mode. Without
/// this the JDK's own `ClassLoader.getResource` runs — and in real-JDK
/// mode the URLClassPath `<clinit>` swallow leaves the loader's resource
/// tables empty, so it returns null even for resources the bulk
/// `getResources` enumerator finds. Wave-1 Task B consistency fix.
pub fn cl_get_resource_essential(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if jdk_only_user_loader_receiver(ctx, args)
        || jdk_only_lazy_app_class_path_receiver(ctx, args)
    {
        // gci i18/vp: a tail call (`continuation_pin::tail_call`) -- the
        // registered callback's answer is the body's, same descriptor (the
        // interpreter's null-name door calls it through the counted funnel
        // too). The body asks the user loader's `findResource`, application
        // code that may park (a lock, a latch, a socket read), which
        // unmounts a virtual thread on HotSpot.
        return cratonvm_native_api::continuation_pin::tail_call(|| {
            ctx.invoke_special_bytecode_only(
                "java/lang/ClassLoader",
                "getResource",
                "(Ljava/lang/String;)Ljava/net/URL;",
                args,
            )
        });
    }
    cl_get_resource(ctx, args)
}

/// `--real-jdk`: a USER-DEFINED loader's `getResource`/`getResources`/
/// `getResourceAsStream` run the JDK's own `ClassLoader` body. That body asks
/// the parent and then the loader's own `findResource(s)`, which for a
/// `URLClassLoader` is the real `URLClassPath` and so the URLs' own handlers,
/// as on HotSpot. These natives answered from the VM's flat class path and
/// minted handler-less URLs instead, so Spring Boot's `LaunchedClassLoader`
/// handed out `jar:file:/app.jar!/BOOT-INF/classes/X.class` where HotSpot
/// hands out `jar:nested:/app.jar/!BOOT-INF/classes/!/X.class` carrying Spring's
/// handler. See
/// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
///
/// Only the INSTANCE forms with a user-loader receiver, counting a BARE
/// `URLClassLoader`, which is a loader of its own there: the static
/// `getSystemResource(s)` share these callbacks with the resource name at
/// `args[0]`, and the built-in loaders' lookups stay native, because the
/// application class path is the VM's own. `--compatible` over a real JDK
/// does the same with the `JdkUrlClassLoaders` row (interpreter round i1
/// wave 52, lane L5; `ctx.jdk_url_class_loaders()`), counted in the
/// alignment census there.
pub fn jdk_only_user_loader_receiver(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    if args.len() < 2 || !ctx.jdk_url_class_loaders() {
        return false;
    }
    let routed = match args.first() {
        Some(Value::Object(Some(receiver))) => {
            is_user_defined_loader(ctx, *receiver) || is_bare_url_class_loader(ctx, *receiver)
        }
        _ => false,
    };
    if routed && !ctx.is_jdk_only() {
        ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkUrlClassLoaders);
    }
    routed
}

/// [`jdk_only_user_loader_receiver`] for a receiver in hand (interpreter round
/// i6 wave 120, lane L7; `CRATONVM_USER_LOADER_RESOURCE_BYTECODE`): the same
/// predicates (answered from the `LoaderKind` memo when
/// `CRATONVM_LOADER_KIND_MEMO` is on) and the same per-call `--compatible`
/// alignment count, for the interpreter's cached hit of a filed
/// `ClassLoader` resource body
/// (`native_override::intercept_force_registered_native_cached`), so that hit
/// runs the body the native would have re-entered.
pub fn user_loader_resource_body_for_receiver(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
) -> bool {
    if !ctx.jdk_url_class_loaders() {
        return false;
    }
    let routed = is_user_defined_loader(ctx, receiver) || is_bare_url_class_loader(ctx, receiver);
    if routed && !ctx.is_jdk_only() {
        ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkUrlClassLoaders);
    }
    routed
}

/// [`jdk_only_user_loader_receiver`]'s verdict for a receiver CLASS
/// (interpreter round i6 wave 119, lane L7; the L5 predicate of
/// `i73-L5-proposal-user-loader-get-resource-without-the-native-hop`): `true`
/// when every instance of `cid` would have its instance `getResource` /
/// `getResources` / `getResourceAsStream` routed by those natives to the
/// JDK's own `ClassLoader` body (a user-defined loader class or a bare
/// `URLClassLoader`, with `jdk_url_class_loaders()`), so the interpreter's
/// virtual-dispatch cache may file that body for the receiver class
/// (`CRATONVM_USER_LOADER_RESOURCE_BYTECODE`). Both halves of the receiver
/// predicate are functions of the class ([`compute_loader_kind`], the memo's
/// own computation); `false` where that computation refuses to answer. It
/// notes no `--compatible` alignment count; the cached hit notes it per call
/// ([`user_loader_resource_body_for_receiver`]).
pub fn user_loader_resource_body_for_class(
    ctx: &mut dyn NativeContext,
    cid: cratonvm_types::ClassId,
) -> bool {
    if !ctx.jdk_url_class_loaders() {
        return false;
    }
    // No alignment count here (wave 120): the cache fill is not a call; every
    // call of a filed body is counted at its hit
    // ([`user_loader_resource_body_for_receiver`]), as the native counts it.
    compute_loader_kind(ctx, cid).is_some_and(|k| k.user_defined || k.bare_url)
}

/// `--real-jdk`: the APPLICATION loader's `getResource`/`getResources`/
/// `getResourceAsStream` run the JDK's own `ClassLoader` body (its
/// `BuiltinClassLoader.findResource(s)` over the real lazy `URLClassPath`)
/// when opening the application class path runs a URL stream handler
/// provider lookup (`NativeContext::class_path_provider_lookup_sensitive`: a
/// manifest `Class-Path` token with a scheme other than `file`, `jar` and
/// `jrt`). There the class lookups already run that `URLClassPath` (a
/// `Class-Path` turns the native gate off), and only the JDK's own walk
/// re-enters it from `URL.lookupViaProviders`' `ServiceLoader`, which is
/// what throws HotSpot's `Error("Circular loading of URL stream handler
/// providers detected")` and loses the jars it loses; the VM's flat answer
/// here let the providers lookup finish instead, so a `--real-jdk` run kept
/// jars HotSpot never opens (interpreter round i5 wave 108, lane L5; probe
/// `tools/probes/interp/L5/L5W107ClassPathDriveToken`). Every other class
/// path keeps the native answer. The static `getSystemResource(s)` forms
/// (name at `args[0]`) are not routed.
pub fn jdk_only_lazy_app_class_path_receiver(ctx: &mut dyn NativeContext, args: &[Value]) -> bool {
    if args.len() < 2 || !ctx.is_jdk_only() || !ctx.class_path_provider_lookup_sensitive() {
        return false;
    }
    match args.first() {
        Some(Value::Object(Some(receiver))) => is_app_loader_object(ctx, *receiver),
        _ => false,
    }
}

/// True iff the object's class is `java/lang/ClassLoader` or a subclass.
///
/// The getResource/getResources natives ALSO serve the STATIC
/// `getSystemResource(s)` forms (same handler registration), where args[0]
/// is the resource-name String, not a receiver. The user-loader delegation
/// must not treat that String as a ClassLoader: doing so dispatched
/// `invoke_virtual(<String>, "findResource")` →
/// `NoSuchMethodError java/lang/String.findResource` and broke every
/// `getSystemResource` caller (kafka-codec / hadoop-conf / hbase-conf
/// regression-pool probes).
pub fn is_classloader_instance(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let mut cur = ctx.class_id_of_object(obj);
    for _ in 0..64 {
        match ctx.class_name_of_id(cur) {
            Some(n) if n == "java/lang/ClassLoader" => return true,
            Some(n) if n == "java/lang/Object" => return false,
            _ => {}
        }
        match ctx.superclass_of(cur) {
            Some(p) if p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

/// Kill switch for the first-hit `ClassLoader.getResource` walk below.
/// `CRATONVM_GETRESOURCE_FIRST_HIT=0` restores the whole-list walk that builds
/// every matching URL and returns element 0. Default ON.
///
/// A same-binary lever, not a safety valve: the two walks are required to
/// answer identically (`class_path.rs`'s
/// `the_incremental_walk_returns_what_the_whole_list_walk_returns_first`), so
/// the only thing this flag can change is how much of the classpath was
/// touched to get there. That makes it the A/B for the cost, which is the one
/// claim a page about a throughput gap has to be able to check.
pub fn get_resource_first_hit_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            None::<std::ffi::OsString>
                .as_deref()
                .and_then(|s| s.to_str()),
            Some("0")
        )
    })
}

/// The first classpath URL for `resource_name`, stopping at the entry that
/// answers.
///
/// The one implementation behind BOTH singular resource doors —
/// `ClassLoader.getResource` here and `Class.getResource` in `lang_class` —
/// because they had the same whole-list-then-take-element-0 shape and a fix to
/// one of them is a fix a bisect can miss on the other.
///
/// Falls back to the whole-list walk for a GLOB name, which can match several
/// names inside a single classpath entry: "the first URL this entry serves"
/// would silently drop the rest, and `resource_name_supports_incremental_scan`
/// is the predicate that knows the difference.
pub fn first_resource_url(
    ctx: &mut dyn NativeContext,
    resource_name: &str,
) -> Option<String> {
    if is_glob_resource_name(resource_name) {
        return ctx
            .find_all_resource_urls(resource_name)
            .into_iter()
            .find(|url| url_names_literal_resource(url, resource_name));
    }
    if get_resource_first_hit_enabled()
        && ctx.resource_name_supports_incremental_scan(resource_name)
    {
        return ctx
            .next_resource_url(resource_name, 0, 0)
            .map(|(url, _, _)| url);
    }
    ctx.find_all_resource_urls(resource_name).into_iter().next()
}

/// Does `name` hold a `*` or `?`? The class-path store treats those as a
/// one-segment glob (`ClassPath`'s `simple_resource_glob`, which this crate's
/// own Rust callers use, e.g. `package_class_glob`), but a Java
/// `ClassLoader.getResource*` / `Class.getResource*` name is LITERAL on
/// HotSpot: `URLClassPath` asks `JarFile.getEntry(name)` or
/// `new File(dir, name)`, so `getResource("p/*.txt")` is `null` unless an entry
/// is literally named `p/*.txt`. Interpreter round i5 wave 100, lane L3
/// (`L3W100ResourceGlobLiteral`): the built-in loaders' doors answered the
/// first glob match.
pub fn is_glob_resource_name(name: &str) -> bool {
    name.contains(|c: char| c == '*' || c == '?')
}

/// Does the class-path URL `url` name the resource `name` itself (its path
/// ends with `/<name>` or `!/<name>`), rather than a glob match of it? The
/// filter [`is_glob_resource_name`] names apply to the store's answers.
pub fn url_names_literal_resource(url: &str, name: &str) -> bool {
    url.strip_suffix(name)
        .is_some_and(|head| head.ends_with('/'))
}

/// The boot-visible (`jrt:`) URL the platform loader -- or a loader whose
/// parent is the bootstrap loader -- answers for `resource_name` in
/// [`cl_get_resource`]: the first `jrt:` URL of the class-path walk.
///
/// With `CRATONVM_PLATFORM_RESOURCE_JRT_WALK` (default off; interpreter round
/// i3 wave 73, lane L5) only the class-path entries that can name a `jrt:`
/// resource are asked (`NativeClassAccess::first_runtime_image_resource_url`),
/// instead of building every entry's URL -- an `exists()` per directory entry,
/// an index probe and a `String` per jar -- and keeping the first `jrt:` one.
/// The answer is the same either way (`ClassPath::first_runtime_image_resource_url`
/// and its unit test). Every `getResource` / `getResourceAsStream` of a
/// `URLClassLoader` whose parent is the platform loader reaches this once,
/// through the JDK's `ClassLoader.getResource` body (`L5W72DefinePathBench`'s
/// `jar-read-bytes`, `L5W73ReadPathBench`'s `parent-get-resource`).
pub fn boot_visible_resource_url(ctx: &mut dyn NativeContext, resource_name: &str) -> Option<String> {
    if crate::jboss_jdkspecific::builtin_loaders_hide_resource(ctx, resource_name) {
        return None;
    }
    if ctx.platform_resource_jrt_walk() {
        return ctx.first_runtime_image_resource_url(resource_name);
    }
    let glob = is_glob_resource_name(resource_name);
    ctx.find_all_resource_urls(resource_name)
        .into_iter()
        .find(|url| {
            url.starts_with("jrt:") && (!glob || url_names_literal_resource(url, resource_name))
        })
}

/// Does the `jrt:/<module>/...` URL name a module the loader kind sees: the
/// boot loader's modules, plus the platform loader's for `with_platform`
/// (the JDK's `ModuleLoaderMap`). Non-`jrt:` URLs and an unreadable map pass.
pub fn jrt_url_visible_to(url: &str, sets: Option<&BuiltinModuleSets>, with_platform: bool) -> bool {
    let (Some(sets), Some(module)) = (
        sets,
        url.strip_prefix("jrt:/").and_then(|rest| rest.split('/').next()),
    ) else {
        return true;
    };
    sets.boot.contains(module) || (with_platform && sets.platform.contains(module))}

/// [`boot_visible_resource_url`] for a loader kind: the first `jrt:` URL of a
/// module the boot loader (and, `with_platform`, the platform loader) defines.
///
/// Interpreter round i4 wave 98, lane L3 (`L3W98ResourcesByLoader` row
/// `platform getResource(com/sun/tools/javac/Main.class)`: HotSpot null, both
/// modes answered `jrt:/jdk.compiler/...`): `jdk.compiler` is the APPLICATION
/// loader's module, so neither the platform loader nor a null-parent loader's
/// `BootLoader.findResource` sees it.
pub fn loader_visible_jrt_url(
    ctx: &mut dyn NativeContext,
    resource_name: &str,
    with_platform: bool,
) -> Option<String> {
    if resource_name == MODULE_INFO_CLASS {
        let kind = if with_platform {
            BuiltinLoaderKind::Platform
        } else {
            BuiltinLoaderKind::Boot
        };
        if let Some(urls) = builtin_loader_module_info_urls(ctx, kind) {
            return urls.into_iter().next();
        }
    }
    let first = boot_visible_resource_url(ctx, resource_name)?;
    let sets = jdk_builtin_module_sets(ctx);
    let sets = sets.as_deref();
    if jrt_url_visible_to(&first, sets, with_platform) {
        return Some(first);
    }
    ctx.find_resource_urls_in_segment(resource_name, 0)
        .into_iter()
        .find(|url| url.starts_with("jrt:") && jrt_url_visible_to(url, sets, with_platform))
}

/// The one resource name every module holds and no module package does.
pub const MODULE_INFO_CLASS: &str = "module-info.class";

/// `String.hashCode()` of `s`: UTF-16 code units, `31 * h + c`, wrapping.
pub fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(i32::from(c)))
}

/// The table length of a `new ConcurrentHashMap<>(32)` holding `count`
/// entries put one at a time: 64 slots (`tableSizeFor(1 + 32 / 0.75)`), doubled
/// whenever the size reaches `sizeCtl = n - n / 4` (`addCount`).
pub fn chm32_table_len(count: usize) -> usize {
    let mut n = 64usize;
    while count >= n - (n >> 2) {
        n <<= 1;
    }
    n
}

/// `ConcurrentHashMap`'s bin of key `name` in a `table_len`-slot table:
/// `spread(h) & (n - 1)`, `spread(h) = (h ^ (h >>> 16)) & HASH_BITS`.
pub fn chm_bucket(name: &str, table_len: usize) -> usize {
    let h = java_string_hash(name) as u32;
    (((h ^ (h >> 16)) & 0x7fff_ffff) as usize) & (table_len - 1)
}

/// HotSpot's answer to `getResources("module-info.class")` asked of a built-in
/// loader of `kind`: `BuiltinClassLoader.findMiscResource` of the boot loader,
/// then (platform, application) the platform loader's, then (application) the
/// application loader's and its class path (`findResourcesOnClassPath`). Each
/// loader contributes one `ModuleReader.find("module-info.class")` URL per
/// module it defines, in its `nameToModule` iteration order. `None` when the
/// boot configuration or the JDK's `ModuleLoaderMap` tables are not known
/// (the caller keeps its class-path walk, which finds nothing).
///
/// Interpreter round i4 wave 99, lane L3 (`L3W99ModuleInfoResources`; page
/// `interpreter-i98-L3-builtin-loader-resource-rows-differ-from-hotspot-FIXED-20261015.md` row
/// 4): HotSpot answers `getResource` with `jrt:/jdk.management.jfr/...` and
/// lists the 62 boot-layer modules; both modes answered null and `[]`,
/// because the VM's class-path index holds no `module-info.class` and the
/// built-in loaders' lookups are this crate's natives in both modes.
///
/// The set is the boot configuration (`boot_configuration_module_names`),
/// split by the JDK's own `ModuleLoaderMap` tables (boot, platform, every
/// other module to the application loader). The ORDER inside a loader is a
/// `ConcurrentHashMap<>(32)`'s: ascending bin (`chm_bucket`), and inside a
/// bin, insertion order. HotSpot's insertion order is `ModuleBootstrap`'s
/// `loadModules` over `Configuration.modules()`, a `Set.of` whose iteration
/// depends on `ImmutableCollections.SALT32L` and `ResolvedModule`'s identity
/// hash; with the default CDS archive the loaders themselves (and so their
/// maps) are restored from dump time (`ArchivedClassLoaders`), so the order is
/// fixed per JDK build but is not a function of the module names, and under
/// `-Xshare:off` it changes from run to run. This VM orders a shared bin by
/// `java.base` first (`ModuleBootstrap` loads it before the rest), then
/// DESCENDING name: an empirical choice, not a rule. It reproduces 8 of the
/// 10 shared bins of Temurin 25.0.3's archived order, including bin 0 of the
/// boot loader (`jdk.management.jfr` before `java.rmi`), so `getResource`
/// answers HotSpot's URL; the platform loader's bin 62 (`jdk.naming.dns`
/// before `jdk.security.jgss`) and the application loader's bin 14
/// (`jdk.internal.md` before `jdk.jstatd`) come out swapped. The bins
/// themselves are HotSpot's. A table past 47 entries is resized and its bins
/// re-split, which no Temurin 25 loader reaches.
pub fn builtin_loader_module_info_urls(
    ctx: &mut dyn NativeContext,
    kind: BuiltinLoaderKind,
) -> Option<Vec<String>> {
    let names = ctx.boot_configuration_module_names();
    if names.is_empty() {
        return None;
    }
    let sets = jdk_builtin_module_sets(ctx)?;
    let mut groups: [Vec<String>; 3] = Default::default();
    for name in names {
        if ctx.module_is_class_path_only(&name) {
            continue;
        }
        let group = if sets.boot.contains(&name) {
            0
        } else if sets.platform.contains(&name) {
            1
        } else {
            2
        };
        groups[group].push(name);
    }
    let upto = match kind {
        BuiltinLoaderKind::Boot => 1,
        BuiltinLoaderKind::Platform => 2,
        BuiltinLoaderKind::Application => 3,
    };
    let mut urls = Vec::new();
    for group in groups.iter_mut().take(upto) {
        let table_len = chm32_table_len(group.len());
        group.sort_by(|a, b| {
            (
                chm_bucket(a, table_len),
                a != "java.base",
                std::cmp::Reverse(a.as_str()),
            )
                .cmp(&(
                    chm_bucket(b, table_len),
                    b != "java.base",
                    std::cmp::Reverse(b.as_str()),
                ))
        });
        for name in group.iter() {
            match ctx.module_path_resource_url(name, MODULE_INFO_CLASS) {
                Some(Some(url)) => urls.push(module_path_hotspot_url(&url)),
                Some(None) => {}
                None => urls.push(format!("jrt:/{name}/{MODULE_INFO_CLASS}")),
            }
        }
    }
    if kind == BuiltinLoaderKind::Application {
        for url in ctx.find_resource_urls_in_segment(MODULE_INFO_CLASS, 2) {
            // A `--module-path` entry the application segment also lists is
            // already in as that module's own URL, spelled HotSpot's way
            // (`jar:file:///...`); its class-path spelling (`jar:file:/...`)
            // is not a second resource (wave 100, lane L3).
            let module_spelling = module_path_hotspot_url(&url);
            if !url.starts_with("jrt:") && !urls.contains(&url) && !urls.contains(&module_spelling)
            {
                urls.push(url);
            }
        }
    }
    Some(urls)
}

/// The built-in loader kind whose `getResource*` a fall-through call of the
/// `ClassLoader` resource natives answers: the receiver's, or the application
/// loader's for the static `getSystemResource*` forms (no receiver).
pub fn builtin_resource_receiver_kind(ctx: &dyn NativeContext, args: &[Value]) -> BuiltinLoaderKind {
    if args.len() >= 2 {
        if let Some(Value::Object(Some(receiver))) = args.first() {
            let class = ctx.class_name_of_id(ctx.class_id_of_object(*receiver));
            if let Some(kind) = builtin_loader_kind(class.as_deref()) {
                return kind;
            }
        }
    }
    BuiltinLoaderKind::Application
}

pub fn cl_get_resource(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // ClassLoader.getResource(String) → URL
    //
    // Spec contract: returns the FIRST URL the parent-delegated search
    // would return for `name`, or null. Must be consistent with
    // `getResources`: if `getResources(name)` returns N≥1 URLs, then
    // `getResource(name)` must return the first of those URLs (not null,
    // not a different URL form). The bulk path walks every classpath
    // entry via `find_all_resource_urls`; the singular path here mirrors
    // that walk and returns its first element so the two stay in lock-step.
    //
    // We scan `args` for the LAST String-typed slot (mirroring the bulk
    // path) so the same native can serve `getSystemResource` (static —
    // name at index 0) and instance `getResource` (name at index 1).
    // The STATIC forms (`getSystemResource`, `getSystemResourceAsStream`) take
    // the name at index 0 with NO receiver, so the `args.len() >= 2` guard just
    // below never saw them and a null name answered null instead of throwing.
    // MEASURED in BOTH modes (`probes/ClassLoaderShadowSweep.java`): HotSpot
    // NullPointerException, this VM no-throw.
    //
    // HotSpot's is `Objects.requireNonNull(name)`'s: no message. Raised as
    // the VM's own NPE (not a built throwable) so the native-call funnel marks
    // it and the capture lists the served JDK method at its raise
    // (`CRATONVM_THROWABLE_SERVED_RAISE_FRAMES`; interpreter round i2 wave
    // 54, lane L5, probe `L5W54ClassLoaderNullResourceName`).
    if args.len() == 1 && !matches!(args.first(), Some(Value::Object(Some(_)))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    if args.len() >= 2 && !matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    let name = {
        let mut found: Option<String> = None;
        for v in args.iter().rev() {
            if let Value::Object(Some(o)) = v {
                if let Some(s) = ctx.read_string(*o) {
                    found = Some(s);
                    break;
                }
            }
        }
        found.unwrap_or_default()
    };
    // A LEADING SLASH makes the lookup FAIL; it is not stripped.
    //
    // This is the asymmetry that catches everyone: `Class.getResource` takes a
    // name that may be absolute (leading `/`) OR relative to the class's
    // package, while `ClassLoader.getResource` takes an always-absolute name
    // that must NOT begin with `/`. The JDK uses the name verbatim, so `/X`
    // simply matches nothing.
    //
    // MEASURED in BOTH modes (`probes/ClassLoaderShadowSweep.java`):
    //
    //   APP.getResource("/ClassLoaderShadowSweep.class")
    //     HotSpot  null      CratonVM  file:/.../ClassLoaderShadowSweep.class
    //
    // `trim_start_matches('/')` made the two spellings equivalent -- more
    // permissive than the JDK in the direction that HIDES a bug: code passing a
    // `Class.getResource`-shaped name to a ClassLoader works here and returns
    // null on every other VM.
    //
    // Scoped to THIS native. The four sibling `trim_start_matches` calls in
    // this file serve `Class.getResource`-shaped doors, where stripping is
    // CORRECT, and were not in the measured set.
    if name.starts_with('/') {
        return Ok(Some(Value::Object(None)));
    }
    let resource_name = name.as_str();

    // A URLClassLoader has a private, receiver-owned URL set. Its public
    // `getResource` is nevertheless parent-first: Spring's
    // `FilteredClassLoader`, for example, has an empty local URL array and
    // relies on its resource-bearing parent. Routing it straight to the local
    // resolver skipped that parent and made a dynamically supplied
    // `hazelcast.xml` invisible, so Hazelcast auto-configuration quietly
    // registered no instance. Search the real parent first, then use the
    // receiver-local resolver; never fall through to the generic flat path,
    // which could leak sibling loader resources.
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        // A platform loader may expose JDK-module resources (`jrt:`), but it
        // cannot see the application's flat classpath. Treating the global
        // resource walk as its implementation makes a child whose explicit
        // parent is platform observe application resources that HotSpot would
        // reject. Spring's ModifiedClassPathClassLoader deliberately uses that
        // topology to exclude individual JARs.
        if is_platform_class_loader(ctx, this_ref) {
            if let Some(first) = loader_visible_jrt_url(ctx, resource_name, true) {
                let url = crate::jboss_module_loader::build_synthetic_url(ctx, &first);
                return Ok(Some(Value::Object(Some(url?))));
            }
            return Ok(Some(Value::Object(None)));
        }
        if object_extends(ctx, this_ref, "java/net/URLClassLoader") {
            // URLClassLoader (bare instance OR a user-defined subclass) is
            // always a user loader for `getResource` purposes: real
            // `ClassLoader.getResource()` delegates to the parent FIRST
            // regardless of whether the receiver's own class is literally
            // `java.net.URLClassLoader` or a subclass. This used to
            // early-return to the local-only `ucl_find_resource` whenever
            // `is_builtin_loader_class(&class_name)` matched — which is true
            // for the literal string "java/net/URLClassLoader" itself (see
            // its `matches!` list), so a plain, directly-instantiated
            // `new URLClassLoader(urls, parent)` — a completely ordinary
            // idiom for a thin resource/class overlay with a real,
            // resource-bearing parent, e.g. Spring Boot's
            // `ServletComponentScanIntegrationTests.indexedComponentsAreRegistered`
            // wrapping just a `@TempDir` holding a generated
            // `META-INF/spring.components` index — silently skipped parent
            // delegation and could only ever see its own (here, near-empty)
            // local URL set. `is_builtin_loader_class`'s other match arms
            // (`jdk/internal/loader/*`, `sun/misc/Launcher$*`) are dead code
            // in this specific branch on a modern JDK: none of those classes
            // actually extend `java.net.URLClassLoader` (JDK 9+ internal
            // loaders derive from `BuiltinClassLoader`, not `URLClassLoader`),
            // so removing the gate does not change behavior for them.
            let this_pin = ctx.pin_native_root(this_ref);
            let name_for_parent = Value::Object(Some(ctx.create_string(&name)));
            let this_live = ctx.read_native_pin(this_pin, this_ref);
            // ModifiedClassPathClassLoader deliberately uses the platform
            // loader as its parent so its URL set is the complete, isolated
            // test class path. Parent-first resource lookup would reintroduce
            // application resources that its exclusions removed.
            //
            // `url_classloader_isolated_from_app` answers "no null parent
            // to consult" with `true` — deliberately correct for that
            // predicate's own job (should an APP-level parent be walked),
            // but a `null` parent is not "nothing to consult": real
            // `ClassLoader.getResource` still asks the BOOTSTRAP loader
            // (`parent != null ? parent.getResource(name) :
            // BootLoader.findResource(name)`). So the null-parent bootstrap
            // fallback below runs unconditionally, ahead of the
            // isolated-from-app gate, which only governs the non-null-parent
            // walk.
            if matches!(
                ctx.get_field_by_name(this_live, "parent"),
                Value::Object(None)
            ) {
                // A bare `new URLClassLoader(urls, null)` — e.g. byte-buddy's
                // `ClassFileLocator.ForClassLoader.ofBootLoader()` proxy, used
                // by `ClassInjector.UsingUnsafe`'s self-bootstrap of an
                // `AccessibleObject` mirror — relies on this to read a
                // `java.base` class file. Search boot-visible (`jrt:`)
                // resources before falling to this loader's own local URLs.
                if let Some(first) = loader_visible_jrt_url(ctx, resource_name, false) {
                    let url = crate::jboss_module_loader::build_synthetic_url(ctx, &first);
                    ctx.unpin_native_roots(this_pin);
                    return Ok(Some(Value::Object(Some(url?))));
                }
            } else if !url_classloader_isolated_from_app(ctx, this_live) {
                if let Value::Object(Some(parent)) = ctx.get_field_by_name(this_live, "parent") {
                    let parent_pin = ctx.pin_native_root(parent);
                    let parent_live = ctx.read_native_pin(parent_pin, parent);
                    let parent_result = ctx.invoke_virtual(
                        parent_live,
                        "getResource",
                        "(Ljava/lang/String;)Ljava/net/URL;",
                        &[name_for_parent],
                    );
                    ctx.unpin_native_roots(parent_pin);
                    if matches!(parent_result, Ok(Some(Value::Object(Some(_))))) {
                        ctx.unpin_native_roots(this_pin);
                        return parent_result;
                    }
                }
            }
            let this_live = ctx.read_native_pin(this_pin, this_ref);
            let name_for_local = Value::Object(Some(ctx.create_string(&name)));
            let local_result =
                ucl_find_resource(ctx, &[Value::Object(Some(this_live)), name_for_local]);
            ctx.unpin_native_roots(this_pin);
            return local_result;
        }
    }

    // User-defined classloader delegation (mirrors cl_get_resources).
    // When the receiver is a non-builtin ClassLoader, invoke findResource()
    // via virtual dispatch so the user's override runs (e.g.
    // EmbeddedImplClassLoader.findResource reads from IMPL-JARS).
    // `is_classloader_instance` gates out the STATIC getSystemResource form
    // (args[0] is the name String there, not a receiver).
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        if is_classloader_instance(ctx, this_ref) {
            let class_id = ctx.class_id_of_object(this_ref);
            if let Some(class_name) = ctx.class_name_of_id(class_id) {
                // URLClassLoader itself is parent-first too.  Its exact
                // native registration may receive the base-loader identity
                // even when the live receiver is a subclass, so restricting
                // this to non-builtin names drops a parent's resource stream.
                if object_extends(ctx, this_ref, "java/net/URLClassLoader")
                    || !is_builtin_loader_class_in(&*ctx, &class_name)
                {
                    // JDK `ClassLoader.getResource` contract: delegate to the
                    // PARENT's getResource FIRST, then fall back to this loader's
                    // own `findResource` override. The previous code skipped
                    // parent delegation and called `findResource` directly, so a
                    // custom loader that overrides only `loadClass` (and inherits
                    // the default `findResource`, which returns null) reported
                    // null for every resource its parent (the app/system loader)
                    // can serve. Hibernate's SerializationHelperTest /
                    // ProxyClassReuseTest custom loaders read class bytes via
                    // getResource(AsStream) and broke on this (CNFE for a class
                    // that exists on the classpath).
                    //
                    // Pin the receiver across each allocating create_string —
                    // a moving GC during it would stale `this_ref`.
                    // gen r4w3/rooting: keep `pin` live across the parent's
                    // `getResource` upcall (arbitrary Java) and the bootstrap
                    // URL build; `this_ref` is re-read before `findResource`.
                    let pin = ctx.pin_native_root(this_ref);
                    let name_for_parent = Value::Object(Some(ctx.create_string(&name)));
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    let parent = ctx.get_field_by_name(this_ref, "parent");
                    match parent {
                        Value::Object(Some(parent_ref)) => {
                            if let Ok(Some(Value::Object(Some(url)))) = ctx.invoke_virtual(
                                parent_ref,
                                "getResource",
                                "(Ljava/lang/String;)Ljava/net/URL;",
                                &[name_for_parent],
                            ) {
                                ctx.unpin_native_roots(pin);
                                return Ok(Some(Value::Object(Some(url))));
                            }
                        }
                        Value::Object(None) => {
                            // Null parent = delegate to the bootstrap class
                            // loader, not "no parent to consult" — see the
                            // matching fix in the URLClassLoader branch above.
                            if let Some(first) =
                                loader_visible_jrt_url(ctx, resource_name, false)
                            {
                                let url =
                                    crate::jboss_module_loader::build_synthetic_url(ctx, &first);
                                ctx.unpin_native_roots(pin);
                                return Ok(Some(Value::Object(Some(url?))));
                            }
                        }
                        _ => {}
                    }
                    // Parent had nothing (or is null/bootstrap): this loader's
                    // own findResource override gets the final say.
                    // gen r4w3/rooting: re-read through the still-held `pin`
                    // (the parent upcall above can move the receiver).
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    let name_for_find = Value::Object(Some(ctx.create_string(&name)));
                    let this_ref = ctx.read_native_pin(pin, this_ref);
                    ctx.unpin_native_roots(pin);
                    return ctx.invoke_virtual(
                        this_ref,
                        "findResource",
                        "(Ljava/lang/String;)Ljava/net/URL;",
                        &[name_for_find],
                    );
                }
            }
        }
    }

    // Prefer the structured URL (jar:file:/... or jrt:/... or file:/...)
    // so getResource and getResources return the same URL form for the
    // same name. Fall back to "classpath:<name>" when only `find_resource`
    // (raw bytes) succeeds — covers synthetic test loaders that override
    // find_resource without participating in the structured walk.
    //
    // STOP AT THE FIRST HIT. `next_resource_url` walks the same segments in
    // the same order and yields the same elements as `find_all_resource_urls`
    // (its own doc states the enumeration-to-exhaustion equivalence), so
    // element 0 is identical either way — but the whole-list call kept
    // scanning after it had the answer. For an archive entry that costs a hash
    // probe; for a DIRECTORY entry it costs an `exists()` and a canonicalize,
    // i.e. filesystem syscalls, on every remaining entry of the classpath.
    //
    // `getResource` is one call per class discovered by a ShrinkWrap package
    // scan (`ClassLoaderAsset.<init>` is `classLoader.getResource(name)`),
    // which is what made a quarkus `TestResourceManager.start()` several times
    // slower than HotSpot — HotSpot's `getResource` returns at the first hit.
    // The incremental walk already existed for the lazy `getResources`
    // enumeration; the singular door had simply never been wired to it.
    //
    // The gate is required: a GLOB name can match several entries WITHIN one
    // classpath entry, and "the first URL this entry serves" would drop the
    // rest — `resource_name_supports_incremental_scan` is what excludes those,
    // and they fall through to the whole-list walk below.
    //
    // A built-in loader never answers a non-class name of a named module's
    // non-open package, from the image or the class path
    // (`builtin_loaders_hide_resource`).
    //
    // `module-info.class`: the boot loader's first module's, as HotSpot's
    // parent chain ends at `BootLoader.findResource` (interpreter round i4
    // wave 99, lane L3; `builtin_loader_module_info_urls`).
    if resource_name == MODULE_INFO_CLASS {
        let kind = builtin_resource_receiver_kind(&*ctx, args);
        if let Some(first) =
            builtin_loader_module_info_urls(ctx, kind).and_then(|urls| urls.into_iter().next())
        {
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, &first);
            return Ok(Some(Value::Object(Some(url?))));
        }
    }
    if crate::jboss_jdkspecific::builtin_loaders_hide_resource(ctx, resource_name) {
        return Ok(Some(Value::Object(None)));
    }
    let url_str = if let Some(first) = first_resource_url(ctx, resource_name) {
        first
    } else if !is_glob_resource_name(resource_name) && ctx.find_resource(resource_name).is_some() {
        format!("classpath:{name}")
    } else {
        let dbg_all = crate::vmflags().loader.dbg_getresources;
        if dbg_all {
            eprintln!(
                "[GRES-DBG] getResource({}) -> NULL (no urls, no bytes)",
                resource_name
            );
        }
        return Ok(Some(Value::Object(None)));
    };
    let url_str = module_path_resource_spelling(ctx, resource_name, url_str);

    let dbg_all = crate::vmflags().loader.dbg_getresources;
    if dbg_all {
        eprintln!("[GRES-DBG] getResource({}) -> {}", resource_name, url_str);
    }

    tracing::debug!(
        target: "cratonvm_vm::runtime::resources",
        resource = %resource_name,
        url = %url_str,
        "ClassLoader.getResource resolved"
    );

    let url = crate::jboss_module_loader::build_synthetic_url(ctx, &url_str);
    Ok(Some(Value::Object(Some(url?))))
}

/// Public re-export of the `getResources` native for `register_essential_natives`
/// so the override is available in real-JDK mode regardless of feature flag.
pub fn cl_get_resources_essential(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if jdk_only_user_loader_receiver(ctx, args)
        || jdk_only_lazy_app_class_path_receiver(ctx, args)
    {
        // gci i18/vp: a tail call, as in `cl_get_resource_essential`.
        return cratonvm_native_api::continuation_pin::tail_call(|| {
            ctx.invoke_special_bytecode_only(
                "java/lang/ClassLoader",
                "getResources",
                "(Ljava/lang/String;)Ljava/util/Enumeration;",
                args,
            )
        });
    }
    // `getResources` MAY delegate a non-builtin loader to its `findResources`.
    cl_get_resources_impl(ctx, args, true)
}

pub fn cl_get_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_get_resources_impl(ctx, args, true)
}

pub fn url_external_form_string(ctx: &mut dyn NativeContext, url: ObjectRef) -> Option<String> {
    let p_url = ctx.pin_native_root(url);
    let url_live = ctx.read_native_pin(p_url, url);
    let result = ctx.invoke_virtual(url_live, "toExternalForm", "()Ljava/lang/String;", &[]);
    let out = match result {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    ctx.unpin_native_roots(p_url);
    out
}

pub fn collect_url_enumeration_strings(
    ctx: &mut dyn NativeContext,
    enumeration: ObjectRef,
    out: &mut Vec<String>,
) {
    const MAX_RESOURCE_ENUMERATION: usize = 16_384;
    let p_enum = ctx.pin_native_root(enumeration);
    for _ in 0..MAX_RESOURCE_ENUMERATION {
        let enumeration = ctx.read_native_pin(p_enum, enumeration);
        let has_more = match ctx.invoke_virtual(enumeration, "hasMoreElements", "()Z", &[]) {
            Ok(Some(Value::Int(v))) => v != 0,
            _ => false,
        };
        if !has_more {
            break;
        }

        let enumeration = ctx.read_native_pin(p_enum, enumeration);
        let next = ctx.invoke_virtual(enumeration, "nextElement", "()Ljava/lang/Object;", &[]);
        if let Ok(Some(Value::Object(Some(url)))) = next {
            if let Some(s) = url_external_form_string(ctx, url) {
                out.push(s);
            }
        }
    }
    ctx.unpin_native_roots(p_enum);
}

/// A URL retained while two resource enumerations are being merged.  The
/// fallback is only for lightweight non-moving test contexts whose global-root
/// implementation is intentionally a no-op.
#[derive(Clone, Copy)]
pub struct RootedUrl {
    pub root: usize,
    pub fallback: ObjectRef,
}

/// The null-parent step of `ClassLoader.getResources`:
/// `BootLoader.findResources(name)` ([`boot_loader_resource_urls`]) appended
/// to `out` as URL objects. Interpreter round i4 wave 99, lane L3
/// (`interpreter-i99-L3-null-parent-get-resources-omits-boot-FIXED-20261015.md`).
pub fn collect_boot_loader_resources(
    ctx: &mut dyn NativeContext,
    name: &str,
    out: &mut Vec<RootedUrl>,
) -> Result<(), MethodCallFailed> {
    let urls = boot_loader_resource_urls(ctx, name);
    if urls.is_empty() {
        return Ok(());
    }
    let enm = enumeration_from_url_strings(ctx, &urls)?;
    collect_url_enumeration_objects(ctx, enm, out);
    Ok(())
}

pub fn collect_url_enumeration_objects(
    ctx: &mut dyn NativeContext,
    enumeration: ObjectRef,
    out: &mut Vec<RootedUrl>,
) {
    const MAX_RESOURCE_ENUMERATION: usize = 16_384;
    let p_enum = ctx.pin_native_root(enumeration);
    for _ in 0..MAX_RESOURCE_ENUMERATION {
        let enumeration = ctx.read_native_pin(p_enum, enumeration);
        let has_more = match ctx.invoke_virtual(enumeration, "hasMoreElements", "()Z", &[]) {
            Ok(Some(Value::Int(v))) => v != 0,
            _ => false,
        };
        if !has_more {
            break;
        }

        let enumeration = ctx.read_native_pin(p_enum, enumeration);
        let next = ctx.invoke_virtual(enumeration, "nextElement", "()Ljava/lang/Object;", &[]);
        if let Ok(Some(Value::Object(Some(url)))) = next {
            out.push(RootedUrl {
                root: ctx.add_global_root(url),
                fallback: url,
            });
        }
    }
    ctx.unpin_native_roots(p_enum);
}

/// Preserve parent-first ordering while removing duplicate URL objects from a
/// public `getResources` result. A URLClassLoader's compatibility fallback can
/// surface the same parent URL through both its inherited scan and local probe;
/// HotSpot exposes that physical resource once.
pub fn deduplicate_rooted_urls(ctx: &mut dyn NativeContext, urls: &mut Vec<RootedUrl>) {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(urls.len());
    for rooted in std::mem::take(urls) {
        let url = if rooted.root != 0 {
            ctx.resolve_global_root(rooted.root)
                .or(Some(rooted.fallback))
        } else {
            Some(rooted.fallback)
        };
        let key = url.and_then(|url| url_external_form_string(ctx, url));
        if key.is_some_and(|key| seen.insert(key)) {
            unique.push(rooted);
        } else if rooted.root != 0 {
            let _ = ctx.remove_global_root(rooted.root);
        }
    }
    *urls = unique;
}

/// A `getResources` enumeration that builds each `java.net.URL` only when the
/// caller asks for it.
///
/// The array holds the spec strings the classpath walk produced; slot 2 of the
/// enumeration marks them as such, so `nextElement` runs
/// `build_synthetic_url` per element handed out rather than per element found.
/// See [`ENUM_ELEMENTS_URL_SPECS`] for the measurement that motivated it.
///
/// Falls back to the eager form when the fabricated `Enumeration$Impl` is
/// refused (`--real-jdk`), since the real `java.util.Enumeration` that lands
/// there has no slot to carry the marker and no native to act on it.
/// A `getResources` enumeration that does not scan the classpath until the
/// caller asks for an element, and then only far enough to find it.
///
/// Returns `None` when this call cannot use the form — the name can match more
/// than once inside a single entry (a glob), the VM has no incremental scan
/// (a mock `NativeContext`), or the fabricated `Enumeration$Impl` is refused
/// (`--real-jdk`) — and the caller falls back to scanning eagerly.
///
/// The empty case is left to the caller too. `cl_get_resources_impl` has a
/// documented fallback for "no entry served this name" (the `classpath:<name>`
/// pseudo-URL) and a cap for `META-INF/MANIFEST.MF`, both of which need the
/// whole result to decide; probing for the FIRST element here is what tells
/// the caller whether either can apply.
pub fn lazy_scan_enumeration(
    ctx: &mut dyn NativeContext,
    resource_name: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // `--real-jdk` refuses to fabricate `Enumeration$Impl` (no image declares
    // it): asking anyway logged a "refusing to fabricate" warning and filed a
    // `compatibility-class-requested` violation per run for every
    // `getResources` caller before the eager fallback answered (interpreter
    // round i1 wave 51, lane L5). JIT round 16 (lane compatdrain) landed the
    // same skip behind `enumeration_impl_known_refused` (kill switch
    // `CRATONVM_ENUMERATION_IMPL_SKIP_REFUSED=0` asks first, as before).
    if enumeration_impl_known_refused(ctx)
        || !ctx.resource_name_supports_incremental_scan(resource_name)
    {
        return Ok(None);
    }
    // If the scan yields nothing at all, the caller's empty-result fallback
    // owns the answer — hand it back rather than returning an empty lazy
    // enumeration that would skip it.
    if ctx.next_resource_url(resource_name, 0, 0).is_none() {
        return Ok(None);
    }
    let name_obj = ctx.create_string(resource_name);
    let name_pin = ctx.pin_native_root(name_obj);
    let out = match try_alloc_concurrent_synthetic(ctx, ENUMERATION_IMPL_CLASS, 5) {
        Ok(enm) => {
            let name_obj = ctx.read_native_pin(name_pin, name_obj);
            ctx.set_field(enm, 0, Value::Object(Some(name_obj)));
            ctx.set_field(enm, 1, Value::Int(0));
            ctx.set_field(enm, 3, Value::Int(ENUM_ELEMENTS_LAZY_SCAN));
            ctx.set_field(enm, 4, Value::Int(0));
            Some(enm)
        }
        Err(_) => None,
    };
    ctx.unpin_native_roots(name_pin);
    Ok(out)
}

pub fn lazy_enumeration_from_url_strings(
    ctx: &mut dyn NativeContext,
    urls: &[String],
) -> Result<ObjectRef, MethodCallFailed> {
    // `--real-jdk` refuses the fabricated `Enumeration$Impl`: the eager form
    // directly, without the refused request (see `lazy_scan_enumeration`).
    // (`enumeration_impl_known_refused`: kill switch
    // `CRATONVM_ENUMERATION_IMPL_SKIP_REFUSED=0`, JIT round 16.)
    if enumeration_impl_known_refused(ctx) {
        return enumeration_from_url_strings(ctx, urls);
    }
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    // GC-safety: `create_string` allocates, and `arr` is written across every
    // iteration, so it has to be read back through the pin each time.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, u) in urls.iter().enumerate() {
        let spec = ctx.create_string(u);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(spec)));
    }
    let out = match try_alloc_concurrent_synthetic(ctx, ENUMERATION_IMPL_CLASS, 5) {
        Ok(enm) => {
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_field(enm, 0, Value::Object(Some(arr)));
            ctx.set_field(enm, 1, Value::Int(0));
            ctx.set_field(enm, 3, Value::Int(ENUM_ELEMENTS_URL_SPECS));
            Ok(enm)
        }
        Err(_) => enumeration_from_url_strings(ctx, urls),
    };
    ctx.unpin_native_roots(arr_pin);
    out
}

pub fn enumeration_from_url_strings(
    ctx: &mut dyn NativeContext,
    urls: &[String],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    // GC-safety: `build_synthetic_url` per iteration allocates (transitively
    // GC-triggering); `arr` is written into again via `set_array_element`
    // afterward, both within the same iteration and across iterations, and
    // once more building the enclosing Enumeration below.
    let arr_pin = ctx.pin_native_root(arr);
    for (i, u) in urls.iter().enumerate() {
        let url_obj = crate::jboss_module_loader::build_synthetic_url(ctx, u);
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(url_obj?)));
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    Ok(enm)
}

/// Build a merged enumeration without converting its URLs through external
/// forms.  Custom URLStreamHandler instances are object state, so rebuilding a
/// URL from its String (as the flat-classpath path does) makes in-memory
/// archives such as ShrinkWrap's `archive:` resources unreadable.
pub fn enumeration_from_rooted_urls(
    ctx: &mut dyn NativeContext,
    urls: &[RootedUrl],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
    let arr_pin = ctx.pin_native_root(arr);
    for (i, rooted) in urls.iter().copied().enumerate() {
        let url = if rooted.root != 0 {
            ctx.resolve_global_root(rooted.root)
                .or(Some(rooted.fallback))
        } else {
            Some(rooted.fallback)
        };
        if let Some(url) = url {
            let arr_live = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr_live, i, Value::Object(Some(url)));
        }
        if rooted.root != 0 {
            let _ = ctx.remove_global_root(rooted.root);
        }
    }
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.unpin_native_roots(arr_pin);
    let enm = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
    Ok(enm)
}

/// `allow_delegate` = whether a non-builtin `ClassLoader` receiver may be
/// dispatched to its `findResources(String)` override. It MUST be `false` when we
/// are already serving `findResources` (see `ucl_find_resources`): a loader that
/// subclasses `URLClassLoader` *without* overriding `findResources` (e.g.
/// `groovy.lang.GroovyClassLoader`) inherits the intercepted
/// `URLClassLoader.findResources` → `ucl_find_resources` → back here; re-delegating
/// would call `findResources` again, recursing until the native stack overflows
/// (the recursion bypasses the `execute()` depth guard). See SB-13.
/// True iff the receiver's class (or an ancestor below `java/lang/ClassLoader`)
/// declares its own `findResources(String)` override. When it does, the
/// `getResources` native delegates to that override; when it does not, the
/// loader relies on the default parent-delegating `ClassLoader.getResources`
/// semantics and the native falls back to the flat classpath scan.
pub fn loader_overrides_find_resources(ctx: &mut dyn NativeContext, this_ref: ObjectRef) -> bool {
    const FIND_RESOURCES_DESC: &str = "(Ljava/lang/String;)Ljava/util/Enumeration;";
    let mut cid = Some(ctx.class_id_of_object(this_ref));
    while let Some(c) = cid {
        match ctx.class_name_arc_of_id(c).as_deref() {
            // Reached the base class (or an untyped class): no override found.
            Some("java/lang/ClassLoader") | Some("java/lang/Object") | None => return false,
            _ => {}
        }
        if ctx.class_declares_method(c, "findResources", FIND_RESOURCES_DESC) {
            return true;
        }
        cid = ctx.superclass_of(c);
    }
    false
}

pub fn cl_get_resources_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    allow_delegate: bool,
) -> MethodCallResult {
    // ClassLoader.getResources(String) → Enumeration<URL>
    // Walks EVERY classpath entry (directories, JARs, JMODs, jimage) and
    // returns a URL per match. This is the B3 fix: URLClassPath.<clinit> in
    // real-JDK mode NPEs before it finishes, leaving the classloader's
    // resource tables empty; this native override bypasses the broken path.
    //
    // The first arg is `this` (ClassLoader); the actual resource name is at
    // index 1 for instance calls. For the static `getSystemResources` the
    // name is at index 0. Pick the LAST String-typed arg to avoid confusion
    // with `this` — ClassLoader has no String fields so `read_string(this)`
    // typically returns None, but belt-and-suspenders.
    // `Objects.requireNonNull(name)`'s message-less NPE, raised as the VM's
    // own (see `cl_get_resource`). The static `getSystemResources(null)`
    // (name at index 0, no receiver) answered an empty enumeration -- under
    // `--compatible` an instance of CratonVM's own `java.util.Enumeration$Impl`
    // -- where HotSpot's body (`getSystemClassLoader().getResources(name)`)
    // throws (interpreter round i2 wave 55, lane L5; probe
    // `L5W55SystemResourceRows`, `null-urls`).
    if args.len() == 1 && !matches!(args.first(), Some(Value::Object(Some(_)))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    if args.len() >= 2 && !matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    let name = {
        let mut found: Option<String> = None;
        for v in args.iter().rev() {
            if let Value::Object(Some(o)) = v {
                if let Some(s) = ctx.read_string(*o) {
                    found = Some(s);
                    break;
                }
            }
        }
        found.unwrap_or_default()
    };
    // A LEADING SLASH matches nothing, as in `cl_get_resource`: the JDK uses the
    // name verbatim, so `getSystemResources("/x")` and a built-in loader's
    // `getResources("/x")` are empty on HotSpot, where the strip below served
    // every `x`, and `getResource("/x")` (already `null` here) disagreed with
    // its plural. Only the static form and a built-in, non-`URLClassLoader`
    // receiver: a user loader's `findResources` override decides for itself.
    // Interpreter round i2 wave 56, lane L5 (probe `L5W56ResourceStreamClasses`,
    // `slash-urls`).
    if name.starts_with('/') {
        let builtin_receiver = match args.first() {
            _ if args.len() == 1 => true,
            Some(Value::Object(Some(this_ref))) => {
                let this_ref = *this_ref;
                let class_name = ctx
                    .class_name_of_id(ctx.class_id_of_object(this_ref))
                    .unwrap_or_default();
                is_builtin_loader_class_in(&*ctx, &class_name)
                    && !object_extends(ctx, this_ref, "java/net/URLClassLoader")            }
            _ => false,
        };
        if builtin_receiver {
            let enm = empty_enumeration_impl(ctx)?;
            return Ok(Some(Value::Object(Some(enm))));
        }
    }
    let resource_name = name.trim_start_matches('/');

    // The platform loader exposes the JDK's own module resources (`jrt:`) and
    // NOTHING from the application classpath. Answering it from the flat scan —
    // which is what the fall-through at the end of this function does, since
    // `jdk/internal/loader/*` is a builtin loader class — hands the whole
    // application classpath to every child whose parent is platform.
    //
    // That topology is exactly what Spring Boot's `ModifiedClassPathClassLoader`
    // is built on: it parents itself to the platform loader precisely so its own
    // (exclusion-filtered) URL array is the complete application view. Its
    // `getResources` is parent-first, so an unrestricted platform parent put the
    // excluded jar's `META-INF/services` entry straight back — the same leak the
    // receiver-local half of this fix addresses one level down.
    //
    // `cl_get_resource` has drawn this line for the singular lookup since the
    // ModifiedClassPath work; this is the plural half of the same rule, and it
    // keeps the two spec-consistent (`getResource` must return the first URL
    // `getResources` would).
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        if is_platform_class_loader(ctx, this_ref) {
            // `module-info.class`: the boot and platform loaders' modules
            // (interpreter round i4 wave 99, lane L3).
            let module_info = if resource_name == MODULE_INFO_CLASS {
                builtin_loader_module_info_urls(ctx, BuiltinLoaderKind::Platform)
            } else {
                None
            };
            let urls: Vec<String> = if let Some(urls) = module_info {
                urls
            } else if crate::jboss_jdkspecific::builtin_loaders_hide_resource(ctx, resource_name) {
                Vec::new()
            } else {
                // Interpreter round i4 wave 98, lane L3: only the boot
                // and platform loaders' modules (`loader_visible_jrt_url`).
                let sets = jdk_builtin_module_sets(ctx);
                let glob = is_glob_resource_name(resource_name);
                ctx.find_all_resource_urls(resource_name)
                    .into_iter()
                    .filter(|url| {
                        url.starts_with("jrt:")
                            && jrt_url_visible_to(url, sets.as_deref(), true)
                            && (!glob || url_names_literal_resource(url, resource_name))
                    })
                    .collect()
            };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, urls.len());
            let p_arr = ctx.pin_native_root(arr);
            for (i, url) in urls.iter().enumerate() {
                let url_obj = crate::jboss_module_loader::build_synthetic_url(ctx, url)?;
                let arr = ctx.read_native_pin(p_arr, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(url_obj)));
            }
            let arr = ctx.read_native_pin(p_arr, arr);
            let enm = make_snapshot_enumeration(ctx, arr)?;
            ctx.unpin_native_roots(p_arr);
            return Ok(Some(Value::Object(Some(enm))));
        }
    }

    // See `cl_get_resource`: the URLClassLoader path must stay local rather
    // than falling into the process-wide resource enumeration.
    if allow_delegate {
        if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
            if object_extends(ctx, this_ref, "java/net/URLClassLoader") {
                // ClassLoader.getResources is parent-first, while
                // URLClassLoader.findResources contributes only this
                // receiver's local URLs. Preserve both halves without ever
                // consulting the flattened process-wide resource path.
                let p_this = ctx.pin_native_root(this_ref);
                let mut urls = Vec::new();
                let name_for_parent = Value::Object(Some(ctx.create_string(&name)));
                let this_live = ctx.read_native_pin(p_this, this_ref);
                if let Value::Object(Some(parent)) = ctx.get_field_by_name(this_live, "parent") {
                    let p_parent = ctx.pin_native_root(parent);
                    let parent_live = ctx.read_native_pin(p_parent, parent);
                    if let Ok(Some(Value::Object(Some(enm)))) = ctx.invoke_virtual(
                        parent_live,
                        "getResources",
                        "(Ljava/lang/String;)Ljava/util/Enumeration;",
                        &[name_for_parent],
                    ) {
                        collect_url_enumeration_objects(ctx, enm, &mut urls);
                    }
                    ctx.unpin_native_roots(p_parent);
                } else {
                    // A null parent is the bootstrap loader, not "no parent":
                    // `BootLoader.findResources(name)` comes first, as in
                    // `cl_get_resource`'s null-parent arm (interpreter round
                    // i4 wave 99, lane L3).
                    if let Err(e) = collect_boot_loader_resources(ctx, &name, &mut urls) {
                        ctx.unpin_native_roots(p_this);
                        return Err(e);
                    }
                }
                let this_live = ctx.read_native_pin(p_this, this_ref);
                let name_for_local = Value::Object(Some(ctx.create_string(&name)));
                if let Ok(Some(Value::Object(Some(enm)))) =
                    ucl_find_resources(ctx, &[Value::Object(Some(this_live)), name_for_local])
                {
                    collect_url_enumeration_objects(ctx, enm, &mut urls);
                }
                ctx.unpin_native_roots(p_this);
                deduplicate_rooted_urls(ctx, &mut urls);
                let enm = enumeration_from_rooted_urls(ctx, &urls)?;
                return Ok(Some(Value::Object(Some(enm))));
            }
        }
    }

    // User-defined classloader delegation: if the receiver is a non-builtin
    // ClassLoader subclass (e.g. EmbeddedImplClassLoader), delegate to its
    // findResources() override instead of the flat classpath scan.
    //
    // The real JDK ClassLoader.getResources(name) calls:
    //   1. parent.getResources(name)  (handled by our flat scan when parent is builtin)
    //   2. this.findResources(name)   (the documented override hook)
    //
    // Our native completely replaces step 2, so custom classloaders that
    // override findResources (like ES EmbeddedImplClassLoader, which reads
    // embedded IMPL-JARS directory trees from the outer jar) never get
    // their resources surfaced to ServiceLoader.
    //
    // Fix: when the receiver is a non-builtin ClassLoader, invoke
    // findResources() via virtual dispatch. The callee runs real Java
    // bytecode (e.g. EmbeddedImplClassLoader.findResources constructs an
    // Enumeration that reads embedded jar entries via parent.getResource)
    // and may recursively call our native for the builtin parent loader.
    //
    // Safety: infinite-recursion is avoided because:
    //  - builtin loaders (URLClassLoader, AppClassLoader, …) take the flat
    //    scan path below (is_builtin_loader_class check), not this branch;
    //  - non-builtin loaders whose findResources calls parent.getResources
    //    will hit this branch again only for the PARENT — which IS builtin.
    //
    // `is_classloader_instance` gates out the STATIC getSystemResources form
    // (args[0] is the name String there, not a receiver).
    if allow_delegate {
        if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
            if is_classloader_instance(ctx, this_ref) {
                let class_id = ctx.class_id_of_object(this_ref);
                if let Some(class_name) = ctx.class_name_of_id(class_id) {
                    // URLClassLoader is a JDK builtin, but its local
                    // findResources implementation is precisely the native
                    // hook that serves constructor-supplied and custom-handler
                    // URLs.  Treat it like a delegating loader here; its native
                    // findResources calls back with allow_delegate=false, so
                    // this cannot recurse.
                    // A third-party URLClassLoader subclass (notably
                    // ShrinkWrapClassLoader) may return URLs backed by an
                    // application URLStreamHandler.  The generic merge below
                    // serialises every URL to a String and rebuilds it, which
                    // discards that handler.  Keep the concrete URL objects
                    // by dispatching straight to findResources for the entire
                    // URLClassLoader family, just as the JDK's implementation
                    // does for this local lookup.
                    let is_url_loader = object_extends(ctx, this_ref, "java/net/URLClassLoader");
                    if !is_builtin_loader_class_in(&*ctx, &class_name) || is_url_loader {
                        // Keep URLClassLoader's returned URL objects intact:
                        // serialising them through the generic parent merge
                        // loses application URLStreamHandler state.
                        if is_url_loader {
                            // Calling findResources virtually can select the
                            // real URLClassLoader bytecode through a subclass
                            // call site.  That bytecode recreates a custom
                            // protocol URL without its application handler.
                            // Invoke the native lookup directly so the URL
                            // objects resolved from the recorded base retain
                            // their handler end-to-end.
                            return ucl_find_resources(ctx, args);
                        }
                        // Real `ClassLoader.getResources` is parent-first:
                        // parent.getResources(name) followed by this loader's
                        // findResources(name). The previous native returned only
                        // the findResources override; for URLClassLoader
                        // subclasses such as JasperLoader that meant only the
                        // JSP scratch-dir URLs were visible, while virtual
                        // WEB-INF/classes resources in the webapp parent
                        // disappeared from classpathGetResources.jsp.
                        let p_this = ctx.pin_native_root(this_ref);
                        let mut delegated_urls = Vec::new();

                        let name_for_parent = Value::Object(Some(ctx.create_string(&name)));
                        let this_live = ctx.read_native_pin(p_this, this_ref);
                        let parent = ctx.get_field_by_name(this_live, "parent");
                        if let Value::Object(Some(parent_ref)) = parent {
                            let p_parent = ctx.pin_native_root(parent_ref);
                            let parent_live = ctx.read_native_pin(p_parent, parent_ref);
                            if let Ok(Some(Value::Object(Some(parent_enum)))) = ctx.invoke_virtual(
                                parent_live,
                                "getResources",
                                "(Ljava/lang/String;)Ljava/util/Enumeration;",
                                &[name_for_parent],
                            ) {
                                collect_url_enumeration_objects(
                                    ctx,
                                    parent_enum,
                                    &mut delegated_urls,
                                );
                            }
                            ctx.unpin_native_roots(p_parent);
                        } else if let Err(e) =
                            collect_boot_loader_resources(ctx, &name, &mut delegated_urls)
                        {
                            // Null parent: `BootLoader.findResources(name)`
                            // first (interpreter round i4 wave 99, lane L3).
                            ctx.unpin_native_roots(p_this);
                            return Err(e);
                        }

                        let this_live = ctx.read_native_pin(p_this, this_ref);
                        if loader_overrides_find_resources(ctx, this_live) {
                            let name_for_find = Value::Object(Some(ctx.create_string(&name)));
                            let this_live = ctx.read_native_pin(p_this, this_ref);
                            if let Ok(Some(Value::Object(Some(own_enum)))) = ctx.invoke_virtual(
                                this_live,
                                "findResources",
                                "(Ljava/lang/String;)Ljava/util/Enumeration;",
                                &[name_for_find],
                            ) {
                                collect_url_enumeration_objects(ctx, own_enum, &mut delegated_urls);
                            }
                        }
                        ctx.unpin_native_roots(p_this);

                        // A user-defined loader's default `getResources` is
                        // strictly parent-delegating. In particular, an empty
                        // result is meaningful: falling through to CratonVM's
                        // process-wide classpath scan leaks resources that are
                        // invisible to the loader (and bypasses test doubles
                        // such as EasyMock ClassLoaders). The optional
                        // findResources override above has already contributed
                        // this loader's local entries, so return the combined
                        // enumeration even when it is empty.
                        deduplicate_rooted_urls(ctx, &mut delegated_urls);
                        let enm = enumeration_from_rooted_urls(ctx, &delegated_urls)?;
                        return Ok(Some(Value::Object(Some(enm))));
                    }
                }
            }
        }
    }

    // `module-info.class`: one URL per boot-layer module the receiver's
    // parent chain and its own `findMiscResource` see, in HotSpot's
    // per-loader order (interpreter round i4 wave 99, lane L3;
    // `builtin_loader_module_info_urls`).
    if resource_name == MODULE_INFO_CLASS {
        let kind = builtin_resource_receiver_kind(&*ctx, args);
        if let Some(urls) = builtin_loader_module_info_urls(ctx, kind) {
            let enm = enumeration_from_url_strings(ctx, &urls)?;
            return Ok(Some(Value::Object(Some(enm))));
        }
    }
    // The built-in loaders' answer: none for a non-class name of a named
    // module's non-open package (`builtin_loaders_hide_resource`).
    if crate::jboss_jdkspecific::builtin_loaders_hide_resource(ctx, resource_name) {
        let enm = empty_enumeration_impl(ctx)?;
        return Ok(Some(Value::Object(Some(enm))));
    }
    // The lazy form first: it is the same enumeration, in the same order, but
    // it stops scanning where the caller stops reading. It declines the cases
    // the two fallbacks below need a whole result for.
    if resource_name != "META-INF/MANIFEST.MF" {
        if let Some(enm) = lazy_scan_enumeration(ctx, resource_name)? {
            return Ok(Some(Value::Object(Some(enm))));
        }
    }

    let mut urls = ctx.find_all_resource_urls(resource_name);
    // A `*` / `?` is literal in a Java resource name (`is_glob_resource_name`;
    // interpreter round i5 wave 100, lane L3).
    let glob = is_glob_resource_name(resource_name);
    if glob {
        urls.retain(|url| url_names_literal_resource(url, resource_name));
    }

    // WF32-fix: bound the per-jar `META-INF/MANIFEST.MF` enumeration.
    //
    // Background: CratonVM approximates JBoss module isolation by dumping
    // every resolved module's `<resource-root>` jars onto a single shared
    // application classpath (see `jboss_module_loader::register_resource_roots`).
    // A real JVM running `java -jar jboss-modules.jar` has exactly ONE
    // classpath entry, so `getResources("META-INF/MANIFEST.MF")` returns one
    // URL. Under CratonVM's flat classpath it returns one URL per module jar
    // — 500-750 for a full WildFly install.
    //
    // WildFly's bootstrap iterates that enumeration, doing a
    // `URL.openStream()` + `new Manifest(stream)` on each. With 500+ jars —
    // several of them carrying very large manifests (e.g. `ecj-3.32.0.jar`
    // ships an 889-section, 124 KB MANIFEST.MF) — the interpreted scan runs
    // long past the 120 s stack-dump watchdog, so the boot never makes
    // forward progress: a hang, not a crash.
    //
    // `META-INF/MANIFEST.MF` is special: it exists in essentially every jar,
    // so a flat-classpath enumeration of it is quadratic-by-construction and
    // is never what a module-isolated caller actually wants. Capping it back
    // toward the real-JVM count keeps the scan bounded. The cap is generous
    // (128 — far more than the 1 a real `java -jar` sees) so legitimate
    // multi-jar manifest probes still work; only the pathological 500+-jar
    // module-jar flood is truncated.
    //
    // This is a bounded fallback, NOT the correct end state. A real
    // `module.xml`-driven resolver must give each JBoss module its own
    // isolated `ModuleClassLoader` whose `getResources` only sees that
    // module's own `<resource-root>` jars — then this cap becomes a no-op.
    const MANIFEST_ENUM_CAP: usize = 128;
    if resource_name == "META-INF/MANIFEST.MF" && urls.len() > MANIFEST_ENUM_CAP {
        eprintln!(
            "[jboss-bf] getResources(META-INF/MANIFEST.MF): capping {} flat-classpath \
             matches to {} (CratonVM module-jar flood; see classloader.rs WF32-fix)",
            urls.len(),
            MANIFEST_ENUM_CAP
        );
        urls.truncate(MANIFEST_ENUM_CAP);
    }

    // Always also offer the "classpath:<name>" form when any entry served it
    // via raw bytes but wasn't discovered via the structured walk (e.g. a
    // synthetic test loader that only overrides `find_resource`).
    if urls.is_empty() && !glob {
        if ctx.find_resource(resource_name).is_some() {
            urls.push(format!("classpath:{name}"));
        }
    }

    // ES2-DBG: env-gated tracing for getResources probe + the original
    // spring.factories trace path is subsumed by the env-gated emitter so
    // a single switch covers both diagnostics surfaces.
    //
    // Set `CRATONVM_DBG_GETRESOURCES=1` to dump every invocation's
    // (resource, count, urls) triple. We also keep the legacy
    // spring-specific trace as a no-op fall-through condition because some
    // older debug runs rely on it being always-on.
    let dbg_all = crate::vmflags().loader.dbg_getresources;
    if dbg_all {
        eprintln!(
            "[GRES-DBG] getResources({}) -> {} URLs",
            resource_name,
            urls.len()
        );
        for u in &urls {
            eprintln!("[GRES-DBG]   url: {}", u);
        }
    }

    tracing::debug!(
        target: "cratonvm_vm::runtime::resources",
        resource = %resource_name,
        matches = urls.len(),
        "ClassLoader.getResources enumerated"
    );

    // Build a URL[] and wrap it in our synthetic Enumeration$Impl.  The
    // Enumeration$Impl natives (hasMoreElements, nextElement, hasNext, next)
    // are registered unconditionally by `register_enumeration_impl_natives`
    // so this works in both synthetic-JDK and real-JDK modes without
    // relying on java.util.Vector's internal layout.
    let enm = lazy_enumeration_from_url_strings(ctx, &urls)?;
    Ok(Some(Value::Object(Some(enm))))
}

// ---------------------------------------------------------------------------
// SB-shutdown — `jdk/internal/loader/URLClassPath` safe stubs
//
// Spring Boot's `ClearCachesApplicationListener.clearClassLoaderCaches`,
// invoked on `ContextRefreshedEvent`, reflectively walks the URLClassPath
// graph reachable from `LaunchedURLClassLoader.clearCache()` and calls
// `getURLs()` and `closeLoaders()` on each.  The real-JDK bytecode for
// `URLClassPath.getURLs()` does:
//     synchronized (urls) { return path.toArray(new URL[path.size()]); }
// where `path` may be left at its default-null value when the instance
// reached us via a code path our `<init>` natives don't cover (Unsafe
// allocation, deserialization, custom factories, etc).  The resulting NPE
// is swallowed into our access-violation handler — we see the trace
// terminate at:
//     [BC] jdk/internal/loader/URLClassPath.getURLs()[Ljava/net/URL;
//     ===== SEH trap fired: code=0xC0000005 ...
// because the deref reaches our null-tag sentinel.
//
// These stubs replace the failing bytecode with safe no-op equivalents:
//   - `getURLs()`         → empty `URL[]`              (URL[0])
//   - `closeLoaders()`    → empty `ArrayList`          (List<IOException>)
//   - `closeLoaders()V`   → no-op                      (older signature)
//   - `<clinit>()V`       → no-op                      (idempotent; prevents
//                                                       any future drift in
//                                                       the JDK clinit body
//                                                       from re-introducing
//                                                       null fields)
//   - `findResource(...)` → null URL                   (no resource)
//
// Both `jdk/internal/loader/URLClassPath` (JDK 9+) and
// `sun/misc/URLClassPath` (JDK 8 legacy) are covered.
//
// We piggy-back registration on `register_enumeration_impl_natives` so
// the stubs are picked up from both the real-JDK `register_essential_natives`
// path (which calls `register_enumeration_impl_natives` directly) and the
// synthetic-jdk `register_classloader_natives` path (which calls it via
// the same helper). Idempotent — re-registration is a no-op.
// ---------------------------------------------------------------------------

pub fn ucp_path_urls(ctx: &mut dyn NativeContext, ucp: ObjectRef) -> Option<ObjectRef> {
    let path = match ctx.get_field_by_name(ucp, "path") {
        Value::Object(Some(path)) if is_array_list_object(ctx, path) => path,
        _ => return None,
    };
    let path_pin = ctx.pin_native_root(path);
    let path = ctx.read_native_pin(path_pin, path);
    let size = match ctx.get_field_by_name(path, "size") {
        Value::Int(size) if size > 0 => size as usize,
        _ => {
            ctx.unpin_native_roots(path_pin);
            return None;
        }
    };
    let elements = match ctx.get_field_by_name(path, "elementData") {
        Value::Object(Some(elements)) => elements,
        _ => {
            ctx.unpin_native_roots(path_pin);
            return None;
        }
    };
    let elements_pin = ctx.pin_native_root(elements);
    // `URL[]`, the declared type (i7-L2): `URLClassLoader.getURLs()` hands
    // this out and callers `checkcast` clones of it. The elements are URLs, so
    // the class is loaded and the no-load lookup finds it.
    let url_component = crate::lang_class::reflection_component_id(ctx, "java/net/URL");
    let result = ctx.new_ref_array(url_component, size);
    for index in 0..size {
        let elements = ctx.read_native_pin(elements_pin, elements);
        ctx.set_array_element(result, index, ctx.get_array_element(elements, index));
    }
    ctx.unpin_native_roots(elements_pin);
    ctx.unpin_native_roots(path_pin);
    Some(result)
}

/// Construct a file URL from an absolute class-path entry. The manifest
/// resolver returns native paths rather than URL text, while Java callers of
/// `URLClassLoader.getURLs()` need real URL objects.
pub fn file_url_spec(path: &str) -> String {
    let path = path.replace('\\', "/");
    let encoded = path
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F");
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

pub fn new_url_from_spec(ctx: &mut dyn NativeContext, spec: &str) -> Option<ObjectRef> {
    let text = ctx.create_string(spec);
    let text_pin = ctx.pin_native_root(text);
    let url = match ctx.new_object("java/net/URL") {
        Ok(Some(Value::Object(Some(url)))) => url,
        _ => {
            ctx.unpin_native_roots(text_pin);
            return None;
        }
    };
    let url_pin = ctx.pin_native_root(url);
    let text = ctx.read_native_pin(text_pin, text);
    let result = ctx.invoke_special(
        "java/net/URL",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(url)), Value::Object(Some(text))],
    );
    let url = ctx.read_native_pin(url_pin, url);
    ctx.unpin_native_roots(text_pin);
    ctx.unpin_native_roots(url_pin);
    result.ok().map(|_| url)
}

/// The real JDK application loader is not a `URLClassLoader`, so Spring
/// Boot's `ModifiedClassPathClassLoader` obtains its individual class-path
/// entries from `java.class.path`. CratonVM exposes an AppClassLoader-shaped
/// URL loader instead; handing Spring the suite runner's single manifest
/// pathing JAR causes its exclusion filter to run *before* that manifest is
/// expanded, and the later local lookup silently restores excluded JARs.
///
/// Expose manifest dependencies as effective URLClassLoader entries. CratonVM
/// resolves a manifest `Class-Path` for every receiver-local lookup, so
/// returning its original pathing JAR here lets callers filter a different
/// class path than the loader will subsequently search.
pub fn expanded_manifest_urls(ctx: &mut dyn NativeContext, urls: ObjectRef) -> Option<ObjectRef> {
    // `new_url_from_spec` below can collect; the source array is revisited
    // while discovering the manifest entries, so keep it visible to a moving
    // collector throughout that phase as well.
    let urls_pin = ctx.pin_native_root(urls);
    let count = ctx.array_length(urls);
    let mut paths = Vec::new();
    let mut expanded_any = false;
    for index in 0..count {
        let urls = ctx.read_native_pin(urls_pin, urls);
        let url = match ctx.get_array_element(urls, index) {
            Value::Object(Some(url)) => url,
            _ => {
                ctx.unpin_native_roots(urls_pin);
                return None;
            }
        };
        let Some(path) = extract_url_path(ctx, url) else {
            ctx.unpin_native_roots(urls_pin);
            return None;
        };
        let manifest_paths = if std::path::Path::new(&path).is_file() {
            cratonvm_classloading::ClassPath::read_jar_manifest(std::path::Path::new(&path))
                .map(|manifest| manifest.resolve_class_path(std::path::Path::new(&path)))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if crate::nbflags().dbg_uclres {
            eprintln!(
                "[UCLURLS-DBG] path={path:?} manifest_entries={}",
                manifest_paths.len()
            );
        }
        if manifest_paths.is_empty() {
            paths.push(path);
        } else {
            expanded_any = true;
            paths.extend(manifest_paths);
        }
    }
    if !expanded_any {
        ctx.unpin_native_roots(urls_pin);
        return None;
    }
    ctx.unpin_native_roots(urls_pin);

    // `URL[]` (i7-L2), as `ucp_path_urls`: this replaces that array.
    let url_component = crate::lang_class::reflection_component_id(ctx, "java/net/URL");
    let result = ctx.new_ref_array(url_component, paths.len());
    let result_pin = ctx.pin_native_root(result);
    for (index, path) in paths.iter().enumerate() {
        let Some(url) = new_url_from_spec(ctx, &file_url_spec(path)) else {
            ctx.unpin_native_roots(result_pin);
            return None;
        };
        let result = ctx.read_native_pin(result_pin, result);
        ctx.set_array_element(result, index, Value::Object(Some(url)));
    }
    let result = ctx.read_native_pin(result_pin, result);
    ctx.unpin_native_roots(result_pin);
    Some(result)
}

/// `URLClassPath.getURLs()[Ljava/net/URL;` — return recorded URL paths or an empty URL[].
///
/// Real-JDK bytecode reads `path` (an ArrayList) under a monitor and
/// builds `URL[path.size()]`.  When `path` is null (because the instance
/// was created through a path our `<init>` shim never saw) the deref
/// crashes the VM with an access violation. An empty array is spec-legal
/// (it just means "this loader contributes no URLs") and lets Spring
/// Boot's clearCache iteration complete in zero iterations.
pub fn ucp_get_urls_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `record_ucl_urls` retains constructor URLs in the real `path` field.
    // Returning a copy preserves URLClassLoader's public isolation contract.
    if let Some(Value::Object(Some(ucp))) = args.first() {
        if let Some(urls) = ucp_path_urls(ctx, *ucp) {
            // gen r4w3/rooting: `expanded_manifest_urls` can collect and then
            // answer `None` (a URL build failed); the `unwrap_or` fallback
            // must be the re-read array, not the pre-call address.
            let urls_pin = ctx.pin_native_root(urls);
            let expanded = expanded_manifest_urls(ctx, urls);
            let urls = ctx.read_native_pin(urls_pin, urls);
            ctx.unpin_native_roots(urls_pin);
            let result = expanded.unwrap_or(urls);
            return Ok(Some(Value::Object(Some(result))));
        }
    }
    tracing::debug!(
        target: "cratonvm_vm::runtime::classloader",
        "[URLClassPath shim] getURLs() returning empty URL[]"
    );
    let url_component = crate::lang_class::reflection_component_id(ctx, "java/net/URL");
    let arr = ctx.new_ref_array(url_component, 0);
    Ok(Some(Value::Object(Some(arr))))
}

/// `URLClassPath.closeLoaders()Ljava/util/List;` — return an empty ArrayList.
///
/// The real method walks `loaders` and accumulates IOExceptions from each
/// `Loader.close()` call.  When `loaders` is null we'd NPE; returning an
/// empty list is equivalent to "no loaders to close, no exceptions raised"
/// and matches Spring Boot's expectation (it just logs and continues).
pub fn ucp_close_loaders_list(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::debug!(
        target: "cratonvm_vm::runtime::classloader",
        "[URLClassPath shim] closeLoaders() returning empty ArrayList"
    );
    let list = match ctx.new_object("java/util/ArrayList")? {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // gen r4w3/rooting: the `<init>` invoke runs Java (GC point); return the
    // re-read list, not the pre-invoke address.
    let list_pin = ctx.pin_native_root(list);
    let _ = ctx.invoke(
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(list))],
    );
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    Ok(Some(Value::Object(Some(list))))
}

/// `URLClassPath.closeLoaders()V` — older void signature (pre-JDK 17).
/// Always succeed without side effects.
pub fn ucp_close_loaders_void(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::debug!(
        target: "cratonvm_vm::runtime::classloader",
        "[URLClassPath shim] closeLoaders()V (void variant) no-op"
    );
    Ok(None)
}

/// `URLClassPath.<clinit>()V` — no-op.
///
/// The real-JDK static initializer wires up a `DEBUG` flag and a couple of
/// SharedSecrets accessors.  Replacing it with a no-op is safe: any
/// subsequent method call on URLClassPath either goes through one of our
/// dedicated shims, or operates on instance fields that our `<init>` shims
/// populate explicitly. Suppressing the real clinit also defuses a class
/// of "clinit swallowed, statics left null" failure modes that would
/// otherwise re-introduce NPEs through any new code path the JDK adds.
pub fn ucp_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::debug!(
        target: "cratonvm_vm::runtime::classloader",
        "[URLClassPath shim] <clinit>() no-op"
    );
    Ok(None)
}

/// `URLClassPath.findResource(Ljava/lang/String;Z)Ljava/net/URL;` — return null.
///
/// Spring Boot doesn't rely on this during clearCache, but registering a
/// safe stub closes the same NPE window for any reflective probe that
/// reaches us with a null `loaders` field.
pub fn ucp_find_resource_null(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// Register the URLClassPath safe-stub natives on both the JDK 9+
/// (`jdk/internal/loader/URLClassPath`) and the JDK 8 legacy
/// (`sun/misc/URLClassPath`) class names.  Idempotent — see
/// `NativeMethodRegistry::register` (last call wins on same (class,
/// method, descriptor) tuple, no panic on duplicate).
pub fn register_url_class_path_safe_stubs(r: &mut NativeMethodRegistry) {
    for cls in &["jdk/internal/loader/URLClassPath", "sun/misc/URLClassPath"] {
        // `<clinit>` — no-op so the real-JDK static init body never runs.
        r.register(cls, "<clinit>", "()V", ucp_clinit_noop, cratonvm_native_api::NativeKind::SyntheticStub);
        // `getURLs` — always return an empty URL[]. Two overloads exist on
        // recent JDK builds: the regular `getURLs()` and a package-private
        // `getURLs(boolean)` that includes/excludes the loaderless entries.
        r.register(cls, "getURLs", "()[Ljava/net/URL;", ucp_get_urls_empty, cratonvm_native_api::NativeKind::SyntheticStub);
        r.register(cls, "getURLs", "(Z)[Ljava/net/URL;", ucp_get_urls_empty, cratonvm_native_api::NativeKind::SyntheticStub);
        // `closeLoaders` — both signatures.
        r.register(
            cls,
            "closeLoaders",
            "()Ljava/util/List;",
            ucp_close_loaders_list, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(cls, "closeLoaders", "()V", ucp_close_loaders_void, cratonvm_native_api::NativeKind::SyntheticStub);
        // `findResource` — return null URL when probed reflectively. Both
        // the public (String) form and the internal (String, boolean) form
        // are covered.
        r.register(
            cls,
            "findResource",
            "(Ljava/lang/String;)Ljava/net/URL;",
            ucp_find_resource_null, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "findResource",
            "(Ljava/lang/String;Z)Ljava/net/URL;",
            ucp_find_resource_null, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
}

/// Register natives for our synthetic `java/util/Enumeration$Impl` helper
/// class. Exposed so `register_essential_natives` can call it — needed in
/// real-JDK mode where `register_classloader_natives` (synthetic-only) is
/// skipped.
///
/// Note: we also chain to `register_url_class_path_safe_stubs` from here
/// because `register_essential_natives` (real-JDK path) calls this
/// function unconditionally, and we need the URLClassPath stubs installed
/// in both real-JDK and synthetic-JDK modes. The two concerns are
/// logically distinct but share a single wiring point.
/// The class CratonVM fabricates for a snapshot `java.util.Enumeration`.
///
/// Not a real JDK name: `java.util.Enumeration` is an INTERFACE and has no
/// `$Impl` nested class in any JDK. So under `--real-jdk` the fabrication is a
/// §1.1 violation the policy refuses, which is why it needs the landing below.
pub const ENUMERATION_IMPL_CLASS: &str = "java/util/Enumeration$Impl";

/// Slot 3 of [`ENUMERATION_IMPL_CLASS`]: element slot 0 holds the values the
/// enumeration yields, verbatim. Every producer except `getResources` uses
/// this, and it is what a zero-initialized instance already means, so an
/// `Enumeration$Impl` built by `new_object` (which writes no slots) behaves
/// exactly as it did before slot 3 existed.
///
/// Slot 3, not slot 2: `Hashtable.keys()`/`elements()` already stamp a
/// keys-vs-values discriminator into slot 2, and 1 is its `keys` value — so a
/// marker there turned every `Hashtable` key into a `java.net.URL`
/// (`ClassCastException: java.net.URL cannot be cast to java.lang.String`,
/// xerces reading SAX features, PomProfileReposEffectivePomTest).
pub const ENUM_ELEMENTS_AS_IS: i32 = 0;

/// Slot 3 of [`ENUMERATION_IMPL_CLASS`]: element slot 0 holds URL *spec
/// strings*, and each is turned into a `java.net.URL` by `nextElement`/`next`
/// at the moment it is handed out.
///
/// `ClassLoader.getResources` is the one producer whose consumers routinely
/// stop early. The JDK's own enumeration is lazy per element, so
/// `classLoader.resources(name).anyMatch(..)` — SmallRye Config's
/// `isInClassloader`, and the shape behind every `findFirst`/`anyMatch` over
/// `resources()` — materializes only as far as the first match. CratonVM's
/// native answered eagerly, building a `java.net.URL` (a 13-slot synthetic
/// plus three `String`s) for EVERY match before the caller looked at one.
///
/// On the Quarkus full-reactor harness that meant 1843 URLs built 1388 times
/// — 2.5M URL objects, ~12M allocations — where HotSpot built about eight per
/// call. Deferring construction to `nextElement` restores the short-circuit
/// without changing what the enumeration yields: the strings were already
/// computed by the classpath walk, and `build_synthetic_url` is the same
/// function the eager path called.
pub const ENUM_ELEMENTS_URL_SPECS: i32 = 1;

/// Slot 3 of [`ENUMERATION_IMPL_CLASS`]: there is no element array at all.
/// Slot 0 holds the RESOURCE NAME, and each element is found by resuming the
/// classpath scan from the cursor in slots 1 (entry index) and 4 (class-path
/// segment).
///
/// [`ENUM_ELEMENTS_URL_SPECS`] stopped `getResources` building 1843
/// `java.net.URL`s for a caller that wanted one, but it still SCANNED all 4230
/// classpath entries to collect the spec strings first. Measured on the Quarkus
/// full-reactor classpath, `resources("").anyMatch(..)` matching on element 1
/// cost 1.20 ms against HotSpot's 0.03 ms — 40x, all of it work the caller
/// never asked for. The JDK's enumeration touches one entry; this makes ours
/// do the same.
///
/// There is deliberately no "peeked element" slot: `hasMoreElements` re-runs
/// the same query `nextElement` does rather than caching. The query is a pure
/// function of (name, cursor) that stops at the first hit, so re-running it
/// costs only the entries between here and the next match — and a peek slot
/// would be one more piece of state to keep consistent with the cursor.
pub const ENUM_ELEMENTS_LAZY_SCAN: i32 = 2;

/// A real `java.util.Enumeration` over `array`, or `None` when this image
/// cannot build one.
///
/// Same rule as the snapshot-iterator and `System.Logger` landings, in its
/// strongest form: prefer a real class the JDK BUILDS ITSELF over one whose
/// fields we fill. The snapshot is already an `Object[]`,
/// `java.util.Arrays$ArrayList` is the JDK's own fixed-size list over exactly
/// that shape, and `Collections.enumeration(Collection)` turns one into a real
/// `Enumeration` — real bytecode the whole way, so nothing here has to know
/// what the resulting anonymous class is CALLED. That matters: it is
/// `java.util.Collections$3` on JDK 25, and an anonymous class's number is
/// precisely the kind of name that must not be written down.
///
/// `native-io`'s `zip_real_jar` already drives `Collections.enumeration` this
/// way for `ZipFile.entries()`, so the invoke is known to reach real bytecode
/// rather than a native of ours.
pub fn real_snapshot_enumeration(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let list = match ctx.new_object_initialized(
        "java/util/Arrays$ArrayList",
        "([Ljava/lang/Object;)V",
        &[Value::Object(Some(array))],
    )? {
        Some(Value::Object(Some(list))) => list,
        _ => return Ok(None),
    };
    match ctx.invoke(
        "java/util/Collections",
        "enumeration",
        "(Ljava/util/Collection;)Ljava/util/Enumeration;",
        &[Value::Object(Some(list))],
    )? {
        Some(Value::Object(Some(enm))) => Ok(Some(enm)),
        _ => Ok(None),
    }
}

/// Whether this VM refuses the fabricated [`ENUMERATION_IMPL_CLASS`] without
/// having to ask: `--real-jdk` (`NativeContext::is_jdk_only`), where
/// `java/util/Enumeration$Impl` is not in the image and the synthetic-class
/// door refuses it -- with a `refusing to fabricate a compatibility stand-in`
/// WARN per requester, logged on every run's first `getResources`. The three
/// producers below take their non-fabricated form directly instead; `=0` of
/// `CRATONVM_ENUMERATION_IMPL_SKIP_REFUSED` asks first, as before.
/// `--compatible` never skips. JIT round 16 (lane compatdrain).
pub fn enumeration_impl_known_refused(ctx: &dyn NativeContext) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    ctx.is_jdk_only()
        && *ON.get_or_init(|| {
            true        })}

/// The snapshot enumeration over `array`: the fabricated
/// [`ENUMERATION_IMPL_CLASS`] in `Compatible` mode, and a real
/// `java.util.Enumeration` when `--real-jdk` refuses that fabrication.
///
/// This is the last of the shapes item 4 of
/// `ensure-synthetic-class-cannot-enforce-only-record.md` listed as "refused
/// with nowhere to land". `Compatible` mode is byte-for-byte what it was: the
/// fallback is reached only from the refusal arm, which only strict mode takes.
pub fn make_snapshot_enumeration(
    ctx: &mut dyn NativeContext,
    array: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    // GC-SAFETY: both arms allocate — the fabricated shell, or a real
    // `Arrays$ArrayList` plus whatever `Collections.enumeration` builds — and
    // `array` is a bare Rust local the collector cannot see. Root it across
    // both and read it back through the pin, the same contract
    // `make_iterator_from_array` documents.
    let pin = ctx.pin_native_root(array);
    // `--real-jdk` refuses the fabricated shell: take the real enumeration
    // first, so the refused request (a "refusing to fabricate" warning and a
    // report violation per run) is not made at all (interpreter round i1 wave
    // 51, lane L5). An image with nothing real falls to the request below,
    // whose refusal is then the answer, as before.
    // (`enumeration_impl_known_refused`: kill switch
    // `CRATONVM_ENUMERATION_IMPL_SKIP_REFUSED=0`, JIT round 16.)
    if enumeration_impl_known_refused(ctx) {
        let array = ctx.read_native_pin(pin, array);
        match real_snapshot_enumeration(ctx, array) {
            Ok(Some(enm)) => {
                ctx.unpin_native_roots(pin);
                return Ok(enm);
            }
            Err(err) => {
                ctx.unpin_native_roots(pin);
                return Err(err);
            }
            Ok(None) => {}
        }
    }
    let out = match try_alloc_concurrent_synthetic(ctx, ENUMERATION_IMPL_CLASS, 5) {
        Ok(enm) => {
            let array = ctx.read_native_pin(pin, array);
            ctx.set_field(enm, 0, Value::Object(Some(array)));
            ctx.set_field(enm, 1, Value::Int(0));
            ctx.set_field(enm, 3, Value::Int(ENUM_ELEMENTS_AS_IS));
            Ok(enm)
        }
        Err(refusal) => {
            let array = ctx.read_native_pin(pin, array);
            match real_snapshot_enumeration(ctx, array) {
                Ok(Some(enm)) => Ok(enm),
                // Nothing real to stand in — an image with no
                // `Arrays$ArrayList` — so the refusal stands rather than
                // silently becoming a fabrication again.
                Ok(None) => Err(refusal),
                Err(err) => Err(err),
            }
        }
    };
    ctx.unpin_native_roots(pin);
    out
}

/// `Enumeration$Impl.nextElement()` / `.next()` — one body, because the two
/// have always been the same code and only one of them may now materialize.
///
/// Slot 3 says how to read slot 0's element: verbatim
/// ([`ENUM_ELEMENTS_AS_IS`], what every producer but `getResources` stores and
/// what a zero-initialized instance already means) or as a URL spec string to
/// be turned into a `java.net.URL` right here ([`ENUM_ELEMENTS_URL_SPECS`]).
/// The next `(spec, segment, index)` a lazy-scan enumeration would yield, or
/// `None` when the classpath is exhausted. Pure in the receiver: it reads the
/// cursor but never advances it, so `hasMoreElements` and `nextElement` both
/// call it and agree.
pub fn enum_impl_peek_lazy(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<(String, u32, u32)> {
    let name = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    let index = ctx.get_field(this, 1).as_int().unwrap_or(0).max(0) as u32;
    let segment = ctx.get_field(this, 4).as_int().unwrap_or(0).max(0) as u32;
    ctx.next_resource_url(&name, segment, index)
}

pub fn enum_impl_is_lazy(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    ctx.get_field(this, 3)
        .as_int()
        .unwrap_or(ENUM_ELEMENTS_AS_IS)
        == ENUM_ELEMENTS_LAZY_SCAN
}

pub fn enum_impl_next_element(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if enum_impl_is_lazy(ctx, this) {
        let Some((spec, segment, index)) = enum_impl_peek_lazy(ctx, this) else {
            return Ok(Some(Value::Object(None)));
        };
        ctx.set_field(this, 4, Value::Int(segment as i32));
        ctx.set_field(this, 1, Value::Int(index as i32));
        let url = crate::jboss_module_loader::build_synthetic_url(ctx, &spec)?;
        return Ok(Some(Value::Object(Some(url))));
    }
    let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(arr);
    if idx >= len {
        return Ok(Some(Value::Object(None)));
    }
    let elem = ctx.get_array_element(arr, idx);
    ctx.set_field(this, 1, Value::Int((idx + 1) as i32));
    if ctx
        .get_field(this, 3)
        .as_int()
        .unwrap_or(ENUM_ELEMENTS_AS_IS)
        != ENUM_ELEMENTS_URL_SPECS
    {
        return Ok(Some(elem));
    }
    let Value::Object(Some(spec_obj)) = elem else {
        return Ok(Some(elem));
    };
    let Some(spec) = ctx.read_string(spec_obj) else {
        return Ok(Some(elem));
    };
    // GC-safety: `build_synthetic_url` allocates (a 13-slot URL plus three
    // `String`s), so nothing raw may be held across it. `this` is not read
    // again after this point and the cursor was already advanced, so there is
    // nothing left to re-read through a pin.
    let url = crate::jboss_module_loader::build_synthetic_url(ctx, &spec)?;
    Ok(Some(Value::Object(Some(url))))
}

pub fn register_enumeration_impl_natives(r: &mut NativeMethodRegistry) {
    // Install the URLClassPath safe stubs alongside the enumeration helpers
    // so both real-JDK (`register_essential_natives`) and synthetic-JDK
    // (`register_classloader_natives`) callers pick them up.
    register_url_class_path_safe_stubs(r);

    let enm = "java/util/Enumeration$Impl";
    r.register(enm, "hasMoreElements", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if enum_impl_is_lazy(ctx, this) {
            let more = enum_impl_peek_lazy(ctx, this).is_some();
            return Ok(Some(Value::Int(i32::from(more))));
        }
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        enm,
        "nextElement",
        "()Ljava/lang/Object;",
        enum_impl_next_element, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(enm, "hasNext", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if enum_impl_is_lazy(ctx, this) {
            let more = enum_impl_peek_lazy(ctx, this).is_some();
            return Ok(Some(Value::Int(i32::from(more))));
        }
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(enm, "next", "()Ljava/lang/Object;", enum_impl_next_element, cratonvm_native_api::NativeKind::SyntheticStub);
    let anon_enm = "cratonvm/synthetic/AnonymousObject$2";
    // `SyntheticStub`, stated for this block. The receiver is the VM's
    // anonymous-object fallback class — minted here, on no image — so §1.5's
    // "what an `ACC_NATIVE` method binds to" has nothing to point at, and the
    // `Bridge` tag was only ever keeping a fabricated class reachable under
    // `--real-jdk`, which §5 forbids outright.
            r.register(anon_enm, "hasMoreElements", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        anon_enm,
        "nextElement",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            if idx >= len {
                return Ok(Some(Value::Object(None)));
            }
            let elem = ctx.get_array_element(arr, idx);
            ctx.set_field(this, 1, Value::Int((idx + 1) as i32));
            Ok(Some(elem))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(anon_enm, "hasNext", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let len = ctx.array_length(arr);
        Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(anon_enm, "next", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = ctx.get_field(this, 1).as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(None))),
        };
        let len = ctx.array_length(arr);
        if idx >= len {
            return Ok(Some(Value::Object(None)));
        }
        let elem = ctx.get_array_element(arr, idx);
        ctx.set_field(this, 1, Value::Int((idx + 1) as i32));
        Ok(Some(elem))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    }

/// `ClassLoader.getSystemResources(String)` — static. Delegates to the
/// instance logic; the arg ordering in `args` is compatible because
/// `cl_get_resources` scans args for the first string.
pub fn cl_get_system_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cl_get_resources(ctx, args)
}

pub fn cl_get_resource_as_stream_essential(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if jdk_only_user_loader_receiver(ctx, args)
        || jdk_only_lazy_app_class_path_receiver(ctx, args)
    {
        // gci i18/vp: a tail call, as in `cl_get_resource_essential` (the
        // body also opens the stream: a socket for a remote URL).
        return cratonvm_native_api::continuation_pin::tail_call(|| {
            ctx.invoke_special_bytecode_only(
                "java/lang/ClassLoader",
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;",
                args,
            )
        });
    }
    cl_get_resource_as_stream(ctx, args)
}

pub fn cl_get_resource_as_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // ClassLoader.getResourceAsStream(String) → InputStream.  Mirrors the
    // T19.H10 hardening on `Class.getResourceAsStream`: validate the name
    // (length, control bytes, `..`, `\`) before consulting `find_resource`,
    // and route the BAIS allocation through the shared helper so
    // ClassLoader-side and Class-side resource lookups stay layout-equal.
    // `Objects.requireNonNull(name)`'s message-less NPE, raised as the VM's
    // own (see `cl_get_resource`). The static `getSystemResourceAsStream(null)`
    // (name at index 0, no receiver) answered `null` where HotSpot's body
    // throws (interpreter round i2 wave 55, lane L5; probe
    // `L5W55SystemResourceRows`, `null-stream`).
    if args.len() == 1 && !matches!(args.first(), Some(Value::Object(Some(_)))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    if args.len() >= 2 && !matches!(args.get(1), Some(Value::Object(Some(_)))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    // The `BuiltinClassLoader` / `AppClassLoader` registrations land here
    // directly: the same routing as `cl_get_resource_as_stream_essential`
    // (interpreter round i5 wave 108, lane L5).
    if jdk_only_lazy_app_class_path_receiver(ctx, args) {
        return cratonvm_native_api::continuation_pin::tail_call(|| {
            ctx.invoke_special_bytecode_only(
                "java/lang/ClassLoader",
                "getResourceAsStream",
                "(Ljava/lang/String;)Ljava/io/InputStream;",
                args,
            )
        });
    }
    let Some(name) = args.iter().rev().find_map(|v| match v {
        Value::Object(Some(o)) => ctx.read_string(*o),
        _ => None,    }) else {
        return Ok(Some(Value::Object(None)));
    };
    // `module-info.class` is answered only as a URL (no class-path index
    // holds it; interpreter round i4 wave 99, lane L3), so the static form
    // opens `getSystemResource`'s URL in either setting.
    if args.len() == 1 && (ctx.resource_streams_open_url() || name == MODULE_INFO_CLASS) {
        return system_resource_stream_through_url(ctx, &name);
    }
    let resource_name = name.trim_start_matches('/');
    // Interpreter round i4 wave 98, lane L3 (`L3W98ResourcesByLoader` rows
    // `app stream()` / `context stream()`): `getResourceAsStream("")` is
    // `getResource("")` (the class-path directory) opened, i.e. the directory
    // listing, on HotSpot. The empty name failed the validation below and
    // answered null. Admitted for a loader receiver where the URL is the
    // JDK's own, so `openStream` is its `FileURLConnection`.
    let empty_through_url = name.is_empty() && args.len() >= 2 && ctx.resource_streams_open_url();
    // A loader receiver's stream is its `getResource(name)` opened, and that
    // door bounds no name length; HotSpot serves a resource whose name is
    // longer than 256 chars (interpreter round i5 wave 100, lane L3,
    // `L3W100LongResourceName`: `getResource` found it, the stream was null).
    // The receiver-less byte fallback keeps the bound.
    //
    // Open (i73 closure): this check also gates the delegation to `getResource`
    // below, which validates nothing, so a built-in loader answers `null` here
    // for a name with a backslash or control byte that its own `getResource`
    // finds. See `i73-classloader-resource-natives-review-residuals-20261102.md`, item 2.
    let name_valid = if args.len() >= 2 {
        crate::lang_class::t19_h10_validate_resource_name_unbounded_pub(resource_name).is_some()
    } else {
        crate::lang_class::t19_h10_validate_resource_name_pub(resource_name).is_some()
    };
    if !empty_through_url && !name_valid {
        return Ok(Some(Value::Object(None)));
    }
    // Mirror JDK `ClassLoader.getResourceAsStream` = `URL u =
    // getResource(name); return u != null ? u.openStream() : null;` for
    // EVERY classloader shape, not only `URLClassLoader`/user-defined ones.
    //
    // This used to gate on `object_extends(.., "java/net/URLClassLoader") ||
    // !is_builtin_loader_class(&class_name)`, which EXCLUDED the three
    // built-in loaders (`is_builtin_loader_class`'s `jdk/internal/loader/`
    // arm: `BuiltinClassLoader`, `ClassLoaders$AppClassLoader`,
    // `ClassLoaders$PlatformClassLoader`) and fell through to the raw,
    // loader-identity-BLIND `ctx.find_resource` below — which searches every
    // classpath regardless of which loader asked. Real
    // `getResourceAsStream` is never overridden by any of the three
    // (`javap -p jdk.internal.loader.ClassLoaders$PlatformClassLoader` shows
    // no override; only base `java.lang.ClassLoader` declares it), so
    // HotSpot always answers through `getResource`, which IS
    // loader-identity-aware for the platform loader because it is real
    // bytecode too (native only shadows `getResource` on `cl` and
    // `URLClassLoader` — see the registrar in this file). That asymmetry
    // between the shadowed `getResourceAsStream` and the un-shadowed
    // `getResource` was exactly the defect measured by
    // `L7LoaderInternalsSweep.java` rows 07/08: the platform
    // loader's `getResourceAsStream` found an application-classpath
    // resource its own `getResource` correctly refused. Delegating
    // universally routes every loader shape through the same
    // loader-identity-aware `getResource` HotSpot itself uses, closing the
    // asymmetry instead of special-casing the one class the census happened
    // to name.
    //
    // Routing through `getResource` (which now performs parent delegation)
    // also ensures a custom loader that doesn't override `findResource`
    // still finds resources its parent serves
    // (SerializationHelperTest/ProxyClassReuseTest), and a plain
    // user-instantiated `new URLClassLoader(urls, parent)` (e.g. Spring
    // Boot's `ServletComponentScanIntegrationTests.indexedComponentsAreRegistered`)
    // sees resources reachable only through the dynamically-registered
    // global URL walk (`ctx.find_all_resource_urls`, used by both
    // `getResource` and `ucl_find_resource`'s own fallback) that the raw
    // `ctx.find_resource` store below does not.
    if let Some(Value::Object(Some(this_ref))) = args.first().copied() {
        if is_classloader_instance(ctx, this_ref) {
            let pin = ctx.pin_native_root(this_ref);
            let name_arg = Value::Object(Some(ctx.create_string(&name)));
            let this_ref = ctx.read_native_pin(pin, this_ref);
            ctx.unpin_native_roots(pin);
            // Interpreter round i5 wave 102, lane L5: an exception `getResource`
            // throws propagates, as from the JDK body (`URL url =
            // getResource(name);` sits outside its `try`); only `openStream`'s
            // `IOException` answers null. The `if let Ok(..)` here turned every
            // such exception into a silent `null` stream (the `--real-jdk` over
            // JDK 21 `L5W100MultiReleaseFeature` `resource=null`, i102-L5 page,
            // was indistinguishable from "not found").
            let found = ctx.invoke_virtual(
                this_ref,
                "getResource",
                "(Ljava/lang/String;)Ljava/net/URL;",
                &[name_arg],
            )?;
            if let Some(Value::Object(Some(url))) = found {
                // Both settings: the JDK body's `catch (IOException e)
                // { return null; }` around `openStream` (the other arm let the
                // `IOException` escape).
                return open_resource_url_stream(ctx, url);
            }
            return Ok(Some(Value::Object(None)));
        }
    }
    // No loader receiver here: the receiver-less static form (`args.len() ==
    // 1`), or `Module.getResourceAsStream`'s `[Module, name]`.
    if let Some(bytes) = defined_class_resource_bytes(ctx, resource_name, args.len() == 1) {
        let len = bytes.len();
        let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
        tracing::debug!(
            target: "cratonvm_vm::runtime::resources",
            resource = %resource_name,
            bytes = len,
            "ClassLoader.getResourceAsStream served DEFINED-CLASS resource"
        );
        return Ok(Some(Value::Object(Some(stream?))));
    }
    // A `*` / `?` is literal (`is_glob_resource_name`; wave 100, lane L3):
    // the store's byte lookup would answer a glob match.
    let found = if is_glob_resource_name(resource_name) {
        None
    } else {
        ctx.find_resource(resource_name)
    };
    match found {
        None => Ok(Some(Value::Object(None))),
        Some(bytes) => {
            let len = bytes.len();
            let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
            tracing::debug!(
                target: "cratonvm_vm::runtime::resources",
                resource = %resource_name,
                bytes = len,
                "ClassLoader.getResourceAsStream served resource"
            );
            Ok(Some(Value::Object(Some(stream?))))
        }
    }
}

/// The static `getSystemResourceAsStream(name)` (no receiver) as its JDK body
/// answers: `URL url = getSystemResource(name); return url != null ?
/// url.openStream() : null;`, so the stream is the URL connection's
/// (`BufferedInputStream` for a `file:` class-path entry on HotSpot, a
/// directory's listing for `""`), not a `ByteArrayInputStream` over bytes read
/// here. `name` is passed as given, unvalidated: `getSystemResource("/x")` is
/// `null` and `getSystemResource("")` the class-path directory, as on HotSpot.
/// Only when no URL names it does a dynamically DEFINED class's own bytes
/// still answer (`defined_class_resource_bytes`, a VM extension HotSpot does
/// not have, kept for the CGLIB re-enhancement it was written for), never for
/// a leading-slash name. `CRATONVM_RESOURCE_STREAMS_OPEN_URL` (interpreter
/// round i2 wave 56, lane L5; probe `L5W56ResourceStreamClasses`, `sys`,
/// `slash`, `dir`).
#[inline(never)]
pub fn system_resource_stream_through_url(ctx: &mut dyn NativeContext, name: &str) -> MethodCallResult {
    let name_arg = Value::Object(Some(ctx.create_string_uninterned(name)));
    if let Some(Value::Object(Some(url))) = ctx.invoke_static(
        "java/lang/ClassLoader",
        "getSystemResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        &[name_arg],
    )? {
        return open_resource_url_stream(ctx, url);
    }
    if !name.starts_with('/')
        && crate::lang_class::t19_h10_validate_resource_name_pub(name).is_some()
    {
        if let Some(bytes) = defined_class_resource_bytes(ctx, name, true) {
            let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes)?;
            return Ok(Some(Value::Object(Some(stream))));
        }
    }
    Ok(Some(Value::Object(None)))
}

/// `url.openStream()` as `ClassLoader.getResourceAsStream`'s and
/// `getSystemResourceAsStream`'s JDK bodies call it: an `IOException` from the
/// open answers `null` (`catch (IOException e) { return null; }`), anything
/// else propagates. `CRATONVM_RESOURCE_STREAMS_OPEN_URL` (interpreter round
/// i2 wave 56, lane L5).
#[inline(never)]
pub fn open_resource_url_stream(ctx: &mut dyn NativeContext, url: ObjectRef) -> MethodCallResult {
    match ctx.invoke_virtual(url, "openStream", "()Ljava/io/InputStream;", &[]) {
        Err(MethodCallFailed::ExceptionThrown(exc))
            if object_extends(&*ctx, exc, "java/io/IOException") =>
        {
            Ok(Some(Value::Object(None)))
        }
        other => other,
    }
}

/// Serve a `"<internal/name>.class"` resource request from a class that was
/// dynamically DEFINED (via `Unsafe.defineClass`/`Lookup.defineClass`/
/// `ClassLoader.defineClass`/the various native `define_class_full` callers
/// such as `ConfigurationClassEnhancer.enhance`'s CGLIB-proxy emitter),
/// rather than found on a classpath jar/directory — `ctx.find_resource`
/// only ever searches the bootstrap/extension/application CLASSPATHS, so a
/// runtime-generated class's bytecode was previously never retrievable
/// through `getResourceAsStream`/`Class.getResourceAsStream` at all.
///
/// This matters for real bytecode-introspection tooling: ASM's
/// `ClassReader` (used by real CGLIB, e.g. when Spring AOP's
/// `proxyTargetClass=true` auto-proxy creator tries to CGLIB-subclass an
/// ALREADY-native-CGLIB-generated `ConfigurationClassEnhancer` proxy class
/// a SECOND time) resolves a class's own bytecode this way — without it,
/// generation fails with cglib's generic `Could not generate CGLIB
/// subclass... Common causes... final class or non-visible class`, even
/// though the class is neither.
///
/// Looked up via the existing `class_id_by_name` + `class_bytes` trait
/// methods — a loader-blind global name lookup, same as `resolve_or_load_
/// class_id` elsewhere in this codebase uses for similar "resolve this
/// well-known name" cases. That is unsound in general once 2+ loaders
/// define a same-named class (see the documented duplicate-`ClassId`
/// family elsewhere in this codebase's history) — but every class this
/// function can actually reach was named by one of THIS crate's own
/// generators, all of which mint a process-globally-unique, monotonically-
/// countered suffix (`$$SpringCGLIB$$<n>`, `...$$FB<n>`, etc.), so no two
/// loaders ever define one under the identical name in practice. A future
/// caller relying on this for a DIFFERENT (non-uniquely-named) class would
/// need a proper (name, defining-loader) lookup instead.
///
/// `CRATONVM_DEFINED_CLASS_RESOURCES_DEFINING_LOADER` (interpreter round i2
/// wave 57, lane L5; probe `L5W57DefinedClassResources`) narrows it to the
/// case it was written for: `system_door` (the receiver-less
/// `getSystemResourceAsStream`, ASM `ClassReader(String)`'s door), a VM CGLIB
/// name (`$$SpringCGLIB$$`), and the APPLICATION loader as the class's known
/// defining loader -- the loader that door asks. Every other door and class
/// answers as HotSpot does: no resource (`Lookup.defineClass`, a user
/// loader's `defineClass`, a proxy). Measured
/// (`docs/feature-designs/native-dispatch-architecture.md`).
pub fn defined_class_resource_bytes(
    ctx: &dyn NativeContext,
    resource_name: &str,
    system_door: bool,
) -> Option<Vec<u8>> {
    let internal_name = resource_name.strip_suffix(".class")?;
    if ctx.defined_class_resources_defining_loader() {
        return defined_cglib_class_resource_bytes(ctx, internal_name, system_door);
    }
    let class_id = ctx.class_id_by_name(internal_name)?;
    ctx.class_bytes(class_id)
}

/// [`defined_class_resource_bytes`] under
/// `CRATONVM_DEFINED_CLASS_RESOURCES_DEFINING_LOADER`.
#[inline(never)]
pub fn defined_cglib_class_resource_bytes(
    ctx: &dyn NativeContext,
    internal_name: &str,
    system_door: bool,
) -> Option<Vec<u8>> {
    if !system_door || !internal_name.contains("$$SpringCGLIB$$") {
        return None;
    }
    let app = cratonvm_types::ClassLoaderId::NATIVE_APPLICATION;
    let class_id = ctx.class_id_defined_by_loader_exact(internal_name, app)?;
    if ctx.loader_id_of_class_definite(class_id) != Some(app as i32) {
        return None;
    }
    ctx.class_bytes(class_id)
}

/// `java.lang.Module.getResourceAsStream(String)` (args: `[Module receiver,
/// name]`).
///
/// kotlin-reflect 2.3.20 ships a multi-release jar; under JDK 9+ the loaded
/// `BuiltInsResourceLoader.loadResource` is the `META-INF/versions/9` variant
/// whose body is `kotlin.Unit.class.getModule().getResourceAsStream(path)` —
/// NOT the base jar's `classLoader.getResource(path)`. For classpath classes
/// the module is the *unnamed* module, whose `getResourceAsStream` delegates to
/// the defining class loader's `getResourceAsStream`. The real-JDK bytecode for
/// `Module.getResourceAsStream` walks module/loader internals (the resource map,
/// `BootLoader`, `BuiltinClassLoader.findResource`) that CratonVM does not
/// populate, so it returns null — kotlin-reflect then loads zero `.kotlin_builtins`
/// fragments and asserts "Built-in class kotlin.Int is not found" (SB-15;
/// HotSpot passes).
///
/// Resolve the resource exactly as the ClassLoader-side native does (a classpath
/// scan via `find_resource`), which is the correct behaviour for the unnamed
/// module — `Module` is `final`, so the receiver's own class carries this native
/// and dispatch hits it directly. The receiver (`args[0]`) is ignored: every
/// classpath class shares the one unnamed module / app loader.
pub fn module_get_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    cl_get_resource_as_stream(ctx, args)
}

pub fn resource_stream_for_last_string_arg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name = args
        .iter()
        .rev()
        .find_map(|v| match v {
            Value::Object(Some(o)) => ctx.read_string(*o),
            _ => None,
        })
        .unwrap_or_default();
    let resource_name = name.trim_start_matches('/');
    // Unbounded: a module's `ModuleReader` bounds no name length (interpreter
    // round i5 wave 105, lane L3; the 256-byte bound read a long name of a
    // named module as null).
    if crate::lang_class::t19_h10_validate_resource_name_unbounded_pub(resource_name).is_none() {
        return Ok(Some(Value::Object(None)));
    }
    // JIT round 20 wave 7 (lane revNat): the lookup is asked BY MODULE (the
    // String before the name), and a system module answers only its own
    // resources. Read before anything allocates: `args` is not used after.
    let module = {
        let strings: Vec<String> = args
            .iter()
            .filter_map(|v| match v {
                Value::Object(Some(o)) => ctx.read_string(*o),
                _ => None,
            })
            .collect();
        (strings.len() >= 2).then(|| strings[strings.len() - 2].clone())
    };
    if let Some(scoped) = module
        .as_deref()
        .and_then(|m| system_module_scoped_resource(ctx, m, resource_name))
    {
        return match scoped {
            Some(bytes) => {
                let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
                Ok(Some(Value::Object(Some(stream?))))
            }
            None => Ok(Some(Value::Object(None))),
        };
    }
    // Interpreter round i4 wave 98, lane L3 (`L3W98ModulePathResource`): a
    // named `--module-path` module answers only from its own roots (its
    // `ModuleReader`), never another entry's copy of the name: under
    // `--compatible` its classes' `Class.classLoader` stays null, so
    // `Class.getResourceAsStream` / `Module.getResourceAsStream` ask here.
    if let Some(answer) = module
        .as_deref()
        .and_then(|m| module_path_module_resource_stream(ctx, m, resource_name))
    {
        return answer;
    }
    // A `*` / `?` is literal (`is_glob_resource_name`; wave 100, lane L3).
    let found = if is_glob_resource_name(resource_name) {
        None
    } else {
        ctx.find_resource(resource_name)
    };
    match found {
        Some(bytes) => {
            let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
            Ok(Some(Value::Object(Some(stream?))))
        }
        None => {
            // The class manager's index is GLOBAL (by resource name) and holds
            // no `module-info.class` (one per module). The lookup is asked by
            // MODULE, so a name the index misses is the system module
            // reader's to answer, as HotSpot's `BootLoader` does
            // (`Module.getResourceAsStream("module-info.class")`, SBR round 5,
            // lane compat2). The module is the String before the name.
            if let (Some(module), "module-info.class") = (module.as_deref(), resource_name) {
                if let Some(bytes) = system_module_resource_bytes(ctx, module, resource_name) {
                    let stream =
                        crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes);
                    return Ok(Some(Value::Object(Some(stream?))));
                }
            }
            Ok(Some(Value::Object(None)))
        }
    }
}

/// JIT round 20 wave 7, lane revNat
/// (`docs/feature-designs/native-dispatch-architecture.md`):
/// `resource_name` as SYSTEM module `module` answers it, or `None` when this
/// cannot decide (the caller then keeps the global class-path lookup).
///
/// `BootLoader.findResourceAsStream(mn, name)` and
/// `BuiltinClassLoader.findResourceAsStream(mn, name)` -- what the JDK's
/// `Class.getResourceAsStream` / `Module.getResourceAsStream` call for a class
/// of a named module -- read module `mn` only. Round 20 made both rows stated
/// `Intrinsic`s under `--compatible`, and their native searched the whole
/// boot -> extension -> application class path by name, so `java.base`
/// served `java/sql/Date.class`, `java/util/logging/Logger.class` and the
/// application's own class files (HotSpot: `null`;
/// `R20RevNatModuleResourceScope`).
///
/// * `Some(None)`: `module` is a boot or platform module (the JDK's own
///   `ModuleLoaderMap` tables) and the run-time image does not list the
///   resource in it -- it is in another module, or on no module at all.
/// * `Some(Some(bytes))`: the image lists it in `module` AND in another
///   module (a non-class resource several modules ship), so the bytes are
///   read from `module` itself (`system_module_resource_bytes`).
/// * `None`: not a boot / platform module (a module of the application
///   loader, or the tables are unreadable), `module-info.class` (the caller's
///   own arm), a glob name, a boot class path that does not name image
///   modules (`jrt:` URLs), or the image lists it in `module` only (the
///   global lookup's bytes are then that module's).
///
/// Cold: one segment lookup per call, after the JDK's own encapsulation
/// checks.
pub fn system_module_scoped_resource(
    ctx: &mut dyn NativeContext,
    module: &str,
    resource_name: &str,
) -> Option<Option<Vec<u8>>> {
    if module.is_empty()
        || resource_name == "module-info.class"
        || resource_name.contains(['*', '?', '[', '{'])
    {
        return None;
    }
    let sets = jdk_builtin_module_sets(ctx)?;
    if !sets.boot.contains(module) && !sets.platform.contains(module) {
        return None;
    }
    let urls = ctx.find_resource_urls_in_segment(resource_name, 0);
    if urls.is_empty() {
        return Some(None);
    }
    let modules: Vec<&str> = urls
        .iter()
        .filter_map(|url| {
            url.strip_prefix("jrt:/")
                .and_then(|rest| rest.split('/').next())
        })
        .collect();
    if modules.len() != urls.len() {
        return None;
    }
    if !modules.contains(&module) {
        return Some(None);
    }
    if modules.iter().all(|m| *m == module) {
        return None;
    }
    Some(system_module_resource_bytes(ctx, module, resource_name))
}

/// The bytes of `name` in system module `module`, read through the JDK's own
/// `ModuleFinder.ofSystem().find(module).open().open(name)`; `None` when the
/// module or the resource is absent.
pub fn system_module_resource_bytes(
    ctx: &mut dyn NativeContext,
    module: &str,
    name: &str,
) -> Option<Vec<u8>> {
    let finder = match ctx.invoke_static(
        "java/lang/module/ModuleFinder",
        "ofSystem",
        "()Ljava/lang/module/ModuleFinder;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(f)))) => f,
        _ => return None,
    };
    let pin = ctx.pin_native_root(finder);
    let result = (|| {
        let m = ctx.create_string_uninterned(module);
        let finder = ctx.read_native_pin(pin, finder);
        let opt = match ctx.invoke_virtual(
            finder,
            "find",
            "(Ljava/lang/String;)Ljava/util/Optional;",
            &[Value::Object(Some(m))],
        ) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
        let mref = match ctx.invoke_virtual(
            opt,
            "orElse",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(None)],
        ) {
            Ok(Some(Value::Object(Some(r)))) => r,
            _ => return None,
        };
        let reader =
            match ctx.invoke_virtual(mref, "open", "()Ljava/lang/module/ModuleReader;", &[]) {
                Ok(Some(Value::Object(Some(r)))) => r,
                _ => return None,
            };
        let rpin = ctx.pin_native_root(reader);
        let n = ctx.create_string_uninterned(name);
        let reader = ctx.read_native_pin(rpin, reader);
        let bytes = (|| {
            let ropt = match ctx.invoke_virtual(
                reader,
                "read",
                "(Ljava/lang/String;)Ljava/util/Optional;",
                &[Value::Object(Some(n))],
            ) {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            };
            let bb = match ctx.invoke_virtual(
                ropt,
                "orElse",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(None)],
            ) {
                Ok(Some(Value::Object(Some(b)))) => b,
                _ => return None,
            };
            let bpin = ctx.pin_native_root(bb);
            let len = match ctx.invoke_virtual(bb, "remaining", "()I", &[]) {
                Ok(Some(Value::Int(l))) if l >= 0 => l as usize,
                _ => return None,
            };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
            let apin = ctx.pin_native_root(arr);
            let bb = ctx.read_native_pin(bpin, bb);
            let arr2 = ctx.read_native_pin(apin, arr);
            ctx.invoke_virtual(
                bb,
                "get",
                "([B)Ljava/nio/ByteBuffer;",
                &[Value::Object(Some(arr2))],
            )
            .ok()?;
            let arr2 = ctx.read_native_pin(apin, arr);
            Some(crate::keystore::read_byte_array(ctx, arr2))
        })();
        let reader = ctx.read_native_pin(rpin, reader);
        let _ = ctx.invoke_virtual(reader, "close", "()V", &[]);
        bytes
    })();
    ctx.unpin_native_roots(pin);
    result
}

/// `jdk.internal.loader.BootLoader.findResourceAsStream(String,String)`.
///
/// `java.lang.Module.getResourceAsStream` delegates here for named boot modules
/// such as `java.desktop`. CratonVM does not populate the JDK's internal module
/// resource maps, but its classpath manager already indexes JMOD/JImage
/// resources by module, so serve the requested resource bytes from that path.
pub fn bootloader_find_resource_as_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    resource_stream_for_last_string_arg(ctx, args)
}

/// `--real-jdk`: `jdk.internal.loader.BootLoader.findResource(String)`, the
/// bootstrap step of the JDK's `ClassLoader.getResource` for a loader with a
/// null parent.
///
/// Its JDK body asks `ClassLoaders.bootLoader()`, a `BuiltinClassLoader` this
/// VM never builds, so it answered null for every name. That did not matter
/// while the `ClassLoader.getResource*` natives answered user loaders
/// themselves. Once those run the JDK's body for user loaders, a null-parent
/// `URLClassLoader` could not see `java/lang/reflect/AccessibleObject.class`.
/// ByteBuddy reads exactly that to build its `Unsafe` injector, and without it
/// EasyMock could not mock `ClassLoader` (Tomcat's `TestWebappServiceLoader`).
///
/// The answer is the image's `jrt:` URL, only for a module the JDK's own
/// `ModuleLoaderMap` gives to the BOOT loader. `java.sql` and the other
/// platform modules are not visible from here, as on HotSpot. See
/// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
pub fn bootloader_find_resource(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match boot_module_resource_urls(ctx, args).first() {
        Some(url) => {
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, url)?;
            Ok(Some(Value::Object(Some(url))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `--real-jdk`: `jdk.internal.loader.BootLoader.findResources(String)`, the
/// plural of [`bootloader_find_resource`].
pub fn bootloader_find_resources(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let urls = boot_module_resource_urls(ctx, args);
    Ok(Some(Value::Object(Some(enumeration_from_url_strings(
        ctx, &urls,
    )?))))
}

/// `--compatible`: `jdk.internal.loader.BootLoader.findResource(String mn,
/// String name)`, what JDK 25's `Class.getResource` calls for a class of a
/// named BOOT module (`cl == null`) once its encapsulation checks passed.
///
/// Interpreter round i4 wave 95, lane L3 (`L3W94CallerSensitiveMatrix` row
/// `res-class-direct`, `Object.class.getResource("Object.class")`: HotSpot
/// `jrt:/java.base/java/lang/Object.class`, `--compatible` null). The JDK body
/// asks the boot `BuiltinClassLoader`'s `nameToModule`, which only
/// `--real-jdk` fills, so every named boot module's own resource read null.
/// `Class.getResource`'s registered native (`lang_class::native_class_get_resource`,
/// whose wave-94 arm `boot_module_class_resource_url` was meant to answer this)
/// is an ambient `Bridge` the JDK body beats at `resolve_dispatch` step 3, so
/// it never runs over a real JDK. This is the URL sibling of
/// [`bootloader_find_resource_as_stream`], stated `Intrinsic` under
/// `--compatible` only, as that row is; the JDK's own `Class` / `Module` code
/// keeps every check that runs before it.
///
/// The answer is HotSpot's `ModuleReader.find(name)` of system module `mn`:
/// the image's `jrt:/<mn>/<name>` URL when the run-time image lists `name` in
/// module `mn` (any system module since i4 wave 97: `--compatible` mirrors of
/// platform- and application-loader image classes carry a null
/// `Class.classLoader`, so their `Class.getResource` reaches this door; see
/// the body); `null` otherwise, and for `mn == null` (the boot class path,
/// which a JDK image launch does not have).
pub fn bootloader_find_module_resource(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let null = Ok(Some(Value::Object(None)));
    let (module, name) = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(m))), Some(Value::Object(Some(n)))) => {
            match (ctx.read_string(*m), ctx.read_string(*n)) {
                (Some(m), Some(n)) if !m.is_empty() && !n.is_empty() => (m, n),
                _ => return null,
            }
        }
        _ => return null,
    };
    // Interpreter round i4 wave 97, lane L3 (`L3W96PlatformModuleResource`,
    // `--compatible` rows `sql-date-*`, `http-client-class`, `javac-main-*`):
    // NOT filtered to the boot set. `--compatible` defines every image class
    // in its boot namespace and never fills `Class.classLoader` for one
    // (`Class.getClassLoader` is an `Intrinsic` that answers the platform or
    // application loader from the image-module rule, the field stays null),
    // so JDK 25's `Class.getResource` takes its `cl == null` arm and asks
    // HERE for a class of `java.sql`, `java.net.http` or `jdk.compiler` too;
    // `BuiltinClassLoader.findResource(mn, name)` (the arm HotSpot takes) is
    // never reached for them. HotSpot's answer there is still module `mn`'s
    // own `ModuleReader.find(name)`, which is what the image lookup below
    // gives: the `jrt:/<mn>/<name>` URL when the image lists `name` in `mn`,
    // never another module's copy (`sql-other-module` stays null). The JDK's
    // own callers pass only boot modules here, so the wider set changes no
    // answer HotSpot gives on this door.
    match system_module_resource_url(ctx, &module, &name) {
        Some(url) => {
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, &url)?;
            Ok(Some(Value::Object(Some(url))))
        }
        // Interpreter round i4 wave 98, lane L3 (`L3W98ModulePathResource`):
        // a named APPLICATION module on `--module-path` reaches this door for
        // the same reason (its mirrors' `Class.classLoader` stays null too).
        // HotSpot's answer is that module's own `ModuleReader.find(name)`.
        None => module_path_module_resource_url(ctx, &module, &name).unwrap_or(null),
    }
}

/// A built-in loader's answer `url` for `name`, as HotSpot spells it when the
/// entry that holds it is a `--module-path` MODULAR JAR: the module's own
/// `ModuleReader.find` URL (`jar:file:///C:/m.jar!/...`,
/// [`module_path_hotspot_url`]), not the class path's `jar:file:/C:/...`. A
/// named module answers a name of one of its packages (and, through
/// `findMiscResource`, a package-less name); the class path's walk found the
/// same jar entry. Only `jar:file:/` URLs are looked at, and only the
/// package's own module (or, for a package-less name, the boot
/// configuration's modules) is asked. Interpreter round i5 wave 100, lane L3
/// (w101 hand-back: `L3W98ModulePathResource` modular-jar rows `cl-class`,
/// `cl-top`; the exploded form already matched).
pub fn module_path_resource_spelling(ctx: &mut dyn NativeContext, name: &str, url: String) -> String {
    if !url.starts_with("jar:file:/") || url.starts_with("jar:file:///") {
        return url;
    }
    let modules: Vec<String> = match name.rsplit_once('/') {
        Some((pkg, _)) => ctx.named_module_for_package(pkg).into_iter().collect(),
        None => ctx.boot_configuration_module_names(),
    };
    for module in modules {
        if let Some(Some(found)) = ctx.module_path_resource_url(&module, name) {
            if found == url {
                return module_path_hotspot_url(&found);
            }
        }
    }
    url
}

/// HotSpot's spelling of a `--module-path` module's `ModuleReader.find` URL,
/// from the application class path's URL of the same entry. A modular jar's
/// reader answers `"jar:" + path.toUri() + "!/" + name`, and `Path.toUri()`
/// is `file:///...`, so HotSpot prints `jar:file:///C:/m.jar!/p/A.class`
/// where the class path spells `jar:file:/C:/m.jar!/p/A.class`. An exploded
/// module's `path.toUri().toURL()` prints `file:/C:/dir/p/A.class`, the class
/// path's own spelling.
pub fn module_path_hotspot_url(class_path_url: &str) -> String {
    match class_path_url.strip_prefix("jar:file:/") {
        Some(rest) if !rest.starts_with('/') => format!("jar:file:///{rest}"),
        _ => class_path_url.to_string(),
    }
}

/// `ModuleReader.find(name)` of `--module-path` module `module` as a
/// `java.net.URL`: `None` when `module` is not a module-path module of this
/// VM, else the answer (a null `URL` when none of its roots holds `name`).
/// Interpreter round i4 wave 98, lane L3.
pub fn module_path_module_resource_url(
    ctx: &mut dyn NativeContext,
    module: &str,
    name: &str,
) -> Option<MethodCallResult> {
    // `--real-jdk` builds the JDK's own module layer: the real loaders' bodies
    // answer, never this.
    if ctx.is_jdk_only() {
        return None;
    }
    let found = ctx.module_path_resource_url(module, name)?;
    Some(match found {
        Some(url) => {
            let url = module_path_hotspot_url(&url);
            crate::jboss_module_loader::build_synthetic_url(ctx, &url)
                .map(|u| Some(Value::Object(Some(u))))
        }
        None => Ok(Some(Value::Object(None))),
    })
}

/// The `InputStream` sibling of [`module_path_module_resource_url`]: the
/// resource's bytes from its root, as a `ByteArrayInputStream` (the shape the
/// system-module stream rows answer).
pub fn module_path_module_resource_stream(
    ctx: &mut dyn NativeContext,
    module: &str,
    name: &str,
) -> Option<MethodCallResult> {
    if ctx.is_jdk_only() {
        return None;
    }
    let found = ctx.module_path_resource_bytes(module, name)?;
    Some(match found {
        Some(bytes) => crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes)
            .map(|s| Some(Value::Object(Some(s)))),
        None => Ok(Some(Value::Object(None))),
    })
}

/// Is `module` a system module of the run-time image in the boot layer, which
/// [`system_module_resource_url`] answers for: named, not a class-path-only
/// label, not a `--module-path` module (the caller asks that one's reader
/// first), an image present (`None` on a synthetic-JDK boot), not patched
/// (`--patch-module` content is not in the image lookup), and the boot
/// configuration resolving it.
pub fn boot_layer_image_module(ctx: &mut dyn NativeContext, module: &str) -> bool {
    !module.is_empty()
        && !ctx.module_is_class_path_only(module)
        && ctx.runtime_image_defines_class("java/lang/Object") == Some(true)
        && ctx.module_patch_paths(module).is_empty()
        && ctx
            .boot_configuration_module_names()
            .iter()
            .any(|m| m == module)
}

/// `Class.getResource`'s named-module answer for a class of boot-layer
/// module `module` (`lang_class::named_module_class_resource`): the module's
/// own `ModuleReader.find(name)` URL as HotSpot spells it, `Some(None)` when
/// the module does not hold `name`, `None` when `module` is neither a
/// `--module-path` module nor a boot-layer image module (the caller keeps its
/// class-path walk). Interpreter round i5 wave 105, lane L3.
pub fn named_module_class_resource_url(
    ctx: &mut dyn NativeContext,
    module: &str,
    name: &str,
) -> Option<Option<String>> {
    if let Some(found) = ctx.module_path_resource_url(module, name) {
        return Some(found.map(|url| module_path_hotspot_url(&url)));
    }
    if !boot_layer_image_module(ctx, module) {
        return None;
    }
    Some(system_module_resource_url(ctx, module, name))
}

/// The bytes sibling of [`named_module_class_resource_url`]
/// (`Class.getResourceAsStream`'s named-module arm), with the same
/// `None` / `Some(None)` meaning.
pub fn named_module_class_resource_bytes(
    ctx: &mut dyn NativeContext,
    module: &str,
    name: &str,
) -> Option<Option<Vec<u8>>> {
    if let Some(found) = ctx.module_path_resource_bytes(module, name) {
        return Some(found);
    }
    if !boot_layer_image_module(ctx, module) {
        return None;
    }
    // Every module holds a `module-info.class` and the global index holds
    // none: the module's own reader reads it (never a class-path jar's copy).
    if name == MODULE_INFO_CLASS {
        return Some(system_module_resource_bytes(ctx, module, name));
    }
    if system_module_resource_url(ctx, module, name).is_none() {
        return Some(None);
    }
    // The image lists `name` in `module`: the copy of that module (when
    // several modules ship it), else the global lookup's, which is then that
    // module's (the image is the first class-path segment).
    Some(match system_module_scoped_resource(ctx, module, name) {
        Some(answer) => answer,
        None => ctx
            .find_resource(name)
            .or_else(|| system_module_resource_bytes(ctx, module, name)),
    })
}

/// The run-time image's `jrt:/<module>/<name>` URL when the image lists `name`
/// in system module `module` (HotSpot's `ModuleReader.find(name)` of that
/// module), `None` otherwise. Never another module's copy of the name.
pub fn system_module_resource_url(
    ctx: &mut dyn NativeContext,
    module: &str,
    name: &str,
) -> Option<String> {
    // `module-info.class`: every system module holds one, and the class
    // manager's index holds none (it is keyed by name, one per module), so
    // the segment lookup below never found it and
    // `Object.class.getResource("/module-info.class")` read null under
    // `--compatible` (HotSpot: `jrt:/java.base/module-info.class`). The
    // stream sibling already asks the module's reader for it
    // (`resource_stream_for_last_string_arg`). Answered for a module of the
    // boot configuration that is not a `--module-path` module (that one's own
    // reader answers, `module_path_module_resource_url`). Interpreter round i5
    // wave 100, lane L3 (`L3W100ModuleInfoByModule`).
    if name == MODULE_INFO_CLASS {
        if ctx.module_path_resource_url(module, name).is_some()
            || ctx.module_is_class_path_only(module)
        {
            return None;
        }
        let in_boot_layer = ctx
            .boot_configuration_module_names()
            .iter()
            .any(|m| m == module);
        return in_boot_layer.then(|| format!("jrt:/{module}/{MODULE_INFO_CLASS}"));
    }
    ctx.find_resource_urls_in_segment(name, 0)
        .into_iter()
        .find(|url| {
            url.strip_prefix("jrt:/")
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(m, rest)| m == module && rest == name)
        })
}

/// `--compatible`: `jdk.internal.loader.BuiltinClassLoader.findResource(String
/// mn, String name)`, what JDK 25's `Class.getResource` calls for a class of a
/// named module defined to the platform or application loader
/// (`java.sql.Date.class.getResource("Date.class")`).
///
/// Interpreter round i4 wave 96, lane L3
/// (`interpreter-i95-L3-compatible-builtin-loader-find-resource-by-module-is-null-FIXED-20261130.md`).
/// The URL sibling of [`builtin_classloader_find_resource_as_stream`], with the
/// same shape: the JDK body runs first and its non-null answer stands. The body
/// reads the loader's `nameToModule`, which only `--real-jdk` fills, so under
/// `--compatible` it answered null for every system module. On that null this
/// answers the image's `jrt:/<mn>/<name>` URL when the loader kind owns `mn`
/// (the JDK's own `ModuleLoaderMap`: boot, platform, or neither for the
/// application loader) and the image lists `name` in `mn`; otherwise the body's
/// null. A named application module on the module path has no `jrt:` URL and
/// keeps the body's answer (still open on the page). `Class.getResource`'s
/// encapsulation checks ran before this, in the JDK's own code.
pub fn builtin_classloader_find_module_resource(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let input = match (args.first(), args.get(1), args.get(2)) {
        (
            Some(Value::Object(Some(loader))),
            Some(Value::Object(Some(module))),
            Some(Value::Object(Some(name))),
        ) => {
            let kind = builtin_loader_kind(
                ctx.class_name_of_id(ctx.class_id_of_object(*loader))
                    .as_deref(),
            );
            ctx.read_string(*module)
                .zip(ctx.read_string(*name))
                .map(|(module, name)| (kind, module, name))        }
        _ => None,
    };
    let loader = obj_arg(args, 0)?;
    let original = ctx.invoke_virtual_bytecode_only(
        loader,
        "findResource",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/URL;",
        &args[1..],
    )?;
    if ctx.is_jdk_only() || matches!(original, Some(Value::Object(Some(_)))) {
        return Ok(original);
    }
    let Some((Some(kind), module, name)) = input else {
        return Ok(original);
    };
    if module.is_empty() || name.is_empty() {
        return Ok(original);
    }
    let Some(sets) = jdk_builtin_module_sets(ctx) else {
        return Ok(original);
    };
    let owns = match kind {
        BuiltinLoaderKind::Boot => sets.boot.contains(&module),
        BuiltinLoaderKind::Platform => sets.platform.contains(&module),
        BuiltinLoaderKind::Application => {
            !sets.boot.contains(&module) && !sets.platform.contains(&module)
        }
    };
    if !owns {
        return Ok(original);
    }
    match system_module_resource_url(ctx, &module, &name) {
        Some(url) => {
            let url = crate::jboss_module_loader::build_synthetic_url(ctx, &url)?;
            Ok(Some(Value::Object(Some(url))))
        }
        // Interpreter round i4 wave 98, lane L3: a named `--module-path`
        // module is the application loader's; its own `ModuleReader.find`.
        None if kind == BuiltinLoaderKind::Application => {
            module_path_module_resource_url(ctx, &module, &name).unwrap_or(Ok(original))
        }
        None => Ok(original),
    }
}

/// The `jrt:` URLs of `args[0]` in modules the JDK defines to the boot loader.
pub fn boot_module_resource_urls(ctx: &mut dyn NativeContext, args: &[Value]) -> Vec<String> {
    let name = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => return Vec::new(),
    };
    boot_loader_resource_urls(ctx, &name)
}

/// `BootLoader.findResources(name)` as HotSpot answers it for a class-path
/// launch (no `-Xbootclasspath/a`): the `jrt:` URLs of `name` in modules the
/// JDK defines to the boot loader, never a platform- or application-loader
/// module's and never a class-path entry's. Interpreter round i4 wave 99,
/// lane L3: also the null-parent step of the `ClassLoader.getResources`
/// natives (`interpreter-i99-L3-null-parent-get-resources-omits-boot-FIXED-20261015.md`).
pub fn boot_loader_resource_urls(ctx: &mut dyn NativeContext, name: &str) -> Vec<String> {
    let name = name.to_string();
    // The JDK uses the name verbatim: a leading `/` matches no module entry
    // (`ModuleReader.find("/x")` is empty), so `BootLoader.findResources("/x")`
    // is empty on HotSpot. Wave 100, lane L3: the null-parent step of a user
    // loader's `getResources("/x")` reaches here with the slash kept.
    if name.starts_with('/') {
        return Vec::new();
    }
    if crate::jboss_jdkspecific::builtin_loaders_hide_resource(ctx, &name) {
        return Vec::new();
    }
    // `module-info.class`: the boot loader's `findMiscResource` (interpreter
    // round i4 wave 99, lane L3).
    if name == MODULE_INFO_CLASS {
        if let Some(urls) = builtin_loader_module_info_urls(ctx, BuiltinLoaderKind::Boot) {
            return urls;
        }
    }
    let Some(sets) = jdk_builtin_module_sets(ctx) else {
        return Vec::new();
    };
    // Segment 0 is the boot class path: the run-time image, which serves every
    // module, so the module filter below is what makes the answer the BOOT
    // loader's. A `.jmod`-form boot path yields no `jrt:` URL and so no answer,
    // which is the pre-existing null.
    let glob = is_glob_resource_name(&name);
    ctx.find_resource_urls_in_segment(&name, 0)
        .into_iter()
        .filter(|url| !glob || url_names_literal_resource(url, &name))
        .filter(|url| {
            url.strip_prefix("jrt:/")
                .and_then(|rest| rest.split('/').next())
                .is_some_and(|module| sets.boot.contains(module))
        })
        .collect()
}

