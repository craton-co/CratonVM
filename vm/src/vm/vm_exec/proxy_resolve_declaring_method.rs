#[allow(unused_imports)]
use super::*;

/// Modifiers for the synthesized proxy-dispatch `Method` object — mirrors
/// `proxy_resolve_declaring_class_mirror`'s interface walk, but returns the
/// declared method's actual `PUBLIC|ABSTRACT` bits instead of always
/// hardcoding `PUBLIC` (1).
///
/// `ELSupport.coerceToFunctionalInterface` (used by every `jakarta.el`
/// lambda-to-functional-interface coercion, e.g. `Function`/`Predicate`)
/// checks `Modifier.isAbstract(method.getModifiers())` on the `Method` its
/// own dynamic-proxy invocation handler receives and rejects the call with
/// "is not abstract" if that bit is missing — real `Function.apply`/
/// `Predicate.test` are declared `public abstract` in the interface
/// (access_flags 0x0401), never default. Hardcoding modifiers=1 (PUBLIC
/// only, no ABSTRACT) made every SAM look like a default method and broke
/// this check for ALL proxied functional interfaces, not just EL's.
/// `equals`/`hashCode`/`toString` (declared on `Object`, never abstract)
/// and the "no declaring interface found" fallback both correctly want
/// PUBLIC-only, so this returns 1 for anything the walk doesn't resolve to
/// a found interface method, matching the prior behavior exactly for those
/// cases.
pub(in crate::vm) fn proxy_resolve_declaring_method_modifiers(
    shared: &SharedVm,
    proxy: ObjectRef,
    method_name: &str,
    descriptor: &str,
) -> i32 {
    const ACC_PUBLIC: i32 = 0x0001;
    const ACC_ABSTRACT: i32 = 0x0400;

    if (method_name == "equals" && descriptor == "(Ljava/lang/Object;)Z")
        || (method_name == "hashCode" && descriptor == "()I")
        || (method_name == "toString" && descriptor == "()Ljava/lang/String;")
    {
        return ACC_PUBLIC;
    }

    let proxy_cid = shared.mem.heap.class_id_of(proxy);
    let has_iface_slot = shared
        .classes
        .class_manager
        .read()
        .get_class(proxy_cid)
        .map(|c| c.num_total_fields >= 2)
        .unwrap_or(false);

    let mut iface_cids: Vec<ClassId> = Vec::new();
    if has_iface_slot {
        if let Value::Object(Some(arr)) = shared.mem.heap.get_field(proxy, 1) {
            let n = shared.mem.heap.array_length(arr);
            for i in 0..n {
                if let Ok(Value::Object(Some(m))) = shared.mem.heap.get_array_element(arr, i) {
                    if let Some(cid) = shared.classes.class_mirrors_reverse.read().get(&m).copied()
                    {
                        iface_cids.push(cid);
                    }
                }
            }
        }
    } else {
        let cids = shared
            .classes
            .class_manager
            .read()
            .get_class(proxy_cid)
            .map(|c| c.interfaces.clone())
            .unwrap_or_default();
        iface_cids.extend(cids);
    }

    for iface_cid in iface_cids {
        let mut visited: rustc_hash::FxHashSet<ClassId> = rustc_hash::FxHashSet::default();
        let mut stack: Vec<ClassId> = vec![iface_cid];
        while let Some(cid) = stack.pop() {
            if !visited.insert(cid) {
                continue;
            }
            let cm = shared.classes.class_manager.read();
            let Some(class) = cm.get_class(cid) else {
                continue;
            };
            if let Some(m) = class
                .methods
                .iter()
                .find(|m| &*m.name == method_name && &*m.descriptor == descriptor)
            {
                return if m.is_abstract() {
                    ACC_PUBLIC | ACC_ABSTRACT
                } else {
                    ACC_PUBLIC
                };
            }
            let supers = class.interfaces.clone();
            drop(cm);
            for s in supers {
                stack.push(s);
            }
        }
    }
    ACC_PUBLIC
}

/// Box a JVM value into a Java wrapper object for use in Object[] arrays.
/// Object references are passed through unchanged.
/// Box a primitive value into its wrapper, choosing the wrapper class from the
/// formal parameter descriptor. `boolean`/`char`/`byte`/`short`/`int` all share
/// the `Value::Int` VM representation, so a descriptor-blind `proxy_box_value`
/// always boxes them as `Integer` — which is wrong for a `Z`/`C`/`B`/`S`
/// parameter. A JDK dynamic proxy's `Object[] args` must hold the wrapper that
/// matches the declared parameter type, or the downstream
/// `Method.invoke`/reflection unbox throws `IllegalArgumentException: cannot
/// convert java/lang/Integer to Z` (e.g. `Connection.setAutoCommit(boolean)`
/// through Hibernate's `JdbcSpies` proxy — 17 CV-only suite classes).
///
/// # Identity, and the one arm that has an observable contract
///
/// This is the FOURTH independent boxing implementation in the tree
/// (`lang_class::box_value`, `lang_class::box_value_canonical`,
/// `lib.rs::native_array_get`, and this) — and it is not even the only one
/// that boxes a PROXY's arguments. `classloading/src/proxy_gen.rs` emits a
/// real `X.valueOf` `invokestatic` per parameter into every generated
/// `$ProxyN` (see its `DescKind::Int` / `Long` / `Float` / `Double` arms), so
/// that path is canonical by construction: it does not implement boxing, it
/// delegates to Java. Which of the two answers is a ROUTE question, not a
/// code-reading one — `is_proxy_dispatch` in
/// `runtime/interpreter/invoke.rs` and the `class_chain_reaches_proxy_instance`
/// check above intercept ANY receiver whose chain reaches
/// `java/lang/reflect/Proxy`, which includes a real generated `$ProxyN`, and
/// they fire before that class's own bytecode runs. So on the interpreted path
/// this function is expected to be the live one and the emitted `valueOf`
/// calls unreached, but that has NOT been run and is not asserted here.
/// `RJdkReflBox --only=proxy` is the discriminator: it is red on the fresh
/// route and green on either correct one.
///
/// Measured on Microsoft OpenJDK 25.0.3+9
/// (`scratchpad/f19/ReflBoxOracle.java`, §2 of
/// `docs/known-issues/jdk-only/F19-1-*.md`), a JDK dynamic proxy's `args[]`
/// holds the CANONICAL wrapper on every primitive parameter type:
///
/// ```text
/// proxy.int  proxy.char  proxy.bool  proxy.long  proxy.byte  proxy.short = true
/// proxy.boolTRUE                                                         = true
/// proxyoob.int1000                                                       = false
/// ```
///
/// because HotSpot's generated proxy class boxes each argument with a
/// `valueOf` invocation in its own bytecode. Everything here allocates
/// unconditionally instead, so every one of those rows is `false` on this VM.
///
/// **The `Z` arm is the one that is not merely an allocation.** `alloc_object`
/// + `set_field(0, …)` produces a `Boolean` that is not `Boolean.TRUE`, and
/// `fFeatures.get(…) == Boolean.TRUE` is exactly the identity test
/// `native_boolean_value_of`'s own comment documents for Xerces'
/// `XML11Configuration.configurePipeline()`. The documentation of that bug and
/// a live instance of it coexisted in this tree: the native was fixed, this
/// copy one layer up was not, and nothing connects the two files. So the `Z`
/// arm now resolves the real `Boolean.TRUE`/`FALSE` statics, the same way the
/// native does — see [`proxy_canonical_boolean`].
///
/// The other five arms (`C B S I`, and `J` via [`proxy_box_value`]) used to be
/// fresh, for a reason that was true when it was written and is not any more:
/// the canonical instances live in `lang_math`'s caches, which were reachable
/// only through a `&mut dyn NativeContext` this function does not have.
/// `lang_math::canonical_wrapper_if_cached` (F29-1 §3) is now a `pub` probe
/// keyed on `vm_identity`, so the five arms read those SAME six caches instead
/// of minting a second `IntegerCache` here. There is exactly ONE boxing cache
/// in this VM and it is not in this file.
///
/// **The probe cannot allocate and cannot run `<clinit>`, and that is a
/// correctness property rather than a performance one** — populating a cache
/// needs `alloc_wrapper` → `ensure_class_initialized` → `<clinit>`, and a
/// proxy invocation is not a legal place to trigger class initialisation. A
/// miss is `None` and the caller keeps its existing allocation. `Z` is not
/// routed through it (the probe declines `Z` deliberately): `Boolean.valueOf`
/// answers with the live `TRUE`/`FALSE` STATICS, which `proxy_canonical_boolean`
/// above resolves directly. `F`/`D` are not routed either, because HotSpot
/// caches neither — `neg.floatValueOf` = false — so a fresh `Float` is the
/// right answer there and not a fallback.
///
/// `None` only when the heap cannot hold a fresh wrapper (gc-common w5-c: the
/// wrappers are allocated with the collection-free `try_alloc_object_full`,
/// and the callers report `OutOfMemoryError`; see `proxy_heap_exhausted`).
pub(in crate::vm) fn proxy_box_value_for_desc(
    shared: &SharedVm,
    value: Value,
    pdesc: &str,
) -> Option<Value> {
    if let Value::Int(v) = value {
        // `Z` before the generic wrapper table: a `boolean` argument has a
        // canonical answer and the table below cannot produce it.
        if pdesc == "Z" {
            if let Some(canonical) = proxy_canonical_boolean(shared, v != 0) {
                return Some(canonical);
            }
            // Fall through to the allocating path — never to `null`. A boxing
            // failure that becomes a null argument is a defect already
            // recorded above `lang_class::create_method_object`.
        }
        // `C B S I`: the canonical instance IF one is already cached, read out
        // of the six caches in `lang_math.rs`. Read-only — it cannot allocate
        // and cannot run `<clinit>`, which is what makes it legal here.
        // Measured canonical on HotSpot: `proxy.char` / `byte` / `short` /
        // `int` all true (F19-1 §2). A miss falls through to the allocating
        // table below, never to `null`.
        //
        // `Z` reaches this line only when `proxy_canonical_boolean` above
        // could not resolve the statics; the probe declines `Z` (F29-1 §3.2),
        // so this is a pass-through for that arm and the table below still
        // does the `val != 0` normalisation.
        if let Some(obj) = cratonvm_native_builtins::lang_math::canonical_wrapper_if_cached(
            shared.vm_identity,
            pdesc,
            Value::Int(v),
        ) {
            return Some(Value::Object(Some(obj)));
        }
        let wrapper = match pdesc {
            "Z" => Some("java/lang/Boolean"),
            "C" => Some("java/lang/Character"),
            "B" => Some("java/lang/Byte"),
            "S" => Some("java/lang/Short"),
            "I" => Some("java/lang/Integer"),
            _ => None,
        };
        if let Some(wname) = wrapper {
            // Normalise the payload for `Z`. The raw slot can carry any
            // non-zero int, and a `Boolean` whose slot holds 5 is a wrong
            // ANSWER, not just a wrong identity: `booleanValue()` and every
            // `Boolean.toString` path read that slot. `native_boolean_value_of`
            // applies the same `val != 0` normalisation.
            let stored = if pdesc == "Z" {
                Value::Int(i32::from(v != 0))
            } else {
                Value::Int(v)
            };
            let class_id = shared
                .classes
                .class_manager_write_settling()
                .load_class(wname)
                .unwrap_or(ClassId::new(0));
            let obj = shared.mem.heap.try_alloc_object_full(class_id, 1)?;
            shared.mem.heap.set_field(obj, 0, stored);
            return Some(Value::Object(Some(obj)));
        }
    }
    // `J` has no `Value::Int` arm above, so it is taken here rather than in
    // the wrapper table. Measured `proxy.long` = true. `F`/`D` are NOT taken:
    // HotSpot caches neither (`neg.floatValueOf` = false), so
    // `canonical_wrapper_if_cached` declines them and `proxy_box_value`'s
    // fresh allocation is the correct answer, not a fallback.
    //
    // `value`, NOT `Value::Int(v)`. This is outside the `if let Value::Int(v)`
    // block on purpose: a `long` slot can present as a compact `Value::Int`,
    // and a descriptor-only lookup would answer `("J", Value::Int(5))` with
    // the cached `Long.valueOf(0)` — an identity fix converted into a WRONG
    // ANSWER. The probe's own variant guard is the second half of that
    // defence; this line is the first. F29-1 §3 records that the pair has
    // already caught two lanes.
    if pdesc == "J" {
        if let Some(obj) = cratonvm_native_builtins::lang_math::canonical_wrapper_if_cached(
            shared.vm_identity,
            pdesc,
            value,
        ) {
            return Some(Value::Object(Some(obj)));
        }
    }
    // Long/Float/Double and reference values: descriptor-independent.
    proxy_box_value(shared, value)
}

/// The live `java.lang.Boolean.TRUE` / `FALSE` instance, or `None`.
///
/// `Boolean.valueOf(boolean)` is literally `return b ? TRUE : FALSE` in the
/// JDK, so those two static fields — not a private mirror of them — ARE the
/// canonical instances. `native_boolean_value_of` resolves them exactly this
/// way; this is the same resolution on the `&SharedVm` side of the boundary,
/// because `vm_exec` cannot reach the native's `&mut dyn NativeContext`.
///
/// Returns `None`, never a fabricated object, when:
///
///   * `java/lang/Boolean` will not load (synthetic-JDK arms where the class
///     is absent), or
///   * the class is loaded but `<clinit>` has not run, so the static slot
///     still holds its default rather than an object.
///
/// The second case is why the value is match-checked for `Object(Some(_))`
/// instead of being unwrapped: `get_static_shared`'s miss answer is
/// `Value::Int(0)`, and a caller that trusted it would put an `Int` into an
/// `Object[]` slot. This function does not RUN `<clinit>` — it has no thread —
/// which is correct as well as necessary: a proxy invocation is not a legal
/// place to trigger class initialisation, and the caller's fallback is a
/// perfectly valid `Boolean`, merely a non-canonical one.
pub(in crate::vm::vm_exec) fn proxy_canonical_boolean(shared: &SharedVm, truthy: bool) -> Option<Value> {
    let class_id = shared
        .classes
        .class_manager_write_settling()
        .load_class("java/lang/Boolean")
        .ok()?;
    let want = if truthy { "TRUE" } else { "FALSE" };
    // Static field INDEX, not slot: `get_static_shared` is indexed by position
    // among the class's static fields only. Same walk as
    // `NativeContext::static_field_index_by_name` above; the guard is dropped
    // before `get_static_shared` reacquires anything.
    let field_index = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let mut static_idx = 0usize;
        let mut found: Option<usize> = None;
        for f in &class.fields {
            if f.is_static() {
                if &*f.name == want {
                    found = Some(static_idx);
                    break;
                }
                static_idx += 1;
            }
        }
        found?
    };
    match super::get_static_shared(shared, class_id, field_index) {
        v @ Value::Object(Some(_)) => Some(v),
        _ => None,
    }
}

/// Box a primitive into a fresh `Integer` / `Long` / `Float` / `Double`; a
/// reference or null passes through. `None` only when the heap cannot hold the
/// wrapper (gc-common w5-c; see [`proxy_box_value_for_desc`]).
pub(in crate::vm) fn proxy_box_value(shared: &SharedVm, value: Value) -> Option<Value> {
    let wrapper = match value {
        Value::Int(_) => "java/lang/Integer",
        Value::Long(_) => "java/lang/Long",
        Value::Float(_) => "java/lang/Float",
        Value::Double(_) => "java/lang/Double",
        other => return Some(other), // already an Object reference or null
    };
    let class_id = shared
        .classes
        .class_manager_write_settling()
        .load_class(wrapper)
        .unwrap_or(ClassId::new(0));
    let obj = shared.mem.heap.try_alloc_object_full(class_id, 1)?;
    shared.mem.heap.set_field(obj, 0, value);
    Some(Value::Object(Some(obj)))
}

/// S111r7 — return `true` for methods declared on `java.lang.Object` so we
/// don't accidentally divert legitimate `Object.equals`/`hashCode`/etc.
/// calls into the receiver-driven fallback.
pub fn is_object_member(method_name: &str, descriptor: &str) -> bool {
    matches!(
        (method_name, descriptor),
        ("equals", "(Ljava/lang/Object;)Z")
            | ("hashCode", "()I")
            | ("toString", "()Ljava/lang/String;")
            | ("getClass", "()Ljava/lang/Class;")
            | ("notify", "()V")
            | ("notifyAll", "()V")
            | ("wait", "()V")
            | ("wait", "(J)V")
            | ("wait", "(JI)V")
            | ("clone", "()Ljava/lang/Object;")
            | ("finalize", "()V")
            | ("<init>", "()V")
    )
}

/// Invoke a method on a specific class by ClassId.
pub fn invoke_on_class_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_on_class_shared_inner(
        shared,
        thread,
        class_id,
        method_name,
        descriptor,
        args,
        OnClassKind::Unknown,
        None,
    )
}

/// [`invoke_on_class_shared`] for a virtual call dispatched on the RECEIVER's
/// own class (`execute_invoke_kind`'s fallback), with the reference the call
/// site resolved: the method is then SELECTED against it (JVMS §5.4.6) rather
/// than found by the first same-signature declaration on the receiver's
/// chain, so a private or cross-package package-private method there does not
/// hide the override. Only a strict `Selected` answer replaces the walk (see
/// `selection::refine_lenient_selection_for`).
pub(crate) fn invoke_on_receiver_class_selecting(
    shared: &SharedVm,
    thread: &mut JvmThread,
    receiver_class_id: ClassId,
    resolved: crate::runtime::resolve::selection::ResolvedRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_on_class_shared_inner(
        shared,
        thread,
        receiver_class_id,
        method_name,
        descriptor,
        args,
        OnClassKind::Unknown,
        Some(resolved),
    )
}

/// WP2.9 вЂ” Invoke a method on a specific class with **no virtual retarget**.
///
/// Used by `MethodHandles.Lookup.findSpecial` and the default-method super-call
/// pattern. Unlike [`invoke_on_class_shared`], this never retargets dispatch
/// to the receiver's concrete class even when the resolved class is an
/// interface or abstract вЂ” that is the *whole point* of invokespecial.
pub fn invoke_on_class_shared_no_retarget(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    invoke_on_class_shared_inner(
        shared,
        thread,
        class_id,
        method_name,
        descriptor,
        args,
        OnClassKind::NoRetarget,
        None,
    )
}

/// `java/util/TreeMap`'s carriers, when `CRATONVM_REAL` has handed the map to
/// real JDK bytecode and wave 8's whole-family retirement therefore has to
/// carry the views and cursors with it.
///
/// A named local because the clusters below are one long `||` chain and the
/// fully-qualified call does not fit on their lines. The predicate, its
/// measurement and the reason it is family-wide rather than per-method live on
/// `env_cache::tree_map_family_yields_to_real_bytecode`.
#[inline]
pub(in crate::vm::vm_exec) fn tm_family_real(class_name: &str) -> bool {
    crate::runtime::env_cache::tree_map_family_yields_to_real_bytecode(class_name)
}

/// Rows of the per-thread [`name_override_memoized`] table (direct-mapped).
pub(in crate::vm::vm_exec) const NAME_OVERRIDE_MEMO_ROWS: usize = 256;

/// One [`name_override_memoized`] row: `(vm_identity, declaring class id,
/// method name, descriptor, answer)`.
pub(in crate::vm::vm_exec) type NameOverrideRow = Option<(usize, u32, Box<str>, Box<str>, bool)>;

/// The per-thread table behind [`name_override_memoized`]: the switch's
/// latched state (`0` unread, `1` on, `2` off; read once per thread, so no
/// per-call flag read) and the rows. It lives on the thread
/// (`JvmThread::name_override_memo`), not in a `thread_local!`, so it adds no
/// static to `vm/src` (`vm/tests/per_vm_state_statics_ratchet.rs`).
#[derive(Default)]
pub(crate) struct NameOverrideMemo {
    pub(in crate::vm::vm_exec) state: u8,
    pub(in crate::vm::vm_exec) rows: Vec<NameOverrideRow>,
    /// gci i8/dm: the JNI dispatch memo's latched switch
    /// (`CRATONVM_JNI_DISPATCH_MEMO`, default ON since 2026-10-15, WARNING
    /// gci-user-flip-20261013, `=0` kills; `0` unread, `1` on, `2`
    /// off) and its direct-mapped rows ([`jni_dispatch_memo_hit`]). Carried on
    /// the same per-thread table so it adds no field to `JvmThread` and no
    /// static to `vm/src`.
    pub(in crate::vm::vm_exec) jni_state: u8,
    /// A boxed slice, not a `Vec` (gci i9/cs): 16 bytes, so the call-site
    /// memo's pointer below adds nothing to `size_of::<NameOverrideMemo>()`
    /// (56) and so nothing to `JvmThread`'s frozen layout
    /// (`runtime/layout_ratchet_tests.rs`).
    pub(in crate::vm::vm_exec) jni_rows: Box<[Option<JniDispatchRow>]>,
    /// gci i9/cs: the JNI call-site memo's latched sub-switch
    /// (`CRATONVM_JNI_CALL_SITE_MEMO`, default ON inside the opt-in
    /// `CRATONVM_JNI_DISPATCH_MEMO`; `0` unread, `1` on, `2` off) and its
    /// tables, allocated on first use ([`JniSiteTables`]).
    pub(in crate::vm::vm_exec) site_state: u8,
    pub(in crate::vm::vm_exec) site: Option<Box<JniSiteTables>>,
    /// gci i10/cr: the door memo's receiver-selected sub-switch
    /// (`CRATONVM_JNI_DISPATCH_MEMO_SELECTED`, default ON inside the opt-in;
    /// `0` unread, `1` on, `2` off; [`jni_dispatch_memo_selected_armed`]).
    /// Two more bytes keep `size_of::<NameOverrideMemo>()` at 56.
    pub(in crate::vm::vm_exec) selected_state: u8,
    /// gci i10/cr: `execute_invokestatic`'s by-name fallback through the
    /// STATIC entries (`CRATONVM_INVOKESTATIC_FALLBACK_STATIC`, default ON;
    /// `0` unread, `1` on, `2` off; [`invokestatic_fallback_static_kind`]).
    pub(in crate::vm::vm_exec) static_fallback_state: u8,
    /// gci i11/cf: the by-name walk reads a STATIC call's first argument as
    /// an argument, not a receiver (`CRATONVM_BYNAME_STATIC_ARG_NOT_RECEIVER`,
    /// default ON; `0` unread, `1` on, `2` off; `byname_static_arg_rule_on`).
    pub(in crate::vm::vm_exec) static_arg_state: u8,
    /// gci i11/cf: the virtual / interface half of the JNI call-site memo
    /// (`CRATONVM_JNI_VIRTUAL_SITE_MEMO`, default ON inside
    /// `CRATONVM_JNI_CALL_SITE_MEMO`; `0` unread, `1` on, `2` off;
    /// [`jni_vsite_memo_armed`]). With i10/cr's two these are seven latch
    /// bytes next to 48 bytes of pointers: `size_of` stays 56.
    pub(in crate::vm::vm_exec) vsite_state: u8,
    /// gci i12/mf: the compiled half of the virtual / interface call-site rows
    /// (`CRATONVM_JNI_JIT_VIRTUAL_SITE_MEMO`, default ON inside
    /// `CRATONVM_JNI_VIRTUAL_SITE_MEMO`; `0` unread, `1` on, `2` off;
    /// [`jni_jit_vsite_memo_armed`]). The eighth latch byte fills the last byte
    /// of padding: `size_of` stays 56, and there is no room for a ninth.
    pub(in crate::vm::vm_exec) jit_vsite_state: u8,
}

/// gci i9/cs: the JNI call-site memo's direct-mapped rows
/// ([`jni_site_memo_lookup`]) and the fill probe the door's memo marks
/// ([`jni_site_probe_arm`]). gci i10/cr: also the negative fill stamps of
/// the interpreter's invoke-cache fills ([`jni_nofill_ask`]).
#[derive(Default)]
pub(in crate::vm::vm_exec) struct JniSiteTables {
    pub(in crate::vm::vm_exec) rows: Vec<Option<JniSiteRow>>,
    pub(in crate::vm::vm_exec) probe: Option<JniSiteProbe>,
    pub(in crate::vm::vm_exec) nofill: Vec<Option<JniNoFillRow>>,
    /// gci i11/cf: the virtual / interface call-site rows ([`jni_vsite_memo_lookup`]).
    pub(in crate::vm::vm_exec) vrows: Vec<Option<JniVSiteRow>>,
    /// gci i12/mf: the compiled virtual / interface call-site rows
    /// ([`jni_jit_vsite_memo_lookup`]).
    pub(in crate::vm::vm_exec) jrows: Vec<Option<JniJitVSiteRow>>,
    /// gci i13/jm (F3): the interpreter sites' "probe missed" stamps
    /// ([`jni_site_probe_missed`], [`jni_site_miss_note`]).
    pub(in crate::vm::vm_exec) miss: Vec<Option<JniProbeMissRow>>,
    /// gci i13/jm: `CRATONVM_JNI_SITE_PROBE_MISS_STAMP` (default ON inside
    /// `CRATONVM_JNI_CALL_SITE_MEMO`; `0` unread, `1` on, `2` off). Kept on
    /// these boxed tables, not on `NameOverrideMemo`, which has no padding
    /// byte left (`size_of` stays 56).
    pub(in crate::vm::vm_exec) miss_state: u8,
}

/// Rows of the per-thread JNI dispatch memo (direct-mapped; a collision
/// evicts, which only costs the next call the full path).
pub(in crate::vm::vm_exec) const JNI_DISPATCH_MEMO_ROWS: usize = 64;

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of a JNI dispatch memo hit.
pub(in crate::vm::vm_exec) const JNI_DISPATCH_MEMO_HIT: &str = "invoke_on_class_shared_inner.jni-memo-hit";

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of a JNI dispatch memo hit for a
/// receiver-selected call (gci i10/cr; `execute_invoke_kind`'s fallback).
pub(in crate::vm::vm_exec) const JNI_DISPATCH_MEMO_SELECTED_HIT: &str = "invoke_on_class_shared_inner.jni-memo-hit-selected";

/// One JNI dispatch memo row: the key (`vm_identity`, the `class_id` the
/// by-name door was asked on, the method name and descriptor), the validity
/// stamps read at the fill (the JNI table's registration generation and the
/// class redefinition count), and what the table-bound arm needs.
pub(in crate::vm::vm_exec) struct JniDispatchRow {
    pub(in crate::vm::vm_exec) vm_identity: usize,
    pub(in crate::vm::vm_exec) class_id: u32,
    pub(in crate::vm::vm_exec) method_name: Box<str>,
    pub(in crate::vm::vm_exec) descriptor: Box<str>,
    pub(in crate::vm::vm_exec) generation: u64,
    pub(in crate::vm::vm_exec) redefinitions: u64,
    pub(in crate::vm::vm_exec) hit: JniDispatchHit,
}

/// What a JNI dispatch memo hit hands [`jni_table_native_call`].
#[derive(Clone)]
pub(in crate::vm::vm_exec) struct JniDispatchHit {
    pub(in crate::vm::vm_exec) fn_ptr: usize,
    pub(in crate::vm::vm_exec) declaring_class_id: ClassId,
    pub(in crate::vm::vm_exec) is_static: bool,
    pub(in crate::vm::vm_exec) is_synchronized: bool,
    /// The DECLARING class's name (the census line and the arity error).
    pub(in crate::vm::vm_exec) class_name: std::sync::Arc<str>,
    /// gci i10/cr: the reference a RECEIVER-SELECTED fill verified the
    /// selection for (JVMS §5.4.6 selection on `class_id` agreed with the
    /// walk), `None` for a fill from an unselected call. A selected call is
    /// served only by a row verified for its own reference
    /// ([`jni_dispatch_memo_serves_selection`]); an unselected call by any row.
    pub(in crate::vm::vm_exec) selected: Option<crate::runtime::resolve::selection::ResolvedRef>,
}

#[inline]
pub(in crate::vm::vm_exec) fn jni_dispatch_memo_index(class_id: u32, method_name: &str, descriptor: &str) -> usize {
    name_override_memo_index(class_id, method_name, descriptor) % JNI_DISPATCH_MEMO_ROWS
}

/// The switch, read once per thread (`CRATONVM_JNI_DISPATCH_MEMO`, default ON
/// since the vi19-jt verdict; `=0` kills the whole family).
#[inline]
pub(in crate::vm::vm_exec) fn jni_dispatch_memo_armed(memo: &mut NameOverrideMemo) -> bool {
    if memo.jni_state == 0 {
        // WARNING gci-user-flip-20261013: default flipped ON by the user's decision on the
        // vi19-jt verdict (E + S + A2 PASS; default noop 1749 -> 1303 ns, 7/7 rounds).
        memo.jni_state = if true {
            1
        } else {
            2
        };
    }
    memo.jni_state == 1
}

/// gci i10/cr: the receiver-selected half of the door memo
/// (`CRATONVM_JNI_DISPATCH_MEMO_SELECTED`, default ON inside the opt-in,
/// `=0` turns it off), read once per thread. Asked only for a call that
/// carries a receiver selection, after [`jni_dispatch_memo_armed`].
#[inline]
pub(in crate::vm::vm_exec) fn jni_dispatch_memo_selected_armed(memo: &mut NameOverrideMemo) -> bool {
    if memo.selected_state == 0 {
        memo.selected_state = {
            1
        };
    }
    memo.selected_state == 1
}

/// gci i10/cr: whether a row filled with the selection `row` may serve a
/// call that carries `call`. An unselected call (`None`) dispatches by the
/// walk from the row's class, which every fill verified the stored method
/// against; a selected call only by a row whose fill verified that JVMS
/// §5.4.6 selection for the SAME resolved reference agreed with the walk
/// (selection is a function of the receiver's class, which is the row's
/// class, and the reference; a redefinition retires the row).
#[inline]
pub(in crate::vm::vm_exec) fn jni_dispatch_memo_serves_selection(
    row: Option<crate::runtime::resolve::selection::ResolvedRef>,
    call: Option<crate::runtime::resolve::selection::ResolvedRef>,
) -> bool {
    match call {
        None => true,
        Some(selection) => row == Some(selection),
    }
}

/// The row for this key, if one is stored; the caller checks its stamps.
pub(in crate::vm::vm_exec) fn jni_dispatch_memo_find<'m>(
    memo: &'m NameOverrideMemo,
    vm_identity: usize,
    class_id: u32,
    method_name: &str,
    descriptor: &str,
) -> Option<&'m JniDispatchRow> {
    match memo.jni_rows.get(jni_dispatch_memo_index(class_id, method_name, descriptor)) {
        Some(Some(row))
            if row.vm_identity == vm_identity
                && row.class_id == class_id
                && &*row.method_name == method_name
                && &*row.descriptor == descriptor =>
        {
            Some(row)
        }
        _ => None,
    }
}

/// The hit for this key when the row's stamps are current.
pub(in crate::vm::vm_exec) fn jni_dispatch_memo_serve(
    memo: &NameOverrideMemo,
    vm_identity: usize,
    class_id: u32,
    method_name: &str,
    descriptor: &str,
    generation: impl FnOnce() -> u64,
    redefinitions: impl FnOnce() -> u64,
) -> Option<JniDispatchHit> {
    let row = jni_dispatch_memo_find(memo, vm_identity, class_id, method_name, descriptor)?;
    (row.generation == generation() && row.redefinitions == redefinitions())
        .then(|| row.hit.clone())
}

pub(in crate::vm::vm_exec) fn jni_dispatch_memo_store(memo: &mut NameOverrideMemo, row: JniDispatchRow) {
    if memo.jni_rows.len() < JNI_DISPATCH_MEMO_ROWS {
        memo.jni_rows = std::iter::repeat_with(|| None)
            .take(JNI_DISPATCH_MEMO_ROWS)
            .collect();
    }
    let at = jni_dispatch_memo_index(row.class_id, &row.method_name, &row.descriptor);
    memo.jni_rows[at] = Some(row);
}

/// Names whose by-name prefix in [`invoke_on_class_shared_inner`] can answer
/// from something other than `(class, name, descriptor)` -- a receiver's
/// class (`toString` on a `Path`, `accept`, `createServerSocket`), the
/// `netty_tcnative_real` latch, a JSSE / attach / `Thread` / `String` door,
/// the signature-polymorphic block, the representation-native namespace --
/// or that have no receiver arm at all (`<init>` / `<clinit>`). None of them
/// is a table-bound JNI native in practice; refusing them keeps every memo
/// row a pure function of its key.
pub(in crate::vm::vm_exec) fn jni_dispatch_memo_admits_names(class_name: &str, method_name: &str) -> bool {
    !(matches!(
        method_name,
        "toString" | "accept" | "createServerSocket" | "<init>" | "<clinit>"
    ) || class_name.is_empty()
        || class_name == "java/lang/Object"
        || class_name == "java/lang/String"
        || class_name == "java/lang/Thread"
        || class_name.starts_with("java/lang/invoke/")
        || class_name.starts_with("javax/net/ssl/")
        || class_name.starts_with("sun/security/ssl/")
        || class_name.starts_with("com/sun/tools/attach/")
        || class_name.starts_with("cratonvm/internal/"))
}

/// The hit half of the JNI dispatch memo, asked at the top of
/// [`invoke_on_class_shared_inner`] (gci i8/dm,
/// `gcd-d5f-jni-default-dispatch-costs-500ns-20260928`). `None` unless the
/// switch is on, a row for `(vm, class_id, name, descriptor)` exists, the JNI
/// table's registration generation and the class redefinition count are the
/// fill's, and `args[0]` is not a lambda-proxy receiver (the prefix's first
/// arm, which a fill on another receiver did not take).
///
/// gci i10/cr: a RECEIVER-SELECTED call (`receiver_selection`, the
/// interpreter's virtual fallback) is served too, under
/// `CRATONVM_JNI_DISPATCH_MEMO_SELECTED`, but only by a row whose fill
/// verified the selection for the same resolved reference
/// ([`jni_dispatch_memo_serves_selection`]).
#[inline]
pub(in crate::vm::vm_exec) fn jni_dispatch_memo_hit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    receiver_selection: Option<crate::runtime::resolve::selection::ResolvedRef>,
) -> Option<JniDispatchHit> {
    if !jni_dispatch_memo_armed(&mut thread.name_override_memo) {
        return None;
    }
    if receiver_selection.is_some()
        && !jni_dispatch_memo_selected_armed(&mut thread.name_override_memo)
    {
        return None;
    }
    #[cfg(feature = "experimental-aot")]
    if crate::native::builtins::aot::is_aot_training() {
        return None;
    }
    let hit = jni_dispatch_memo_serve(
        &thread.name_override_memo,
        shared.vm_identity,
        class_id.as_u32(),
        method_name,
        descriptor,
        || {
            shared
                .natives
                .jni_native_generation
                .load(std::sync::atomic::Ordering::Acquire)
        },
        cratonvm_classloading::class_redefinition_count,
    )?;
    if !jni_dispatch_memo_serves_selection(hit.selected, receiver_selection) {
        return None;
    }
    if let Some(Value::Object(Some(recv))) = args.first().copied() {
        if shared
            .classes
            .is_lambda_proxy_class(shared.mem.heap.class_id_of(recv))
        {
            return None;
        }
    }
    // gci i9/cs: the call-site memo's fill probe (a no-op unless armed).
    jni_site_probe_mark(
        &mut thread.name_override_memo,
        class_id.as_u32(),
        method_name,
        descriptor,
    );
    Some(hit)
}

/// The fill half of the JNI dispatch memo, at the table-bound JNI arm of
/// [`invoke_on_class_shared_inner`]. The caller has checked what only it
/// knows (no C25 retarget, no exact-class native preference; a receiver
/// selection is passed in, see below); this re-checks the rest under one
/// `class_manager` read:
/// both class names pass [`jni_dispatch_memo_admits_names`]; an instance
/// method's class is neither an interface nor abstract (so no receiver can
/// ever retarget the call); the method walk from `class_id` lands on an
/// `ACC_NATIVE` method of `declaring_class_id` with the same static and
/// synchronized flags (so the method came from the walk, not from a
/// name-keyed override or the not-found fallback). An `ACC_NATIVE` method
/// is never `native_shadows_bytecode`, and the native registry cannot gain a
/// row after boot (`NativeMethodRegistry::register` takes `&mut self`), so
/// `registry_native` stays `None` for the key. The generation is read
/// BEFORE `fn_ptr` is re-confirmed, so a racing `RegisterNatives` leaves the
/// row on an old generation, which the next hit refuses.
///
/// gci i10/cr: a RECEIVER-SELECTED call (`receiver_selection`) fills too,
/// under `CRATONVM_JNI_DISPATCH_MEMO_SELECTED`. The door's method came from
/// JVMS §5.4.6 selection on `class_id` (the receiver's own class) against
/// that reference; the walk check below admits the row only when the walk
/// lands on the same method, so the row answers an unselected call exactly
/// as before, and a selected call only for the reference it records.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(in crate::vm::vm_exec) fn jni_dispatch_memo_fill(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    declaring_class_id: ClassId,
    is_static: bool,
    is_synchronized: bool,
    declaring_name: &str,
    method_name: &str,
    descriptor: &str,
    fn_ptr: usize,
    receiver_selection: Option<crate::runtime::resolve::selection::ResolvedRef>,
) {
    if !jni_dispatch_memo_armed(&mut thread.name_override_memo) {
        return;
    }
    if receiver_selection.is_some()
        && !jni_dispatch_memo_selected_armed(&mut thread.name_override_memo)
    {
        return;
    }
    #[cfg(feature = "experimental-aot")]
    if crate::native::builtins::aot::is_aot_training() {
        return;
    }
    let generation = shared
        .natives
        .jni_native_generation
        .load(std::sync::atomic::Ordering::Acquire);
    let redefinitions = cratonvm_classloading::class_redefinition_count();
    if crate::native::jni::find_jni_native(&shared.natives, declaring_name, method_name, descriptor)
        != Some(fn_ptr)
    {
        return;
    }
    let admitted = {
        let cm = shared.classes.class_manager.read();
        match cm.get_class(class_id) {
            None => false,
            Some(class) => {
                jni_dispatch_memo_admits_names(&class.name, method_name)
                    && jni_dispatch_memo_admits_names(declaring_name, method_name)
                    && !shared.natives.native_methods.has_app_workaround_prefix(
                        cratonvm_native_api::AppWorkaroundKind::SkipJniMemo,
                        &class.name,
                    )
                    && !shared.natives.native_methods.has_app_workaround_prefix(
                        cratonvm_native_api::AppWorkaroundKind::SkipJniMemo,
                        declaring_name,
                    )
                    && (is_static || !(class.is_interface() || class.is_abstract()))
                    && crate::classloading::find_method_recursive(
                        class_id,
                        method_name,
                        descriptor,
                        &cm.class_store,
                    )
                    .is_some_and(|(method, declaring)| {
                        declaring == declaring_class_id
                            && method.is_native()
                            && method.is_static() == is_static
                            && method.is_synchronized() == is_synchronized
                    })
            }
        }
    };
    if !admitted {
        return;
    }
    jni_dispatch_memo_store(
        &mut thread.name_override_memo,
        JniDispatchRow {
            vm_identity: shared.vm_identity,
            class_id: class_id.as_u32(),
            method_name: Box::from(method_name),
            descriptor: Box::from(descriptor),
            generation,
            redefinitions,
            hit: JniDispatchHit {
                fn_ptr,
                declaring_class_id,
                is_static,
                is_synchronized,
                class_name: std::sync::Arc::from(declaring_name),
                selected: receiver_selection,
            },
        },
    );
    // gci i9/cs: the call-site memo's fill probe (a no-op unless armed).
    jni_site_probe_mark(
        &mut thread.name_override_memo,
        class_id.as_u32(),
        method_name,
        descriptor,
    );
}

// ───────────────── gci i9/cs: the JNI call-site memo (caller half) ─────────────────
//
// `gci-i8dm-jni-native-call-sites-re-resolve-by-name-every-call-FIXED-20261017`: no
// call-site cache holds a table-bound JNI native, so an `invokestatic` of one
// re-ran the whole by-name slow path on every call before it reached the
// door (`execute_invokestatic`: the constant-pool resolve, the registry and
// ancestor walks under `class_manager` reads, the loader-aware owner block,
// the owner's init check, `try_stackless_invoke`'s miss, `invoke_or_native`'s
// walk, and a `populate_invoke_cache` that stores nothing; the JIT's kind-3
// arm: the bytecode-callee refusal, `jit_static_owner_override`, the init
// prelude and `invoke_static_or_native_prepared`'s walk).
//
// The memo is a per-thread, direct-mapped table keyed by the CALL SITE
// (`vm_identity`, the caller's class id, and the site: the interpreter's
// constant-pool index or the JIT's `JitInvokeInfo` address). A row holds
// the class the door was asked on, and a hit calls the door on that class
// directly ([`jni_site_memo_call`]), where the i8/dm door memo answers it.
//
// FILL, only from a slow call that PROVABLY ended in the door's JNI arm for
// this site's key: the caller arms a probe ([`jni_site_probe_arm`]) around
// its by-name fallback; the door's memo marks it when it serves or stores
// the `(class, name, descriptor)` the probe is keyed on
// ([`jni_site_probe_mark`]); the caller then stores the row
// ([`jni_site_probe_finish`]) only when the marked class is the owner the
// site resolved, nothing else with that key reached a different class, the
// call completed, the JNI registration generation and the redefinition
// count did not move during the call, the door's row is a STATIC native
// with the same stamps, the names pass [`jni_site_memo_admits`] (no
// argument-keyed arm of the by-name walk or the force-native intercept can
// answer them) and the class whose `<clinit>` the call owes is initialized
// (JVMS §5.5; initialization is monotone).
//
// A hit re-checks only what can change: the generation (`RegisterNatives`,
// `UnregisterNatives`, a `dlsym` bind, a JVMTI rebind) and the redefinition
// count (a redefined caller's constant pool, a redefined native's class).
// The owner identity is kept, as JVMS §5.4.3 keeps a resolved symbolic
// reference; the by-name route this skips re-resolved it by name on every
// call. Behind the i8/dm opt-in `CRATONVM_JNI_DISPATCH_MEMO` (the probe is
// marked by its door memo); `CRATONVM_JNI_CALL_SITE_MEMO=0` turns only this
// caller half off. Collector-independent and mode-independent code.

/// Rows of the per-thread JNI call-site memo (direct-mapped).
pub(in crate::vm::vm_exec) const JNI_SITE_MEMO_ROWS: usize = 64;

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of an interpreter call-site hit.
pub(crate) const JNI_SITE_HIT_INTERP: &str = "execute_invokestatic.jni-site-hit";

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of a compiled call-site hit.
pub(crate) const JNI_SITE_HIT_JIT: &str = "jit_dispatch_static.jni-site-hit";

/// One call-site row: the key, the stamps read when the probe was armed,
/// and what a hit needs.
pub(in crate::vm::vm_exec) struct JniSiteRow {
    pub(in crate::vm::vm_exec) vm_identity: usize,
    pub(in crate::vm::vm_exec) caller: u32,
    pub(in crate::vm::vm_exec) site: u64,
    pub(in crate::vm::vm_exec) generation: u64,
    pub(in crate::vm::vm_exec) redefinitions: u64,
    pub(in crate::vm::vm_exec) hit: JniSiteHit,
}

/// What a call-site hit hands its caller: the class the door is asked on,
/// the site's constant-pool names and the parameter count to pop.
#[derive(Clone)]
pub(crate) struct JniSiteHit {
    pub(crate) door_class_id: ClassId,
    pub(crate) class_name: std::sync::Arc<str>,
    pub(crate) method_name: std::sync::Arc<str>,
    pub(crate) descriptor: std::sync::Arc<str>,
    pub(crate) num_params: usize,
}

/// The armed fill probe: the key hash of the `(name, descriptor)` it
/// watches, the first class the door's memo marked for it, and whether a
/// second, different class was marked too.
pub(in crate::vm::vm_exec) struct JniSiteProbe {
    pub(in crate::vm::vm_exec) key: u64,
    pub(in crate::vm::vm_exec) door: Option<u32>,
    pub(in crate::vm::vm_exec) conflict: bool,
}

/// What [`jni_site_probe_arm`] hands back to [`jni_site_probe_finish`]: the
/// probe it displaced (restored at the finish, so nested slow calls each see
/// their own) and the stamps read before the slow call.
#[must_use]
pub(crate) struct JniSiteProbeSave {
    pub(in crate::vm::vm_exec) prev: Option<JniSiteProbe>,
    pub(in crate::vm::vm_exec) generation: u64,
    pub(in crate::vm::vm_exec) redefinitions: u64,
}

/// The site word of an interpreter `invokestatic` (its constant-pool index).
#[inline]
pub(crate) fn jni_interp_site(cp_index: u16) -> u64 {
    (1u64 << 62) | u64::from(cp_index)
}

/// The site word of a compiled `invokestatic` (its `JitInvokeInfo` address).
/// A JIT row also re-checks the site's three names on every hit, since a
/// freed compilation's address can be handed to a new one.
#[inline]
pub(crate) fn jni_jit_site(info_ptr: usize) -> u64 {
    // Cast: an address widened into the site word; the tag bit keeps it apart
    // from every interpreter site.
    (2u64 << 62) ^ (info_ptr as u64)
}

#[inline]
pub(in crate::vm::vm_exec) fn jni_site_memo_index(caller: u32, site: u64) -> usize {
    let h = (u64::from(caller) ^ site.rotate_left(17)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // Cast: the top bits of a 64-bit hash, reduced below the row count.
    ((h >> 40) as usize) % JNI_SITE_MEMO_ROWS
}

/// FNV-1a over `name`, a separator and `descriptor`: the probe's key.
#[inline]
pub(in crate::vm::vm_exec) fn jni_site_probe_key(method_name: &str, descriptor: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in method_name
        .bytes()
        .chain(std::iter::once(0u8))
        .chain(descriptor.bytes())
    {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Both switches: the i8/dm door memo (`CRATONVM_JNI_DISPATCH_MEMO`,
/// opt-in), and this caller half (`CRATONVM_JNI_CALL_SITE_MEMO`, default
/// ON inside it, `=0` turns it off), each read once per thread.
#[inline]
pub(in crate::vm::vm_exec) fn jni_site_memo_armed(memo: &mut NameOverrideMemo) -> bool {
    if !jni_dispatch_memo_armed(memo) {
        return false;
    }
    if memo.site_state == 0 {
        memo.site_state =
            {
                1
            };
    }
    memo.site_state == 1
}

/// Names a call-site row is never kept for: the door memo's refusals, and
/// every method name an argument- or receiver-keyed arm of the by-name walk
/// (`invoke_or_native_impl_walk`, entered with `ByNameKind::Virtual` by the
/// interpreter's fallback under `CRATONVM_INVOKESTATIC_FALLBACK_STATIC=0`;
/// `ByNameKind::Static` by default since gci i10/cr, which skips those arms)
/// or of `intercept_force_registered_native` reads. A row is then a function
/// of its key, whatever the arguments.
pub(in crate::vm::vm_exec) fn jni_site_memo_admits(class_name: &str, method_name: &str) -> bool {
    jni_dispatch_memo_admits_names(class_name, method_name)
        && !matches!(
            method_name,
            "type"
                | "invoke"
                | "invokeExact"
                | "invokeBasic"
                | "apply"
                | "loadClass"
                | "setDefaultAssertionStatus"
                | "getResource"
                | "getResources"
                | "getResourceAsStream"
                | "getClassLoader"
                | "newFileChannel"
                | "createSymbolicLink"
                | "createLink"
                | "readSymbolicLink"
        )
        && !class_name.starts_with("java/lang/Class")
        && !class_name.starts_with("java/net/")
        && !class_name.starts_with("java/lang/reflect/")
        && !class_name.starts_with("java/lang/foreign/")
}

/// The door's mark: the armed probe (if any) learns that the door's memo
/// served or stored `(class_id, method_name, descriptor)`.
#[inline]
pub(in crate::vm::vm_exec) fn jni_site_probe_mark(
    memo: &mut NameOverrideMemo,
    class_id: u32,
    method_name: &str,
    descriptor: &str,
) {
    if let Some(probe) = memo.site.as_mut().and_then(|site| site.probe.as_mut()) {
        if probe.key == jni_site_probe_key(method_name, descriptor) {
            match probe.door {
                None => probe.door = Some(class_id),
                Some(door) if door != class_id => probe.conflict = true,
                Some(_) => {}
            }
        }
    }
}

/// The row for `(vm, caller, site)` when its stamps are `generation` /
/// `redefinitions` (and, for a JIT row, its names are `verify`). A row on
/// older stamps is cleared.
pub(in crate::vm::vm_exec) fn jni_site_memo_serve(
    memo: &mut NameOverrideMemo,
    vm_identity: usize,
    caller: u32,
    site: u64,
    verify: Option<(&str, &str, &str)>,
    generation: u64,
    redefinitions: u64,
) -> Option<JniSiteHit> {
    let at = jni_site_memo_index(caller, site);
    let rows = &mut memo.site.as_mut()?.rows;
    let current = match rows.get(at) {
        Some(Some(row))
            if row.vm_identity == vm_identity
                && row.caller == caller
                && row.site == site
                && match verify {
                    None => true,
                    Some((c, m, d)) => {
                        &*row.hit.class_name == c
                            && &*row.hit.method_name == m
                            && &*row.hit.descriptor == d
                    }
                } =>
        {
            row.generation == generation && row.redefinitions == redefinitions
        }
        _ => return None,
    };
    if !current {
        rows[at] = None;
        return None;
    }
    rows[at].as_ref().map(|row| row.hit.clone())
}

pub(in crate::vm::vm_exec) fn jni_site_memo_store(memo: &mut NameOverrideMemo, row: JniSiteRow) {
    let rows = &mut memo.site.get_or_insert_with(Box::default).rows;
    if rows.len() < JNI_SITE_MEMO_ROWS {
        rows.resize_with(JNI_SITE_MEMO_ROWS, || None);
    }
    let at = jni_site_memo_index(row.caller, row.site);
    rows[at] = Some(row);
}

/// The hit half, asked by a static call site before its by-name slow path:
/// `Some` when the switches are on and a current row exists for the site.
/// The caller then pops the arguments and calls [`jni_site_memo_call`].
#[inline]
pub(crate) fn jni_site_memo_lookup(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    site: u64,
    verify: Option<(&str, &str, &str)>,
    label: &str,
) -> Option<JniSiteHit> {
    if !jni_site_memo_armed(&mut thread.name_override_memo) {
        return None;
    }
    if thread.name_override_memo.site.is_none() {
        return None;
    }
    #[cfg(feature = "experimental-aot")]
    if crate::native::builtins::aot::is_aot_training() {
        return None;
    }
    let hit = jni_site_memo_serve(
        &mut thread.name_override_memo,
        shared.vm_identity,
        caller.as_u32(),
        site,
        verify,
        shared
            .natives
            .jni_native_generation
            .load(std::sync::atomic::Ordering::Acquire),
        cratonvm_classloading::class_redefinition_count(),
    )?;
    // The engagement count (`CRATONVM_DBG_DISPATCH_TALLY`).
    dbg_dispatch_tally(label, "", &hit.method_name, &hit.descriptor);
    Some(hit)
}

/// A call-site hit's dispatch: the door on the class the site resolved, as
/// a static call (no receiver arm, no retarget), where the i8/dm door memo
/// runs `jni_table_native_call` under the method's monitor. Should the
/// door's row be gone (evicted, or a racing re-registration), the door
/// takes its full path on the same class, which ends in the same arm.
pub(crate) fn jni_site_memo_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    hit: &JniSiteHit,
    args: &[Value],
) -> MethodCallResult {
    invoke_static_on_class_shared(
        shared,
        thread,
        hit.door_class_id,
        &hit.method_name,
        &hit.descriptor,
        args,
    )
}

/// Arm the fill probe around a static call site's by-name fallback. `None`
/// when the memo is off (nothing to finish). The displaced probe and the
/// stamps travel in the save.
pub(crate) fn jni_site_probe_arm(
    shared: &SharedVm,
    thread: &mut JvmThread,
    method_name: &str,
    descriptor: &str,
) -> Option<JniSiteProbeSave> {
    // The offload hook sits on the slow path a hit would skip.
    if cfg!(feature = "gpu-offload") {
        return None;
    }
    if !jni_site_memo_armed(&mut thread.name_override_memo) {
        return None;
    }
    #[cfg(feature = "experimental-aot")]
    if crate::native::builtins::aot::is_aot_training() {
        return None;
    }
    let generation = shared
        .natives
        .jni_native_generation
        .load(std::sync::atomic::Ordering::Acquire);
    let redefinitions = cratonvm_classloading::class_redefinition_count();
    let tables = thread.name_override_memo.site.get_or_insert_with(Box::default);
    let prev = tables.probe.replace(JniSiteProbe {
        key: jni_site_probe_key(method_name, descriptor),
        door: None,
        conflict: false,
    });
    Some(JniSiteProbeSave {
        prev,
        generation,
        redefinitions,
    })
}

/// Close the probe [`jni_site_probe_arm`] opened (restoring the displaced
/// one) and store the site's row when the slow call provably ended in the
/// door's JNI arm for this key (see the block comment above). `owner` is the
/// class the site dispatched on when it chose one (a self call, a loader's
/// selection); `None` means the flat by-name answer for `class_name`.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn jni_site_probe_finish(
    shared: &SharedVm,
    thread: &mut JvmThread,
    save: JniSiteProbeSave,
    caller: ClassId,
    site: u64,
    owner: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    num_params: usize,
    completed: bool,
) {
    let JniSiteProbeSave {
        prev,
        generation,
        redefinitions,
    } = save;
    let probe = std::mem::replace(
        &mut thread
            .name_override_memo
            .site
            .get_or_insert_with(Box::default)
            .probe,
        prev,
    );
    let Some(probe) = probe else {
        return;
    };
    // gci i13/jm (F3): the interpreter site's "probe missed" stamp -- a
    // completed call the door never marked counts a miss (only while the
    // stamps read at the arm are still current), a marked call clears it.
    if jni_site_is_interp(site) {
        let outcome = if probe.door.is_some() {
            Some(false)
        } else if completed {
            Some(true)
        } else {
            None
        };
        if let Some(missed) = outcome {
            let current = shared
                .natives
                .jni_native_generation
                .load(std::sync::atomic::Ordering::Acquire)
                == generation
                && cratonvm_classloading::class_redefinition_count() == redefinitions;
            if let Some(tables) = thread.name_override_memo.site.as_deref_mut() {
                if !missed || (current && jni_site_miss_stamp_armed(tables)) {
                    jni_site_miss_note(
                        tables,
                        shared.vm_identity,
                        caller.as_u32(),
                        site,
                        generation,
                        redefinitions,
                        missed,
                    );
                }
            }
        }
    }
    let door = match probe.door {
        Some(door) if completed && !probe.conflict && door != 0 => door,
        _ => return,
    };
    if probe.key != jni_site_probe_key(method_name, descriptor)
        || !jni_site_memo_admits(class_name, method_name)
        || shared.natives.native_methods.has_app_workaround_prefix(
            cratonvm_native_api::AppWorkaroundKind::SkipJniMemo,
            class_name,
        )
    {
        return;
    }
    let owner = match owner {
        Some(owner) => owner,
        None => match shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(class_name)
        {
            Some(owner) => owner,
            None => return,
        },
    };
    if owner.as_u32() != door {
        return;
    }
    if shared
        .natives
        .jni_native_generation
        .load(std::sync::atomic::Ordering::Acquire)
        != generation
        || cratonvm_classloading::class_redefinition_count() != redefinitions
    {
        return;
    }
    let door_row_static_and_current = jni_dispatch_memo_find(
        &thread.name_override_memo,
        shared.vm_identity,
        door,
        method_name,
        descriptor,
    )
    .is_some_and(|row| {
        row.generation == generation && row.redefinitions == redefinitions && row.hit.is_static
    });
    if !door_row_static_and_current {
        return;
    }
    // JVMS §5.5: a hit skips the slow path's initialization of the class
    // that declares the method, so a row exists only once it is initialized.
    if static_owner_init_pending(shared, owner, class_name, method_name, descriptor).is_some() {
        return;
    }
    jni_site_memo_store(
        &mut thread.name_override_memo,
        JniSiteRow {
            vm_identity: shared.vm_identity,
            caller: caller.as_u32(),
            site,
            generation,
            redefinitions,
            hit: JniSiteHit {
                door_class_id: owner,
                class_name: std::sync::Arc::from(class_name),
                method_name: std::sync::Arc::from(method_name),
                descriptor: std::sync::Arc::from(descriptor),
                num_params,
            },
        },
    );
}

// ───────────── gci i13/jm: the interpreter site's "probe missed" stamp (F3) ─────────────
//
// `gci-i10cr-jni-call-path-review-findings-FIXED-20261016` F3: with the memo on,
// `execute_invokestatic` arms the fill probe around EVERY by-name fallback,
// also at a site whose target never reaches the door's JNI arm (a registry
// native, a native-override owner, a compatibility stub, a Java body the
// invoke cache does not hold): an FNV hash of the name and descriptor and a
// probe swap per slow call, for a row that can never be stored.
//
// The stamp: after `JNI_SITE_MISS_STAMP_AFTER` CONSECUTIVE completed slow calls
// of one interpreter site that the door never marked, at unchanged JNI
// generation and redefinition count, the site stops arming the probe until
// either stamp moves. A marked call clears the stamp.
//
// Why a miss is stable at unchanged stamps (i12/mf's objection): the door
// marks a probe from exactly two places, its memo hit and its memo fill, and
// the fill runs in the table-bound JNI arm whenever the arm's `(class, name,
// descriptor)` passes `jni_dispatch_memo_fill`'s checks -- all functions of
// the class, the names, the JNI table (generation) and the redefinition
// count. The one arm that reached a JNI native WITHOUT the fill was the
// lazy `dlsym` bind (`resolve_jni_native_in_libraries`, the native's first
// call): since i10/cr a fresh bind does not bump the generation, so a stamp
// taken there would have kept a JNI site off its row for good. Since gci
// i13/jm that arm fills the door memo too (and so marks the probe), with the
// address it just registered, exactly as the table arm does on the next call.
// A fresh `RegisterNatives` key does not bump either, but a call that reached
// no native before it completed only through a registry native or Java code,
// both of which win over the JNI table for the key, so it stays a miss.
// Two misses, not one, keep a first call that runs `<clinit>` (or binds a
// library in it) from stamping the site. The stamp only skips the probe: a
// wrong stamp costs the site its call-site row (a lost optimisation), never
// a wrong dispatch. JIT sites are never stamped (a freed compilation's
// `JitInvokeInfo` address can be reused by another site).

/// Consecutive completed, unmarked slow calls before an interpreter site
/// stops arming the fill probe.
pub(in crate::vm::vm_exec) const JNI_SITE_MISS_STAMP_AFTER: u8 = 2;

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of a slow call whose probe the
/// miss stamp skipped (the stamp's engagement count).
pub(crate) const JNI_SITE_PROBE_MISS_SKIP: &str = "execute_invokestatic.jni-site-probe-miss-skip";

/// One "probe missed" stamp: the site key, the stamps read when the probe was
/// armed, and how many consecutive completed calls missed at those stamps.
pub(in crate::vm::vm_exec) struct JniProbeMissRow {
    pub(in crate::vm::vm_exec) vm_identity: usize,
    pub(in crate::vm::vm_exec) caller: u32,
    pub(in crate::vm::vm_exec) site: u64,
    pub(in crate::vm::vm_exec) generation: u64,
    pub(in crate::vm::vm_exec) redefinitions: u64,
    pub(in crate::vm::vm_exec) misses: u8,
}

/// Is `site` an interpreter site word ([`jni_interp_site`])? A JIT site word
/// ([`jni_jit_site`]) carries tag `2` in its top two bits.
#[inline]
pub(in crate::vm::vm_exec) fn jni_site_is_interp(site: u64) -> bool {
    site >> 62 == 1
}

/// The stamp's sub-switch, read once per thread
/// (`CRATONVM_JNI_SITE_PROBE_MISS_STAMP`, default ON inside the call-site
/// memo, `=0` arms the probe on every slow call as before).
#[inline]
pub(in crate::vm::vm_exec) fn jni_site_miss_stamp_armed(tables: &mut JniSiteTables) -> bool {
    if tables.miss_state == 0 {
        tables.miss_state = {
            1
        };
    }
    tables.miss_state == 1
}

/// Does `row` stamp `(vm, caller, site)` as missed at these stamps? Pure.
pub(in crate::vm::vm_exec) fn jni_site_miss_row_skips(
    row: Option<&JniProbeMissRow>,
    vm_identity: usize,
    caller: u32,
    site: u64,
    generation: u64,
    redefinitions: u64,
) -> bool {
    row.is_some_and(|r| {
        r.vm_identity == vm_identity
            && r.caller == caller
            && r.site == site
            && r.generation == generation
            && r.redefinitions == redefinitions
            && r.misses >= JNI_SITE_MISS_STAMP_AFTER
    })
}

/// Record one slow call's outcome for an interpreter site: `missed` (it
/// completed and the door never marked the probe) counts one more miss at
/// these stamps (a row on other stamps restarts at one); a marked call
/// (`!missed`) clears the site's row. A clear never allocates the table.
pub(in crate::vm::vm_exec) fn jni_site_miss_note(
    tables: &mut JniSiteTables,
    vm_identity: usize,
    caller: u32,
    site: u64,
    generation: u64,
    redefinitions: u64,
    missed: bool,
) {
    let at = jni_site_memo_index(caller, site);
    let same_site = |r: &JniProbeMissRow| {
        r.vm_identity == vm_identity && r.caller == caller && r.site == site
    };
    if !missed {
        let clear = tables
            .miss
            .get(at)
            .and_then(Option::as_ref)
            .is_some_and(|r| same_site(r));
        if clear {
            tables.miss[at] = None;
        }
        return;
    }
    if tables.miss.len() < JNI_SITE_MEMO_ROWS {
        tables.miss.resize_with(JNI_SITE_MEMO_ROWS, || None);
    }
    let same_stamps = tables.miss[at].as_ref().is_some_and(|r| {
        same_site(r) && r.generation == generation && r.redefinitions == redefinitions
    });
    if same_stamps {
        if let Some(r) = tables.miss[at].as_mut() {
            r.misses = r.misses.saturating_add(1);
        }
    } else {
        tables.miss[at] = Some(JniProbeMissRow {
            vm_identity,
            caller,
            site,
            generation,
            redefinitions,
            misses: 1,
        });
    }
}

/// Asked by `execute_invokestatic` right before [`jni_site_probe_arm`]:
/// `true` when this interpreter site's stamp says the probe would miss again
/// (the caller then runs its fallback unprobed). `false` whenever the memo or
/// the stamp is off, the site is a JIT site, or no current stamp exists.
pub(crate) fn jni_site_probe_missed(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    site: u64,
    method_name: &str,
    descriptor: &str,
) -> bool {
    if !jni_site_is_interp(site) || !jni_site_memo_armed(&mut thread.name_override_memo) {
        return false;
    }
    let Some(tables) = thread.name_override_memo.site.as_deref_mut() else {
        return false;
    };
    if tables.miss.is_empty() || !jni_site_miss_stamp_armed(tables) {
        return false;
    }
    let caller = caller.as_u32();
    let skips = jni_site_miss_row_skips(
        tables.miss.get(jni_site_memo_index(caller, site)).and_then(Option::as_ref),
        shared.vm_identity,
        caller,
        site,
        shared
            .natives
            .jni_native_generation
            .load(std::sync::atomic::Ordering::Acquire),
        cratonvm_classloading::class_redefinition_count(),
    );
    if skips {
        // The engagement count (`CRATONVM_DBG_DISPATCH_TALLY`).
        dbg_dispatch_tally(JNI_SITE_PROBE_MISS_SKIP, "", method_name, descriptor);
    }
    skips
}

// ───────────── gci i10/cr: negative fill stamps, and the static fallback ─────────────
//
// `i9-cs-report.md` section 6 item 3: after every call of a table-bound JNI
// native that no call-site cache holds, the interpreter's invoke-cache fill
// (`dispatch_static::populate_invoke_cache`, `dispatch_virtual::
// populate_virtual_invoke_cache`) re-ran its whole resolution (the constant-
// pool metadata, two or three `class_manager` reads, the method walk or
// selection, the registry asks) and stored nothing, because the `Native` /
// `VirtualNative` entries hold registry natives only. A negative stamp per
// `(vm, caller, site)` -- site = the constant-pool index, the fill's
// `is_special`, and for a virtual fill the receiver's class -- remembers that
// outcome, and the next fill for the site returns at once while the
// redefinition count is the stamp's.
//
// Sound by construction: an invoke-cache entry is only ever an optimisation
// (the slow path dispatches correctly without one), so a stamp can cost a
// later fill that would now store something, never a wrong dispatch. What
// could make the fill store something for a JNI native's site: a registry
// native for the triple (the registry cannot gain a row after boot,
// `NativeMethodRegistry::register` takes `&mut self`), or a redefinition (the
// stamp's count). Behind the i8/dm opt-in `CRATONVM_JNI_DISPATCH_MEMO`; off,
// one byte compare per fill.

/// Rows of the per-thread negative fill stamp table (direct-mapped).
pub(in crate::vm::vm_exec) const JNI_NOFILL_ROWS: usize = 64;

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of a fill a stamp skipped.
pub(in crate::vm::vm_exec) const JNI_NOFILL_SKIP: &str = "invoke_cache_fill.jni-nofill-skip";

/// One negative fill stamp: an interpreter invoke-cache fill for this site
/// ended at a table-bound JNI native and stored nothing, as of the
/// redefinition count `redefinitions`.
pub(in crate::vm::vm_exec) struct JniNoFillRow {
    pub(in crate::vm::vm_exec) vm_identity: usize,
    pub(in crate::vm::vm_exec) caller: u32,
    pub(in crate::vm::vm_exec) site: u64,
    pub(in crate::vm::vm_exec) redefinitions: u64,
}

/// What [`jni_nofill_ask`] tells a fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JniNoFill {
    /// The memo is off: fill as before; nothing is stamped.
    Off,
    /// A current stamp: the fill would store nothing again; skip it.
    Stamped,
    /// No current stamp: fill, and stamp ([`jni_nofill_stamp`]) as of this
    /// redefinition count if it ends at a table-bound JNI native with
    /// nothing stored.
    Open(u64),
}

/// The site word of a negative stamp: the constant-pool index, the fill's
/// `is_special`, and for a virtual fill the receiver's class (`None` for a
/// static, special or private fill, whose target has no receiver guard).
#[inline]
pub(crate) fn jni_nofill_site(cp_index: u16, is_special: bool, receiver: Option<ClassId>) -> u64 {
    (u64::from(receiver.map_or(0, |r| r.as_u32())) << 20)
        | (u64::from(is_special) << 16)
        | u64::from(cp_index)
}

#[inline]
pub(in crate::vm::vm_exec) fn jni_nofill_index(caller: u32, site: u64) -> usize {
    jni_site_memo_index(caller, site) % JNI_NOFILL_ROWS
}

/// Whether `rows` hold a stamp for `(vm, caller, site)` as of `redefinitions`.
pub(in crate::vm::vm_exec) fn jni_nofill_current(
    rows: &[Option<JniNoFillRow>],
    vm_identity: usize,
    caller: u32,
    site: u64,
    redefinitions: u64,
) -> bool {
    matches!(
        rows.get(jni_nofill_index(caller, site)),
        Some(Some(row))
            if row.vm_identity == vm_identity
                && row.caller == caller
                && row.site == site
                && row.redefinitions == redefinitions
    )
}

pub(in crate::vm::vm_exec) fn jni_nofill_store(rows: &mut Vec<Option<JniNoFillRow>>, row: JniNoFillRow) {
    if rows.len() < JNI_NOFILL_ROWS {
        rows.resize_with(JNI_NOFILL_ROWS, || None);
    }
    let at = jni_nofill_index(row.caller, row.site);
    rows[at] = Some(row);
}

/// Asked first by an interpreter invoke-cache fill: [`JniNoFill::Stamped`]
/// when a current stamp says the fill would store nothing again (the caller
/// returns), else whether to stamp the outcome. One byte compare when the
/// memo is off.
#[inline]
pub(crate) fn jni_nofill_ask(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    site: u64,
) -> JniNoFill {
    if !jni_dispatch_memo_armed(&mut thread.name_override_memo) {
        return JniNoFill::Off;
    }
    let redefinitions = cratonvm_classloading::class_redefinition_count();
    let stamped = thread.name_override_memo.site.as_ref().is_some_and(|tables| {
        jni_nofill_current(
            &tables.nofill,
            shared.vm_identity,
            caller.as_u32(),
            site,
            redefinitions,
        )
    });
    if stamped {
        dbg_dispatch_tally(JNI_NOFILL_SKIP, "", "", "");
        JniNoFill::Stamped
    } else {
        JniNoFill::Open(redefinitions)
    }
}

/// Stamp a fill that ended at a table-bound JNI native (an `ACC_NATIVE`
/// method with no registry native) and stored nothing. A no-op unless `ask`
/// is [`JniNoFill::Open`]; the stamp carries the count read before the
/// fill, so a redefinition during the fill leaves it stale at once.
#[cold]
pub(crate) fn jni_nofill_stamp(
    shared: &SharedVm,
    thread: &mut JvmThread,
    ask: JniNoFill,
    caller: ClassId,
    site: u64,
) {
    let JniNoFill::Open(redefinitions) = ask else {
        return;
    };
    #[cfg(feature = "experimental-aot")]
    if crate::native::builtins::aot::is_aot_training() {
        return;
    }
    let tables = thread.name_override_memo.site.get_or_insert_with(Box::default);
    jni_nofill_store(
        &mut tables.nofill,
        JniNoFillRow {
            vm_identity: shared.vm_identity,
            caller: caller.as_u32(),
            site,
            redefinitions,
        },
    );
}

/// gci i10/cr (`i9-cs-report.md` section 6 item 2): whether
/// `execute_invokestatic`'s by-name fallback dispatches through the STATIC
/// entries (`invoke_static_or_native`, `ByNameKind::Static`; and, for an
/// owner the site selected, [`invoke_static_on_class_shared`],
/// `OnClassKind::Static`) rather than `invoke_or_native` /
/// `invoke_on_class_shared`, which read a static's first ARGUMENT as a
/// receiver (the receiver-keyed arms of `invoke_or_native_impl_walk`, the
/// door's lambda-proxy arm) and ran the symbolic owner's `<clinit>` from the
/// instance tail. `CRATONVM_INVOKESTATIC_FALLBACK_STATIC`, default ON (a
/// correctness fix; `=0` restores the old entries), read once per thread.
#[inline]
pub(crate) fn invokestatic_fallback_static_kind(thread: &mut JvmThread) -> bool {
    let memo = &mut thread.name_override_memo;
    if memo.static_fallback_state == 0 {
        memo.static_fallback_state = {
            1
        };
    }
    memo.static_fallback_state == 1
}

// ───────────── gci i11/cf: the virtual / interface JNI call-site rows ─────────────
//
// `gci-i8dm-jni-native-call-sites-re-resolve-by-name-every-call-FIXED-20261017` item 2:
// an interpreted `invokevirtual` / `invokeinterface` of a table-bound JNI native
// re-ran `execute_invoke_kind` on every call (the constant-pool resolve, the
// owner and §6.5 checks, the private-target walk, the proxy and intercept arms,
// `try_stackless_invoke`'s miss) before its fallback reached the door, where the
// i8/dm memo (with i10/cr's receiver-selected half) answered.
//
// A row is keyed by the CALL SITE (`vm_identity`, the caller's class id, the
// site word [`jni_vsite`] of the constant-pool index) and holds the receiver
// class the fill saw (a monomorphic row: a hit compares the receiver's class at
// the operand-stack depth the row names, a different class takes the slow path),
// the reference the site's §5.4.6 selection used, and the names. A hit pops the
// arguments as the slow path does and calls the door on the receiver's class
// with that selection, where the door memo runs `jni_table_native_call`.
//
// FILL, from the slow path's fallback only (`execute_invoke_kind`, the arm that
// dispatches on the receiver's own class), under the i9/cs probe: the door's
// memo marked exactly the receiver's class for the site's `(name, descriptor)`,
// the call completed, the JNI generation and the redefinition count did not
// move, the door's row is an INSTANCE native with the same stamps that serves
// this selection ([`jni_dispatch_memo_serves_selection`]), and the census below
// admits the site ([`jni_vsite_admits`] on the constant-pool class and
// [`jni_site_memo_admits`] on the receiver's class; the caller adds: not a
// lambda-proxy or array receiver, no private target, no loader-interface or
// loader-aware override, the receiver's class does not reach `Proxy$Instance`,
// not an obsolete frame).
//
// THE CENSUS (what the hit skips, in `execute_invoke_kind` order, and why each
// is a function of the row's key or refused):
//  - the recorded owner failure / null-receiver owner resolution: a site that
//    completed has a resolved owner entry; a null receiver never matches a row;
//  - the §6.5 kind and static-flag checks, `owner_known_to_caller`, the
//    selection's resolved reference, the KAFKA interface stash: functions of
//    the caller's constant pool and the loaded classes (redefinition stamp);
//  - argument popping and the forwarding barrier: the hit does the same;
//  - `MergedAnnotation$Adapt.isIn` (names, refused), the stale-mirror recovery
//    (an all-zero header has class id 0, which no row holds), the lambda-proxy
//    arm (refused at fill; the class is fixed), the private target (refused),
//    the interface ICCE checks (the receiver class and the site's interface
//    are the row's; the fill passed them), the array-site arms (array
//    receivers refused), the receiver-class `invoke_class` choice (it only
//    keys the intercepts, which the names refuse);
//  - the proxy arm (the receiver's chain is refused at fill), the annotation
//    proxy (`java/lang/annotation/`, refused), `Thread.clone`'s report (names);
//  - `intercept_force_registered_native` (the file-system-provider and
//    `ClassLoader` resource names, `getClassLoader`, `java/lang/Class*`,
//    `java/net/`: refused; its registry arm cannot answer a triple the door's
//    JNI arm answered, the door memo admits no registry native), the Jython,
//    `URLClassLoader` and `ClassLoader` resource intercepts (names, refused);
//  - the loader-interface / loader-aware overrides (refused while
//    `loader_aware_resolution` is on), the `VarHandle` exact gate
//    (`java/lang/invoke/`, refused);
//  - `try_stackless_invoke`'s miss and the fill after the fallback (the
//    negative stamp's skip): a function of the key and the stamps.
// Behind `CRATONVM_JNI_DISPATCH_MEMO` (opt-in) and `CRATONVM_JNI_CALL_SITE_MEMO`
// (default on inside it); `CRATONVM_JNI_VIRTUAL_SITE_MEMO=0` turns only this half
// off. Collector-independent and mode-independent code.

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of a virtual call-site hit.
pub(crate) const JNI_VSITE_HIT_INTERP: &str = "execute_invoke_kind.jni-vsite-hit";

/// One virtual call-site row: the key, the stamps read when the probe was
/// armed, and what a hit needs.
pub(in crate::vm::vm_exec) struct JniVSiteRow {
    pub(in crate::vm::vm_exec) vm_identity: usize,
    pub(in crate::vm::vm_exec) caller: u32,
    pub(in crate::vm::vm_exec) site: u64,
    pub(in crate::vm::vm_exec) generation: u64,
    pub(in crate::vm::vm_exec) redefinitions: u64,
    pub(in crate::vm::vm_exec) hit: JniVSiteHit,
}

/// What a virtual call-site hit hands `execute_invoke_kind`.
#[derive(Clone)]
pub(crate) struct JniVSiteHit {
    /// The receiver class the fill dispatched on (the door's key).
    pub(crate) receiver_class_id: ClassId,
    /// The reference the fill's §5.4.6 selection used (`None`: the fill
    /// dispatched without one, `invoke_on_class_shared`).
    pub(crate) selected: Option<crate::runtime::resolve::selection::ResolvedRef>,
    pub(crate) method_name: std::sync::Arc<str>,
    pub(crate) descriptor: std::sync::Arc<str>,
    /// The parameter count (the receiver sits at this operand-stack depth).
    pub(crate) num_params: usize,
}

/// The site word of an interpreter `invokevirtual` / `invokeinterface` (its
/// constant-pool index; an index names one `Methodref` or
/// `InterfaceMethodref`, so one opcode). Tag `3` keeps it apart from the
/// static rows' interpreter (`1`) and JIT (`2`) words.
#[inline]
pub(crate) fn jni_vsite(cp_index: u16) -> u64 {
    (3u64 << 62) | u64::from(cp_index)
}

/// All three switches: the door memo (opt-in), the call-site memo (default ON
/// inside it), and this virtual half (`CRATONVM_JNI_VIRTUAL_SITE_MEMO`,
/// default ON inside both, `=0` off), each read once per thread.
#[inline]
pub(in crate::vm::vm_exec) fn jni_vsite_memo_armed(memo: &mut NameOverrideMemo) -> bool {
    if !jni_site_memo_armed(memo) {
        return false;
    }
    if memo.vsite_state == 0 {
        memo.vsite_state =
            {
                1
            };
    }
    memo.vsite_state == 1
}

/// Names a virtual row is never kept for: the static rows' refusals
/// ([`jni_site_memo_admits`]) plus every name an arm of `execute_invoke_kind`
/// before its fallback keys on (the census above): the Jython, `URLClassLoader`
/// and Spring `isIn` intercepts, and `Object`'s members (the array, proxy and
/// `Thread.clone` arms; none is a JNI native of a user class in practice).
pub(in crate::vm::vm_exec) fn jni_vsite_admits(class_name: &str, method_name: &str) -> bool {
    jni_site_memo_admits(class_name, method_name)
        && !matches!(
            method_name,
            "__findattr__"
                | "__findattr_ex__"
                | "findClass"
                | "addURL"
                | "isIn"
                | "clone"
                | "getClass"
                | "hashCode"
                | "equals"
                | "wait"
                | "notify"
                | "notifyAll"
                | "finalize"
        )
        && !class_name.starts_with("java/lang/annotation/")
}

/// The row for `(vm, caller, site)` when its stamps are `generation` /
/// `redefinitions`. A row on older stamps is cleared.
pub(in crate::vm::vm_exec) fn jni_vsite_memo_serve(
    memo: &mut NameOverrideMemo,
    vm_identity: usize,
    caller: u32,
    site: u64,
    generation: u64,
    redefinitions: u64,
) -> Option<JniVSiteHit> {
    let at = jni_site_memo_index(caller, site);
    let rows = &mut memo.site.as_mut()?.vrows;
    let current = match rows.get(at) {
        Some(Some(row))
            if row.vm_identity == vm_identity && row.caller == caller && row.site == site =>
        {
            row.generation == generation && row.redefinitions == redefinitions
        }
        _ => return None,
    };
    if !current {
        rows[at] = None;
        return None;
    }
    rows[at].as_ref().map(|row| row.hit.clone())
}

pub(in crate::vm::vm_exec) fn jni_vsite_memo_store(memo: &mut NameOverrideMemo, row: JniVSiteRow) {
    let rows = &mut memo.site.get_or_insert_with(Box::default).vrows;
    if rows.len() < JNI_SITE_MEMO_ROWS {
        rows.resize_with(JNI_SITE_MEMO_ROWS, || None);
    }
    let at = jni_site_memo_index(row.caller, row.site);
    rows[at] = Some(row);
}

/// The hit half, asked at the top of `execute_invoke_kind` for a non-special
/// site: `Some` when the switches are on and a current row exists. The
/// caller then compares the receiver's class with the row's, pops the
/// arguments and calls the door ([`jni_vsite_memo_call`]); a different
/// receiver class takes the slow path.
#[inline]
pub(crate) fn jni_vsite_memo_lookup(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    site: u64,
) -> Option<JniVSiteHit> {
    if !jni_vsite_memo_armed(&mut thread.name_override_memo) {
        return None;
    }
    if thread.name_override_memo.site.is_none() {
        return None;
    }
    #[cfg(feature = "experimental-aot")]
    if crate::native::builtins::aot::is_aot_training() {
        return None;
    }
    jni_vsite_memo_serve(
        &mut thread.name_override_memo,
        shared.vm_identity,
        caller.as_u32(),
        site,
        shared
            .natives
            .jni_native_generation
            .load(std::sync::atomic::Ordering::Acquire),
        cratonvm_classloading::class_redefinition_count(),
    )
}

/// A virtual call-site hit's dispatch: the door on the receiver's class, with
/// the selection the fill used (the slow path's fallback, exactly), where the
/// door memo answers. Should the door's row be gone, the door takes its full
/// path on the same class, which ends in the same arm.
pub(crate) fn jni_vsite_memo_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    hit: &JniVSiteHit,
    args: &[Value],
) -> MethodCallResult {
    dbg_dispatch_tally(JNI_VSITE_HIT_INTERP, "", &hit.method_name, &hit.descriptor);
    match hit.selected {
        Some(resolved) => invoke_on_receiver_class_selecting(
            shared,
            thread,
            hit.receiver_class_id,
            resolved,
            &hit.method_name,
            &hit.descriptor,
            args,
        ),
        None => invoke_on_class_shared(
            shared,
            thread,
            hit.receiver_class_id,
            &hit.method_name,
            &hit.descriptor,
            args,
        ),
    }
}

/// Arm the fill probe around `execute_invoke_kind`'s receiver-class fallback.
/// `None` when this half is off (nothing to finish).
pub(crate) fn jni_vsite_probe_arm(
    shared: &SharedVm,
    thread: &mut JvmThread,
    method_name: &str,
    descriptor: &str,
) -> Option<JniSiteProbeSave> {
    if !jni_vsite_memo_armed(&mut thread.name_override_memo) {
        return None;
    }
    jni_site_probe_arm(shared, thread, method_name, descriptor)
}

/// Close the probe [`jni_vsite_probe_arm`] opened (restoring the displaced
/// one) and store the site's row when the fallback provably ended in the
/// door's JNI arm for `receiver_class_id` (see the block comment above).
/// `eligible` carries what only the caller knows (the call completed, and the
/// interpreter-side refusals of the census).
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn jni_vsite_probe_finish(
    shared: &SharedVm,
    thread: &mut JvmThread,
    save: JniSiteProbeSave,
    caller: ClassId,
    site: u64,
    receiver_class_id: ClassId,
    selected: Option<crate::runtime::resolve::selection::ResolvedRef>,
    cp_class_name: &str,
    method_name: &str,
    descriptor: &str,
    num_params: usize,
    eligible: bool,
) {
    let JniSiteProbeSave {
        prev,
        generation,
        redefinitions,
    } = save;
    let probe = std::mem::replace(
        &mut thread
            .name_override_memo
            .site
            .get_or_insert_with(Box::default)
            .probe,
        prev,
    );
    let Some(probe) = probe else {
        return;
    };
    let door = match probe.door {
        Some(door) if eligible && !probe.conflict && door != 0 => door,
        _ => return,
    };
    if door != receiver_class_id.as_u32()
        || probe.key != jni_site_probe_key(method_name, descriptor)
        || !jni_vsite_admits(cp_class_name, method_name)
        || shared.natives.native_methods.has_app_workaround_prefix(
            cratonvm_native_api::AppWorkaroundKind::SkipJniMemo,
            cp_class_name,
        )
    {
        return;
    }
    let receiver_admitted = shared
        .classes
        .class_manager
        .read()
        .get_class(receiver_class_id)
        .is_some_and(|class| {
            jni_vsite_admits(&class.name, method_name)
                && !shared.natives.native_methods.has_app_workaround_prefix(
                    cratonvm_native_api::AppWorkaroundKind::SkipJniMemo,
                    &class.name,
                )
        });
    if !receiver_admitted {
        return;
    }
    if shared
        .natives
        .jni_native_generation
        .load(std::sync::atomic::Ordering::Acquire)
        != generation
        || cratonvm_classloading::class_redefinition_count() != redefinitions
    {
        return;
    }
    let door_row_serves = jni_dispatch_memo_find(
        &thread.name_override_memo,
        shared.vm_identity,
        door,
        method_name,
        descriptor,
    )
    .is_some_and(|row| {
        row.generation == generation
            && row.redefinitions == redefinitions
            && !row.hit.is_static
            && jni_dispatch_memo_serves_selection(row.hit.selected, selected)
    });
    if !door_row_serves {
        return;
    }
    jni_vsite_memo_store(
        &mut thread.name_override_memo,
        JniVSiteRow {
            vm_identity: shared.vm_identity,
            caller: caller.as_u32(),
            site,
            generation,
            redefinitions,
            hit: JniVSiteHit {
                receiver_class_id,
                selected,
                method_name: std::sync::Arc::from(method_name),
                descriptor: std::sync::Arc::from(descriptor),
                num_params,
            },
        },
    );
}

// ───────────── gci i12/mf: the compiled virtual / interface JNI call-site rows ─────────────
//
// `gci-i8dm-jni-native-call-sites-re-resolve-by-name-every-call-FIXED-20261017`, the JIT's
// virtual arms (what i11/cf left): a compiled `invokevirtual` / `invokeinterface` of
// a table-bound JNI native reaches the door through one of three by-name tails of
// `vm/src/jit/helpers.rs` -- `jit_invoke_dispatch_body`'s blind `0 | 2` arm, and
// `jit_invoke_virtual_mic_body`'s entryless-hit and miss arms -- each of which re-ran,
// per call, the (site, receiver class) dispatch-target memo, the compile probe or its
// throttle, the bytecode-callee template's refusal and `invoke_or_native`'s whole
// by-name walk (`invoke_or_native_impl_walk`, `ByNameKind::Virtual`) before the door's
// i8/dm memo answered.
//
// A row is keyed by the CALL SITE (`vm_identity`, the caller's class id
// `JitInvokeInfo::declaring_class_id`, the site word [`jni_jit_vsite`] of the info's
// address, re-checked against the info's three names on every hit, since a freed
// compilation's address can be handed to a new one) and holds the receiver class the
// fill saw (monomorphic: a different receiver class takes the slow path, which may
// refill the row). A hit calls the door's entry the walk's tail calls,
// `invoke_by_class_id_shared` on the receiver's class, where the door memo runs
// `jni_table_native_call`.
//
// FILL, from the three tails only, under the i9/cs probe: the door's memo marked
// exactly the RECEIVER's class for the site's `(name, descriptor)` (so the walk's
// tail dispatched on the receiver's own class -- a §5.4.6 retarget to a supertype
// copy marks that copy and is refused); the call completed; the JNI generation, the
// redefinition count and the class-definition epoch of the VM's store did not move
// (the epoch: a second loader defining a name the slow path resolves by name changes
// its answer without a redefinition -- the JIT's own (site, class) memos flush on it,
// `flush_class_identity_dispatch_memos`); the door's row is an INSTANCE native with
// the same stamps; the receiver is a plain, concrete, non-lambda object with no
// selection error; and the census below admits the site's class, the dispatch class
// and the receiver's class ([`jni_jit_vsite_admits`]).
//
// THE CENSUS (what a hit skips, tail by tail, and why each is a function of the row's
// key and stamps, or refused):
//  - `try_jit_virtual_bytecode_callee` (all three): a template exists only for a
//    method with bytecode; the row's method is `ACC_NATIVE` (the door memo's fill
//    walked to it) and a redefinition cannot change that (the redefinition stamp);
//  - `virtual_dispatch_target_cached` (all three): a pure function of (site, receiver
//    class), flushed on the class-definition epoch and the first redefinition (both
//    stamps); a selection error is refused at the fill;
//  - the MIC arms' compile probe and its `MIC_COMPILE_DECLINED` throttle,
//    `try_mic_rust_cached_entry`, `mic_tail_call_selected_declaring_body`,
//    `call_body_the_mic_probe_just_found`, the publications (`mic.update`,
//    `pic.install`, `publish_mega_dispatch_entry`, `bind_mega_selector_at_resolution`):
//    compiled bodies and the caches that hold them; a native method has none, and a
//    skipped publication is only a later miss;
//  - `invoke_or_native_impl_walk` (all three): its receiver-keyed arms key on the
//    receiver's class (the row's) or on names (`DowncallHandle` `type` / `invoke*`,
//    `AnnotationProxy`, `Proxy$Instance`, the `ClassLoader` resource / assertion /
//    `loadClass` names, the functional-interface `apply` redirect, the Spring loader
//    classes: refused by [`jni_site_memo_admits`] / [`jni_vsite_admits`]); the
//    signature-polymorphic call-site arm (`java/lang/invoke/`, refused); the registry
//    arms (a registry row cannot appear after boot, and the door's JNI arm answered
//    the key, so none exists); the redefine guard (the redefinition stamp) and its ZIP
//    waiver, which reads the receiver OBJECT (`java/util/zip/`, `java/util/jar/`:
//    refused); the ancestor walk and the tail's class choice (the receiver's class,
//    proved by the door's mark, under the epoch); the census notes
//    (`lookup_census`, the `ClassLoader` null-name retirement note) are counts only;
//  - what every tail does on an error (the S111r12 rescue through the site's class):
//    the hit keeps it, with the name the tail it stands in for asks
//    (`helpers.rs` `jit_jni_vsite_call`).
// Everything before the three tails (the native site caches, the lambda / annotation
// / `VarHandle` / reflective / `ClassLoader` null-name arms, the compiled-entry
// probes) still runs before the hit. Behind `CRATONVM_JNI_DISPATCH_MEMO` (opt-in),
// `CRATONVM_JNI_CALL_SITE_MEMO` and `CRATONVM_JNI_VIRTUAL_SITE_MEMO` (default on
// inside it); `CRATONVM_JNI_JIT_VIRTUAL_SITE_MEMO=0` turns only this compiled half
// off. Collector-independent and mode-independent code.

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of a compiled virtual call-site hit.
pub(crate) const JNI_JIT_VSITE_HIT: &str = "jit_dispatch_virtual.jni-vsite-hit";

/// One compiled virtual call-site row: the key, the stamps read when the probe
/// was armed, and what a hit needs.
pub(in crate::vm::vm_exec) struct JniJitVSiteRow {
    pub(in crate::vm::vm_exec) vm_identity: usize,
    pub(in crate::vm::vm_exec) caller: u32,
    pub(in crate::vm::vm_exec) site: u64,
    pub(in crate::vm::vm_exec) generation: u64,
    pub(in crate::vm::vm_exec) redefinitions: u64,
    pub(in crate::vm::vm_exec) class_epoch: u64,
    pub(in crate::vm::vm_exec) hit: JniJitVSiteHit,
}

/// What a compiled virtual call-site hit hands its tail.
#[derive(Clone)]
pub(crate) struct JniJitVSiteHit {
    /// The receiver class the fill dispatched on: the door's key, and the only
    /// receiver class the row serves.
    pub(crate) receiver_class_id: ClassId,
    /// The receiver class's name (the blind arm's S111r12 rescue asks it).
    pub(crate) receiver_name: std::sync::Arc<str>,
    /// The (site, receiver class) dispatch target's class name (the MIC arms'
    /// S111r12 rescue asks it).
    pub(crate) dispatch_class: std::sync::Arc<str>,
    /// The site's constant-pool names, re-checked on every hit.
    pub(crate) class_name: std::sync::Arc<str>,
    pub(crate) method_name: std::sync::Arc<str>,
    pub(crate) descriptor: std::sync::Arc<str>,
}

/// What [`jni_jit_vsite_probe_arm`] hands [`jni_jit_vsite_probe_finish`]: the
/// i9/cs probe's save and the class-definition epoch read before the call.
#[must_use]
pub(crate) struct JniJitVSiteProbeSave {
    pub(in crate::vm::vm_exec) probe: JniSiteProbeSave,
    pub(in crate::vm::vm_exec) class_epoch: u64,
}

/// The site word of a compiled `invokevirtual` / `invokeinterface` (its
/// `JitInvokeInfo` address). Top bits `00` with bit 61 set keep it apart from the
/// interpreter's static (`01`) and virtual (`11`) words and the compiled static
/// word (`10`) for every user-space address (below 2^48).
#[inline]
pub(crate) fn jni_jit_vsite(info_ptr: usize) -> u64 {
    // Cast: an address widened into the site word.
    (1u64 << 61) ^ (info_ptr as u64)
}

/// All four switches: the door memo (opt-in), the call-site memo and the
/// interpreter's virtual half (default ON inside it), and this compiled half
/// (`CRATONVM_JNI_JIT_VIRTUAL_SITE_MEMO`, default ON inside them, `=0` off),
/// each read once per thread.
#[inline]
pub(in crate::vm::vm_exec) fn jni_jit_vsite_memo_armed(memo: &mut NameOverrideMemo) -> bool {
    if !jni_vsite_memo_armed(memo) {
        return false;
    }
    if memo.jit_vsite_state == 0 {
        memo.jit_vsite_state = {
            1
        };
    }
    memo.jit_vsite_state == 1
}

/// Classes and names a compiled virtual row is never kept for: the interpreter
/// rows' refusals ([`jni_vsite_admits`]) plus the arms only the by-name walk has
/// (the ZIP redefine waiver, which reads the receiver object, and the array
/// dispatch of an array-typed site).
pub(in crate::vm::vm_exec) fn jni_jit_vsite_admits(
    registry: &cratonvm_native_api::NativeMethodRegistry,
    class_name: &str,
    method_name: &str,
) -> bool {
    jni_vsite_admits(class_name, method_name)
        && !registry.has_app_workaround_prefix(
            cratonvm_native_api::AppWorkaroundKind::SkipJniMemo,
            class_name,
        )
        && !class_name.starts_with('[')
        && !class_name.starts_with("java/util/zip/")
        && !class_name.starts_with("java/util/jar/")
}

/// The row for `(vm, caller, site)` with the site's names `verify`, when it
/// holds `receiver` and its stamps are `stamps()` (generation, redefinitions,
/// class-definition epoch). A row on older stamps is cleared; a row for another
/// receiver class is kept (the slow path decides whether to refill it).
/// `receiver` is `None` for a statically bound (kind-1) site, whose row is not
/// keyed by the receiver's class.
pub(in crate::vm::vm_exec) fn jni_jit_vsite_memo_serve(
    memo: &mut NameOverrideMemo,
    vm_identity: usize,
    caller: u32,
    site: u64,
    verify: (&str, &str, &str),
    receiver: Option<u32>,
    stamps: impl FnOnce() -> (u64, u64, u64),
) -> Option<JniJitVSiteHit> {
    let at = jni_site_memo_index(caller, site);
    let rows = &mut memo.site.as_mut()?.jrows;
    let (current, same_receiver) = match rows.get(at) {
        Some(Some(row))
            if row.vm_identity == vm_identity
                && row.caller == caller
                && row.site == site
                && &*row.hit.class_name == verify.0
                && &*row.hit.method_name == verify.1
                && &*row.hit.descriptor == verify.2 =>
        {
            (
                (row.generation, row.redefinitions, row.class_epoch) == stamps(),
                receiver.is_none_or(|receiver| row.hit.receiver_class_id.as_u32() == receiver),
            )
        }
        _ => return None,
    };
    if !current {
        rows[at] = None;
        return None;
    }
    if !same_receiver {
        return None;
    }
    rows[at].as_ref().map(|row| row.hit.clone())
}

pub(in crate::vm::vm_exec) fn jni_jit_vsite_memo_store(memo: &mut NameOverrideMemo, row: JniJitVSiteRow) {
    let rows = &mut memo.site.get_or_insert_with(Box::default).jrows;
    if rows.len() < JNI_SITE_MEMO_ROWS {
        rows.resize_with(JNI_SITE_MEMO_ROWS, || None);
    }
    let at = jni_site_memo_index(row.caller, row.site);
    rows[at] = Some(row);
}

/// The hit half, asked by each of the three compiled virtual tails before its
/// slow work: `Some` when the switches are on and a current row for the site
/// holds `receiver_class_id`. The caller then calls
/// [`jni_jit_vsite_memo_call`] (`helpers.rs` `jit_jni_vsite_call`).
#[inline]
pub(crate) fn jni_jit_vsite_memo_lookup(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    site: u64,
    verify: (&str, &str, &str),
    receiver_class_id: ClassId,
) -> Option<JniJitVSiteHit> {
    if !jni_jit_vsite_memo_armed(&mut thread.name_override_memo) {
        return None;
    }
    if thread.name_override_memo.site.is_none() {
        return None;
    }
    #[cfg(feature = "experimental-aot")]
    if crate::native::builtins::aot::is_aot_training() {
        return None;
    }
    let hit = jni_jit_vsite_memo_serve(
        &mut thread.name_override_memo,
        shared.vm_identity,
        caller.as_u32(),
        site,
        verify,
        Some(receiver_class_id.as_u32()),
        || jni_jit_row_stamps(shared),
    )?;
    // The engagement count (`CRATONVM_DBG_DISPATCH_TALLY`).
    dbg_dispatch_tally(JNI_JIT_VSITE_HIT, "", &hit.method_name, &hit.descriptor);
    Some(hit)
}

/// A compiled row's three stamps, as read now: the JNI table's registration
/// generation, the redefinition count, and the class-definition epoch of the
/// VM's class store.
#[inline]
pub(in crate::vm::vm_exec) fn jni_jit_row_stamps(shared: &SharedVm) -> (u64, u64, u64) {
    (
        shared
            .natives
            .jni_native_generation
            .load(std::sync::atomic::Ordering::Acquire),
        cratonvm_classloading::class_redefinition_count(),
        cratonvm_classloading::store_epochs(shared.jit.class_layout_domain).class_definition_epoch(),
    )
}

// ───────────── gci i12/mf: the compiled statically bound (kind-1) JNI call-site rows ─────────────
//
// The residual the i8/dm page narrowed: a compiled `invokespecial`, or an `invokevirtual` of a
// `private` / `final` method the compiler devirtualized (`JitInvokeInfo::invoke_kind == 1`), of a
// table-bound JNI native -- the usual shape of a JNI library's `private native` helpers -- took
// `jit_invoke_dispatch_body`'s `1 =>` arm every call: the bytecode-callee template's refusal, the
// owner's resolution (`jit_substituted_owner` / `jit_bridged_owner_entry`), the woven-body question,
// then `invoke_special_shared_on_class` -> `invoke_special_shared_impl` (the registry probe on the
// constant-pool name, the owner's initialization check, the walk to the declaring class) -> the door
// on the DECLARING class with no retarget, where the i8/dm door memo answered.
//
// A row lives in the same per-thread table as the virtual rows (`jrows`), under its own site word
// ([`jni_jit_ssite`]) and the same sub-switch (`CRATONVM_JNI_JIT_VIRTUAL_SITE_MEMO`: no ninth latch
// byte), keyed `(vm, caller, info address)` with the names re-checked and NO receiver key (the target
// is statically bound: the receiver's class cannot change it). Its `receiver_class_id` field holds the
// DOOR's class (the declaring class). A hit calls `invoke_on_class_shared_no_retarget` on it -- the
// call `invoke_special_shared_impl` ends in -- where the door memo answers.
//
// FILL around the arm's `invoke_special_shared_on_class` call only (a resolved owner, no woven body),
// under the i9/cs probe, when: the call completed; the door marked exactly the class
// `find_method_recursive(owner, ..)` declares the method in, as an instance `ACC_NATIVE` method; no
// class was ever redefined (`any_class_redefined`: the woven-body question and the registry probe's
// yield rules read agent state and the receiver object only after a redefinition; a hit re-asks it,
// one relaxed load); the three stamps did not move; the owner is initialized (the hit skips the
// arm's initialization check; initialization is monotone); the door row is an instance native on the
// same stamps; and the census admits the constant-pool class, the owner and the declaring class
// ([`jni_jit_vsite_admits`]). What the hit skips: the template refusal (no template for a native),
// the owner's resolution (a function of the caller's loader and the class-definition epoch), the
// woven question (refused while anything is redefined), the registry probe on the constant-pool name
// (a registry row cannot appear after boot, and the door's JNI arm answered the declaring class's
// key; its yield predicates read redefinition state, refused), the initialization (proved at the
// fill) and the walk (a function of the classes, under the epoch and the redefinition count).

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of a compiled kind-1 call-site hit.
pub(crate) const JNI_JIT_SSITE_HIT: &str = "jit_dispatch_special.jni-site-hit";

/// The site word of a compiled statically bound (kind-1) call (its
/// `JitInvokeInfo` address). Top bits `0011` keep it apart from the compiled
/// virtual word (`0010`) and the three others (`01`, `10`, `11`) for every
/// user-space address.
#[inline]
pub(crate) fn jni_jit_ssite(info_ptr: usize) -> u64 {
    // Cast: an address widened into the site word.
    (3u64 << 60) ^ (info_ptr as u64)
}

/// The hit half for a kind-1 site, asked by `jit_invoke_dispatch_body`'s `1 =>`
/// arm after the bytecode-callee template declined: `Some` when the switches
/// are on, nothing was ever redefined and a current row exists.
#[inline]
pub(crate) fn jni_jit_special_memo_lookup(
    shared: &SharedVm,
    thread: &mut JvmThread,
    caller: ClassId,
    site: u64,
    verify: (&str, &str, &str),
) -> Option<JniJitVSiteHit> {
    if !jni_jit_vsite_memo_armed(&mut thread.name_override_memo) {
        return None;
    }
    if thread.name_override_memo.site.is_none() {
        return None;
    }
    #[cfg(feature = "experimental-aot")]
    if crate::native::builtins::aot::is_aot_training() {
        return None;
    }
    if crate::classloading::any_class_redefined() {
        return None;
    }
    let hit = jni_jit_vsite_memo_serve(
        &mut thread.name_override_memo,
        shared.vm_identity,
        caller.as_u32(),
        site,
        verify,
        None,
        || jni_jit_row_stamps(shared),
    )?;
    // The engagement count (`CRATONVM_DBG_DISPATCH_TALLY`).
    dbg_dispatch_tally(JNI_JIT_SSITE_HIT, "", &hit.method_name, &hit.descriptor);
    Some(hit)
}

/// A kind-1 hit's dispatch: the door on the declaring class with no retarget
/// (the call `invoke_special_shared_impl` ends in), where the door memo answers;
/// should the door's row be gone, the door takes its full path on the same
/// class, which ends in the same arm.
pub(crate) fn jni_jit_special_memo_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    hit: &JniJitVSiteHit,
    args: &[Value],
) -> MethodCallResult {
    invoke_on_class_shared_no_retarget(
        shared,
        thread,
        hit.receiver_class_id,
        &hit.method_name,
        &hit.descriptor,
        args,
    )
}

/// Close the probe [`jni_jit_vsite_probe_arm`] opened around the kind-1 arm's
/// `invoke_special_shared_on_class(owner, ..)` and store the site's row when the
/// call provably ended in the door's JNI arm for the declaring class (see the
/// block comment above). `completed` is the call's `is_ok()`.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn jni_jit_special_probe_finish(
    shared: &SharedVm,
    thread: &mut JvmThread,
    save: JniJitVSiteProbeSave,
    caller: ClassId,
    site: u64,
    owner: ClassId,
    cp_class_name: &str,
    method_name: &str,
    descriptor: &str,
    completed: bool,
) {
    let JniJitVSiteProbeSave { probe: save, class_epoch } = save;
    let JniSiteProbeSave {
        prev,
        generation,
        redefinitions,
    } = save;
    let probe = std::mem::replace(
        &mut thread
            .name_override_memo
            .site
            .get_or_insert_with(Box::default)
            .probe,
        prev,
    );
    let Some(probe) = probe else {
        return;
    };
    let door = match probe.door {
        Some(door) if completed && !probe.conflict && door != 0 => door,
        _ => return,
    };
    if probe.key != jni_site_probe_key(method_name, descriptor)
        || !jni_jit_vsite_admits(&shared.natives.native_methods, cp_class_name, method_name)
        || crate::classloading::any_class_redefined()
    {
        return;
    }
    let door_name = {
        let cm = shared.classes.class_manager.read();
        let declares = crate::classloading::find_method_recursive(
            owner,
            method_name,
            descriptor,
            &cm.class_store,
        )
        .is_some_and(|(method, declaring)| {
            declaring.as_u32() == door && method.is_native() && !method.is_static()
        });
        match (
            declares,
            cm.get_class(owner),
            cm.get_class(ClassId::new(door)),
        ) {
            (true, Some(owner_class), Some(door_class))
                if jni_jit_vsite_admits(
                    &shared.natives.native_methods,
                    &owner_class.name,
                    method_name,
                ) && jni_jit_vsite_admits(
                    &shared.natives.native_methods,
                    &door_class.name,
                    method_name,
                ) =>
            {
                door_class.name.clone()
            }
            _ => return,
        }
    };
    if jni_jit_row_stamps(shared) != (generation, redefinitions, class_epoch) {
        return;
    }
    // The hit skips the arm's initialization of the owner (monotone).
    if !super::is_class_initialized_via_manager(shared, owner) {
        return;
    }
    let door_row_instance_and_current = jni_dispatch_memo_find(
        &thread.name_override_memo,
        shared.vm_identity,
        door,
        method_name,
        descriptor,
    )
    .is_some_and(|row| {
        row.generation == generation && row.redefinitions == redefinitions && !row.hit.is_static
    });
    if !door_row_instance_and_current {
        return;
    }
    jni_jit_vsite_memo_store(
        &mut thread.name_override_memo,
        JniJitVSiteRow {
            vm_identity: shared.vm_identity,
            caller: caller.as_u32(),
            site,
            generation,
            redefinitions,
            class_epoch,
            hit: JniJitVSiteHit {
                receiver_class_id: ClassId::new(door),
                receiver_name: std::sync::Arc::clone(&door_name),
                dispatch_class: door_name,
                class_name: std::sync::Arc::from(cp_class_name),
                method_name: std::sync::Arc::from(method_name),
                descriptor: std::sync::Arc::from(descriptor),
            },
        },
    );
}

/// A compiled virtual call-site hit's dispatch: the walk's tail entry on the
/// receiver's class (`invoke_by_class_id_shared`), where the door memo answers;
/// should the door's row be gone, the door takes its full path on the same
/// class, which ends in the same arm. With `keep_current` (the tail's S111r12
/// rescue may resend `args`), `args` is pinned across the call and re-read from
/// the pins when it fails, exactly as [`invoke_or_native_keeping_args_current`].
pub(crate) fn jni_jit_vsite_memo_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    hit: &JniJitVSiteHit,
    args: &mut [Value],
    keep_current: bool,
) -> MethodCallResult {
    if !keep_current || shared.jit.jit_cache.by_name_tail_unpins_args() {
        return invoke_by_class_id_shared(
            shared,
            thread,
            hit.receiver_class_id,
            &hit.method_name,
            &hit.descriptor,
            args,
        );
    }
    let roots = PinnedDispatchArgs::new(thread, args);
    let result = invoke_by_class_id_shared(
        shared,
        thread,
        hit.receiver_class_id,
        &hit.method_name,
        &hit.descriptor,
        args,
    );
    if result.is_err() {
        roots.refresh(args);
    }
    drop(roots);
    result
}

/// Arm the fill probe around a compiled virtual tail's by-name call. `None`
/// when this half is off (nothing to finish). The class-definition epoch is
/// read before the i9/cs probe's own stamps, so a definition during the call
/// leaves the row unstored.
pub(crate) fn jni_jit_vsite_probe_arm(
    shared: &SharedVm,
    thread: &mut JvmThread,
    method_name: &str,
    descriptor: &str,
) -> Option<JniJitVSiteProbeSave> {
    if !jni_jit_vsite_memo_armed(&mut thread.name_override_memo) {
        return None;
    }
    let class_epoch =
        cratonvm_classloading::store_epochs(shared.jit.class_layout_domain).class_definition_epoch();
    let probe = jni_site_probe_arm(shared, thread, method_name, descriptor)?;
    Some(JniJitVSiteProbeSave { probe, class_epoch })
}

/// Close the probe [`jni_jit_vsite_probe_arm`] opened (restoring the displaced
/// one) and store the site's row when the tail provably ended in the door's JNI
/// arm for the receiver's own class (see the block comment above). `eligible`
/// carries what only the caller knows: the call completed, and the receiver is
/// a plain object whose dispatch target had no selection error.
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn jni_jit_vsite_probe_finish(
    shared: &SharedVm,
    thread: &mut JvmThread,
    save: JniJitVSiteProbeSave,
    caller: ClassId,
    site: u64,
    receiver_class_id: ClassId,
    dispatch_class: &str,
    cp_class_name: &str,
    method_name: &str,
    descriptor: &str,
    eligible: bool,
) {
    let JniJitVSiteProbeSave { probe: save, class_epoch } = save;
    let JniSiteProbeSave {
        prev,
        generation,
        redefinitions,
    } = save;
    let probe = std::mem::replace(
        &mut thread
            .name_override_memo
            .site
            .get_or_insert_with(Box::default)
            .probe,
        prev,
    );
    let Some(probe) = probe else {
        return;
    };
    let door = match probe.door {
        Some(door) if eligible && !probe.conflict && door != 0 => door,
        _ => return,
    };
    if door != receiver_class_id.as_u32()
        || probe.key != jni_site_probe_key(method_name, descriptor)
        || !jni_jit_vsite_admits(&shared.natives.native_methods, cp_class_name, method_name)
        || !jni_jit_vsite_admits(&shared.natives.native_methods, dispatch_class, method_name)
        || shared.classes.is_lambda_proxy_class(receiver_class_id)
    {
        return;
    }
    let receiver_name = {
        let cm = shared.classes.class_manager.read();
        match cm.get_class(receiver_class_id) {
            Some(class)
                if !class.is_interface()
                    && !class.is_abstract()
                    && jni_jit_vsite_admits(
                        &shared.natives.native_methods,
                        &class.name,
                        method_name,
                    ) =>
            {
                class.name.clone()
            }
            _ => return,
        }
    };
    if shared
        .natives
        .jni_native_generation
        .load(std::sync::atomic::Ordering::Acquire)
        != generation
        || cratonvm_classloading::class_redefinition_count() != redefinitions
        || cratonvm_classloading::store_epochs(shared.jit.class_layout_domain)
            .class_definition_epoch()
            != class_epoch
    {
        return;
    }
    let door_row_instance_and_current = jni_dispatch_memo_find(
        &thread.name_override_memo,
        shared.vm_identity,
        door,
        method_name,
        descriptor,
    )
    .is_some_and(|row| {
        row.generation == generation && row.redefinitions == redefinitions && !row.hit.is_static
    });
    if !door_row_instance_and_current {
        return;
    }
    jni_jit_vsite_memo_store(
        &mut thread.name_override_memo,
        JniJitVSiteRow {
            vm_identity: shared.vm_identity,
            caller: caller.as_u32(),
            site,
            generation,
            redefinitions,
            class_epoch,
            hit: JniJitVSiteHit {
                receiver_class_id,
                receiver_name,
                dispatch_class: std::sync::Arc::from(dispatch_class),
                class_name: std::sync::Arc::from(cp_class_name),
                method_name: std::sync::Arc::from(method_name),
                descriptor: std::sync::Arc::from(descriptor),
            },
        },
    );
}


/// The row a triple maps to: a function of the class id, both lengths and a
/// few bytes, so computing it never walks a whole name.
#[inline]
pub(in crate::vm::vm_exec) fn name_override_memo_index(declaring_id: u32, method_name: &str, descriptor: &str) -> usize {
    let m = method_name.as_bytes();
    let d = descriptor.as_bytes();
    let byte = |s: &[u8], i: usize| u64::from(s.get(i).copied().unwrap_or(0));
    // Cast: lengths widened into the hash; truncation is harmless there.
    let mut h = u64::from(declaring_id)
        ^ ((m.len() as u64) << 16)
        ^ ((d.len() as u64) << 24)
        ^ (byte(m, 0) << 8)
        ^ (byte(m, m.len().wrapping_sub(1)) << 32)
        ^ (byte(d, d.len() / 2) << 40)
        ^ (byte(d, d.len().wrapping_sub(2)) << 48);
    h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // Cast: the top bits of a 64-bit hash, reduced below the row count.
    ((h >> 40) as usize) % NAME_OVERRIDE_MEMO_ROWS
}

// nio21 wave 18, lane L2 (`r21-by-name-invoke-ladder-is-paid-per-call-CLOSED-20261012.md`):
// the `name_override` disjunction of [`invoke_on_class_shared_inner`]'s
// `check_override` header, remembered per thread.
//
// That chain (~300 disjuncts and ~45 `is_*_native_override` helpers) is a
// pure function of the declaring class's NAME, the method name and the
// descriptor: it reads no receiver, argument, VM state or class-manager
// data, and every helper it calls takes exactly those three strings (plus
// process-latched switches). It was evaluated in full on every by-name call
// that reached a non-native method -- under `--real-jdk` too, where it only
// feeds the strict-refusal census -- and was most of the function's
// `eq<str>` / `memcmp` time in the w16 profiles (`rlock`, `hdr`, `ite`).
//
// Keyed by `(vm_identity, declaring class id)` -- a class id is per VM, never
// recycled, and names one class name for life (a redefinition keeps the
// name) -- plus the method name and descriptor compared IN FULL on a hit.
// A collision evicts, which only costs the next call a full evaluation.
// Nothing is invalidated: no input can change. The one side effect inside
// the chain is a `tracing::debug!` line for `System.loadLibrary`, now logged
// once per thread and triple instead of per call.
//
// Kill switch `CRATONVM_BY_NAME_OVERRIDE_MEMO=0` (read once per thread).
// JIT round 23 wave 2f (lane L1): out of line, so `invoke_on_class_shared_inner`'s
// frame (live under every call it makes) does not reserve this lookup's locals.
#[inline(never)]
pub(in crate::vm::vm_exec) fn name_override_memoized(
    memo: &mut NameOverrideMemo,
    vm_identity: usize,
    declaring_id: ClassId,
    method_name: &str,
    descriptor: &str,
    compute: impl FnOnce() -> bool,
) -> bool {
    let cid = declaring_id.as_u32();
    let at = name_override_memo_index(cid, method_name, descriptor);
    let hit = (|| {
        if memo.state == 0 {
            memo.state =
                {
                    1
                };
        }
        if memo.state != 1 {
            return Err(false);
        }
        match memo.rows.get(at) {
            Some(Some((vm, row_cid, m, d, answer)))
                if *vm == vm_identity
                    && *row_cid == cid
                    && &**m == method_name
                    && &**d == descriptor =>
            {
                Ok(*answer)
            }
            _ => Err(true),
        }
    })();
    let record = match hit {
        Ok(answer) => return answer,
        Err(record) => record,
    };
    // The chain's helpers never call back here, and they borrow nothing of
    // the thread, so the table can be held across them.
    let answer = compute();
    if record {
        if memo.rows.len() < NAME_OVERRIDE_MEMO_ROWS {
            memo.rows.resize_with(NAME_OVERRIDE_MEMO_ROWS, || None);
        }
        memo.rows[at] = Some((
            vm_identity,
            cid,
            Box::from(method_name),
            Box::from(descriptor),
            answer,
        ));
    }
    answer
}


/// `writeReplace()` called on `receiver`, a `FLAG_SERIALIZABLE` lambda proxy:
/// the `SerializedLambda` its spun method returns
/// (`lang_class::lambda_proxy_write_replace`), or `None` for any other method
/// or with `CRATONVM_MH_LAMBDA_PROXY_MEMBERS=0`. Interpreter round i2 wave 61,
/// lane L4 (`L4W61LambdaProxyMemberHandles`).
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn lambda_proxy_write_replace_call(
    ctx: &mut NativeContextImpl<'_>,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
) -> Option<MethodCallResult> {
    if method_name != "writeReplace"
        || descriptor != "()Ljava/lang/Object;"
        || !cratonvm_types::flags().natives.mh_lambda_proxy_members
    {
        return None;
    }
    cratonvm_native_builtins::lang_class::lambda_proxy_write_replace(ctx, receiver)
}

pub(in crate::vm::vm_exec) fn object_serialization_hook_neutral_result(
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<Option<Value>> {
    match (method_name, descriptor) {
        ("readObject", "(Ljava/io/ObjectInputStream;)V")
        | ("readObjectNoData", "()V")
        | ("writeObject", "(Ljava/io/ObjectOutputStream;)V") => Some(None),
        ("readResolve", "()Ljava/lang/Object;") | ("writeReplace", "()Ljava/lang/Object;") => {
            Some(Some(args.first().copied().unwrap_or(Value::Object(None))))
        }
        _ => None,
    }
}

/// A lambda-proxy SAM call `NativeContextImpl::invoke_virtual` admitted
/// (`lambda_accepts_descriptor`), run by the interpreter's dispatcher
/// `try_lambda_dispatch` — the one copy bytecode, the JIT and
/// `invoke_on_class_shared_inner` use. `args` excludes the receiver.
///
/// Every implementation kind comes here: wave 16 (lane L3) moved
/// `REF_invokeSpecial` and the field kinds, wave 17 (lane L4) the rest and
/// deleted the native per-kind copy. Differences the collapse settled in the
/// interpreter's favour: its `NoSuchMethodError` retry fires only when the
/// error names the receiver's class (HIB-CV-31), where the native copy's
/// fired for any error naming the implementation member; its constructor
/// arm resolves the owner loader-faithfully (the native copy's used the
/// global `load_class`); a static implementation is served by the cached
/// owner (`lambda_global_impl_owner`) instead of a by-name lookup per call.
///
/// The native copy's one extra behaviour is kept around it: a
/// `NoSuchMethodError` for a missing serialization hook answers the neutral
/// result ([`object_serialization_hook_neutral_result`]) over the
/// implementation's arguments (captures, then `args`), coerced to the SAM's
/// return as the native copy did. The receiver and `args` are pinned across
/// the dispatch for that read.
#[allow(clippy::too_many_arguments)]
pub(in crate::vm::vm_exec) fn native_callback_lambda_via_interpreter(
    shared: &SharedVm,
    thread: &mut JvmThread,
    receiver: ObjectRef,
    receiver_class_id: ClassId,
    lcs: &crate::classloading::resolution::LambdaCallSite,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(receiver);
    push_object_arg_pins(thread, args);
    let dispatched = crate::runtime::interpreter::try_lambda_dispatch(
        shared,
        thread,
        receiver,
        receiver_class_id,
        method_name,
        descriptor,
        args,
    );
    let receiver = thread
        .native_pin_roots
        .get(pin_base)
        .copied()
        .unwrap_or(receiver);
    let fresh = args_refreshed_if_moved(thread, pin_base + 1, args);
    thread.native_pin_roots.truncate(pin_base);
    let args = fresh.as_deref().unwrap_or(args);
    match dispatched {
        Ok(Some(result)) => Ok(result),
        // Unreachable for an admitted call (the dispatcher asks the same
        // predicate of the same call site); refused rather than guessed.
        Ok(None) => Err(VmError::Internal {
            message: format!(
                "invoke_virtual: lambda dispatch declined {}.{method_name}{descriptor}",
                lcs.functional_interface
            ),
        }
        .into()),
        Err(MethodCallFailed::InternalError(VmError::Linkage(
            LinkageError::NoSuchMethodError {
                class_name,
                method_name: missing,
                method_descriptor,
            },
        ))) => {
            // Wave 86 (lane L4): an accidentally serializable proxy's hostile
            // pair throws instead of answering the neutral hook result.
            if !lcs.serializable_flag && (missing == "writeObject" || missing == "readObject") {
                let mut ctx = NativeContextImpl {
                    shared,
                    thread: &mut *thread,
                };
                if let Some(failure) =
                    cratonvm_native_builtins::lang_class::lambda_proxy_hostile_serialization_call(
                        &mut ctx,
                        receiver,
                        &missing,
                        &method_descriptor,
                    )
                {
                    return Err(failure);
                }
            }
            let first = if lcs.capture_types.is_empty() {
                args.first().copied()
            } else {
                Some(shared.mem.heap.get_field(receiver, 0))
            };
            let impl_args: &[Value] = match &first {
                Some(value) => std::slice::from_ref(value),
                None => &[],
            };
            match object_serialization_hook_neutral_result(&missing, &method_descriptor, impl_args)
            {
                Some(result) => {
                    let sam_ret = crate::runtime::interpreter::descriptor_return_ref(descriptor);
                    let impl_desc = lcs.impl_handle.descriptor.clone();
                    let impl_ret = crate::runtime::interpreter::descriptor_return_ref(&impl_desc);
                    crate::runtime::interpreter::coerce_return(
                        shared, thread, &sam_ret, &impl_ret, result,
                    )
                }
                None => Err(MethodCallFailed::InternalError(VmError::Linkage(
                    LinkageError::NoSuchMethodError {
                        class_name,
                        method_name: missing,
                        method_descriptor,
                    },
                ))),
            }
        }
        Err(err) => Err(err),
    }
}

/// SBR wave 5 (lane JSSE): `true` when this VM runs `--real-jdk` AND the JSSE
/// family's shadows are retired (`RETIRED_SHADOW_SBR_JSSE_TRIPLES`, keyed on
/// `SSLContext.getInstance` like `t27_tls::real_jsse_family`). Then every
/// `sun/security/ssl/*Impl` object is the JDK's own, running its own bytecode,
/// and the dispatch doors' by-name arms that routed such objects to the
/// natives on the public `javax/net/ssl` classes (here and in
/// `interpreter::invoke`) are closed. Keyed on the row, not on the mode alone,
/// so `CRATONVM_UNRETIRE_NATIVE_SHADOW` reopens them with the family;
/// `--compatible` never closes them. The retirement half is a pure function of
/// the build and the environment, so it is computed once; the mode is per VM.
pub(crate) fn jsse_impl_doors_closed(shared: &SharedVm) -> bool {
    static RETIRED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    shared
        .natives
        .native_methods
        .aligned(cratonvm_types::HotSpotAlignment::JdkJsse)
        && *RETIRED.get_or_init(|| {
            cratonvm_native_api::retired_shadow::triple_is_retired_shadow(
                "javax/net/ssl/SSLContext",
                "getInstance",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
            )
        })
}

/// [`invoke_on_class_shared`] for a STATIC method, from a by-name static tail
/// that resolved it ([`invoke_static_on_owner`], the settled arm of
/// [`invoke_static_settling`]): `args[0]` is an argument, never a receiver,
/// so there is no lambda-proxy receiver to serve and no retarget to judge
/// ([`OnClassKind::Static`]). Under `VmConfig::by_name_static_kind` off, the
/// call is [`invoke_on_class_shared`] exactly. `pub(crate)` for
/// `execute_invokestatic`'s fallback on an owner the site selected (gci
/// i10/cr, [`invokestatic_fallback_static_kind`]).
pub(crate) fn invoke_static_on_class_shared(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    let kind = if shared.config.by_name_static_kind {
        OnClassKind::Static
    } else {
        OnClassKind::Unknown
    };
    invoke_on_class_shared_inner(
        shared,
        thread,
        class_id,
        method_name,
        descriptor,
        args,
        kind,
        None,
    )
}

/// The `CRATONVM_DBG_DISPATCH_TALLY` label of an [`OnClassKind::Static`] call.
pub(in crate::vm::vm_exec) const ON_CLASS_STATIC_KIND: &str = "invoke_on_class_shared_inner.static-kind";

/// What the caller of [`invoke_on_class_shared_inner`] knows about the call
/// (the `ByNameKind` of stage 4 of
/// `docs/feature-designs/real-jdk-mode.md`,
/// interpreter round i1 wave 52, lane L4; it replaces the `no_retarget` bool).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::vm::vm_exec) enum OnClassKind {
    /// A caller that cannot tell (the public [`invoke_on_class_shared`] and
    /// its many callers): a lambda-proxy `args[0]` is served as a receiver,
    /// and an interface / abstract `class_id` is retargeted onto a concrete
    /// receiver unless a hierarchy walk finds the method static.
    Unknown,
    /// No retarget (`findSpecial`, the default-method super call, a bound JNI
    /// native): the method on `class_id` itself.
    NoRetarget,
    /// A static method the caller resolved ([`invoke_static_on_class_shared`]):
    /// no receiver arm and no retarget, without the walk.
    Static,
}

/// A class name copied out from under the class-manager guard without a heap
/// allocation (names up to 64 bytes live inline).
#[derive(Default)]
pub(in crate::vm::vm_exec) struct NameBuf(pub(in crate::vm::vm_exec) smallvec::SmallVec<[u8; 64]>);

impl NameBuf {
    pub(in crate::vm::vm_exec) fn new(name: &str) -> Self {
        Self(smallvec::SmallVec::from_slice(name.as_bytes()))
    }
}

impl std::ops::Deref for NameBuf {
    type Target = str;
    fn deref(&self) -> &str {
        // SAFETY: the bytes are a verbatim copy of a `&str` (see `new`).
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }
}

impl PartialEq<&str> for NameBuf {
    fn eq(&self, other: &&str) -> bool {
        **self == **other
    }
}

impl std::fmt::Display for NameBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

