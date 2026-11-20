#[allow(unused_imports)]
use super::*;

// ---------------------------------------------------------------------------
// Missing JNI functions (Phase 2 additions)
// ---------------------------------------------------------------------------

/// `AllocObject` (`init` `None`) or `NewObject*` (`init` the constructor's
/// id and its `jvalue`s) of a lambda's class, as HotSpot answers them: a NEW
/// instance of the hidden class, its `arg$N` capture fields the constructor's
/// arguments (`NewObject*`, the spun constructor only stores them) or their
/// default values (`AllocObject`). MEASURED, HotSpot 25.0.3 (a C library
/// built with MSVC, `L3W120JniLambdaNewObject.java`
/// and `.c`): `GetMethodID(lambdaClass, "<init>", "(Ljava/lang/String;I)V")`
/// finds it, `NewObject` / `NewObjectA` answer an instance of that class
/// whose `get()` returns the new captures, and `AllocObject` answers an
/// object. Before: `GetMethodID` raised `NoSuchMethodError`, `AllocObject`
/// answered null (the class store holds no lambda class).
///
/// Interpreter round i6 wave 120, lane L3, under
/// `CRATONVM_REFLECT_LAMBDA_PROXY_FACTS` (default ON since the i6 wave-126 flip, `=0` off). `None`: not a lambda
/// class (or the switch is off, or the id names another method), and the
/// caller goes on as before. A fresh object every call: the zero-capture
/// singleton cache of the `invokedynamic` path is neither read nor filled.
/// The captures are pinned across the allocation, which may collect.
pub(in crate::native::jni) fn lambda_proxy_jni_construct(
    clazz: JClass,
    init: Option<(JMethodID, *const JValue)>,
) -> Option<JObject> {
    if clazz == 0 || !cratonvm_types::flags().natives.reflect_lambda_proxy_facts {
        return None;
    }
    let class_id = jclass_class_id(clazz);
    with_jni_context(|shared, thread| {
        if !shared.classes.is_lambda_proxy_class(class_id) {
            return None;
        }
        let site = shared.classes.lambda_call_site_for(class_id)?;
        let fields = crate::runtime::lambda_proxy::lambda_proxy_fields(&site);
        let init_descriptor = format!(
            "({})V",
            fields.iter().map(|(_, d)| d.as_str()).collect::<String>()
        );
        let default_of = |descriptor: &str| match descriptor.as_bytes().first() {
            Some(b'J') => Value::Long(0),
            Some(b'F') => Value::Float(0.0),
            Some(b'D') => Value::Double(0.0),
            Some(b'L' | b'[') => Value::Object(None),
            _ => Value::Int(0),
        };
        let mut captures: Vec<Value> = match init {
            Some((mid, args)) => {
                let (decl, index) = decode_method_id(mid);
                let (name, descriptor) = lambda_proxy_jni_method(shared, decl, index)?;
                if decl != class_id || &*name != "<init>" || *descriptor != *init_descriptor {
                    return None;
                }
                // Safety: a non-null `args` holds one `JValue` per parameter
                // of the constructor the id names (the JNI contract).
                unsafe { jni_upcall_args(&shared.mem.heap, None, args, &init_descriptor) }.to_vec()
            }
            None => Vec::new(),
        };
        // A null `args` (or `AllocObject`): the defaults.
        for (i, (_, descriptor)) in fields.iter().enumerate() {
            if i >= captures.len() {
                captures.push(default_of(descriptor));
            }
        }
        captures.truncate(fields.len());
        let pin_base = thread.native_pin_roots.len();
        let pins: Vec<Option<usize>> = captures
            .iter()
            .map(|c| match c {
                Value::Object(Some(o)) => {
                    thread.native_pin_roots.push(*o);
                    Some(thread.native_pin_roots.len() - 1)
                }
                _ => None,
            })
            .collect();
        let made = crate::runtime::interpreter::alloc_object_shared(
            shared,
            thread,
            class_id,
            captures.len(),
        );
        let obj = match made {
            Ok(obj) => obj,
            Err(failure) => {
                thread.native_pin_roots.truncate(pin_base);
                let _ = jni_surface_jdk_only(shared, thread, Err(failure));
                return Some(0);
            }
        };
        for (i, (value, pin)) in captures.iter().zip(&pins).enumerate() {
            let value = match pin {
                Some(at) => Value::Object(Some(thread.native_pin_roots[*at])),
                None => *value,
            };
            shared.mem.heap.set_field(obj, i, value);
        }
        thread.native_pin_roots.truncate(pin_base);
        Some(new_local_handle(obj))
    })
    .flatten()
}

// ---- Index 28: AllocObject ----
// Allocates a new Java object without calling any constructor.
pub(in crate::native::jni) extern "C" fn jni_alloc_object(_env: JNIEnv, clazz: JClass) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 {
        return 0;
    }
    // Interpreter round i6 wave 120, lane L3 (`CRATONVM_REFLECT_LAMBDA_PROXY_FACTS`):
    // a lambda's class, which the class store does not hold.
    if let Some(made) = lambda_proxy_jni_construct(clazz, None) {
        return made;
    }
    if !jni_alloc_object_prepare(jclass_class_id(clazz)) {
        return 0;
    }
    with_shared_vm(|shared| {
        let class_id = jclass_class_id(clazz);
        // `clazz` must resolve to a real class. If it does not (stale or
        // bogus handle), return null rather than allocating an object against
        // an unresolved/`ClassId(0)` class: a wrongly-classed object whose
        // declared field count is unknown corrupts every later field access.
        let num_fields = match shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
        {
            Some(n) => n,
            None => {
                tracing::warn!(
                    target: "cratonvm::jni",
                    clazz,
                    "AllocObject: JClass does not resolve to a loaded class"
                );
                return 0;
            }
        };
        let Some(obj) = jni_alloc_or_oom(shared, "an object (AllocObject)", |heap| {
            heap.try_alloc_object_full(class_id, num_fields)
        }) else {
            return 0;
        };
        // Typed field defaults, as a bytecode `new` and `Unsafe.allocateInstance`
        // (`NativeContextImpl::allocate_instance`) give them; AllocObject left
        // the allocator's raw slots (gc-common w4-g). No finalizer
        // registration: no constructor runs (see
        // `register_if_constructed_finalizable`; a later `<init>` call
        // registers, `jni_constructor_call_registers`).
        //
        // JIT round 21 wave 8, lane jni (`CRATONVM_NATIVE_ALLOC_NOTES_REFERENCES`,
        // default ON, read once per VM; page
        // `docs/feature-designs/real-jdk-mode.md`,
        // "JNI residual"): a `java.lang.ref` subclass is noted for discovery
        // HERE, at the allocation, as the interpreter's `new` notes it --
        // HotSpot discovers a `Reference` by its class whoever allocated it.
        // `AllocObject` + `CallNonvirtualVoidMethod(<init>)` never noted one,
        // so its referent was never cleared nor the reference enqueued.
        // `NewObject*` allocates through here and no longer notes a second
        // time (`register_if_constructed_finalizable`).
        crate::runtime::interpreter::init_no_constructor_instance(shared, obj, class_id);
        new_local_handle(obj)
    })
    .unwrap_or(0)
}

/// HotSpot's `alloc_object` (JNI `AllocObject`, and `NewObject*` through it)
/// before it allocates: `check_valid_for_instantiation` -- an interface, an
/// abstract or an array class is an `InstantiationException` naming it,
/// `java.lang.Class` an `IllegalAccessException` naming it -- then
/// `initialize` (JVMS §5.5: `<clinit>` runs before the constructor-less
/// instance exists, and before `NewObject`'s `<init>`; its failure is the
/// answer). `true`: allocate. `false`: the refusal or the initializer's
/// throwable is pending. Round 15 wave 4, lane jdkonly6
/// (`r15w4-jdkonly5-native-allocation-skips-class-init-FIXED-20260930.md` item 2):
/// `AllocObject` allocated anything and initialized nothing, so a native
/// library's `NewObject` of a fresh class ran its constructor before its
/// `<clinit>`. A class that does not resolve, and a thread with no JNI
/// context (it cannot run Java), keep the old answer. Kill switch
/// `CRATONVM_JNI_ALLOC_OBJECT_INITIALIZES=0` (read only when a refusal or an
/// initialization is due, never for an initialized, instantiable class).
pub(in crate::native::jni) fn jni_alloc_object_prepare(class_id: ClassId) -> bool {
    use cratonvm_types::access_flags::{ACC_ABSTRACT, ACC_INTERFACE};
    let probed = with_shared_vm(|shared| {
        let refusal = {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(class_id)?;
            let name: &str = &class.name;
            // `external_name()`: a hidden class keeps its `/0x<hex>` tail
            // (round 15 wave 7, N5 of `r15w6-jni6-review-findings`; identical
            // to the old `replace('/', ".")` for every other class).
            let external = || crate::runtime::resolve::selection::external_class_name(class);
            if name.starts_with('[')
                || class.access_flags.bits() & (ACC_INTERFACE | ACC_ABSTRACT) != 0
            {
                Some(("java/lang/InstantiationException", external()))
            } else if name == "java/lang/Class" {
                Some(("java/lang/IllegalAccessException", external()))
            } else {
                None
            }
        };
        Some((
            refusal,
            crate::vm::is_class_initialized_via_manager(shared, class_id),
        ))
    })
    .flatten();
    let Some((refusal, initialized)) = probed else {
        return true;
    };
    if refusal.is_none() && initialized {
        return true;
    }
    if let Some((exception_class, message)) = refusal {
        return with_jni_context(|shared, thread| {
            let failure = match crate::runtime::exceptions::create_exception_object(
                shared,
                thread,
                exception_class,
                Some(message.as_str()),
            ) {
                Ok(exc) => crate::error::MethodCallFailed::ExceptionThrown(exc),
                Err(failure) => failure,
            };
            let _ = jni_surface_jdk_only(shared, thread, Err(failure));
            false
        })
        .unwrap_or(true);
    }
    // `<clinit>` is Java: a foreign-attached thread must be a counted mutator
    // while it runs (as `jni_find_class_initialize`).
    let _fg = ForeignCallGuard::enter();
    with_jni_context(|shared, thread| {
        match crate::vm::ensure_class_initialized_shared(shared, thread, class_id) {
            Ok(()) => true,
            Err(err) => {
                let _ = jni_surface_jdk_only(shared, thread, Err(err));
                false
            }
        }
    })
    .unwrap_or(true)
}

/// Register `obj` for finalization if `class_id` overrides `finalize()`, for an
/// instance whose constructor is about to run. Returns whether it registered.
///
/// gc-common w4-g, the JNI half of
/// `handoff-w2d-finalizer-registration-native` (page
/// `common-d-finalizable-objects-from-native-allocation-paths-are-never-registered`).
/// HotSpot registers a finalizable instance when its `Object.<init>` returns
/// (`RegisterFinalizersAtInit`), so JNI `NewObject` / `NewObjectV` /
/// `NewObjectA` register and `AllocObject` -- no constructor -- does not. This
/// VM registers at allocation (`gc_and_alloc::init_new_instance`), so the one
/// JNI site is `NewObjectA` (the other two funnel into it), right after
/// `AllocObject`'s allocation and before `<init>`: the same point the
/// interpreter's `new` registers at. Exactly one registration: `AllocObject`
/// does not register, and nothing else registers a JNI-minted object.
pub(in crate::native::jni) fn register_if_constructed_finalizable(
    shared: &SharedVm,
    obj: ObjectRef,
    class_id: ClassId,
) -> bool {
    // gcg g4/rs: opt-in discovery at allocation, at the point a constructed
    // finalizable is registered. JIT round 21 wave 8, lane jni: `AllocObject`
    // (which every `NewObject*` allocates through) notes the reference itself
    // now (`init_no_constructor_instance`), so it is noted here only when that
    // switch is off -- one note per object either way (a second note would be
    // a second unbound row for the binder).
    if !shared.config.native_alloc_notes_references {
        crate::memory::ref_discovery::note_allocation(shared, obj, class_id);
    }
    let has_finalizer = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(class_id)
        .map_or(false, |c| c.has_finalizer);
    if has_finalizer {
        shared.register_finalizable(obj.as_ptr() as usize); // Cast: GC object pointer to address
    }
    has_finalizer
}

/// JIT round 21 wave 8, lane jni (`CRATONVM_JNI_CTOR_CALL_REGISTERS_FINALIZABLE`,
/// default ON, `=0` restores the defect; read only for a `<init>` call; page
/// `docs/feature-designs/real-jdk-mode.md`,
/// "JNI residual"): a JNI `Call[Nonvirtual]<T>Method*` whose `methodID` is a
/// constructor is the constructor call of an `AllocObject`-built instance
/// (the JNI spec's own `AllocObject` + `CallNonvirtualVoidMethod(<init>)`
/// idiom). HotSpot registers a finalizable instance when its `Object.<init>`
/// returns (`RegisterFinalizersAtInit`), however the constructor was reached;
/// this VM registers at the allocations a constructor always follows, and
/// `AllocObject` (rightly) is not one, so such an instance was never
/// finalized. The receiver is registered here, before its constructor runs --
/// as `NewObjectA` (`register_if_constructed_finalizable`), the interpreter's
/// `new` and the constructor `MethodHandle`'s `linkToSpecial` do. At most one
/// row per object (`ReferenceProcessor::discover_reference` ignores a second
/// registration of an active finalizer row), so a constructor chained on an
/// instance `NewObject` already registered is not finalized twice. Returns
/// whether the receiver was offered to the registration.
pub(in crate::native::jni) fn jni_constructor_call_registers(shared: &SharedVm, method_name: &str, receiver: ObjectRef) -> bool {
    // Read once per VM (`VmConfig::jni_ctor_call_registers_finalizable`; JIT
    // round 21 wave 13, review rev-native w13 item 5).
    if method_name != "<init>" || !shared.config.jni_ctor_call_registers_finalizable {
        return false;
    }
    crate::runtime::interpreter::register_if_finalizable_shared(shared, receiver);
    true
}

// ---- Index 30: NewObjectA ----
// Allocates a new Java object and invokes the constructor indicated by `mid`.
pub(in crate::native::jni) extern "C" fn jni_new_object_a(
    _env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    args: *const JValue,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 || mid == 0 {
        return 0;
    }
    // Become a counted mutator for the whole alloc+<init> on a foreign thread
    // (the alloc itself touches the heap, so it must run as a mutator, not while
    // idle/blocked). Inert for non-foreign threads.
    let _fg = ForeignCallGuard::enter();
    // Interpreter round i6 wave 120, lane L3 (`CRATONVM_REFLECT_LAMBDA_PROXY_FACTS`):
    // a lambda's class with its own constructor's id (`NewObject` and
    // `NewObjectV` reach here too).
    if let Some(made) = lambda_proxy_jni_construct(clazz, Some((mid, args))) {
        return made;
    }
    // First allocate the object.
    let obj_handle = jni_alloc_object(_env, clazz);
    if obj_handle == 0 {
        return 0;
    }
    // Where `jni_alloc_object` recorded the handle (gc-common w3-g). `<init>`
    // may allocate and a MOVING collection may relocate the new object; the
    // recorded slot is rewritten by `update_local_refs_after_gc`, `obj_handle`
    // is not. Returning `obj_handle` handed the native the from-space address
    // on exactly that path.
    let slot = recorded_local_slot_of(obj_handle);
    // Then call the constructor (<init>) on the allocated object.
    with_jni_context(|shared, thread| {
        let oref = jobject_to_obj(obj_handle)?;
        let (decl_class_id, method_index) = decode_method_id(mid);
        let (method_name, descriptor) = {
            let cm = shared.classes.class_manager.read();
            let class = cm.class_store.get(decl_class_id)?;
            let method = class.methods.get(method_index as usize)?;
            (method.name.clone(), method.descriptor.clone())
        };
        // JLS §12.6: a finalizable instance whose constructor runs is
        // registered; `AllocObject` alone (above) must not register. Only
        // once the `methodID`'s row was read (round 15 wave 7, N6 of
        // `r15w6-jni6-review-findings`): an unreadable `methodID` returns 0
        // above and runs no constructor, so it registers nothing.
        let registered = register_if_constructed_finalizable(shared, oref, jclass_class_id(clazz));
        // JIT round 21 wave 10, lane finalize: `oref` is current here (the
        // call below takes it as its receiver), so this epoch pins it.
        let epoch_before_call = shared.mem.address_epoch();
        // gci i15/jk: built in place (`jni_upcall_args`).
        let jvm_args = unsafe { jni_upcall_args(&shared.mem.heap, Some(oref), args, &descriptor) };
        // Real-JDK §7: the `<init>` dispatch decision is taken inside
        // `invoke_on_class_shared`. The constructor's return value is `void` and
        // has always been discarded; only a Real-JDK refusal is surfaced, so the
        // handle is still returned exactly as before and `Compatible` mode is
        // untouched. The caller sees the pending exception via
        // `ExceptionCheck`/`ExceptionOccurred`.
        //
        // Round 15 wave 6 (lane jni6; J2 of
        // `r15w4-jdkonly6-review-findings-CLOSED-20260930.md`, K1 of
        // `r15w5-jdkonly7-review-findings-CLOSED-20260930.md`): HotSpot's
        // `jni_NewObjectA` invokes exactly `methodID` (`JNI_NONVIRTUAL`), so a
        // superclass constructor's `methodID` runs THAT constructor on the new
        // `clazz` instance. Dispatching `<init>` by name on `clazz` ran the
        // subclass's same-descriptor constructor instead. `<init>` is never
        // re-targeted, so the declaring class names the body exactly; the
        // ordinary call (`methodID` declared on `clazz`) is unchanged.
        // `CRATONVM_JNI_NEW_OBJECT_EXACT_MID=0` restores the by-name lookup
        // (read only when the two classes differ).
        let clazz_id = jclass_class_id(clazz);
        let ctor_class_id = if decl_class_id != clazz_id
        {
            decl_class_id
        } else {
            clazz_id
        };
        let result = invoke_on_class_shared(
            shared,
            thread,
            ctor_class_id,
            &method_name,
            &descriptor,
            &jvm_args,
        );
        let threw = result.is_err();
        // JIT round 21 wave 10, lane finalize (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`):
        // a constructor that threw before `Object.<init>` returned leaves no
        // registration, as on HotSpot (decided by the unwinder's proof).
        if threw && registered {
            crate::runtime::interpreter::withdraw_unconstructed_finalizable_at_door(
                shared,
                oref,
                epoch_before_call,
            );
        }
        let _ = jni_surface_jdk_only(shared, thread, result);
        // Round 15 wave 4, lane jdkonly6: HotSpot's `jni_NewObjectA` runs the
        // constructor with `CHECK_NULL` -- a constructor that threw leaves its
        // exception pending AND answers NULL; this answered the half-built
        // object, which a native's `if (obj == NULL)` took for success.
        // `CRATONVM_JNI_NEW_OBJECT_NULL_ON_THROW=0` restores that (read only
        // when the constructor threw).
        if threw
        {
            return Some(0);
        }
        // The object's CURRENT address if a collection moved it during
        // `<init>`; `obj_handle` when there was no open frame to record it in.
        Some(slot.and_then(recorded_local_at).unwrap_or(obj_handle))
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Indices 155-158: SetStaticBoolean/Byte/Char/ShortField ----
pub(in crate::native::jni) extern "C" fn jni_set_static_boolean_field(
    _env: JNIEnv,
    _clazz: JClass,
    fid: JFieldID,
    val: JBoolean,
) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(fid, val as JInt);
}

pub(in crate::native::jni) extern "C" fn jni_set_static_byte_field(_env: JNIEnv, _clazz: JClass, fid: JFieldID, val: JByte) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(fid, val as JInt);
}

pub(in crate::native::jni) extern "C" fn jni_set_static_char_field(_env: JNIEnv, _clazz: JClass, fid: JFieldID, val: JChar) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(fid, val as JInt);
}

pub(in crate::native::jni) extern "C" fn jni_set_static_short_field(_env: JNIEnv, _clazz: JClass, fid: JFieldID, val: JShort) {
    let _fx = ForeignJniEntry::enter();
    set_static_int_raw(fid, val as JInt);
}

// ---- Index 163: NewString (UTF-16) ----
// Creates a java.lang.String from a UTF-16 char array.
pub(in crate::native::jni) extern "C" fn jni_new_string(_env: JNIEnv, unicode: *const JChar, len: JSize) -> JString {
    // gcd d10/j: as `jni_new_string_utf` (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    // gcj j2/jn2 (`gcj-j2jn2-jni-new-string-edges-are-not-hotspots-20261019`,
    // `CRATONVM_JNI_NEW_STRING_HOTSPOT_EDGES`, default ON on Generational,
    // `=0` kills): HotSpot's `jni_NewString` builds the String from `len`
    // units without looking at the pointer when `len` is 0 (`NewString(NULL,
    // 0)` is `""`), and a negative `len` reaches the byte-array allocation,
    // which leaves `NegativeArraySizeException: <len>` pending. This answered
    // NULL with nothing pending for both.
    if (len <= 0 || unicode.is_null()) && jni_new_string_hotspot_edges() {
        if len < 0 {
            raise_jni_negative_array_size(len);
            return 0;
        }
        if len == 0 {
            return with_shared_vm(|shared| {
                match jni_alloc_or_oom(shared, "a java/lang/String", |_| {
                    crate::vm::try_create_java_string_from_units(shared, &[])
                }) {
                    Some(obj) => new_local_handle(obj),
                    None => 0,
                }
            })
            .unwrap_or(0);
        }
    }
    if unicode.is_null() || len < 0 {
        return 0;
    }
    let chars: &[u16] = unsafe { std::slice::from_raw_parts(unicode, len as usize) };
    if len >= i32::MAX / 2
        && with_shared_vm(|shared| {
            jni_string_storage_refused(shared, chars.len(), chars.iter().all(|&u| u <= 0xff))
        }).unwrap_or(false)
    {
        return 0;
    }
    // gc-common w7-c: a NEW, un-interned String built from the exact UTF-16
    // units, allocated fallibly; see `jni_new_string_utf` for the leak and
    // identity defects of the interned form. Building from the units also
    // keeps a lone surrogate, which the old `from_utf16_lossy` replaced with
    // U+FFFD.
    with_shared_vm(|shared| {
        match jni_alloc_or_oom(shared, "a java/lang/String", |_| {
            crate::vm::try_create_java_string_from_units(shared, chars)
        }) {
            Some(obj) => new_local_handle(obj),
            None => 0,
        }
    })
    .unwrap_or(0)
}

/// HotSpot String::basic_create allocates byte[length] for Latin1 and
/// byte[length << 1] for UTF16. The signed shift can yield a negative array
/// length; that is NegativeArraySizeException, not a heap or VM-limit OOME.
pub(in crate::native::jni) fn jni_string_storage_refused(shared: &SharedVm, units: usize, latin1: bool) -> bool {
    let compact = shared.classes.latin1_coder_allowed();
    let length = if compact && latin1 { units as i32 } else { (units as i32).wrapping_mul(2) };
    if length < 0 {
        raise_jni_negative_array_size(length);
        true
    } else {
        jni_array_refused_for_its_length(shared, length, "a java/lang/String backing array")
    }
}

/// Count modified UTF8 code units without allocating the decoded payload.
/// Standard UTF8 is accepted by the existing decoder too (four bytes = two
/// surrogate units). For modified UTF8 each non-continuation byte starts one
/// UTF16 unit, including each separately encoded surrogate.
pub(in crate::native::jni) fn jni_utf8_storage_shape(bytes: &[u8]) -> (usize, bool) {
    if let Ok(s) = std::str::from_utf8(bytes) {
        (s.encode_utf16().count(), s.chars().all(|c| c as u32 <= 0xff))
    } else {
        let units = bytes.iter().filter(|&&b| b & 0xc0 != 0x80).count();
        let latin1 = bytes.iter().all(|&b| b < 0xc4);
        (units, latin1)
    }
}

/// gcj j2/jn2: does [`jni_new_string`] take HotSpot's two edges (a zero
/// length answers `""` whatever the pointer; a negative one leaves
/// `NegativeArraySizeException` pending)? `CRATONVM_JNI_NEW_STRING_HOTSPOT_EDGES`
/// (default ON, `=0` kills; latched per VM, `JniSwitches::new_string_hotspot_edges`)
/// on a Generational heap -- G1 and ZGC keep the old answer (out of the gcj
/// round's scope; their owners may drop the gate). Read only on those edges.
pub(in crate::native::jni) fn jni_new_string_hotspot_edges() -> bool {
    with_shared_vm(|shared| {
        jni_switches_of(shared).new_string_hotspot_edges
    })
    .unwrap_or(false)
}

// ---- Index 164: GetStringLength ----
pub(in crate::native::jni) extern "C" fn jni_get_string_length(_env: JNIEnv, str_obj: JString) -> JSize {
    // gcd d5/f: a leaf, as `GetStringUTFLength`.
    let _fx = ForeignJniEntry::enter_leaf(str_obj);
    if str_obj == 0 {
        return 0;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        // gci i13/jc (F12): the unit count of the exact units (one bulk copy
        // of the payload); it decoded the whole string into a Rust `String`
        // (UTF-16 -> UTF-8, lossy) and re-counted it per call. Same answer: a
        // lone surrogate was one U+FFFD, one unit. Not the O(1) form: that
        // needs the `(value, coder)` pair, whose only lock-free reader
        // (`vm_object::java_string_value_and_coder`) is private, and this is a
        // LEAF (`enter_leaf`: no lock a pausing thread can hold, so no class
        // manager lookup by name here). See the i13/jc report's cross-lane hunk.
        // gci i14/jh: the O(1) form, `vm_object::java_string_unit_count` (the
        // value array's length and the coder, behind the same shape guards;
        // lock-free, so still a leaf). Same answer as the unit count.
        let n = crate::vm::java_string_unit_count(&shared.mem.heap, oref)?;
        Some(n as JSize)
    })
    .flatten()
    .unwrap_or(0)
}

/// A real-JDK `java.lang.String`'s `byte[] value` and its `coder` (0 LATIN1,
/// 1 UTF16), resolved BY NAME and descriptor on the object's own class (gci
/// i13/jc). `None` for any other shape (a stub `String` with no fields, a
/// legacy `char[]` payload, a coder outside `{0, 1}`): callers fall back to
/// the speculative readers.
pub(in crate::native::jni) fn jni_string_byte_payload(shared: &SharedVm, oref: ObjectRef) -> Option<(ObjectRef, i32)> {
    let heap = &shared.mem.heap;
    if heap.kind_of(oref) == crate::memory::heap::ObjectKind::Array {
        return None;
    }
    let class_id = heap.class_id_of(oref);
    let (value_idx, coder_idx) = {
        let cm = shared.classes.class_manager.read();
        (
            crate::vm::vm_exec::resolve_field_index_in_hierarchy_desc(
                class_id,
                "value",
                Some("[B"),
                &cm.class_store,
            )?,
            crate::vm::vm_exec::resolve_field_index_in_hierarchy_desc(
                class_id,
                "coder",
                Some("B"),
                &cm.class_store,
            )?,
        )
    };
    let value = match heap.get_field(oref, value_idx) {
        Value::Object(Some(v)) => v,
        _ => return None,
    };
    if heap.kind_of(value) != crate::memory::heap::ObjectKind::Array
        || heap.array_element_type(value) != Some(ArrayElementType::Byte)
    {
        return None;
    }
    match heap.get_field(oref, coder_idx) {
        Value::Int(c @ (0 | 1)) => Some((value, c)),
        _ => None,
    }
}

// ---- Index 165/166: GetStringChars / ReleaseStringChars (UTF-16) ----
pub(in crate::native::jni) extern "C" fn jni_get_string_chars(
    _env: JNIEnv,
    str_obj: JString,
    is_copy: *mut JBoolean,
) -> *const JChar {
    string_chars_copy(str_obj, is_copy, false)
}

/// `GetStringChars` (`critical == false`) and `GetStringCritical` (`true`):
/// a NUL-terminated copy of the string's exact UTF-16 units, recorded for
/// `ReleaseStringChars`. `isCopy` is `JNI_TRUE`, except that a critical copy
/// of a UTF16-coded string answers `JNI_FALSE`, as HotSpot's
/// `jni_GetStringCritical` does (it pins a UTF16 value array and hands out its
/// body; a LATIN1 one it copies and widens). The chars are `const` and a String
/// is immutable, so a copy reads as the body would: only the flag differed
/// (gci i13/jc F14). A String of another shape keeps `JNI_TRUE`.
pub(in crate::native::jni) fn string_chars_copy(str_obj: JString, is_copy: *mut JBoolean, critical: bool) -> *const JChar {
    // gcd d10/j: runs no Java (see `ForeignJniEntry::enter_vm_only`).
    let _fx = ForeignJniEntry::enter_vm_only();
    if str_obj == 0 {
        return std::ptr::null();
    }
    let mut copy_flag = JNI_TRUE;
    let result = with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        // gci i14/pn: the pin needs the value array, so the payload is read
        // when the G1-pin switch is on even with a NULL `isCopy`.
        let pin = critical;
        if critical && (!is_copy.is_null() || pin) {
            if let Some((value, 1)) = jni_string_byte_payload(shared, oref) {
                copy_flag = JNI_FALSE;
                // gci i14/pn: HotSpot G1's pinned arm -- the UTF16 value
                // array's own body (little-endian units, the host order this
                // VM's `StringUTF16.isBigEndian()` answers), not terminated.
                if pin {
                    let bytes = shared.mem.heap.array_length(value);
                    if let Some(body) =
                        open_critical_g1_pin(shared, value, ArrayElementType::Byte, bytes, 1)
                    {
                        return Some(body as *const JChar);
                    }
                }
            }
        }
        // gci i13/jc: the string's exact UTF-16 units (`read_java_string`
        // went through a Rust `str`, so a lone surrogate came back as U+FFFD
        // and `NewString(GetStringChars(s), len)` was a different string),
        // plus a NUL unit: HotSpot's `jni_GetStringChars` (and the copying arm
        // of `jni_GetStringCritical`) allocates `len + 1` units and zero-
        // terminates, and a native that scans for the terminator read past
        // this buffer's end. The terminator also gives an empty string a real,
        // uniquely-addressed allocation (it was the shared dangling sentinel).
        //
        // j27 wave 3 (lane DISPATCH2): the whole `len + 1` buffer is reserved
        // FALLIBLY up front and a failed reserve answers NULL with NO pending
        // exception, as HotSpot 25's `jni_GetStringChars` and
        // `jni_GetStringCritical` do (`NEW_C_HEAP_ARRAY_RETURN_NULL`, then
        // copy only when non-null; no `THROW`). `GetStringUTFChars` is
        // different on HotSpot (it throws `out_of_memory_error_c_heap`); see
        // `string_utf_chars_copy` (j27 wave 8, lane W8JNI).
        let count = crate::vm::java_string_unit_count(&shared.mem.heap, oref)?;
        let mut units: Vec<JChar> = reserve_elements_copy(count.checked_add(1)?)?;
        if !crate::vm::append_java_string_units(&shared.mem.heap, oref, &mut units) {
            return None;
        }
        units.push(0);
        let allocated = units.len();
        let boxed = units.into_boxed_slice();
        let ptr = boxed.as_ptr();
        std::mem::forget(boxed); // OWNERSHIP: buffer transferred to native caller, freed by jni_release_string_chars via Vec::from_raw_parts

        // Track the allocation so ReleaseStringChars can reconstruct the
        // correct Vec layout (pointer + length) for deallocation. In the VM's
        // table, so a release on any thread finds it (gc-common w10-c). The
        // record is the ALLOCATED unit count, terminator included (never 0).
        shared
            .natives
            .jni_global_refs
            .lock()
            .open_string_copy(ptr as usize, allocated);
        Some(ptr)
    })
    .flatten();
    match result {
        Some(ptr) => {
            if !is_copy.is_null() {
                unsafe {
                    *is_copy = copy_flag;
                }
            }
            ptr
        }
        None => std::ptr::null(),
    }
}

pub(in crate::native::jni) extern "C" fn jni_release_string_chars(_env: JNIEnv, _str: JString, chars: *const JChar) {
    if !chars.is_null() {
        // Look up the original element count so we can reconstruct the Vec
        // with the correct layout.  Without the length the global allocator
        // would receive a mismatched dealloc (single-element vs slice).
        let len = with_shared_vm(|shared| {
            shared
                .natives
                .jni_global_refs
                .lock()
                .take_string_copy(chars as usize)
        })
        .flatten()
        .unwrap_or(0);
        if len > 0 {
            unsafe {
                drop(Vec::from_raw_parts(chars as *mut JChar, len, len));
            }
        }
        // len == 0 means the pointer wasn't tracked (shouldn't happen in
        // correct usage).  We intentionally leak rather than corrupt the
        // allocator with a wrong layout.
    }
}

// ---------------------------------------------------------------------------
// va_list helpers — extract JNI arguments from a C va_list pointer
// ---------------------------------------------------------------------------
//
// On x86-64 Windows the calling convention passes a va_list as a simple
// pointer to an 8-byte-aligned argument block.  Each slot is 8 bytes wide
// regardless of the actual argument type.
//
// On x86-64 SysV (Linux/macOS) va_list is a *struct { gp_offset, fp_offset,
// overflow_arg_area, reg_save_area }*, which is considerably more complex.
// For portability we define both paths and use cfg to select.

/// Opaque C va_list pointer.
pub type VaList = *mut u8;

/// A walk over a C `va_list`, in whichever shape the target ABI gives it.
///
/// This is not a detail that can be papered over with "8-byte slots". On
/// **System V x86-64** a `va_list` is a four-field struct — two offsets, an
/// overflow pointer and a register-save area — because the first six integer
/// and first eight FP arguments were never on the stack: they live in a save
/// area the variadic callee spilled them into, and the two classes advance
/// through it INDEPENDENTLY. Reading that struct as an array of arguments
/// decodes its own `gp_offset`/`fp_offset` header as argument one.
///
/// On **Windows x64** (and Apple's arm64) a `va_list` really is a bare pointer
/// into a contiguous run of 8-byte slots, which is the shape this code
/// previously assumed on every platform.
pub(in crate::native::jni) enum VaCursor {
    /// Contiguous 8-byte slots: Windows x64, Apple arm64.
    Flat { p: *const u8 },
    /// System V x86-64: a pointer to one `__va_list_tag`.
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    SysV { tag: *mut SysVVaListTag },
}

/// The System V x86-64 `__va_list_tag`. `va_list` is a one-element array of
/// it, so a `va_list` argument decays to a pointer to this.
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
#[repr(C)]
pub(in crate::native::jni) struct SysVVaListTag {
    /// Byte offset of the next unconsumed INTEGER register slot, 0..48.
    pub(in crate::native::jni) gp_offset: u32,
    /// Byte offset of the next unconsumed SSE register slot, 48..176.
    pub(in crate::native::jni) fp_offset: u32,
    /// Next stack-passed argument.
    pub(in crate::native::jni) overflow_arg_area: *mut u8,
    /// 6 GP slots of 8 bytes, then 8 SSE slots of 16 bytes = 176 bytes.
    pub(in crate::native::jni) reg_save_area: *mut u8,
}

/// End of the six integer register slots in a System V register-save area.
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
pub(in crate::native::jni) const SYSV_GP_LIMIT: u32 = 6 * 8;
/// End of the eight SSE register slots in a System V register-save area.
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
pub(in crate::native::jni) const SYSV_FP_LIMIT: u32 = 6 * 8 + 8 * 16;

impl VaCursor {
    pub(in crate::native::jni) fn new(va: VaList) -> Self {
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        {
            VaCursor::SysV {
                tag: va as *mut SysVVaListTag,
            }
        }
        #[cfg(not(all(target_arch = "x86_64", not(target_os = "windows"))))]
        {
            VaCursor::Flat { p: va as *const u8 }
        }
    }

    /// Consume one argument and return its raw 8 bytes. `is_fp` selects the
    /// SSE class (`float`/`double`); every other JNI argument type, references
    /// included, is INTEGER class.
    ///
    /// # Safety
    /// The cursor must be positioned on an argument the caller actually
    /// pushed, in the class the caller pushed it as.
    pub(in crate::native::jni) unsafe fn next(&mut self, is_fp: bool) -> u64 {
        match self {
            VaCursor::Flat { p } => {
                let v = *(*p as *const u64);
                *p = p.add(8);
                v
            }
            #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
            VaCursor::SysV { tag } => {
                let t = &mut **tag;
                if is_fp {
                    if t.fp_offset < SYSV_FP_LIMIT {
                        let p = t.reg_save_area.add(t.fp_offset as usize);
                        t.fp_offset += 16;
                        return *(p as *const u64);
                    }
                } else if t.gp_offset < SYSV_GP_LIMIT {
                    let p = t.reg_save_area.add(t.gp_offset as usize);
                    t.gp_offset += 8;
                    return *(p as *const u64);
                }
                // Spilled. Every overflow argument occupies exactly 8 bytes —
                // a `double` included; only 16-byte-aligned types differ, and
                // no JNI argument type is one.
                let p = t.overflow_arg_area;
                t.overflow_arg_area = t.overflow_arg_area.add(8);
                *(p as *const u64)
            }
        }
    }
}

/// Read a JNI method's arguments from a va_list and pack them into a JValue
/// array.  The caller is responsible for passing the correct `mid` so that we
/// can look up the method descriptor and determine argument types.
///
/// Returns `std::ptr::null()` on failure and for a method without parameters;
/// otherwise a heap JValue array of `len` entries that the caller must hand
/// back to [`free_jvalues`] with that `len`.
///
/// gci i16/dp (i15/jk R1): the tags are read in place ([`param_type_tags`],
/// the same bytes as `parse_param_types_inner`), not through the descriptor
/// cache (a `String`-keyed probe and a `Vec` copy per call); a call with at
/// most [`VA_SPARE_SLOTS`] arguments fills this thread's spare buffer
/// ([`NativeCallTls::va_spare`]), which `free_jvalues` gives back, so a
/// steady `Call<Type>MethodV` / variadic loop allocates nothing here. Same
/// values, same order (`gci_i16dp_va_args_reuse_one_buffer_with_the_same_values`).
pub(in crate::native::jni) fn va_list_to_jvalues(mid: JMethodID, va: VaList) -> (*const JValue, usize) {
    if mid == 0 || va.is_null() {
        return (std::ptr::null(), 0);
    }
    // Look up the method descriptor to know argument types.
    // Wave 60 (lane L1): a lambda proxy's own method id too.
    let descriptor =
        with_shared_vm(|shared| jni_method_name_and_descriptor(shared, mid).map(|(_, d)| d))
            .flatten();
    let descriptor = match descriptor {
        Some(d) => d,
        None => return (std::ptr::null(), 0),
    };
    va_args_by_descriptor(&descriptor, va)
}

/// gci i16/dp: [`va_list_to_jvalues`] once the descriptor is known: the
/// arguments `descriptor`'s parameter tags name, read from `va` (a live
/// `va_list` holding exactly them). Same return contract.
pub(in crate::native::jni) fn va_args_by_descriptor(descriptor: &str, va: VaList) -> (*const JValue, usize) {
    let count = param_type_tags(descriptor).count();
    if count == 0 {
        return (std::ptr::null(), 0);
    }
    let mut cursor = VaCursor::new(va);
    let mut read = |tag: u8| -> JValue {
        // C's default argument promotions apply to everything that reaches a
        // `...`, and therefore to everything that reaches the `va_list` form:
        // a `jfloat` argument arrives as a DOUBLE. Reading 'F' as raw 32 bits
        // — which this loop used to do — decoded half of a double's mantissa.
        let is_fp = tag == b'F' || tag == b'D';
        // SAFETY: `cursor` walks a live `va_list` the caller vouched for, and
        // the tags come from the method's own descriptor, so it consumes
        // exactly the arguments the caller pushed, in their register classes.
        let raw: u64 = unsafe { cursor.next(is_fp) };
        jvalue_from_va_word(tag, raw)
    };
    if count <= VA_SPARE_SLOTS {
        let mut buf = take_va_spare();
        for (slot, tag) in buf.iter_mut().zip(param_type_tags(descriptor)) {
            *slot = read(tag);
        }
        // OWNERSHIP: the spare-sized buffer goes to the caller; `free_jvalues`
        // (`len <= VA_SPARE_SLOTS`) rebuilds it as this type and keeps it.
        return (Box::into_raw(buf).cast::<JValue>().cast_const(), count);
    }
    let mut jvalues: Vec<JValue> = Vec::with_capacity(count);
    for tag in param_type_tags(descriptor) {
        jvalues.push(read(tag));
    }
    let len = jvalues.len();
    let boxed = jvalues.into_boxed_slice();
    let ptr = boxed.as_ptr();
    std::mem::forget(boxed); // OWNERSHIP: buffer transferred to caller, freed by free_jvalues via Box::from_raw
    (ptr, len)
}

/// gci i16/dp: the size of [`NativeCallTls::va_spare`]. A `va_list` call
/// with at most this many arguments uses (and gives back) that buffer; a
/// longer one allocates its own exact-length array, as before.
pub(in crate::native::jni) const VA_SPARE_SLOTS: usize = 16;

/// gci i16/dp: an all-zero `JValue`, the fill of a fresh spare buffer.
pub(in crate::native::jni) const JVALUE_ZERO: JValue = JValue { j: 0 };

/// gci i16/dp: one argument word read from a `va_list` as the `JValue` of
/// descriptor tag `tag` -- [`va_list_to_jvalues`]' conversion, unchanged.
pub(in crate::native::jni) fn jvalue_from_va_word(tag: u8, raw: u64) -> JValue {
    match tag {
        b'Z' => JValue { z: raw as JBoolean },
        b'B' => JValue { b: raw as JByte },
        b'C' => JValue { c: raw as JChar },
        b'S' => JValue { s: raw as JShort },
        b'I' => JValue { i: raw as JInt },
        b'J' => JValue { j: raw as JLong },
        b'F' => JValue { f: f64::from_bits(raw) as f32 },
        b'D' => JValue { d: f64::from_bits(raw) },
        _ /* L, [ */ => JValue { l: raw },
    }
}

/// gci i16/dp: this thread's spare argument buffer, or a fresh one when it is
/// out (a nested `va_list` call) or the thread-local is gone.
pub(in crate::native::jni) fn take_va_spare() -> Box<[JValue; VA_SPARE_SLOTS]> {
    JNI_NATIVE_CALL
        .try_with(|t| t.va_spare.take())
        .ok()
        .flatten()
        .unwrap_or_else(|| Box::new([JVALUE_ZERO; VA_SPARE_SLOTS]))
}

/// Free a JValue array returned by `va_list_to_jvalues`.
///
/// gci i16/dp: an array of at most [`VA_SPARE_SLOTS`] entries is always a
/// spare-sized buffer ([`va_list_to_jvalues`] makes no other below that
/// size); it becomes this thread's spare again (replacing, and freeing, any
/// spare a nested call already gave back).
pub(in crate::native::jni) unsafe fn free_jvalues(ptr: *const JValue, len: usize) {
    if !ptr.is_null() && len > 0 && len <= VA_SPARE_SLOTS {
        // SAFETY: made by `Box::into_raw` of exactly this type in
        // `va_args_by_descriptor`, handed back once.
        let buf = Box::from_raw(ptr.cast_mut().cast::<[JValue; VA_SPARE_SLOTS]>());
        // If the thread-local is gone the closure is dropped unrun, and the
        // buffer with it.
        let _ = JNI_NATIVE_CALL.try_with(move |t| t.va_spare.set(Some(buf)));
        return;
    }
    if !ptr.is_null() && len > 0 {
        // Rebuild the `Box<[JValue]>` that `va_list_to_jvalues` forgot.
        // `slice_from_raw_parts_mut` builds the fat pointer directly, instead
        // of materialising a `&mut [JValue]` and casting it back to a raw
        // pointer: the reference form asserts an exclusive borrow of memory
        // this function is about to hand to `Box::from_raw`, which is what
        // `clippy::cast_slice_from_raw_parts` (denied here) objects to. Same
        // pointer, same length, no intermediate reference.
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            ptr as *mut JValue,
            len,
        )));
    }
}

// ---------------------------------------------------------------------------
// V-variant method call wrappers (take va_list instead of JValue*)
// ---------------------------------------------------------------------------

// --- NewObjectV (slot 29) ---
pub(in crate::native::jni) extern "C" fn jni_new_object_v(env: JNIEnv, clazz: JClass, mid: JMethodID, va: VaList) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let result = jni_new_object_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    result
}

// --- Instance CallXxxMethodV (slots 35, 38, 41, 44, 47, 50, 53, 56, 59, 62) ---
pub(in crate::native::jni) extern "C" fn jni_call_object_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_object_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_boolean_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JBoolean {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_boolean_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_byte_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JByte {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_byte_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_char_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JChar {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_char_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_short_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JShort {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_short_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_int_method_v(env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList) -> JInt {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_int_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_long_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JLong {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_long_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_float_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JFloat {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_float_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_double_method_v(
    env: JNIEnv,
    obj: JObject,
    mid: JMethodID,
    va: VaList,
) -> JDouble {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_double_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_void_method_v(env: JNIEnv, obj: JObject, mid: JMethodID, va: VaList) {
    let (args, len) = va_list_to_jvalues(mid, va);
    jni_call_void_method_a(env, obj, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
}

// --- Nonvirtual CallNonvirtualXxxMethodV (slots 65, 68, 71, 74, 77, 80, 83, 86, 89, 92) ---
pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_object_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_object_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_boolean_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JBoolean {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_boolean_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_byte_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JByte {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_byte_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_char_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JChar {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_char_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_short_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JShort {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_short_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_int_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JInt {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_int_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_long_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JLong {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_long_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_float_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JFloat {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_float_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_double_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JDouble {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_nonvirtual_double_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_nonvirtual_void_method_v(
    env: JNIEnv,
    obj: JObject,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) {
    let (args, len) = va_list_to_jvalues(mid, va);
    jni_call_nonvirtual_void_method_a(env, obj, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
}

// --- Static CallStaticXxxMethodV (slots 115, 118, 121, 124, 127, 130, 133, 136, 139, 142) ---
pub(in crate::native::jni) extern "C" fn jni_call_static_object_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JObject {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_object_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_static_boolean_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JBoolean {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_boolean_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_static_byte_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JByte {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_byte_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_static_char_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JChar {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_char_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_static_short_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JShort {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_short_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_static_int_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JInt {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_int_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_static_long_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JLong {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_long_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_static_float_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JFloat {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_float_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_static_double_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) -> JDouble {
    let (args, len) = va_list_to_jvalues(mid, va);
    let r = jni_call_static_double_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
    r
}

pub(in crate::native::jni) extern "C" fn jni_call_static_void_method_v(
    env: JNIEnv,
    clazz: JClass,
    mid: JMethodID,
    va: VaList,
) {
    let (args, len) = va_list_to_jvalues(mid, va);
    jni_call_static_void_method_a(env, clazz, mid, args);
    unsafe {
        free_jvalues(args, len);
    }
}

// ---------------------------------------------------------------------------
// NIO Direct ByteBuffer support (slots 229-231)
// ---------------------------------------------------------------------------
//
// A DirectByteBuffer's native address and capacity live in the buffer object's
// own fields — there is no side table. Which slots those are depends on the
// class we actually got:
//
//   * **real-JDK mode** — `java/nio/DirectByteBuffer` resolves to the real
//     class, so `address` (`J`) and `capacity` (`I`) are inherited from
//     `java.nio.Buffer` at their real layout slots. `dbb_slots` finds them by
//     name+descriptor and both the setter and the getters use those slots.
//   * **synthetic-jdk / stub mode** — the class is a fabricated stub with no
//     declared fields, so nothing resolves by name and we fall back to the
//     historical fixed slots 0 (address, `Long`) and 1 (capacity, `Long`),
//     which a stub object stores as tagged `Value`s and therefore round-trips
//     exactly.
//
// PROCESS-GLOBAL-STATE ROUND 2 — this replaces a `LazyLock<Mutex<HashMap<u64,
// (SendPtr, i64)>>>` side table keyed on `obj_to_jobject(obj)`, i.e. on the
// buffer's **raw heap address**. That table was never remapped after a moving
// collection, never swept when a buffer died, shared by every VM in the
// process, and consulted BEFORE the authoritative field read. Once the
// collector recycled a dead buffer's address for any new object, a
// `GetDirectBufferAddress` on that new object returned the *previous* buffer's
// `malloc` pointer — an arbitrary out-of-bounds native read/write reachable
// from ordinary Java code (Netty and Elasticsearch both drive this path).
//
// The table also masked a second bug: writing `Value::Long(address)` into slot
// 0 of a *real* `java.nio.Buffer` targets `mark` (an `int`), and
// `write_compact_field`'s `Int` arm stores 0 for a non-`Int` value — so in
// real-JDK mode the buffer handed back to Java had `address == 0`,
// `mark == 0` and `position == capacity`. Only the side table made the JNI
// getters appear to work. Writing by name fixes the object itself, which is
// what Java-side `ByteBuffer` operations read.

/// Resolve the `(address, capacity)` layout slots of a real `java.nio.Buffer`
/// hierarchy, or `None` when the class is a fabricated stub with no declared
/// fields (synthetic-jdk mode).
///
/// Descriptor-qualified so a subclass field that merely shares the name cannot
/// shadow `Buffer.address` / `Buffer.capacity`. Both slots must resolve — a
/// half-resolved layout is not a layout we can round-trip through, so the
/// caller falls back to the fixed-slot form for *both* values and the setter
/// and getters therefore always agree.
pub(in crate::native::jni) fn dbb_slots(shared: &SharedVm, class_id: ClassId) -> Option<(usize, usize)> {
    let cm = shared.classes.class_manager.read();
    let addr = crate::vm::vm_exec::resolve_field_index_in_hierarchy_desc(
        class_id,
        "address",
        Some("J"),
        &cm.class_store,
    )?;
    let cap = crate::vm::vm_exec::resolve_field_index_in_hierarchy_desc(
        class_id,
        "capacity",
        Some("I"),
        &cm.class_store,
    )?;
    Some((addr, cap))
}

/// Read a slot that may hold either an `I` or a `J` payload, widening to i64.
/// Returns `None` for any other `Value` (notably `Object(None)`, which is what
/// the heap's bounds guard returns for an out-of-range slot).
pub(in crate::native::jni) fn dbb_read_integral(shared: &SharedVm, obj: ObjectRef, index: usize) -> Option<i64> {
    match shared.mem.heap.get_field(obj, index) {
        Value::Long(v) => Some(v),
        Value::Int(v) => Some(v as i64),
        _ => None,
    }
}

/// The capacity `GetDirectBufferCapacity` answers for this buffer.
///
/// Factored out because `GetDirectBufferAddress` now needs the SAME number to
/// bound the pointer it publishes (see `direct_buffer_native_address`). Two
/// copies of this resolution would be two chances for the pair to disagree,
/// which is precisely the failure the bound is there to prevent.
///
/// A capacity of 0 is legal (`NewDirectByteBuffer(addr, 0)`), so unlike the
/// address there is no "non-zero means present" test available. Resolution
/// decides: if the class has a real `capacity` field, that field is
/// authoritative; otherwise slot 1 is. The `or_else` covers the
/// stub-upgraded-under-a-live-object case, as in the address getter.
pub(in crate::native::jni) fn dbb_capacity(shared: &SharedVm, oref: ObjectRef, class_id: ClassId) -> Option<i64> {
    match dbb_slots(shared, class_id) {
        Some((_, cap_idx)) => {
            dbb_read_integral(shared, oref, cap_idx).or_else(|| dbb_read_integral(shared, oref, 1))
        }
        None => dbb_read_integral(shared, oref, 1),
    }
}

/// The `IllegalArgumentException` message `NewDirectByteBuffer` leaves pending
/// for `capacity`, or `None` when the capacity is in range (gci i13/jc). The
/// text is the JDK 25 `java.nio.DirectByteBuffer.checkCapacity`'s, which
/// HotSpot's `jni_NewDirectByteBuffer` reaches through the
/// `DirectByteBuffer(long, long)` constructor.
pub(in crate::native::jni) fn direct_buffer_capacity_refusal(capacity: JLong) -> Option<String> {
    if capacity < 0 {
        Some(format!(
            "JNI NewDirectByteBuffer passed capacity < 0: ({capacity})"
        ))
    } else if capacity > i64::from(i32::MAX) {
        Some(format!(
            "JNI NewDirectByteBuffer passed capacity > Integer.MAX_VALUE: ({capacity})"
        ))
    } else {
        None
    }
}

// Index 229: NewDirectByteBuffer
pub(in crate::native::jni) extern "C" fn jni_new_direct_byte_buffer(
    _env: JNIEnv,
    address: *mut u8,
    capacity: JLong,
) -> JObject {
    let _fx = ForeignJniEntry::enter();
    // gci i13/jc: HotSpot 25's `jni_NewDirectByteBuffer` runs the JDK's
    // `DirectByteBuffer(long addr, long cap)` constructor, whose
    // `checkCapacity` refuses a capacity outside `[0, Integer.MAX_VALUE]`
    // with `IllegalArgumentException` pending; this answered NULL with
    // nothing pending for a negative one and TRUNCATED a larger one into the
    // `int` fields (a wrapped, often negative, capacity). A NULL `address` is
    // not refused: HotSpot passes it through (`NewDirectByteBuffer(NULL, 0)`
    // is the empty buffer some natives return), and `GetDirectBufferAddress`
    // answers NULL for it on both VMs.
    if let Some(msg) = direct_buffer_capacity_refusal(capacity) {
        raise_jni_throwable("java/lang/IllegalArgumentException", &msg);
        return 0;
    }
    with_shared_vm(|shared| {
        // Allocate a java/nio/DirectByteBuffer-like object with at least 2
        // fields. The object's class must declare those fields — allocating
        // with `ClassId::new(0)` (`java/lang/Object`, zero declared fields)
        // yields an undersized object that the GC's `get_field` bounds guard
        // rejects on every access.
        let Some(dbb_class_id) = jni_class_or_refuse(shared, "java/nio/DirectByteBuffer", 2) else {
            return 0;
        };
        let num_fields = shared
            .classes
            .class_manager
            .read()
            .get_class(dbb_class_id)
            .map_or(2, |c| c.num_total_fields.max(2));
        let Some(obj) = jni_alloc_or_oom(shared, "a java.nio.DirectByteBuffer", |heap| {
            heap.try_alloc_object_full(dbb_class_id, num_fields)
        }) else {
            return 0;
        };
        let heap = &shared.mem.heap;
        match dbb_slots(shared, dbb_class_id) {
            Some((addr_idx, cap_idx)) => {
                // Real `java.nio.Buffer` layout. Seed the whole invariant set
                // (`mark`/`position`/`limit`/`capacity`), not just the two the
                // JNI getters read: the object is handed straight to Java, and
                // `java.nio.Buffer`'s own methods assume
                // `mark <= position <= limit <= capacity`. A default-zeroed
                // `limit` would make every `get`/`put` throw
                // `BufferUnderflow`/`BufferOverflow`.
                // Resolve every slot FIRST and release the class-manager read
                // lock before touching the heap: `set_field`'s diagnostic paths
                // can resolve class metadata themselves, and holding a reader
                // across them risks a writer-starvation stall.
                //
                // gci i13/jc: and `ByteBuffer`'s two field initialisers,
                // `bigEndian = true` and `nativeByteOrder = (native order is
                // big)`, which javac compiles into the constructor this mint
                // skips. Left at the zero default, `order()` answered
                // LITTLE_ENDIAN where HotSpot answers BIG_ENDIAN, and every
                // typed read (`getInt`, `getLong`, ...) of a JNI-made buffer
                // was byte-swapped relative to HotSpot -- the defect
                // `native_io::seed_buffer_byte_order` fixed for the other
                // constructor-less buffer mints, missed at this one.
                let extra: Vec<(usize, Value)> = {
                    let cm = shared.classes.class_manager.read();
                    [
                        ("limit", "I", Value::Int(capacity as i32)),
                        ("position", "I", Value::Int(0)),
                        ("mark", "I", Value::Int(-1)),
                        ("bigEndian", "Z", Value::Int(1)),
                        (
                            "nativeByteOrder",
                            "Z",
                            Value::Int(i32::from(cfg!(target_endian = "big"))),
                        ),
                    ]
                    .into_iter()
                    .filter_map(|(name, desc, value)| {
                        crate::vm::vm_exec::resolve_field_index_in_hierarchy_desc(
                            dbb_class_id,
                            name,
                            Some(desc),
                            &cm.class_store,
                        )
                        .map(|idx| (idx, value))
                    })
                    .collect()
                };
                heap.set_field(obj, addr_idx, Value::Long(address as i64));
                heap.set_field(obj, cap_idx, Value::Int(capacity as i32));
                for (idx, value) in extra {
                    heap.set_field(obj, idx, value);
                }
            }
            None => {
                // Fabricated-stub layout: tagged `Value` slots, so a `Long`
                // round-trips verbatim. Historical fixed slots.
                heap.set_field(obj, 0, Value::Long(address as i64));
                heap.set_field(obj, 1, Value::Long(capacity));
            }
        }
        new_local_handle(obj)
    })
    .unwrap_or(0)
}

/// Turn the `long` a direct buffer carries in its `address` field into
/// something a native library can actually dereference.
///
/// `Unsafe.allocateMemory` on this VM does NOT return an OS pointer: it returns
/// a *handle* into a Rust-side arena, carrying `ARENA_TAG` (bit 62) precisely so
/// it can never be confused with a real pointer. `ByteBuffer.allocateDirect`
/// runs the real JDK's bytecode, which allocates through `Unsafe`, so every
/// direct buffer this VM hands to Java has a tagged handle in `Buffer.address`.
///
/// Inside the VM that is invisible — the `Unsafe` get/put natives route a tagged
/// address back to the arena. It stops being invisible at the JNI boundary,
/// because `GetDirectBufferAddress` returns a bare `void*` that the native
/// *itself* dereferences. lz4-java's `XXH32BB` is one line —
/// `XXH32(GetDirectBufferAddress(env, buf) + off, len, seed)` — and zstd-jni's
/// `getDirectByteBufferFrameContentSize` is another; returning a handle to
/// either is an immediate SIGSEGV in third-party C, with no CratonVM frame at
/// the fault to say why.
///
/// So a tagged handle is translated to the real address of the arena block's
/// backing bytes. Writes by the native land in the arena directly, which is the
/// aliasing a direct buffer is defined to have, and a subsequent `Unsafe` read
/// through the handle sees them. An untagged address is already a real pointer
/// (a native's own `NewDirectByteBuffer` allocation) and is passed through.
///
/// A tagged address that no *live* block contains — a use-after-free, or an
/// offset past the end — resolves to `None` and the caller answers JNI NULL.
/// NULL is what the JNI spec already reserves for "not a direct buffer", so
/// natives that check at all check for it; returning the raw handle instead
/// would trade a null check for a wild store.
/// Is this object a DIRECT buffer — the only kind `GetDirectBufferAddress` and
/// `GetDirectBufferCapacity` may answer for?
///
/// This used to be inferred from `Buffer.address != 0`, and that inference was
/// correct on JDK 8 and is WRONG on JDK 21+. `Buffer.address` was repurposed:
/// a heap buffer now carries `ARRAY_BYTE_BASE_OFFSET` there — literally `16` —
/// as the base for the `Unsafe` accesses that pair it with `hb`. Measured on
/// Temurin 25.0.3.9, `ByteBuffer.allocate(16).address == 16` on HotSpot AND on
/// CratonVM; the two agree about the field, so the divergence was entirely in
/// reading it as a pointer. `GetDirectBufferAddress(heapBuffer)` therefore
/// answered `(void*)16` where HotSpot answers NULL — a wild pointer handed to
/// any native that trusts it, which is the same crash family as the arena
/// handle this function's sibling comment describes, from a different cause.
///
/// The rule is the JDK's own: a direct buffer is one that implements
/// `sun.nio.ch.DirectBuffer`. The class-name check on `java/nio/DirectByteBuffer`
/// is checked alongside it because a buffer minted by `NewDirectByteBuffer`
/// under `--synthetic-jdk` has that class but need not have the interface.
pub(in crate::native::jni) fn is_direct_buffer(shared: &SharedVm, oref: ObjectRef) -> bool {
    // An array's header carries its COMPONENT's class id, so a
    // `DirectByteBuffer[]` would pass the walk below and have its elements read
    // as buffer fields.
    if shared.mem.heap.kind_of(oref) == crate::memory::heap::ObjectKind::Array {
        return false;
    }
    let cm = shared.classes.class_manager.read();
    let mut current = shared.mem.heap.class_id_of(oref);
    // Bounded: a malformed or cyclic hierarchy must not spin inside a JNI call.
    for _ in 0..64 {
        let Some(class) = cm.get_class(current) else {
            return false;
        };
        if &*class.name == "java/nio/DirectByteBuffer"
            || &*class.name == "java/nio/MappedByteBuffer"
        {
            return true;
        }
        if class.interfaces.iter().any(|&i| {
            cm.get_class(i)
                .is_some_and(|c| &*c.name == "sun/nio/ch/DirectBuffer")
        }) {
            return true;
        }
        match class.superclass {
            Some(sc) => current = sc,
            None => return false,
        }
    }
    false
}

/// The real address `GetDirectBufferAddress` may publish for `raw`, given that
/// `GetDirectBufferCapacity` is about to answer `capacity` for the same buffer.
///
/// The two JNI entry points ARE the bound: the spec's contract is that a native
/// may touch `capacity` bytes starting at the address, so publishing them
/// separately without checking they agree hands out a pointer with a bound
/// nobody enforced. That is what
/// `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` recorded as "the
/// handle TRANSLATED, and the bound dropped" — `unsafe_arena_real_ptr` knows
/// how many bytes are left in the block and this call site used to discard it.
///
/// A tagged handle whose block cannot cover `capacity` is refused, not clamped:
/// the capacity getter reads a Java field this function cannot correct, so the
/// only self-consistent pair on offer is `(NULL, capacity)` — and NULL is the
/// answer natives already check for.
pub(in crate::native::jni) fn direct_buffer_native_address(raw: i64, capacity: i64) -> Option<*mut u8> {
    if raw == 0 {
        return None;
    }
    if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(raw) {
        // A negative capacity is not a length; treat it as zero rather than
        // wrapping it into a colossal `usize` that refuses every buffer.
        let want = usize::try_from(capacity).unwrap_or(0);
        return cratonvm_native_builtins::unsafe_arena_real_ptr_bounded(raw, want);
    }
    Some(raw as *mut u8)
}

/// The element width in bytes of a direct buffer class (gci i13/jc F16): the
/// first `java/nio/Direct<Type>Buffer*` on its superclass chain names it
/// ([`direct_buffer_width_for_name`]); 1 for a byte buffer and for any other
/// shape (a `MappedByteBuffer`, a stub).
pub(in crate::native::jni) fn direct_buffer_element_width(shared: &SharedVm, class_id: ClassId) -> usize {
    let cm = shared.classes.class_manager.read();
    let mut current = class_id;
    // Bounded, as `is_direct_buffer`'s walk.
    for _ in 0..64 {
        let Some(class) = cm.get_class(current) else {
            return 1;
        };
        if let Some(width) = direct_buffer_width_for_name(&class.name) {
            return width;
        }
        match class.superclass {
            Some(sc) => current = sc,
            None => return 1,
        }
    }
    1
}

/// The element width a `java/nio/Direct<Type>Buffer{,R}{,U,S,RU,RS}` class
/// name implies, or `None` for any other name. Pure.
pub(in crate::native::jni) fn direct_buffer_width_for_name(name: &str) -> Option<usize> {
    let rest = name.strip_prefix("java/nio/Direct")?;
    [
        ("Byte", 1),
        ("Char", 2),
        ("Short", 2),
        ("Int", 4),
        ("Float", 4),
        ("Long", 8),
        ("Double", 8),
    ]
    .into_iter()
    .find(|(ty, _)| rest.starts_with(ty) && rest[ty.len()..].starts_with("Buffer"))
    .map(|(_, width)| width)
}

// Index 230: GetDirectBufferAddress
pub(in crate::native::jni) extern "C" fn jni_get_direct_buffer_address(_env: JNIEnv, buf: JObject) -> *mut u8 {
    let _fx = ForeignJniEntry::enter();
    if buf == 0 {
        return std::ptr::null_mut();
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(buf)?;
        // Non-direct buffers answer NULL — see `is_direct_buffer` for why the
        // address field alone cannot decide this on JDK 21+.
        if !is_direct_buffer(shared, oref) {
            return None;
        }
        let class_id = shared.mem.heap.class_id_of(oref);
        // The same resolution the setter used, so setter and getter always
        // address the same slot. The `or_else` covers exactly one drift case:
        // the class was a stub when the buffer was allocated and has since been
        // upgraded to the real class, so the *object* is still stub-shaped and
        // the named slot reads out of bounds (`Object(None)` -> `None`). Only a
        // `None` falls through — a named slot that reads a real value is always
        // preferred, so a real `Buffer`'s `mark` can never be mistaken for an
        // address.
        // The capacity this buffer is about to advertise through
        // `GetDirectBufferCapacity`. A buffer that cannot answer one at all
        // gets 0, which bounds nothing away: an arena block always covers zero
        // bytes, so such a buffer behaves exactly as it did before the bound
        // was carried.
        let capacity = dbb_capacity(shared, oref, class_id).unwrap_or(0);
        // gci i13/jc (F16): `capacity` counts ELEMENTS (an `asIntBuffer()` view
        // of a direct buffer is a `DirectBuffer` too, and HotSpot answers its
        // address), so the bytes a native may touch are capacity x element
        // width; the bound took capacity as bytes and under-checked such a
        // view by its element size.
        let capacity_bytes =
            capacity.saturating_mul(direct_buffer_element_width(shared, class_id) as i64);
        match dbb_slots(shared, class_id) {
            Some((addr_idx, _)) => dbb_read_integral(shared, oref, addr_idx)
                .or_else(|| dbb_read_integral(shared, oref, 0)),
            None => dbb_read_integral(shared, oref, 0),
        }
        .and_then(|raw| direct_buffer_native_address(raw, capacity_bytes))
    })
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

// Index 231: GetDirectBufferCapacity
pub(in crate::native::jni) extern "C" fn jni_get_direct_buffer_capacity(_env: JNIEnv, buf: JObject) -> JLong {
    let _fx = ForeignJniEntry::enter();
    if buf == 0 {
        return -1;
    }
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(buf)?;
        // -1 for a non-direct buffer, symmetric with the address getter. A heap
        // buffer has a perfectly good `capacity` field, so without this the
        // capacity said "yes, a direct buffer of N bytes" about the very object
        // the address getter refuses — which is worse than either answer alone.
        if !is_direct_buffer(shared, oref) {
            return None;
        }
        let class_id = shared.mem.heap.class_id_of(oref);
        dbb_capacity(shared, oref, class_id)
    })
    .flatten()
    .unwrap_or(-1)
}

// Index 232: GetObjectRefType (correct JNI 1.6 slot)
// Already implemented at index 19 (jni_get_object_ref_type) — we alias it here.

// Index 233: GetModule (JNI 9+)
//
// gcf f1/j: the class's `java.lang.Module`, as a local ref -- what
// `clazz.getModule()` answers in Java, which is what HotSpot's
// `jni_GetModule` returns (`Modules::get_module`, the mirror's module field;
// never NULL for a loaded class: an unnamed module object for a class-path
// class). It answered NULL for every class, so a native that asked for its
// class's module and passed the answer on (`Module.isNamed`, `addReads`,
// `getResourceAsStream`) passed `null`. Both modes. A NULL `clazz` is NULL,
// as it is in HotSpot's checked-JNI-free path.
pub(in crate::native::jni) extern "C" fn jni_get_module(_env: JNIEnv, clazz: JClass) -> JObject {
    let _fx = ForeignJniEntry::enter();
    if clazz == 0 {
        return 0;
    }
    // A `jclass` and a `Class` reference both resolve to the mirror.
    let Some(mirror) = jobject_to_obj(clazz) else {
        return 0;
    };
    with_jni_context(|shared, thread| {
        let class_class = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/Class")?;
        // The mirror is the call's receiver argument, so the up-call's frame
        // roots it from here on.
        match invoke_on_class_shared(
            shared,
            thread,
            class_class,
            "getModule",
            "()Ljava/lang/Module;",
            &[Value::Object(Some(mirror))],
        ) {
            Ok(Some(Value::Object(Some(module)))) => Some(new_local_handle(module)),
            _ => None,
        }
    })
    .flatten()
    .unwrap_or(0)
}

// ---- Index 234: IsVirtualThread (JNI 21) ----
pub(in crate::native::jni) extern "C" fn jni_is_virtual_thread(env: JNIEnv, obj: JObject) -> JBoolean {
    let _fx = ForeignJniEntry::enter();
    if obj == 0 {
        return JNI_FALSE;
    }
    let clazz = jni_get_object_class(env, obj);
    if clazz == 0 {
        return JNI_FALSE;
    }
    // Same predicate `vm_exec` uses to decide a thread is virtual: every
    // virtual thread implementation (`VirtualThread`, `ThreadBuilders$
    // BoundVirtualThread`) extends `java.lang.BaseVirtualThread`. If that
    // class was never loaded, no virtual thread exists yet and the answer is
    // `false` for every receiver.
    let base = with_shared_vm(|shared| {
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/BaseVirtualThread")
    })
    .flatten();
    match base {
        Some(b) => jni_is_assignable_from(env, clazz, class_id_to_jclass(b)),
        None => JNI_FALSE,
    }
}

// ---- Index 235: GetStringUTFLengthAsLong (JNI 24) ----
pub(in crate::native::jni) extern "C" fn jni_get_string_utf_length_as_long(env: JNIEnv, str_obj: JString) -> JLong {
    // gci i15/jk (i14/jh's R2): a leaf, as its twin `GetStringUTFLength`
    // (gcd d5/f): the same reads into Rust memory, no Java, no allocation on
    // the Java heap, no new reference. It took the full `enter()`.
    let _fx = ForeignJniEntry::enter_leaf(str_obj);
    // Identical measurement to GetStringUTFLength, in the wider return type
    // that exists so a string longer than `jsize` can report its real modified
    // UTF-8 length instead of overflowing.
    if str_obj == 0 {
        return 0;
    }
    let _ = env;
    with_shared_vm(|shared| {
        let oref = jobject_to_obj(str_obj)?;
        // gci i14/jh: the exact units, as `GetStringUTFLength`.
        let units = crate::vm::read_java_string_units(&shared.mem.heap, oref)?;
        Some(modified_utf8_len_units(&units) as JLong)
    })
    .flatten()
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// JNI Function Table (flat array)
// ---------------------------------------------------------------------------

/// Build the JNI function table at runtime.
pub(in crate::native::jni) fn build_function_table() -> Box<[usize; JNI_FUNCTION_COUNT]> {
    let stub = jni_stub as *const () as usize;
    let mut t = [stub; JNI_FUNCTION_COUNT];

    // Version
    t[4] = jni_get_version as *const () as usize;

    // Class operations
    t[5] = jni_define_class as *const () as usize;
    t[6] = jni_find_class as *const () as usize;
    t[7] = jni_from_reflected_method as *const () as usize;
    t[8] = jni_from_reflected_field as *const () as usize;
    t[9] = jni_to_reflected_method as *const () as usize;
    t[10] = jni_get_superclass as *const () as usize;
    t[11] = jni_is_assignable_from as *const () as usize;
    t[12] = jni_to_reflected_field as *const () as usize;

    // Exceptions
    t[13] = jni_throw as *const () as usize;
    t[14] = jni_throw_new as *const () as usize;
    t[15] = jni_exception_occurred as *const () as usize;
    t[16] = jni_exception_describe as *const () as usize;
    t[17] = jni_exception_clear as *const () as usize;
    t[18] = jni_fatal_error as *const () as usize;

    // Local frame management.
    //
    // Slots 19..=27 used to sit one HIGHER than `jni.h` puts them, because a
    // second `GetObjectRefType` had been wired at 19 (its real slot is 232,
    // still wired below) and pushed everything after it along. A native does
    // not name these functions — it loads slot N and calls it — so the shift
    // was silent and total: `DeleteLocalRef` ran `DeleteGlobalRef`,
    // `NewGlobalRef` ran `PopLocalFrame` (popping a frame the VM still
    // believed was live), `DeleteGlobalRef` ran `NewGlobalRef` (so every
    // release leaked a fresh global root), and `IsSameObject` — the idiom
    // every library uses for `o == null` and for reference identity — ran the
    // `void` `DeleteLocalRef` and returned whatever was left in RAX.
    t[19] = jni_push_local_frame as *const () as usize;
    t[20] = jni_pop_local_frame as *const () as usize;

    // References
    t[21] = jni_new_global_ref as *const () as usize;
    t[22] = jni_delete_global_ref as *const () as usize;
    t[23] = jni_delete_local_ref as *const () as usize;
    t[24] = jni_is_same_object as *const () as usize;
    t[25] = jni_new_local_ref as *const () as usize;
    t[26] = jni_ensure_local_capacity as *const () as usize;
    t[27] = jni_alloc_object as *const () as usize;
    // t[28] = NewObject (bare varargs) — wired with the other `...` slots below.
    t[29] = jni_new_object_v as *const () as usize;
    t[30] = jni_new_object_a as *const () as usize;

    // Object operations
    t[31] = jni_get_object_class as *const () as usize;
    t[32] = jni_is_instance_of as *const () as usize;

    // Method IDs
    t[33] = jni_get_method_id as *const () as usize;

    // The bare-varargs `...` slots — NewObject (28), Call<T>Method (34,37,…),
    // CallNonvirtual<T>Method (64,67,…) and CallStatic<T>Method (114,117,…) —
    // go to the assembly trampolines above, which build the platform `va_list`
    // and hand off to the `V` sibling. On an architecture with no trampoline
    // they keep `jni_varargs_unsupported`, which raises UnsatisfiedLinkError
    // rather than fabricating a 0/null return.
    for (slot, fn_addr) in jni_varargs_slots() {
        t[slot] = fn_addr;
    }

    // Call<Type>Method/V/A — virtual instance (groups of 3: varargs, va_list, array)
    t[35] = jni_call_object_method_v as *const () as usize;
    t[36] = jni_call_object_method_a as *const () as usize;
    t[38] = jni_call_boolean_method_v as *const () as usize;
    t[39] = jni_call_boolean_method_a as *const () as usize;
    t[41] = jni_call_byte_method_v as *const () as usize;
    t[42] = jni_call_byte_method_a as *const () as usize;
    t[44] = jni_call_char_method_v as *const () as usize;
    t[45] = jni_call_char_method_a as *const () as usize;
    t[47] = jni_call_short_method_v as *const () as usize;
    t[48] = jni_call_short_method_a as *const () as usize;
    t[50] = jni_call_int_method_v as *const () as usize;
    t[51] = jni_call_int_method_a as *const () as usize;
    t[53] = jni_call_long_method_v as *const () as usize;
    t[54] = jni_call_long_method_a as *const () as usize;
    t[56] = jni_call_float_method_v as *const () as usize;
    t[57] = jni_call_float_method_a as *const () as usize;
    t[59] = jni_call_double_method_v as *const () as usize;
    t[60] = jni_call_double_method_a as *const () as usize;
    t[62] = jni_call_void_method_v as *const () as usize;
    t[63] = jni_call_void_method_a as *const () as usize;

    // CallNonvirtual<Type>Method/V/A (groups of 3); the `...` slots are wired above.
    t[65] = jni_call_nonvirtual_object_method_v as *const () as usize;
    t[66] = jni_call_nonvirtual_object_method_a as *const () as usize;
    t[68] = jni_call_nonvirtual_boolean_method_v as *const () as usize;
    t[69] = jni_call_nonvirtual_boolean_method_a as *const () as usize;
    t[71] = jni_call_nonvirtual_byte_method_v as *const () as usize;
    t[72] = jni_call_nonvirtual_byte_method_a as *const () as usize;
    t[74] = jni_call_nonvirtual_char_method_v as *const () as usize;
    t[75] = jni_call_nonvirtual_char_method_a as *const () as usize;
    t[77] = jni_call_nonvirtual_short_method_v as *const () as usize;
    t[78] = jni_call_nonvirtual_short_method_a as *const () as usize;
    t[80] = jni_call_nonvirtual_int_method_v as *const () as usize;
    t[81] = jni_call_nonvirtual_int_method_a as *const () as usize;
    t[83] = jni_call_nonvirtual_long_method_v as *const () as usize;
    t[84] = jni_call_nonvirtual_long_method_a as *const () as usize;
    t[86] = jni_call_nonvirtual_float_method_v as *const () as usize;
    t[87] = jni_call_nonvirtual_float_method_a as *const () as usize;
    t[89] = jni_call_nonvirtual_double_method_v as *const () as usize;
    t[90] = jni_call_nonvirtual_double_method_a as *const () as usize;
    t[92] = jni_call_nonvirtual_void_method_v as *const () as usize;
    t[93] = jni_call_nonvirtual_void_method_a as *const () as usize;

    // CallStatic<Type>Method/V/A (groups of 3); the `...` slots are wired above.
    t[115] = jni_call_static_object_method_v as *const () as usize;
    t[116] = jni_call_static_object_method_a as *const () as usize;
    t[118] = jni_call_static_boolean_method_v as *const () as usize;
    t[119] = jni_call_static_boolean_method_a as *const () as usize;
    t[121] = jni_call_static_byte_method_v as *const () as usize;
    t[122] = jni_call_static_byte_method_a as *const () as usize;
    t[124] = jni_call_static_char_method_v as *const () as usize;
    t[125] = jni_call_static_char_method_a as *const () as usize;
    t[127] = jni_call_static_short_method_v as *const () as usize;
    t[128] = jni_call_static_short_method_a as *const () as usize;
    t[130] = jni_call_static_int_method_v as *const () as usize;
    t[131] = jni_call_static_int_method_a as *const () as usize;
    t[133] = jni_call_static_long_method_v as *const () as usize;
    t[134] = jni_call_static_long_method_a as *const () as usize;
    t[136] = jni_call_static_float_method_v as *const () as usize;
    t[137] = jni_call_static_float_method_a as *const () as usize;
    t[139] = jni_call_static_double_method_v as *const () as usize;
    t[140] = jni_call_static_double_method_a as *const () as usize;
    t[142] = jni_call_static_void_method_v as *const () as usize;
    t[143] = jni_call_static_void_method_a as *const () as usize;

    // Instance field access
    t[94] = jni_get_field_id as *const () as usize;
    t[95] = jni_get_object_field as *const () as usize;
    t[96] = jni_get_boolean_field as *const () as usize;
    t[97] = jni_get_byte_field as *const () as usize;
    t[98] = jni_get_char_field as *const () as usize;
    t[99] = jni_get_short_field as *const () as usize;
    t[100] = jni_get_int_field as *const () as usize;
    t[101] = jni_get_long_field as *const () as usize;
    t[102] = jni_get_float_field as *const () as usize;
    t[103] = jni_get_double_field as *const () as usize;
    t[104] = jni_set_object_field as *const () as usize;
    t[105] = jni_set_boolean_field as *const () as usize;
    t[106] = jni_set_byte_field as *const () as usize;
    t[107] = jni_set_char_field as *const () as usize;
    t[108] = jni_set_short_field as *const () as usize;
    t[109] = jni_set_int_field as *const () as usize;
    t[110] = jni_set_long_field as *const () as usize;
    t[111] = jni_set_float_field as *const () as usize;
    t[112] = jni_set_double_field as *const () as usize;

    // Static method IDs
    t[113] = jni_get_static_method_id as *const () as usize;

    // Static field access
    t[144] = jni_get_static_field_id as *const () as usize;
    t[145] = jni_get_static_object_field as *const () as usize;
    t[146] = jni_get_static_boolean_field as *const () as usize;
    t[147] = jni_get_static_byte_field as *const () as usize;
    t[148] = jni_get_static_char_field as *const () as usize;
    t[149] = jni_get_static_short_field as *const () as usize;
    t[150] = jni_get_static_int_field as *const () as usize;
    t[151] = jni_get_static_long_field as *const () as usize;
    t[152] = jni_get_static_float_field as *const () as usize;
    t[153] = jni_get_static_double_field as *const () as usize;
    t[154] = jni_set_static_object_field as *const () as usize;
    t[155] = jni_set_static_boolean_field as *const () as usize;
    t[156] = jni_set_static_byte_field as *const () as usize;
    t[157] = jni_set_static_char_field as *const () as usize;
    t[158] = jni_set_static_short_field as *const () as usize;
    t[159] = jni_set_static_int_field as *const () as usize;
    t[160] = jni_set_static_long_field as *const () as usize;
    t[161] = jni_set_static_float_field as *const () as usize;
    t[162] = jni_set_static_double_field as *const () as usize;

    // String operations
    t[163] = jni_new_string as *const () as usize;
    t[164] = jni_get_string_length as *const () as usize;
    t[165] = jni_get_string_chars as *const () as usize;
    t[166] = jni_release_string_chars as *const () as usize;
    t[167] = jni_new_string_utf as *const () as usize;
    t[168] = jni_get_string_utf_length as *const () as usize;
    t[169] = jni_get_string_utf_chars as *const () as usize;
    t[170] = jni_release_string_utf_chars as *const () as usize;

    // Array operations
    t[171] = jni_get_array_length as *const () as usize;
    t[172] = jni_new_object_array as *const () as usize;
    t[173] = jni_get_object_array_element as *const () as usize;
    t[174] = jni_set_object_array_element as *const () as usize;
    t[175] = jni_new_boolean_array as *const () as usize;
    t[176] = jni_new_byte_array as *const () as usize;
    t[177] = jni_new_char_array as *const () as usize;
    t[178] = jni_new_short_array as *const () as usize;
    t[179] = jni_new_int_array as *const () as usize;
    t[180] = jni_new_long_array as *const () as usize;
    t[181] = jni_new_float_array as *const () as usize;
    t[182] = jni_new_double_array as *const () as usize;
    t[183] = jni_get_boolean_array_elements as *const () as usize;
    t[184] = jni_get_byte_array_elements as *const () as usize;
    t[185] = jni_get_char_array_elements as *const () as usize;
    t[186] = jni_get_short_array_elements as *const () as usize;
    t[187] = jni_get_int_array_elements as *const () as usize;
    t[188] = jni_get_long_array_elements as *const () as usize;
    t[189] = jni_get_float_array_elements as *const () as usize;
    t[190] = jni_get_double_array_elements as *const () as usize;
    t[191] = jni_release_boolean_array_elements as *const () as usize;
    t[192] = jni_release_byte_array_elements as *const () as usize;
    t[193] = jni_release_char_array_elements as *const () as usize;
    t[194] = jni_release_short_array_elements as *const () as usize;
    t[195] = jni_release_int_array_elements as *const () as usize;
    t[196] = jni_release_long_array_elements as *const () as usize;
    t[197] = jni_release_float_array_elements as *const () as usize;
    t[198] = jni_release_double_array_elements as *const () as usize;
    t[199] = jni_get_boolean_array_region as *const () as usize;
    t[200] = jni_get_byte_array_region as *const () as usize;
    t[201] = jni_get_char_array_region as *const () as usize;
    t[202] = jni_get_short_array_region as *const () as usize;
    t[203] = jni_get_int_array_region as *const () as usize;
    t[204] = jni_get_long_array_region as *const () as usize;
    t[205] = jni_get_float_array_region as *const () as usize;
    t[206] = jni_get_double_array_region as *const () as usize;
    t[207] = jni_set_boolean_array_region as *const () as usize;
    t[208] = jni_set_byte_array_region as *const () as usize;
    t[209] = jni_set_char_array_region as *const () as usize;
    t[210] = jni_set_short_array_region as *const () as usize;
    t[211] = jni_set_int_array_region as *const () as usize;
    t[212] = jni_set_long_array_region as *const () as usize;
    t[213] = jni_set_float_array_region as *const () as usize;
    t[214] = jni_set_double_array_region as *const () as usize;

    // RegisterNatives / UnregisterNatives
    t[215] = jni_register_natives as *const () as usize;
    t[216] = jni_unregister_natives as *const () as usize;

    // Synchronization
    t[217] = jni_monitor_enter as *const () as usize;
    t[218] = jni_monitor_exit as *const () as usize;

    // JavaVM
    t[219] = jni_get_java_vm as *const () as usize;

    // String region operations
    t[220] = jni_get_string_region as *const () as usize;
    t[221] = jni_get_string_utf_region as *const () as usize;

    // Critical array / string operations
    t[222] = jni_get_primitive_array_critical as *const () as usize;
    t[223] = jni_release_primitive_array_critical as *const () as usize;
    t[224] = jni_get_string_critical as *const () as usize;
    t[225] = jni_release_string_critical as *const () as usize;

    // Weak global references
    t[226] = jni_new_weak_global_ref as *const () as usize;
    t[227] = jni_delete_weak_global_ref as *const () as usize;

    // Exception check
    t[228] = jni_exception_check as *const () as usize;

    // NIO Direct ByteBuffer (JNI 1.4)
    t[229] = jni_new_direct_byte_buffer as *const () as usize;
    t[230] = jni_get_direct_buffer_address as *const () as usize;
    t[231] = jni_get_direct_buffer_capacity as *const () as usize;

    // GetObjectRefType (JNI 1.6)
    t[232] = jni_get_object_ref_type as *const () as usize;

    // GetModule (JNI 9+)
    t[233] = jni_get_module as *const () as usize;

    // IsVirtualThread (JNI 21+) and GetStringUTFLengthAsLong (JNI 24+). Both
    // are declared by JDK 25's `jni.h`, so a native compiled against it can
    // load either slot; before the table was widened those two loads read off
    // the end of the array.
    t[234] = jni_is_virtual_thread as *const () as usize;
    t[235] = jni_get_string_utf_length_as_long as *const () as usize;

    Box::new(t)
}

// ---------------------------------------------------------------------------
// JavaVM Invocation Interface
// ---------------------------------------------------------------------------

/// `jint DestroyJavaVM(JavaVM *vm)` — invocation-table slot 3.
///
/// HotSpot semantics: the calling thread waits until it is the only remaining
/// non-daemon thread, then unloads the VM; after a successful return the VM may
/// not be used and (per the spec) cannot be recreated in the same process. We
/// honour the load-bearing part of that contract for an embedded VM: tear the VM
/// down by invoking the embedder-registered teardown hook (which drops the parked
/// `Vm`, releasing its `Arc<SharedVm>` / heap / threads, clears the calling
/// thread's JNI TLS context, and resets the one-VM-per-process registry so
/// `JNI_GetCreatedJavaVMs` then reports 0).
///
/// We do NOT block waiting for other non-daemon threads: the embedding contract
/// (see `docs/feature-designs/embedding-api.md`) is that the host calls
/// `DestroyJavaVM` from the creating thread once it has quiesced its own Java
/// activity — mirroring how an embedder drives a single VM. Returns the hook's
/// status (`JNI_OK` when no hook is registered, i.e. nothing to tear down).
pub(in crate::native::jni) extern "C" fn jni_destroy_java_vm(_vm: JavaVM) -> JInt {
    run_destroy_vm_hook()
}

/// `jint GetEnv(JavaVM *vm, void **env, jint version)` — invocation-table
/// slot 6.
///
/// Interpreter round i1 wave 11, lane L1: a version naming another interface
/// than JNI — JVMTI (`0x30000000 | ...`, e.g. `JVMTI_VERSION_1_2`), or the
/// retired JVMPI / JVMDI — answers `JNI_EVERSION` with `*env` set to NULL,
/// as HotSpot does for an interface it does not provide. It answered the
/// `JNIEnv*`, so a library probing for JVMTI from `JNI_OnLoad` (async-profiler
/// does) got a JNI table and called through it as a `jvmtiEnv` — slot numbers
/// of one table taken as functions of the other. Wave 12: a supported JVMTI
/// version answers a C `jvmtiEnv` (`jvmti::native_env`) in
/// `experimental-debug` builds; JVMPI, JVMDI and other versions still answer
/// `JNI_EVERSION`.
pub(in crate::native::jni) extern "C" fn jni_get_env(_vm: JavaVM, env: *mut *mut std::ffi::c_void, version: JInt) -> JInt {
    if env.is_null() {
        return JNI_ERR;
    }
    if version & JNI_VERSION_INTERFACE_MASK != 0 {
        // Wave 12: a JVMTI version this VM provides gets a C `jvmtiEnv`
        // (`jvmti::native_env`, debug builds, where JVMTI lives).
        #[cfg(feature = "experimental-debug")]
        {
            if let Some(jvmti) = crate::jvmti::native_env::new_env_for_version(version) {
                // SAFETY: `env` is non-null (checked above) and points at
                // the caller's `void*` out-parameter.
                unsafe {
                    *env = jvmti;
                }
                return JNI_OK;
            }
        }
        // SAFETY: `env` is non-null (checked above) and points at the
        // caller's `void*` out-parameter.
        unsafe {
            *env = std::ptr::null_mut();
        }
        return JNI_EVERSION;
    }
    // Interpreter round i1 wave 14: a JNI version the VM does not provide is
    // `JNI_EVERSION` with a NULL env, as in HotSpot
    // (`Threads::is_supported_jni_version_including_1_1`); every value
    // answered the `JNIEnv*`.
    if !is_supported_jni_version(version) {
        // SAFETY: `env` is non-null (checked above) and points at the
        // caller's `void*` out-parameter.
        unsafe {
            *env = std::ptr::null_mut();
        }
        return JNI_EVERSION;
    }
    // gcf f2/j (`gcf-f1j-jni-getenv-on-an-unattached-thread-answers-jni-ok`):
    // an OS thread that is not attached answers `JNI_EDETACHED` and a NULL env,
    // as HotSpot's `jni_GetEnv` does (`Thread::current_or_null()` is no
    // `JavaThread`). The canonical callback idiom attaches on exactly this
    // answer; `JNI_OK` made a host thread call JNI functions with no thread
    // behind them. Both modes, both values of the foreign-attach gate: with
    // the gate off the attach that follows is env-only, as before. HotSpot
    // tests attachment before the version; a JVMTI version keeps its answer
    // above (an agent's `GetEnv` from its load hook runs before this VM has
    // registered every thread).
    if !current_os_thread_is_vm_thread() {
        // SAFETY: `env` is non-null (checked above) and points at the
        // caller's `void*` out-parameter.
        unsafe {
            *env = std::ptr::null_mut();
        }
        return JNI_EDETACHED;
    }
    let jni_env = get_jni_env();
    // SAFETY: `env` is non-null (checked above) and points at the caller's
    // `void*` out-parameter.
    unsafe {
        *env = jni_env as *mut std::ffi::c_void;
    }
    JNI_OK
}

/// gcf f2/j: is the calling OS thread attached to a VM -- HotSpot's "is
/// `Thread::current_or_null()` a `JavaThread`"? Exact, and with no process
/// global of its own; cheapest test first:
///
/// 1. a JNI context is installed here (`JNI_SHARED_VM` / `JNI_THREAD`): a JNI
///    native dispatch, `JNI_OnLoad`, a JVMTI callback, or a thread that
///    attached and kept its context;
/// 2. this OS thread owns a foreign attachment ([`is_foreign_attached`]; an
///    idle one has withdrawn its OS tid from the registry);
/// 3. an alive registry entry of the process VM claims this OS tid
///    (`ThreadRegistry::current_os_tid_claimed`): every VM thread publishes
///    it at startup, every mount republishes it -- this is the VM thread
///    outside every JNI dispatch, e.g. C code an FFM downcall runs.
///
/// "Unknown" -- no VM, or a platform with no OS-tid backend -- answers `true`,
/// the answer every caller got before: a wrong `false` would make a library
/// attach a SECOND `JvmThread` for a VM thread, a wrong `true` only keeps the
/// old `JNI_OK`.
pub(in crate::native::jni) fn current_os_thread_is_vm_thread() -> bool {
    #[cfg(test)]
    {
        if let Some(on) = JNI_NATIVE_CALL
            .try_with(|t| t.vm_thread_override.get())
            .ok()
            .flatten()
        {
            return on;
        }
    }
    let has_context = JNI_SHARED_VM
        .try_with(|c| c.try_borrow().map_or(true, |b| b.is_some()))
        .unwrap_or(false);
    let has_thread = !JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut())
        .is_null();
    let env_only = JNI_NATIVE_CALL
        .try_with(|t| t.env_only_attached.get())
        .unwrap_or(false);
    if has_context || has_thread || env_only || is_foreign_attached() {
        return true;
    }
    vm_registry_claims_current_os_thread().unwrap_or(true)
}

/// gcf f2/j: step 3 of [`current_os_thread_is_vm_thread`] alone: `None` when
/// there is no VM or the platform cannot tell.
pub(in crate::native::jni) fn vm_registry_claims_current_os_thread() -> Option<bool> {
    process_vm()?
        .threads
        .thread_registry
        .current_os_tid_claimed()
}

/// The JNI versions JDK 25's HotSpot accepts from `GetEnv`: 1.1, 1.2, 1.4,
/// 1.6, 1.8, 9, 10, 19, 20, 21 and 24 (`jni.h`'s `JNI_VERSION_*`).
pub(crate) fn is_supported_jni_version(version: JInt) -> bool {
    matches!(
        version,
        0x0001_0001
            | 0x0001_0002
            | 0x0001_0004
            | 0x0001_0006
            | 0x0001_0008
            | 0x0009_0000
            | 0x000a_0000
            | 0x0013_0000
            | 0x0014_0000
            | 0x0015_0000
            | 0x0018_0000
    )
}

/// Write the process-global `JNIEnv*` into `*penv`, if non-null.
///
/// `JNIEnv` is a pointer-to-pointer-to-function-table (`*const *const usize`):
/// native code dereferences it twice — `(*env)[slot]`. We therefore hand back
/// [`get_jni_env`] (`&JNI_TABLE_PTR`), NOT the table-array pointer
/// `JNI_TABLE_PTR.load()` itself, which is one indirection too shallow and would
/// make `(*env)[slot]` read a function's code bytes as a slot pointer (the
/// historical `AttachCurrentThread` stub had this bug, harmless only because no
/// caller ever drove the table through the attach env).
pub(in crate::native::jni) fn write_jni_env(penv: *mut *mut std::ffi::c_void) {
    if penv.is_null() {
        return;
    }
    let env = get_jni_env();
    unsafe {
        *penv = env as *mut std::ffi::c_void;
    }
}

pub(in crate::native::jni) extern "C" fn jni_attach_current_thread(
    vm: JavaVM,
    penv: *mut *mut std::ffi::c_void,
    args: *mut std::ffi::c_void,
) -> JInt {
    attach_current_thread_impl(vm, penv, args, false)
}

/// `AttachCurrentThreadAsDaemon` (invocation slot 7) — identical to
/// `AttachCurrentThread` except the registered thread is a **daemon** (the VM's
/// non-daemon-wait at shutdown ignores it). Wired in Step 7.
pub(in crate::native::jni) extern "C" fn jni_attach_current_thread_as_daemon(
    vm: JavaVM,
    penv: *mut *mut std::ffi::c_void,
    args: *mut std::ffi::c_void,
) -> JInt {
    attach_current_thread_impl(vm, penv, args, true)
}

/// Shared body for `AttachCurrentThread` / `AttachCurrentThreadAsDaemon`.
///
/// When the foreign-attach gate is off (default until Step 7) this preserves the
/// historical env-only behaviour. When on, a genuinely foreign thread is
/// registered as a first-class GC-safe Java thread (see
/// `foreign-thread-attach.md`).
pub(in crate::native::jni) fn attach_current_thread_impl(
    vm: JavaVM,
    penv: *mut *mut std::ffi::c_void,
    args: *mut std::ffi::c_void,
    daemon: bool,
) -> JInt {
    // Already-attached fast path. A thread whose JNI context is set — the
    // bootstrap/creating thread (main, id 0), or a foreign thread re-attaching —
    // is a no-op that just hands back the env pointer (matches HotSpot: a
    // redundant AttachCurrentThread returns JNI_OK).
    let current_vm = JNI_SHARED_VM.with(|c| c.borrow().clone());
    if let Some(cur) = current_vm {
        if !vm.is_null() {
            match vm_for_java_vm(vm) {
                Some(target) if Arc::ptr_eq(&cur, &target) => {}
                _ => return JNI_ERR,
            }
        }
        write_jni_env(penv);
        tracing::trace!("JNI AttachCurrentThread: thread already attached");
        return JNI_OK;
    }

    if foreign_attach_enabled() {
        // Repair case: this OS thread still OWNS a foreign JvmThread but its JNI
        // context was cleared (a nested true-JNI-native call's guard clears the
        // TLS context on return). Re-publish rather than attaching a second time
        // (which would leak a JvmThread and inflate alive_count).
        if is_foreign_attached() {
            // The attachment's own VM (gc-common w10-c), not the newest one.
            if let Some(shared) = foreign_attachment_vm() {
                set_jni_context_arc(shared);
            }
            with_foreign_thread(|jt| {
                // gci i9/jw: see `attach_foreign_thread_idle`'s publication.
                publish_local_frames_owner(&jt.gc_block_state);
                set_jni_thread(jt as *mut JvmThread)
            });
            write_jni_env(penv);
            return JNI_OK;
        }
        // gcf f2/j: a VM thread outside every JNI dispatch (C code an FFM
        // downcall runs, with no JNI context installed) is already attached:
        // HotSpot answers `JNI_OK` and attaches nothing. Registering it here
        // made a SECOND `JvmThread` for one OS thread (a second barrier
        // participant, a second `alive_count`). Same test `GetEnv` uses.
        if vm_registry_claims_current_os_thread() == Some(true) {
            write_jni_env(penv);
            tracing::debug!("JNI AttachCurrentThread: a VM thread, already attached");
            return JNI_OK;
        }
        // Genuine first attach: resolve the live VM named by `vm` (`JavaVM*`).
        let shared = match vm_for_java_vm(vm) {
            Some(s) => s,
            None => return JNI_ERR,
        };
        // SAFETY: `args`, when non-null, is a valid `JavaVMAttachArgs` per ABI.
        let (name, group) = unsafe { read_attach_args(args) };
        // JIT round 17 wave 6 (lane eintr2,
        // `CRATONVM_JNI_ATTACH_UNBLOCK_TAKEOVER_SIGNAL`): before the thread
        // joins the roster, so every take-over that can see it can reach it
        // (HotSpot's attach unblocks the same signal).
        crate::jit::xt_root_scan::unblock_takeover_signal_on_attach();
        // Register, go idle (§3.3), publish the TLS context last.
        let ledger = Arc::clone(shared.mem.gc_barrier.pause_ledger());
        #[cfg(feature = "experimental-debug")]
        let for_events = Arc::clone(&shared);
        attach_foreign_thread_idle(shared, daemon, name.as_deref());
        count_attachment_raw_locals(ledger);
        // Wave 85 (lane L6): the thread's `java.lang.Thread`, now, in the
        // attach's group -- HotSpot's `attach_current_thread` builds it
        // (`allocate_threadObj`) before it returns.
        build_foreign_thread_object(group);
        write_jni_env(penv);
        #[cfg(feature = "experimental-debug")]
        post_attached_thread_life(&for_events, true);
        tracing::debug!("JNI AttachCurrentThread: foreign thread attached + registered");
        return JNI_OK;
    }

    // Gate off — historical behaviour: env pointer only, no registration.
    // gcf f2/j: remembered, so `GetEnv` answers `JNI_OK` until the detach.
    let _ = JNI_NATIVE_CALL.try_with(|t| t.env_only_attached.set(true));
    write_jni_env(penv);
    tracing::debug!("JNI AttachCurrentThread: thread attached (env-only, gate off)");
    JNI_OK
}

/// Is a JNI native method of the thread this OS thread's JNI binding names on
/// its stack? `JniContextGuard::install_for_native` sets the thread's
/// `jni_native_class` for the dispatch and restores it at the return, so it
/// is `Some` from the outermost native's entry to its exit. `try_with`: the
/// detach can arrive from a TSD destructor with the TLS gone (then: no).
pub(in crate::native::jni) fn jni_native_frame_on_stack() -> bool {
    jni_native_dispatch_thread().is_some()
}

/// gcd d10/j: the `JvmThread` of the JNI native dispatch in progress on this
/// OS thread (`JniContextGuard::install_for_native` set its
/// `jni_native_class`), if any: a thread with live Java frames under a native.
pub(in crate::native::jni) fn jni_native_dispatch_thread() -> Option<*mut JvmThread> {
    let bound = JNI_THREAD
        .try_with(|c| c.get())
        .unwrap_or(std::ptr::null_mut());
    // SAFETY: a non-null `JNI_THREAD` points at a live `JvmThread` while it
    // is installed (`set_jni_thread`'s contract); one field is read.
    (!bound.is_null() && unsafe { (*(bound as *const JvmThread)).jni_native_class.is_some() })
        .then_some(bound as *mut JvmThread)
}

/// gcd d4/k2 (lane m's request 1, `gc_quiescence::note_raw_jni_locals_open`):
/// count this OS thread's just-made foreign attachment in its VM's pause
/// ledger as holding RAW JNI locals, for the life of the attachment, when its
/// locals are raw (`local_refs_may_be_held_raw`: unless both
/// `CRATONVM_JNI_INDIRECT_LOCALS` and `CRATONVM_JNI_FOREIGN_TRANSITIONS` are on,
/// an attached thread's handles are addresses its host code may keep across
/// idle windows) and anything reads the count ([`raw_locals_counted`]).
/// Remembered in the attachment's `ForeignThreadBox`, which closes it at the
/// detach or when the OS thread exits attached.
///
/// Only `AttachCurrentThread` counts: the VM's own attached service threads
/// (the AIO dispatcher, `run_attached_service`) are Rust and hold no handle
/// outside a Java call; a JNI native they reach through Java is counted by its
/// dispatch like any other.
pub(in crate::native::jni) fn count_attachment_raw_locals(ledger: Arc<cratonvm_gc::gc_quiescence::PauseLedger>) {
    if !local_refs_may_be_held_raw() {
        return;
    }
    count_attachment_known_raw_locals(ledger);
}

/// The raw-escape path already knows a raw handle was handed out. Avoid
/// resolving flags or borrowing the box again while recording that fact.
pub(in crate::native::jni) fn count_attachment_known_raw_locals(ledger: Arc<cratonvm_gc::gc_quiescence::PauseLedger>) {
    let _ = FOREIGN_THREAD_BOX.try_with(|c| {
        if let Ok(mut slot) = c.try_borrow_mut() {
            if let Some(bx) = slot.as_mut() {
                if bx.raw_locals_ledger.is_none() {
                    cratonvm_gc::gc_quiescence::note_raw_jni_locals_open(&ledger);
                    bx.raw_locals_ledger = Some(ledger);
                }
            }
        }
    });
}

/// JDWP `THREAD_START` / `THREAD_DEATH` for a thread that attached through
/// JNI `AttachCurrentThread` (`start`) or is detaching through
/// `DetachCurrentThread` (interpreter round i4 wave 84, lane L1). HotSpot's
/// `attach_current_thread` posts JVMTI `ThreadStart` and the detach's
/// `JavaThread::exit` `ThreadEnd`, which its JDWP back end reports like any
/// other thread's; only `Thread.start` reported them here, so a debugger
/// never saw a native library's callback thread come or go (and its
/// `ThreadStart` / `ThreadDeath` requests with `SUSPEND_EVENT_THREAD` never
/// stopped one). The VM's own attached service threads
/// (`run_attached_service`: the JDWP heap service, the AIO dispatcher) do not
/// come through here, as HotSpot's back end does not report its own threads.
/// Since wave 86 the C JVMTI table's `ThreadStart` / `ThreadEnd` for such a
/// thread are posted here too (`jvmti::native_env::post_attached_thread_event`).
/// A no-op without a listening agent or an attached debugger's matching
/// request.
#[cfg(feature = "experimental-debug")]
pub(in crate::native::jni) fn post_attached_thread_life(shared: &SharedVm, start: bool) {
    let Some(tid) = with_foreign_thread(|jt| jt.thread_id) else {
        return;
    };
    // Wave 86 (lane L1): a C agent's `ThreadStart` / `ThreadEnd` too, before
    // the JDWP event (the C table's envs come first in agent order here as
    // for a started thread).
    if shared.debug.debugger_gates.native_thread_events_armed() {
        crate::jvmti::native_env::post_attached_thread_event(shared, tid.0 as u64, start);
    }
    if !shared.debug.debugger_gates.session_attached() {
        return;
    }
    if start {
        crate::debug::send_thread_event(
            shared,
            crate::debug::events::EventKind::ThreadStart,
            tid.0 as u64,
        );
        return;
    }
    // Interpreter round i5 wave 107 (lane L1, page
    // `interpreter-i106-L1-jni-detach-posts-thread-death-after-join-FIXED-20261016`): a
    // `ThreadDeath` whose own event set suspended this thread
    // (`SUSPEND_EVENT_THREAD` / `SUSPEND_ALL`) holds the detach here until
    // the resume, before `ensure_join` (TERMINATED, the joiners' wake-up),
    // as HotSpot's `JavaThread::exit` and the started thread's end in
    // `vm_exec`. Only that event's suspension holds (the wave-106 hand-back
    // rule). The thread is idle-blocked between its JNI calls (GC-safe), so
    // it waits with no further transition.
    if crate::debug::send_thread_death_event(shared, tid.0 as u64) {
        crate::debug::hold_ending_thread_while_suspended(shared, tid.0 as u64);
    }
}

pub(in crate::native::jni) extern "C" fn jni_detach_current_thread(_vm: JavaVM) -> JInt {
    // Reachable from a `pthread` TSD destructor with this thread's TLS already
    // destroyed — see the note above `is_foreign_attached`. Nothing below may
    // use `LocalKey::with`.
    //
    // `is_foreign_attached()` answers `false` in that state, and the
    // never-attached tail below is `try_with`-safe too, so the whole function
    // degrades to "nothing to detach, report success" rather than panicking
    // out of an `extern "C"` frame.
    if is_foreign_attached() {
        // JNI forbids detaching a thread that still has Java frames on its stack;
        // for us that means a call is in flight on this OS thread (depth > 0).
        // Return JNI_ERR rather than corrupt state (matches HotSpot).
        if FOREIGN_CALL_DEPTH.try_with(|c| c.get()).unwrap_or(0) != 0 {
            tracing::warn!("JNI DetachCurrentThread: refusing detach with a Java call in flight");
            return JNI_ERR;
        }
        // The attachment's own VM (gc-common w10-c): `process_vm()` is the
        // newest VM, whose barrier this thread was never blocked in.
        if let Some(shared) = foreign_attachment_vm() {
            // Wave 84 (lane L6): `Thread.exit()` and `ensure_join`, as
            // HotSpot's detach; then the values and the registration go.
            // Wave 107 (lane L1): `JavaThread::exit`'s order -- `Thread.exit()`,
            // then `ThreadEnd` / `ThreadDeath` (and the hold while that event
            // set suspended the thread), then `ensure_join` -- so a joiner of
            // the attached thread wakes only after the debugger or the agent
            // heard of its death (it woke first, and the hold never happened).
            end_foreign_thread_java_side(false);
            #[cfg(feature = "experimental-debug")]
            post_attached_thread_life(&shared, false);
            end_foreign_thread_java_side(true);
            detach_foreign_thread_idle(&shared);
        } else {
            // No live VM (process shutdown) — just drop our owned box.
            let _ = FOREIGN_THREAD_BOX.try_with(|c| *c.borrow_mut() = None);
            let _ = FOREIGN_CALL_DEPTH.try_with(|c| c.set(0));
            if let Some(base) = FOREIGN_ATTACH_FRAME.try_with(Cell::take).ok().flatten() {
                truncate_local_frames(base);
            }
            let _ = FOREIGN_ATTACH_VM.try_with(|c| c.try_borrow_mut().map(|mut w| w.take()));
        }
        clear_jni_thread();
        clear_jni_context();
        tracing::debug!("JNI DetachCurrentThread: foreign thread detached");
        return JNI_OK;
    }

    // gcd d3/k: a VM thread calling from inside a JNI native method has Java
    // frames below the native. HotSpot refuses (`JNI_ERR`) and detaches
    // nothing; this cleared the JNI context instead, so every JNIEnv call the
    // native made after it was answered NULL / 0 (`with_jni_context` found no
    // thread), and the dispatch's `JniContextGuard` put it back only at the
    // native's return.
    if jni_native_frame_on_stack() {
        tracing::warn!("JNI DetachCurrentThread: refusing detach from inside a native method");
        return JNI_ERR;
    }

    // gcf f2/j: an env-only attachment ends here (`try_with`: see above).
    let _ = JNI_NATIVE_CALL.try_with(|t| t.env_only_attached.set(false));
    // Non-foreign thread (bootstrap/creating thread, or never-attached): clear
    // the JNI TLS context if present — historical behaviour, unchanged.
    let had_context = JNI_SHARED_VM
        .try_with(|c| c.borrow().is_some())
        .unwrap_or(false);
    if had_context {
        clear_jni_context();
        clear_jni_thread();
        tracing::debug!("JNI DetachCurrentThread: thread detached");
    } else {
        tracing::trace!("JNI DetachCurrentThread: thread was not attached");
    }
    JNI_OK
}

pub(in crate::native::jni) fn build_invoke_table() -> Box<[usize; JNI_INVOKE_FUNCTION_COUNT]> {
    let stub = jni_stub as *const () as usize;
    let mut t = [stub; JNI_INVOKE_FUNCTION_COUNT];
    t[3] = jni_destroy_java_vm as *const () as usize;
    t[4] = jni_attach_current_thread as *const () as usize;
    t[5] = jni_detach_current_thread as *const () as usize;
    t[6] = jni_get_env as *const () as usize;
    t[7] = jni_attach_current_thread_as_daemon as *const () as usize; // AttachCurrentThreadAsDaemon
    Box::new(t)
}

// ---------------------------------------------------------------------------
// Global table storage
// ---------------------------------------------------------------------------

// The two JNI function tables are leaked, process-lifetime singletons. They are
// stored in `AtomicPtr`s rather than `static mut` so that reads/writes are
// well-defined under concurrent access (raw `static mut` access is UB-adjacent
// on the 2024 edition). `JNI_TABLE_INIT` (a `Once`) still guarantees the values
// are written exactly once; the atomics only make the publication well-defined.
pub(in crate::native::jni) static JNI_TABLE_PTR: std::sync::atomic::AtomicPtr<usize> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
pub(in crate::native::jni) static JNI_INVOKE_TABLE_PTR: std::sync::atomic::AtomicPtr<usize> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
pub(in crate::native::jni) static JNI_TABLE_INIT: std::sync::Once = std::sync::Once::new();

pub(in crate::native::jni) fn init_jni_table() {
    use std::sync::atomic::Ordering;
    JNI_TABLE_INIT.call_once(|| {
        let table = build_function_table();
        let raw = Box::into_raw(table) as *mut usize; // OWNERSHIP: transferred to JNI_TABLE_PTR static; intentionally leaked (process-lifetime singleton)
        let invoke_table = build_invoke_table();
        let invoke_raw = Box::into_raw(invoke_table) as *mut usize; // OWNERSHIP: transferred to JNI_INVOKE_TABLE_PTR static; intentionally leaked (process-lifetime singleton)
        JNI_TABLE_PTR.store(raw, Ordering::Release);
        JNI_INVOKE_TABLE_PTR.store(invoke_raw, Ordering::Release);
    });
}

/// Get a JNIEnv pointer.
/// JNIEnv = `*const *const usize` — a pointer to a pointer to the function table.
pub fn get_jni_env() -> JNIEnv {
    init_jni_table();
    // `AtomicPtr<usize>` has the same layout as `*mut usize`, so the address of
    // the atomic itself is a valid `*const *const usize` pointing at the table
    // pointer published by `init_jni_table`.
    std::ptr::addr_of!(JNI_TABLE_PTR).cast()
}

/// The process's JNI function table, [`JNI_FUNCTION_COUNT`] entries, which
/// every `JNIEnv*` this VM hands out points at: JVMTI
/// `GetJNIFunctionTable` copies it and `SetJNIFunctionTable` overwrites it
/// in place (interpreter round i2 wave 63, lane L1), as HotSpot copies an
/// agent's table into its one `jni_NativeInterface`.
pub(crate) fn jni_function_table() -> *mut usize {
    init_jni_table();
    JNI_TABLE_PTR.load(std::sync::atomic::Ordering::Acquire)
}

/// Get a JavaVM pointer for the current thread's VM (or the process VM).
pub fn get_java_vm() -> JavaVM {
    init_jni_table();
    if let Some(shared) = JNI_SHARED_VM
        .try_with(|c| c.borrow().clone())
        .ok()
        .flatten()
    {
        return java_vm_for(&shared);
    }
    if let Some(shared) = process_vm() {
        return java_vm_for(&shared);
    }
    std::ptr::addr_of!(JNI_INVOKE_TABLE_PTR).cast()
}

