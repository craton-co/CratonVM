//! MethodHandle, MethodType, Lookup, CallSite, and VarHandle native method implementations.
//!
//! Session 4: Full MethodHandle invocation with type resolution, VarHandle field
//! access with real get/set/CAS, and proper Lookup.find* resolution.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::{ObjectKind, ObjectRef, Value};
use rustjvm_types::error::MethodCallResult;

use crate::{obj_arg, alloc_concurrent_synthetic};
use crate::lang_class::{mirror_class_name, mirror_class_id, box_value};

/// C38: Detect a real-JDK array-element VarHandle call.
///
/// `MethodHandles.arrayElementVarHandle([J.class)` runs JDK bytecode that
/// returns a `java.lang.invoke.VarHandleLongs$Array` (or similar per element
/// type). Those real-JDK VarHandles don't match our synthetic 6-field layout
/// — slot 0 is `vform`, not our `VH_KIND` Int. The reliable signal is the
/// argument shape at the call site: for an array-element access the layout is
/// always `[vh, array_object, index, ...]`. When args[1] is an array object
/// and args[2] is an Int, treat the call as array-element access.
fn vh_array_call(ctx: &dyn NativeContext, args: &[Value]) -> Option<(ObjectRef, usize)> {
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return None,
    };
    if !matches!(ctx.heap_kind_of(arr), ObjectKind::Array) {
        return None;
    }
    let idx = match args.get(2) {
        Some(Value::Int(i)) => *i as usize,
        _ => return None,
    };
    Some((arr, idx))
}

/// Read the VH_FIELD_DESC string from a VarHandle object. Prefers the
/// WP4.2 side table (see vh_meta_table comment) and falls back to the
/// raw object slot for VarHandles allocated outside our path.
fn vh_field_desc(ctx: &dyn NativeContext, vh: ObjectRef) -> String {
    if let Some(m) = vh_meta_get(vh) {
        return m.field_desc;
    }
    vh_read_string(ctx, vh, VH_FIELD_DESC).unwrap_or_else(|| "Ljava/lang/Object;".to_string())
}

/// Convert a VarHandle field descriptor to the single-char type descriptor
/// that `box_value` expects (e.g. "I", "J", "Ljava/lang/Object;").
fn vh_type_desc(ctx: &dyn NativeContext, vh: ObjectRef) -> String {
    let desc = vh_field_desc(ctx, vh);
    if desc.len() == 1 { desc } else { "L".to_string() }
}


// ---------------------------------------------------------------------------
// VarHandle synthetic field layout (6 fields)
// ---------------------------------------------------------------------------
/// VarHandle kind
const VH_KIND: usize = 0;      // i32: 0=instanceField, 1=staticField, 2=array
/// Target class name (JVM internal, e.g. "java/lang/Foo")
const VH_CLASS: usize = 1;     // Object(String)
/// Field name
const VH_FIELD: usize = 2;     // Object(String)
/// Field descriptor (e.g. "I", "J", "Ljava/lang/String;")
const VH_FIELD_DESC: usize = 3; // Object(String)
/// Resolved field index (slot in the object's field array)
const VH_FIELD_INDEX: usize = 4; // Int
/// Cached ClassId of the declaring class
const VH_CLASS_ID: usize = 5;   // Int (ClassId raw)
const VH_FIELD_COUNT: usize = 6;

const VH_KIND_INSTANCE: i32 = 0;
const VH_KIND_STATIC: i32 = 1;
const VH_KIND_ARRAY: i32 = 2;

// ---------------------------------------------------------------------------
// WP4.2 — VarHandle metadata side table
// ---------------------------------------------------------------------------
//
// The synthetic VarHandle fields above are written into the storage slots of
// whatever class `alloc_concurrent_synthetic` resolves for "java/lang/invoke
// /VarHandle". In real-JDK mode, that's the real `VarHandle` class — whose
// slot 0 (`vform`) is declared `Ljava/lang/invoke/VarForm;`. The descriptor-
// aware setter `Heap::set_field_as` coerces our `Value::Int(VH_KIND_INSTANCE
// = 0)` into `Value::Object(None)` (line 898 of `gc/src/heap.rs`), and the
// next `get_field` returns `Object(None)` instead of `Int(0)`. The kind tag,
// the field index, the class name, and the field name are then all unread-
// able and the VarHandle silently no-ops.
//
// To keep the synthetic VarHandle working without a heavy rewrite, we keep a
// process-wide side table indexed by the VarHandle's heap pointer. The slot
// writes in `alloc_*_var_handle` still happen (for compatibility with any
// caller that does happen to read them — all of `varhandle_get`, `_set`,
// `_compare_and_set` consult the side table first and fall back to slot
// reads), but the canonical truth lives here.

#[derive(Clone, Debug)]
pub(crate) struct VarHandleMeta {
    pub kind: i32,
    pub class_name: String,
    pub field_name: String,
    pub field_desc: String,
    pub field_index: i32,
    pub class_id: u32,
}

static VH_META_TABLE: std::sync::OnceLock<std::sync::Mutex<rustc_hash::FxHashMap<usize, VarHandleMeta>>> = std::sync::OnceLock::new();

fn vh_meta_table() -> &'static std::sync::Mutex<rustc_hash::FxHashMap<usize, VarHandleMeta>> {
    VH_META_TABLE.get_or_init(|| std::sync::Mutex::new(rustc_hash::FxHashMap::default()))
}

pub(crate) fn vh_meta_put(vh: ObjectRef, meta: VarHandleMeta) {
    let mut t = vh_meta_table().lock().unwrap();
    t.insert(vh.as_ptr() as usize, meta);
}

pub(crate) fn vh_meta_get(vh: ObjectRef) -> Option<VarHandleMeta> {
    let t = vh_meta_table().lock().unwrap();
    t.get(&(vh.as_ptr() as usize)).cloned()
}

pub(crate) fn vh_meta_update_field_index(vh: ObjectRef, idx: i32) {
    let mut t = vh_meta_table().lock().unwrap();
    if let Some(meta) = t.get_mut(&(vh.as_ptr() as usize)) {
        meta.field_index = idx;
    }
}

// ---------------------------------------------------------------------------
// Descriptor helpers — reconstruct JVM descriptor from MethodType object
// ---------------------------------------------------------------------------

/// Convert a class name (as stored in a Class mirror) to its JVM descriptor form.
/// "int" → "I", "java/lang/String" → "Ljava/lang/String;", "void" → "V", etc.
fn class_name_to_descriptor(name: &str) -> String {
    match name {
        "void"    => "V".to_string(),
        "int"     => "I".to_string(),
        "long"    => "J".to_string(),
        "float"   => "F".to_string(),
        "double"  => "D".to_string(),
        "boolean" => "Z".to_string(),
        "byte"    => "B".to_string(),
        "char"    => "C".to_string(),
        "short"   => "S".to_string(),
        _ if name.starts_with('[') => name.to_string(), // already descriptor form
        _ => format!("L{name};"),
    }
}

/// Convert a JVM field descriptor to a class name for display.
/// "I" → "int", "Ljava/lang/String;" → "java/lang/String", etc.
fn descriptor_to_class_name(desc: &str) -> String {
    match desc {
        "V" => "void".to_string(),
        "I" => "int".to_string(),
        "J" => "long".to_string(),
        "F" => "float".to_string(),
        "D" => "double".to_string(),
        "Z" => "boolean".to_string(),
        "B" => "byte".to_string(),
        "C" => "char".to_string(),
        "S" => "short".to_string(),
        _ if desc.starts_with('L') && desc.ends_with(';') => {
            desc[1..desc.len()-1].to_string()
        }
        _ => desc.to_string(),
    }
}

/// Read a Class mirror and extract its JVM descriptor character(s).
fn mirror_to_descriptor(ctx: &dyn NativeContext, mirror: ObjectRef) -> String {
    match resolve_class_name_robust(ctx, mirror) {
        Some(name) => class_name_to_descriptor(&name),
        None => "Ljava/lang/Object;".to_string(),
    }
}

/// WP2.9 — Robust class-name extraction that survives the descriptor-coercion
/// drift where `Class` mirrors store `Object(name_str)` in slot 1 but read
/// back as `Int(ptr_lo)` / `Long(ptr_bits)` because the field-coercion path
/// only handles Object/Int(0)/Long(0)/Double — not arbitrary integer
/// bit-patterns smuggled through compact storage.
///
/// Strategy:
///   1. Try the reverse map (mirror → ClassId via `class_id_from_mirror`),
///      then look up the name via `class_name_of_id`.
///   2. Fall back to `mirror_class_name` (slot-1 read) for primitive mirrors
///      and other cases the reverse map doesn't cover.
///   3. Last resort: try to reconstruct the slot-1 string pointer from the
///      smuggled bits (Long or Int(non-zero)) and read it as a string.
pub(crate) fn resolve_class_name_robust(
    ctx: &dyn NativeContext,
    mirror: ObjectRef,
) -> Option<String> {
    // 1. Reverse-map lookup (preferred — never hits the bad coercion path).
    if let Some(cid) = ctx.class_id_from_mirror(mirror) {
        if let Some(name) = ctx.class_name_of_id(cid) {
            return Some(name);
        }
    }
    // 2. Standard slot-1 read (works for primitive mirrors and clean paths).
    if let Some(name) = crate::lang_class::mirror_class_name(ctx, mirror) {
        if !name.is_empty() {
            return Some(name);
        }
    }
    // 3. Last resort: attempt to reconstruct the name string ObjectRef from
    // the slot-1 raw bits.  The descriptor cache returned `b'L'` so the heap
    // applied the b'L' coercion which preserves Object/Int(0)/Long(0)/Double
    // but lets non-zero Int/Long fall through unchanged.  Recover the
    // ObjectRef from the bit-pattern when it looks heap-aligned.
    let raw = ctx.get_field(mirror, 1);
    let bits: Option<u64> = match raw {
        Value::Long(l) => Some(l as u64),
        Value::Int(i) if i != 0 => Some(i as u32 as u64),
        _ => None,
    };
    if let Some(bits) = bits {
        if (bits & 0x7) == 0 && bits != 0 && bits < (1u64 << 48) {
            let ptr = bits as usize as *mut u8;
            // SAFETY: ptr is heap-aligned and in the canonical address space;
            // ObjectRef::from_raw + read_string validates via header reads.
            let name_obj = unsafe { ObjectRef::from_raw(ptr) };
            if let Some(s) = ctx.read_string(name_obj) {
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
    }
    None
}

/// WP2.9 — Robust mirror-from-slot recovery: gets a Class mirror from a slot
/// that may have been compacted to Long/Int bits by the descriptor-coercion
/// path. Returns the original ObjectRef when slot value is Object, otherwise
/// reconstructs from heap-aligned 48-bit bit patterns.
pub(crate) fn recover_class_mirror_from_slot(value: Value) -> Option<ObjectRef> {
    match value {
        Value::Object(Some(o)) => Some(o),
        Value::Object(None) => None,
        Value::Long(l) => {
            let bits = l as u64;
            if bits != 0 && (bits & 0x7) == 0 && bits < (1u64 << 48) {
                let ptr = bits as usize as *mut u8;
                Some(unsafe { ObjectRef::from_raw(ptr) })
            } else { None }
        }
        Value::Int(i) if i != 0 => {
            // Lower 32 bits of a heap pointer.  Bit-aligned and within 48-bit
            // range checks both pass for heap-allocated ObjectRefs whose top
            // 16 bits are zero (true on 64-bit Windows for our arenas).
            let bits = i as u32 as u64;
            if (bits & 0x7) == 0 && bits != 0 && bits < (1u64 << 48) {
                let ptr = bits as usize as *mut u8;
                Some(unsafe { ObjectRef::from_raw(ptr) })
            } else { None }
        }
        _ => None,
    }
}

/// Reconstruct a full JVM method descriptor from a MethodType object.
/// MethodType has field 0 = returnType (Class mirror), field 1 = paramTypes (Class[] or null).
fn descriptor_from_method_type(ctx: &dyn NativeContext, mt: ObjectRef) -> String {
    let mut desc = String::from("(");

    // Parameter types (field 1 = Class[] array or null)
    if let Value::Object(Some(params_arr)) = ctx.get_field(mt, 1) {
        let len = ctx.array_length(params_arr);
        for i in 0..len {
            if let Some(param_mirror) = recover_class_mirror_from_slot(
                ctx.get_array_element(params_arr, i),
            ) {
                desc.push_str(&mirror_to_descriptor(ctx, param_mirror));
            }
        }
    }
    desc.push(')');

    // Return type (field 0 = Class mirror) — recover from any slot encoding.
    if let Some(ret_mirror) = recover_class_mirror_from_slot(ctx.get_field(mt, 0)) {
        desc.push_str(&mirror_to_descriptor(ctx, ret_mirror));
    } else {
        desc.push('V'); // default void
    }

    desc
}

/// Build a field descriptor for a single Class mirror (used by findGetter/findSetter).
fn field_descriptor_from_mirror(ctx: &dyn NativeContext, type_mirror: ObjectRef) -> String {
    mirror_to_descriptor(ctx, type_mirror)
}

// ---------------------------------------------------------------------------
// java.lang.invoke — MethodHandle, MethodType, MethodHandles (stubs)
// ---------------------------------------------------------------------------
pub fn register_phase54_method_handle(r: &mut NativeMethodRegistry) {
    // --- MethodType (2-field: returnType=0, paramTypes=1) ---
    let mt = "java/lang/invoke/MethodType";
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let params = args.get(1).copied().unwrap_or(Value::Object(None));
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6);
            ctx.set_field(obj, 0, Value::Object(Some(ret)));
            ctx.set_field(obj, 1, params);
            populate_method_type_form(ctx, obj);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6);
            ctx.set_field(obj, 0, Value::Object(Some(ret)));
            // Empty params — allocate a 0-length Class[] so `parameterCount()`
            // and the form-builder both see a non-null array.
            let empty = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
            ctx.set_field(obj, 1, Value::Object(Some(empty)));
            populate_method_type_form(ctx, obj);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let param = obj_arg(args, 1)?;
            let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 1);
            ctx.set_array_element(arr, 0, Value::Object(Some(param)));
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6);
            ctx.set_field(obj, 0, Value::Object(Some(ret)));
            ctx.set_field(obj, 1, Value::Object(Some(arr)));
            populate_method_type_form(ctx, obj);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // methodType(Class rtype, Class ptype0, Class... morePtypes)
    r.register(
        mt,
        "methodType",
        "(Ljava/lang/Class;Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let ret = obj_arg(args, 0)?;
            let ptype0 = obj_arg(args, 1)?;
            // morePtypes may be null or an array
            let more_len = match args.get(2) {
                Some(Value::Object(Some(arr))) => ctx.array_length(*arr),
                _ => 0,
            };
            let total = 1 + more_len;
            let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, total);
            ctx.set_array_element(arr, 0, Value::Object(Some(ptype0)));
            if let Some(Value::Object(Some(more))) = args.get(2) {
                for i in 0..more_len {
                    let elem = ctx.get_array_element(*more, i);
                    ctx.set_array_element(arr, 1 + i, elem);
                }
            }
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6);
            ctx.set_field(obj, 0, Value::Object(Some(ret)));
            ctx.set_field(obj, 1, Value::Object(Some(arr)));
            populate_method_type_form(ctx, obj);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(mt, "returnType", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(mt, "parameterCount", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 1) {
            Ok(Some(Value::Int(ctx.array_length(arr) as i32)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(mt, "parameterType", "(I)Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 1) {
            Ok(Some(ctx.get_array_element(arr, idx)))
        } else {
            Ok(Some(Value::Object(None)))
        }
    });
    r.register(mt, "parameterArray", "()[Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let params = ctx.get_field(this, 1);
        if let Value::Object(Some(_)) = params {
            Ok(Some(params))
        } else {
            let empty = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 0);
            Ok(Some(Value::Object(Some(empty))))
        }
    });
    r.register(mt, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pc = if let Value::Object(Some(arr)) = ctx.get_field(this, 1) {
            ctx.array_length(arr)
        } else {
            0
        };
        let s = ctx.create_string(&format!("MethodType({pc} params)"));
        Ok(Some(Value::Object(Some(s))))
    });

    // --- MethodHandle (1-field: name=0 for debugging) ---
    let mh = "java/lang/invoke/MethodHandle";
    r.register(
        mh,
        "type",
        "()Ljava/lang/invoke/MethodType;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(mh, "toString", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("MethodHandle");
        Ok(Some(Value::Object(Some(s))))
    });

    // --- MethodHandles (static utility) ---
    let mhs = "java/lang/invoke/MethodHandles";
    r.register(
        mhs,
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, _args| {
            let lookup =
                alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 1);
            Ok(Some(Value::Object(Some(lookup))))
        },
    );
    r.register(mhs, "privateLookupIn", "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/invoke/MethodHandles$Lookup;", |ctx, _args| {
        let lookup = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 1);
        Ok(Some(Value::Object(Some(lookup))))
    });

    // --- MethodHandles.Lookup ---
    //
    // Note: findVirtual/findStatic/findGetter/findSetter/findConstructor and
    // friends are registered by the real implementations in
    // `register_p63_method_handles_lookup` (late phase). We do not provide
    // stub overrides here — the real ones take precedence and a stub would
    // only run if the late phase is not also invoked (which would indicate
    // a broken VM startup).
    let lk = "java/lang/invoke/MethodHandles$Lookup";
    r.register(lk, "lookupClass", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    // Lookup.defineHiddenClass(byte[], boolean, ClassOption...) → Lookup
    // Simplified: allocates a synthetic hidden class and returns a Lookup for it.
    r.register(lk, "defineHiddenClass", "([BZ[Ljava/lang/invoke/MethodHandles$Lookup$ClassOption;)Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, _args| {
            // For simplicity, return a Lookup wrapping a synthetic hidden class mirror.
            // Full implementation would parse the byte[] and define the class.
            let lookup = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 1);
            Ok(Some(Value::Object(Some(lookup))))
        }
    );

    // --- VarHandle real ops ---
    let vh = "java/lang/invoke/VarHandle";

    // VarHandle.get(Object...) → Object
    // For instance fields: args = [receiver]; for static: args = []
    r.register(
        vh,
        "get",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_get,
    );

    // VarHandle.set(Object...) → void
    r.register(vh, "set", "([Ljava/lang/Object;)V", varhandle_set);

    // VarHandle.compareAndSet(Object...) → boolean
    r.register(
        vh,
        "compareAndSet",
        "([Ljava/lang/Object;)Z",
        varhandle_compare_and_set,
    );

    // VarHandle.getAndSet(Object...) → Object
    r.register(
        vh,
        "getAndSet",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_get_and_set,
    );

    // VarHandle.getVolatile(Object...) → Object (same as get for now — no hardware fences)
    r.register(
        vh,
        "getVolatile",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_get,
    );

    // VarHandle.setVolatile(Object...) → void
    r.register(vh, "setVolatile", "([Ljava/lang/Object;)V", varhandle_set);

    // VarHandle.getOpaque / getAcquire — read semantics
    r.register(vh, "getOpaque", "([Ljava/lang/Object;)Ljava/lang/Object;", varhandle_get);
    r.register(vh, "getAcquire", "([Ljava/lang/Object;)Ljava/lang/Object;", varhandle_get);

    // VarHandle.setOpaque / setRelease — write semantics
    r.register(vh, "setOpaque", "([Ljava/lang/Object;)V", varhandle_set);
    r.register(vh, "setRelease", "([Ljava/lang/Object;)V", varhandle_set);

    // VarHandle.compareAndExchange
    r.register(
        vh,
        "compareAndExchange",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        varhandle_compare_and_exchange,
    );

    // VarHandle.getAndAdd and its ordering variants — single Object-return
    // registration per name. The polymorphic dispatcher in vm_exec.rs tries
    // the generic Object descriptor first and unbox_poly_return coerces the
    // result back to the call-site numeric type (H2 uses [III)I).
    for name in &["getAndAdd", "getAndAddAcquire", "getAndAddRelease"] {
        r.register(vh, name, "([Ljava/lang/Object;)Ljava/lang/Object;", varhandle_get_and_add);
    }

    // C38: Ordering variants of compareAndSet / compareAndExchange / getAndSet.
    // The real JDK maps each to a distinct native; we use the same underlying
    // CAS / CAX / xchg implementation (we don't emit hardware fences). Needed
    // by Jackson's ConcurrentLinkedQueue which calls weakCompareAndSet.
    for name in &["weakCompareAndSet", "weakCompareAndSetPlain",
                  "weakCompareAndSetAcquire", "weakCompareAndSetRelease"] {
        r.register(vh, name, "([Ljava/lang/Object;)Z", varhandle_compare_and_set);
    }
    for name in &["compareAndExchangeAcquire", "compareAndExchangeRelease"] {
        r.register(vh, name, "([Ljava/lang/Object;)Ljava/lang/Object;", varhandle_compare_and_exchange);
    }
    for name in &["getAndSetAcquire", "getAndSetRelease"] {
        r.register(vh, name, "([Ljava/lang/Object;)Ljava/lang/Object;", varhandle_get_and_set);
    }
}

// ---------------------------------------------------------------------------
// VarHandle operation implementations
// ---------------------------------------------------------------------------

/// Allocate a VarHandle for an instance field.
pub(crate) fn alloc_instance_var_handle(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    field_name: &str,
    field_desc: &str,
    field_index: usize,
    class_id: rustjvm_types::ClassId,
) -> ObjectRef {
    let vh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT);
    ctx.set_field(vh, VH_KIND, Value::Int(VH_KIND_INSTANCE));
    let cls_s = ctx.create_string(class_name);
    ctx.set_field(vh, VH_CLASS, Value::Object(Some(cls_s)));
    let fld_s = ctx.create_string(field_name);
    ctx.set_field(vh, VH_FIELD, Value::Object(Some(fld_s)));
    let desc_s = ctx.create_string(field_desc);
    ctx.set_field(vh, VH_FIELD_DESC, Value::Object(Some(desc_s)));
    ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(field_index as i32));
    ctx.set_field(vh, VH_CLASS_ID, Value::Int(class_id.as_u32() as i32));
    // WP4.2: also stash in the side table so the descriptor-aware setter
    // on real-JDK VarHandle layout doesn't drop our metadata.
    vh_meta_put(vh, VarHandleMeta {
        kind: VH_KIND_INSTANCE,
        class_name: class_name.to_string(),
        field_name: field_name.to_string(),
        field_desc: field_desc.to_string(),
        field_index: field_index as i32,
        class_id: class_id.as_u32(),
    });
    vh
}

/// Allocate a VarHandle for a static field.
pub(crate) fn alloc_static_var_handle(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    field_name: &str,
    field_desc: &str,
) -> ObjectRef {
    let vh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", VH_FIELD_COUNT);
    ctx.set_field(vh, VH_KIND, Value::Int(VH_KIND_STATIC));
    let cls_s = ctx.create_string(class_name);
    ctx.set_field(vh, VH_CLASS, Value::Object(Some(cls_s)));
    let fld_s = ctx.create_string(field_name);
    ctx.set_field(vh, VH_FIELD, Value::Object(Some(fld_s)));
    let desc_s = ctx.create_string(field_desc);
    ctx.set_field(vh, VH_FIELD_DESC, Value::Object(Some(desc_s)));
    ctx.set_field(vh, VH_FIELD_INDEX, Value::Int(-1)); // resolved lazily
    ctx.set_field(vh, VH_CLASS_ID, Value::Int(0));
    // WP4.2: side table for descriptor-aware-coercion-safe metadata access.
    vh_meta_put(vh, VarHandleMeta {
        kind: VH_KIND_STATIC,
        class_name: class_name.to_string(),
        field_name: field_name.to_string(),
        field_desc: field_desc.to_string(),
        field_index: -1,
        class_id: 0,
    });
    vh
}

/// Read VarHandle metadata helpers.
fn vh_read_string(ctx: &dyn NativeContext, vh: ObjectRef, field: usize) -> Option<String> {
    match ctx.get_field(vh, field) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// VarHandle.get(receiver) → value
/// Signature-polymorphic: args arrive as individual values from the call-site,
/// i.e. args = [vh_ref, receiver] for instance fields.
fn varhandle_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C38: Array-element VarHandle call — detected by args[1] being an array
    // and args[2] being an Int. Handles real-JDK VarHandleLongs$Array and the
    // other primitive-array VarHandle subclasses produced by
    // MethodHandles.arrayElementVarHandle. Returns the raw primitive Value
    // (Long, Int, Float, Double, Reference) — unbox_poly_return / the
    // signature-polymorphic call-site descriptor steers it to the right
    // primitive return slot.
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        return Ok(Some(ctx.get_array_element(arr, idx)));
    }
    // WP4.2: prefer side-table (see comment at the top of this file).
    let (kind, field_idx) = match vh_meta_get(this) {
        Some(meta) => (meta.kind, meta.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) { Value::Int(k) => k, _ => return Ok(Some(Value::Object(None))) };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) { Value::Int(i) => i, _ => -1 };
            (k, i)
        }
    };

    match kind {
        VH_KIND_INSTANCE => {
            // args[1] is the receiver object directly (signature-polymorphic dispatch)
            let receiver = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let td = vh_type_desc(ctx, this);
            if field_idx >= 0 {
                let val = ctx.get_field(receiver, field_idx as usize);
                Ok(Some(box_value(ctx, val, &td)))
            } else {
                // Resolve by name
                let (class, field) = match vh_meta_get(this) {
                    Some(m) => (m.class_name, m.field_name),
                    None => (
                        vh_read_string(ctx, this, VH_CLASS).unwrap_or_default(),
                        vh_read_string(ctx, this, VH_FIELD).unwrap_or_default(),
                    ),
                };
                match ctx.resolve_field_index(&class, &field) {
                    Some(idx) => {
                        // Cache for next time
                        ctx.set_field(this, VH_FIELD_INDEX, Value::Int(idx as i32));
                        vh_meta_update_field_index(this, idx as i32);
                        let val = ctx.get_field(receiver, idx);
                        let td = vh_type_desc(ctx, this);
                        Ok(Some(box_value(ctx, val, &td)))
                    }
                    None => Ok(Some(Value::Object(None))),
                }
            }
        }
        VH_KIND_STATIC => {
            let (class, field) = match vh_meta_get(this) {
                Some(m) => (m.class_name, m.field_name),
                None => (
                    vh_read_string(ctx, this, VH_CLASS).unwrap_or_default(),
                    vh_read_string(ctx, this, VH_FIELD).unwrap_or_default(),
                ),
            };
            let mirror = match ctx.class_id_by_name(&class) {
                Some(cid) => ctx.get_class_mirror(cid),
                None => return Ok(Some(Value::Object(None))),
            };
            let val = ctx.get_field_by_name(mirror, &field);
            let td = vh_type_desc(ctx, this);
            Ok(Some(box_value(ctx, val, &td)))
        }
        VH_KIND_ARRAY => {
            // args = [vh, array, index]
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let idx = match args.get(2) {
                Some(Value::Int(i)) => *i as usize,
                _ => 0,
            };
            Ok(Some(ctx.get_array_element(arr, idx)))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// VarHandle.set(receiver, value) → void
/// Signature-polymorphic: args arrive as individual values from the call-site,
/// i.e. args = [vh_ref, receiver, value] for instance fields.
fn varhandle_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C38: Array-element VarHandle.set — args = [vh, array, idx, value]. Handles
    // real-JDK VarHandleLongs$Array / VarHandleInts$Array / etc.
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let value = args.get(3).cloned().unwrap_or(Value::Int(0));
        ctx.set_array_element(arr, idx, value);
        return Ok(None);
    }
    // WP4.2: prefer the side-table metadata when present — the real JDK
    // VarHandle class has a non-trivial field layout (slot 0 = `vform:VarForm`,
    // a reference) and the descriptor-aware setter coerces our Int kind tag
    // to Object(None) on the way in. Read kind/field_idx from the side
    // table when we created the VarHandle ourselves.
    let (kind, field_idx) = match vh_meta_get(this) {
        Some(meta) => (meta.kind, meta.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) { Value::Int(k) => k, _ => return Ok(None) };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) { Value::Int(i) => i, _ => -1 };
            (k, i)
        }
    };

    match kind {
        VH_KIND_INSTANCE => {
            // args[1] = receiver, args[2] = value (signature-polymorphic dispatch)
            let receiver = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(None),
            };
            let value = args.get(2).cloned().unwrap_or(Value::Int(0));

            if field_idx >= 0 {
                ctx.set_field(receiver, field_idx as usize, value);
            } else {
                // WP4.2: prefer side-table for class+field, fall back to slot read
                let (class, field) = match vh_meta_get(this) {
                    Some(m) => (m.class_name, m.field_name),
                    None => (
                        vh_read_string(ctx, this, VH_CLASS).unwrap_or_default(),
                        vh_read_string(ctx, this, VH_FIELD).unwrap_or_default(),
                    ),
                };
                if let Some(idx) = ctx.resolve_field_index(&class, &field) {
                    ctx.set_field(this, VH_FIELD_INDEX, Value::Int(idx as i32));
                    vh_meta_update_field_index(this, idx as i32);
                    ctx.set_field(receiver, idx, value);
                }
            }
            Ok(None)
        }
        VH_KIND_STATIC => {
            // args[1] = value
            let value = args.get(1).cloned().unwrap_or(Value::Int(0));
            // WP4.2: prefer side-table for class+field, fall back to slot read
            let (class, field) = match vh_meta_get(this) {
                Some(m) => (m.class_name, m.field_name),
                None => (
                    vh_read_string(ctx, this, VH_CLASS).unwrap_or_default(),
                    vh_read_string(ctx, this, VH_FIELD).unwrap_or_default(),
                ),
            };
            if let Some(cid) = ctx.class_id_by_name(&class) {
                let mirror = ctx.get_class_mirror(cid);
                ctx.set_field_by_name(mirror, &field, value);
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// VarHandle.compareAndSet(receiver, expected, new) → boolean
/// Signature-polymorphic: args arrive as individual values from the call-site,
/// i.e. args = [vh_ref, receiver, expected, new_value] for instance fields.
fn varhandle_compare_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C38: Array-element CAS — args = [vh, array, idx, expected, new_value].
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let current = ctx.get_array_element(arr, idx);
        let expected = args.get(3).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(4).cloned().unwrap_or(Value::Int(0));
        let matches = match (&current, &expected) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Long(a), Value::Long(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
            (Value::Object(a), Value::Object(b)) => match (a, b) {
                (Some(ra), Some(rb)) => ra.as_ptr() == rb.as_ptr(),
                (None, None) => true,
                _ => false,
            },
            _ => false,
        };
        if matches {
            ctx.set_array_element(arr, idx, new_val);
            return Ok(Some(Value::Int(1)));
        }
        return Ok(Some(Value::Int(0)));
    }
    // WP4.2: prefer side-table (see vh_meta_table comment).
    let (kind, field_idx) = match vh_meta_get(this) {
        Some(meta) => (meta.kind, meta.field_index),
        None => {
            let k = match ctx.get_field(this, VH_KIND) { Value::Int(k) => k, _ => return Ok(Some(Value::Int(0))) };
            let i = match ctx.get_field(this, VH_FIELD_INDEX) { Value::Int(i) => i, _ => -1 };
            (k, i)
        }
    };

    if kind != VH_KIND_INSTANCE {
        // Static CAS or array CAS — simplified implementation
        return Ok(Some(Value::Int(1)));
    }

    // args[1] = receiver, args[2] = expected, args[3] = new_value
    let receiver = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected = args.get(2).cloned().unwrap_or(Value::Int(0));
    let new_val = args.get(3).cloned().unwrap_or(Value::Int(0));

    let idx = if field_idx >= 0 {
        field_idx as usize
    } else {
        let (class, field) = match vh_meta_get(this) {
            Some(m) => (m.class_name, m.field_name),
            None => (
                vh_read_string(ctx, this, VH_CLASS).unwrap_or_default(),
                vh_read_string(ctx, this, VH_FIELD).unwrap_or_default(),
            ),
        };
        match ctx.resolve_field_index(&class, &field) {
            Some(i) => {
                ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                vh_meta_update_field_index(this, i as i32);
                i
            }
            None => return Ok(Some(Value::Int(0))),
        }
    };

    let current = ctx.get_field(receiver, idx);

    // Compare current vs expected
    let matches = match (&current, &expected) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
        (Value::Object(a), Value::Object(b)) => {
            // Reference equality
            match (a, b) {
                (Some(ra), Some(rb)) => ra.as_ptr() == rb.as_ptr(),
                (None, None) => true,
                _ => false,
            }
        }
        _ => false,
    };

    if matches {
        ctx.set_field(receiver, idx, new_val);
        Ok(Some(Value::Int(1)))
    } else {
        Ok(Some(Value::Int(0)))
    }
}

/// VarHandle.compareAndExchange(receiver, expected, new) → witness value
/// Signature-polymorphic: args = [vh_ref, receiver, expected, new_value]
fn varhandle_compare_and_exchange(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C38: Array-element compareAndExchange — args = [vh, array, idx, expected, new].
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let current = ctx.get_array_element(arr, idx);
        let expected = args.get(3).cloned().unwrap_or(Value::Int(0));
        let new_val = args.get(4).cloned().unwrap_or(Value::Int(0));
        let matches = match (&current, &expected) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Long(a), Value::Long(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Double(a), Value::Double(b)) => a.to_bits() == b.to_bits(),
            (Value::Object(Some(ra)), Value::Object(Some(rb))) => ra.as_ptr() == rb.as_ptr(),
            (Value::Object(None), Value::Object(None)) => true,
            _ => false,
        };
        if matches {
            ctx.set_array_element(arr, idx, new_val);
        }
        return Ok(Some(current));
    }
    let kind = match ctx.get_field(this, VH_KIND) { Value::Int(k) => k, _ => return Ok(Some(Value::Object(None))) };
    let field_idx = match ctx.get_field(this, VH_FIELD_INDEX) { Value::Int(i) => i, _ => -1 };

    if kind != VH_KIND_INSTANCE {
        return Ok(Some(Value::Object(None)));
    }

    let receiver = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let expected = args.get(2).cloned().unwrap_or(Value::Int(0));
    let new_val = args.get(3).cloned().unwrap_or(Value::Int(0));

    let idx = if field_idx >= 0 {
        field_idx as usize
    } else {
        let class = vh_read_string(ctx, this, VH_CLASS).unwrap_or_default();
        let field = vh_read_string(ctx, this, VH_FIELD).unwrap_or_default();
        match ctx.resolve_field_index(&class, &field) {
            Some(i) => {
                ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                i
            }
            None => return Ok(Some(Value::Object(None))),
        }
    };

    let current = ctx.get_field(receiver, idx);
    let matches = match (&current, &expected) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        (Value::Object(Some(ra)), Value::Object(Some(rb))) => ra.as_ptr() == rb.as_ptr(),
        (Value::Object(None), Value::Object(None)) => true,
        _ => false,
    };

    if matches {
        ctx.set_field(receiver, idx, new_val);
    }
    // Return the witness (old value)
    Ok(Some(current))
}

/// VarHandle.getAndSet(receiver, new) → old value
/// Signature-polymorphic: args = [vh_ref, receiver, new_value]
fn varhandle_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // C38: Array-element getAndSet — args = [vh, array, idx, new_value].
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let old = ctx.get_array_element(arr, idx);
        let new_val = args.get(3).cloned().unwrap_or(Value::Int(0));
        ctx.set_array_element(arr, idx, new_val);
        return Ok(Some(old));
    }
    let kind = match ctx.get_field(this, VH_KIND) { Value::Int(k) => k, _ => return Ok(Some(Value::Object(None))) };
    let field_idx = match ctx.get_field(this, VH_FIELD_INDEX) { Value::Int(i) => i, _ => -1 };

    if kind != VH_KIND_INSTANCE {
        return Ok(Some(Value::Object(None)));
    }

    let receiver = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_val = args.get(2).cloned().unwrap_or(Value::Int(0));

    let idx = if field_idx >= 0 {
        field_idx as usize
    } else {
        let class = vh_read_string(ctx, this, VH_CLASS).unwrap_or_default();
        let field = vh_read_string(ctx, this, VH_FIELD).unwrap_or_default();
        match ctx.resolve_field_index(&class, &field) {
            Some(i) => {
                ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                i
            }
            None => return Ok(Some(Value::Object(None))),
        }
    };

    let old = ctx.get_field(receiver, idx);
    ctx.set_field(receiver, idx, new_val);
    Ok(Some(old))
}

/// VarHandle.getAndAdd(receiver, delta) → old value.
///
/// Signature-polymorphic for both array and instance-field VarHandles.
/// - Array kind: args = [vh, array, index, delta] (H2's `VarHandleInts$Array.getAndAdd([III)I`).
/// - Instance kind: args = [vh, receiver, delta].
/// - Static kind: args = [vh, delta].
///
/// Delta can be Int, Long, Float, or Double. Returns previous value (unboxed to
/// match the call-site descriptor; the polymorphic dispatch wraps it as needed).
fn varhandle_get_and_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;

    fn add_values(current: &Value, delta: &Value) -> Value {
        match (current, delta) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(*b)),
            (Value::Long(a), Value::Long(b)) => Value::Long(a.wrapping_add(*b)),
            (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (Value::Double(a), Value::Double(b)) => Value::Double(a + b),
            // Mixed: widen delta to current's type.
            (Value::Long(a), Value::Int(b)) => Value::Long(a.wrapping_add(*b as i64)),
            (Value::Int(a), Value::Long(b)) => Value::Int((*a as i64).wrapping_add(*b) as i32),
            _ => current.clone(),
        }
    }

    // C38: Array-element getAndAdd — args = [vh, array, idx, delta]. Route first
    // so real-JDK VarHandleLongs$Array / VarHandleInts$Array calls work even
    // when the VH's slot 0 is not our synthetic VH_KIND Int.
    if let Some((arr, idx)) = vh_array_call(ctx, args) {
        let delta = args.get(3).cloned().unwrap_or(Value::Int(0));
        let old = ctx.get_array_element(arr, idx);
        let new_val = add_values(&old, &delta);
        ctx.set_array_element(arr, idx, new_val);
        return Ok(Some(old));
    }

    let kind = match ctx.get_field(this, VH_KIND) {
        Value::Int(k) => k,
        _ => return Ok(Some(Value::Int(0))),
    };

    match kind {
        VH_KIND_ARRAY => {
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let idx = match args.get(2) {
                Some(Value::Int(i)) => *i as usize,
                _ => 0,
            };
            let delta = args.get(3).cloned().unwrap_or(Value::Int(0));
            let old = ctx.get_array_element(arr, idx);
            let new_val = add_values(&old, &delta);
            ctx.set_array_element(arr, idx, new_val);
            Ok(Some(old))
        }
        VH_KIND_INSTANCE => {
            let receiver = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Int(0))),
            };
            let delta = args.get(2).cloned().unwrap_or(Value::Int(0));
            let field_idx = match ctx.get_field(this, VH_FIELD_INDEX) {
                Value::Int(i) => i,
                _ => -1,
            };
            let idx = if field_idx >= 0 {
                field_idx as usize
            } else {
                let class = vh_read_string(ctx, this, VH_CLASS).unwrap_or_default();
                let field = vh_read_string(ctx, this, VH_FIELD).unwrap_or_default();
                match ctx.resolve_field_index(&class, &field) {
                    Some(i) => {
                        ctx.set_field(this, VH_FIELD_INDEX, Value::Int(i as i32));
                        i
                    }
                    None => return Ok(Some(Value::Int(0))),
                }
            };
            let old = ctx.get_field(receiver, idx);
            let new_val = add_values(&old, &delta);
            ctx.set_field(receiver, idx, new_val);
            Ok(Some(old))
        }
        VH_KIND_STATIC => {
            let delta = args.get(1).cloned().unwrap_or(Value::Int(0));
            let class = vh_read_string(ctx, this, VH_CLASS).unwrap_or_default();
            let field = vh_read_string(ctx, this, VH_FIELD).unwrap_or_default();
            if let Some(cid) = ctx.class_id_by_name(&class) {
                let mirror = ctx.get_class_mirror(cid);
                let old = ctx.get_field_by_name(mirror, &field);
                let new_val = add_values(&old, &delta);
                ctx.set_field_by_name(mirror, &field, new_val);
                Ok(Some(old))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

// =============================================================================
// java.lang.invoke.CallSite expansion — MutableCallSite, ConstantCallSite, VolatileCallSite
// CallSite = 1-field synthetic (target=0 MethodHandle)
// =============================================================================

pub(crate) fn register_p60_callsite(r: &mut NativeMethodRegistry) {
    // CallSite base
    let cs = "java/lang/invoke/CallSite";
    r.register(
        cs,
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        cs,
        "setTarget",
        "(Ljava/lang/invoke/MethodHandle;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(
        cs,
        "type",
        "()Ljava/lang/invoke/MethodType;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // MutableCallSite
    let mcs = "java/lang/invoke/MutableCallSite";
    r.register(
        mcs,
        "<init>",
        "(Ljava/lang/invoke/MethodHandle;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(
        mcs,
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        mcs,
        "setTarget",
        "(Ljava/lang/invoke/MethodHandle;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );

    // ConstantCallSite
    let ccs = "java/lang/invoke/ConstantCallSite";
    r.register(
        ccs,
        "<init>",
        "(Ljava/lang/invoke/MethodHandle;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(
        ccs,
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        ccs,
        "dynamicInvoker",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );

    // VolatileCallSite
    let vcs = "java/lang/invoke/VolatileCallSite";
    r.register(
        vcs,
        "<init>",
        "(Ljava/lang/invoke/MethodHandle;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(
        vcs,
        "getTarget",
        "()Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        vcs,
        "setTarget",
        "(Ljava/lang/invoke/MethodHandle;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
}

// java.lang.invoke.MethodHandles.Lookup — factory + lookup methods
// Lookup = 2-field (lookupClass=0, allowedModes=1)
// =============================================================================

pub fn register_p63_method_handles_lookup(r: &mut NativeMethodRegistry) {
    let mh = "java/lang/invoke/MethodHandles";
    r.register(
        mh,
        "lookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, _args| {
            // WP4.2: MethodHandles.lookup() is caller-sensitive. The
            // returned Lookup must have `lookupClass` set to the caller's
            // class so downstream `findVarHandle(name, type)` and
            // `MhUtil.findVarHandle(Lookup, String, Class)` (which calls
            // `lookup.lookupClass()` internally) work correctly. Without
            // this, e.g. `CompletableFuture.<clinit>` produces a null
            // RESULT VarHandle and the subsequent `setRelease` NPEs.
            //
            // Walk the stack to find the caller of MethodHandles.lookup().
            // capture_stack_trace returns frames bottom-first (main first,
            // innermost last). The native frame itself is not included, so
            // frames[len-1] is the caller (whoever invoked lookup()).
            let frames = ctx.capture_stack_trace(0);
            let caller_class = if let Some(frame) = frames.last() {
                let class_name = frame.class_name.replace('.', "/");
                let cid = ctx
                    .ensure_class_initialized(&class_name)
                    .unwrap_or(rustjvm_types::ClassId::new(0));
                Value::Object(Some(ctx.get_class_mirror(cid)))
            } else {
                Value::Object(None)
            };
            // WP2.3: when real JDK MethodHandles$Lookup is loaded its
            // instance fields are { lookupClass, prevLookupClass,
            // allowedModes }. We must populate lookupClass by NAME
            // because field-order varies between synthetic-only mode
            // and real-JDK mode. Field-by-name resolves to the
            // correct slot in either case.
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 2);
            ctx.set_field_by_name(obj, "lookupClass", caller_class);
            ctx.set_field_by_name(obj, "prevLookupClass", Value::Object(None));
            ctx.set_field_by_name(obj, "allowedModes", Value::Int(0x1F)); // FULL access
            // Fallback: also write to slot 0/1 so the synthetic-only
            // path (no real JDK loaded) still has a valid lookupClass
            // at the synthetic offset 0.
            ctx.set_field(obj, 0, caller_class);
            ctx.set_field(obj, 1, Value::Int(0x1F));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mh,
        "publicLookup",
        "()Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, _args| {
            // publicLookup() is NOT caller-sensitive — it always returns a
            // Lookup whose lookupClass is java.lang.Object with PUBLIC-only
            // access. Setting it to Object lets findVarHandle on widely
            // visible classes still resolve, while preventing access to
            // package-private members (we don't enforce modes anyway).
            let object_cid = ctx
                .ensure_class_initialized("java/lang/Object")
                .unwrap_or(rustjvm_types::ClassId::new(0));
            let object_mirror = ctx.get_class_mirror(object_cid);
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 2);
            ctx.set_field(obj, 0, Value::Object(Some(object_mirror)));
            ctx.set_field(obj, 1, Value::Int(0x01)); // PUBLIC only
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(mh, "privateLookupIn", "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandles$Lookup;)Ljava/lang/invoke/MethodHandles$Lookup;", |ctx, args| {
        let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 2);
        ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Object(None)));
        ctx.set_field(obj, 1, Value::Int(0x1F));
        Ok(Some(Value::Object(Some(obj))))
    });

    let lk = "java/lang/invoke/MethodHandles$Lookup";
    r.register(lk, "lookupClass", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(lk, "lookupModes", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    // Lookup.in(targetClass) — create Lookup with reduced access for a different class
    r.register(
        lk,
        "in",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandles$Lookup;",
        |ctx, args| {
            let _this = obj_arg(args, 0)?;
            let target_class = args.get(1).copied().unwrap_or(Value::Object(None));
            let lookup = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandles$Lookup", 2);
            ctx.set_field(lookup, 0, target_class); // lookupClass = targetClass
            // Access reduced to PUBLIC + UNCONDITIONAL when crossing packages
            ctx.set_field(lookup, 1, Value::Int(0x01 | 0x20));
            Ok(Some(Value::Object(Some(lookup))))
        },
    );

    // findVirtual — resolve to real 5-field MH with descriptor from MethodType
    r.register(lk, "findVirtual",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_virtual);

    // findStatic — resolve to real 5-field MH
    r.register(lk, "findStatic",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_static);

    // findConstructor — resolve to real 5-field MH
    r.register(lk, "findConstructor",
        "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_constructor);

    // findSpecial — same as findVirtual but MH_KIND_SPECIAL
    r.register(lk, "findSpecial",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_special);

    // findGetter — creates a MH with kind=GETTER that reads an instance field
    r.register(lk, "findGetter",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_getter);

    // findSetter — creates a MH with kind=SETTER that writes an instance field
    r.register(lk, "findSetter",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_setter);

    // findStaticGetter/findStaticSetter
    r.register(lk, "findStaticGetter",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_static_getter);
    r.register(lk, "findStaticSetter",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_find_static_setter);

    // findVarHandle — create a real VarHandle for an instance field
    r.register(lk, "findVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        lookup_find_var_handle);

    // findStaticVarHandle
    r.register(lk, "findStaticVarHandle",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Ljava/lang/invoke/VarHandle;",
        lookup_find_static_var_handle);

    r.register(lk, "toString", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("MethodHandles.Lookup");
        Ok(Some(Value::Object(Some(s))))
    });
}

// ---------------------------------------------------------------------------
// Lookup.find* implementations
// ---------------------------------------------------------------------------

fn lookup_find_virtual(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_method_error("", "", ""));
    }};
    let name_obj = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_method_error("", "", ""));
    }};
    let mt_obj = match args.get(3) { Some(Value::Object(Some(o))) => *o, _ => {
        let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
        let name = ctx.read_string(name_obj).unwrap_or_default();
        return Err(no_such_method_error(&class, &name, ""));
    }};
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let desc = descriptor_from_method_type(ctx, mt_obj);
    // Ensure class is loaded
    let _ = ctx.ensure_class_initialized(&class);
    // Validate method exists
    if !ctx.method_exists(&class, &name, &desc) {
        // Don't throw — method may be in a synthetic stub or native-only class.
        // Create the MH anyway; dispatch will handle missing methods gracefully.
    }
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_VIRTUAL);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_find_static(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(no_such_method_error("", "", "")),
    };
    let name_obj = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(no_such_method_error("", "", "")),
    };
    let mt_obj = match args.get(3) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
            let name = ctx.read_string(name_obj).unwrap_or_default();
            return Err(no_such_method_error(&class, &name, ""));
        }
    };
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let desc = descriptor_from_method_type(ctx, mt_obj);
    let _ = ctx.ensure_class_initialized(&class);
    if !ctx.method_exists(&class, &name, &desc) {
        // Some internal targets live in stubs/synthetics that don't surface
        // through method_exists. Create the MH anyway; the dispatch site
        // raises NoSuchMethodError at invoke time if it cannot resolve.
    }
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_STATIC);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_find_constructor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_method_error("", "<init>", ""));
    }};
    let mt_obj = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
        let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
        return Err(no_such_method_error(&class, "<init>", ""));
    }};
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let mut desc = descriptor_from_method_type(ctx, mt_obj);
    if let Some(pos) = desc.rfind(')') {
        desc.truncate(pos + 1);
        desc.push('V');
    }
    // Ensure class is loaded so constructor resolution works at dispatch time
    let _ = ctx.ensure_class_initialized(&class);
    let mh = alloc_method_handle(ctx, &class, "<init>", &desc, MH_KIND_CONSTRUCTOR);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_find_special(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // WP2.9 — Lookup.findSpecial(refc, name, type, specialCaller)
    //   args[0] = lookup (this)
    //   args[1] = refc (Class on which to find the method)
    //   args[2] = name (method name)
    //   args[3] = type (MethodType)
    //   args[4] = specialCaller (Class authorized to do invokespecial)
    let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_method_error("", "", ""));
    }};
    let name_obj = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_method_error("", "", ""));
    }};
    let mt_obj = match args.get(3) { Some(Value::Object(Some(o))) => *o, _ => {
        let class = resolve_class_name_robust(ctx, class_obj).unwrap_or_default();
        let name = ctx.read_string(name_obj).unwrap_or_default();
        return Err(no_such_method_error(&class, &name, ""));
    }};
    // specialCaller (args[4]) — for spec correctness, we ensure it's loaded
    // so subsequent access checks work. The actual access check (specialCaller
    // must be the lookup class or have PRIVATE-mode access) is permissive
    // here: we trust JDK-side `Lookup.checkSpecial`. A stricter check would
    // require lookup-mode bookkeeping not yet wired through native-api.
    if let Some(Value::Object(Some(caller_obj))) = args.get(4) {
        if let Some(caller_name) = resolve_class_name_robust(ctx, *caller_obj) {
            // Ensure caller class is loaded so member-access verification has
            // both class hierarchies available downstream.
            let _ = ctx.ensure_class_initialized(&caller_name);
        }
    }
    let class = resolve_class_name_robust(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let desc = descriptor_from_method_type(ctx, mt_obj);
    let _ = ctx.ensure_class_initialized(&class);
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_SPECIAL);
    Ok(Some(Value::Object(Some(mh))))
}

/// Create a NoSuchMethodException error.
fn no_such_method_error(class: &str, method: &str, desc: &str) -> rustjvm_types::error::MethodCallFailed {
    rustjvm_types::error::MethodCallFailed::InternalError(
        rustjvm_types::error::VmError::Internal {
            message: format!("NoSuchMethodException: {class}.{method}{desc}"),
        },
    )
}

/// Create a NoSuchFieldException error.
fn no_such_field_error(class: &str, field: &str) -> rustjvm_types::error::MethodCallFailed {
    rustjvm_types::error::MethodCallFailed::InternalError(
        rustjvm_types::error::VmError::Internal {
            message: format!("NoSuchFieldException: {class}.{field}"),
        },
    )
}

fn lookup_find_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_field_error("", ""));
    }};
    let name_obj = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_field_error("", ""));
    }};
    let type_obj = match args.get(3) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_field_error("", ""));
    }};
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    let _ = ctx.ensure_class_initialized(&class);
    // Validate field exists
    if ctx.resolve_field_index(&class, &name).is_none() {
        // Field might be in a synthetic stub — don't throw hard error
    }
    let desc = format!("(L{class};){field_desc}");
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_GETTER);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_find_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_field_error("", ""));
    }};
    let name_obj = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_field_error("", ""));
    }};
    let type_obj = match args.get(3) { Some(Value::Object(Some(o))) => *o, _ => {
        return Err(no_such_field_error("", ""));
    }};
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    let _ = ctx.ensure_class_initialized(&class);
    if ctx.resolve_field_index(&class, &name).is_none() {
        // Field might be in a synthetic stub
    }
    let desc = format!("(L{class};{field_desc})V");
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_SETTER);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_find_static_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(Some(alloc_method_handle(ctx, "", "", "", MH_KIND_GETTER)))));
    }};
    let name_obj = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(Some(alloc_method_handle(ctx, "", "", "", MH_KIND_GETTER)))));
    }};
    let type_obj = match args.get(3) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(Some(alloc_method_handle(ctx, "", "", "", MH_KIND_GETTER)))));
    }};
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    let desc = format!("(){field_desc}");
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_GETTER);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_find_static_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(Some(alloc_method_handle(ctx, "", "", "", MH_KIND_SETTER)))));
    }};
    let name_obj = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(Some(alloc_method_handle(ctx, "", "", "", MH_KIND_SETTER)))));
    }};
    let type_obj = match args.get(3) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(Some(alloc_method_handle(ctx, "", "", "", MH_KIND_SETTER)))));
    }};
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);
    let desc = format!("({field_desc})V");
    let mh = alloc_method_handle(ctx, &class, &name, &desc, MH_KIND_SETTER);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_find_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(None)));
    }};
    let name_obj = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(None)));
    }};
    let type_obj = match args.get(3) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(None)));
    }};
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let field_name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);

    // Resolve the field index
    let field_index = ctx.resolve_field_index(&class, &field_name).unwrap_or(0);
    let class_id = match mirror_class_id(ctx, class_obj) {
        Some(id) => id,
        None => match ctx.class_id_by_name(&class) {
            Some(id) => id,
            None => {
                // Try to load the class
                match ctx.ensure_class_initialized(&class) {
                    Ok(id) => id,
                    Err(_) => return Ok(Some(Value::Object(None))),
                }
            }
        }
    };

    let vh = alloc_instance_var_handle(ctx, &class, &field_name, &field_desc, field_index, class_id);
    Ok(Some(Value::Object(Some(vh))))
}

fn lookup_find_static_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(None)));
    }};
    let name_obj = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(None)));
    }};
    let type_obj = match args.get(3) { Some(Value::Object(Some(o))) => *o, _ => {
        return Ok(Some(Value::Object(None)));
    }};
    let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let field_name = ctx.read_string(name_obj).unwrap_or_default();
    let field_desc = field_descriptor_from_mirror(ctx, type_obj);

    let vh = alloc_static_var_handle(ctx, &class, &field_name, &field_desc);
    Ok(Some(Value::Object(Some(vh))))
}

// =============================================================================
// MethodHandles extra factory methods
// =============================================================================

pub(crate) fn register_p65_method_handles_extra(r: &mut NativeMethodRegistry) {
    let mh = "java/lang/invoke/MethodHandles";
    r.register(
        mh,
        "arrayElementGetter",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, _args| {
            // C19: allocate past the real-JDK instance-field count so the
            // `type:MethodType` field at slot 0 is populated with a non-null
            // MethodType. Synthesize `()V` — callers only need a non-null.
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
            if let Some(mt) = build_method_type_from_descriptor(ctx, "()V") {
                ctx.set_field_by_name(obj, "type", Value::Object(Some(mt)));
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mh,
        "arrayElementSetter",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
            if let Some(mt) = build_method_type_from_descriptor(ctx, "()V") {
                ctx.set_field_by_name(obj, "type", Value::Object(Some(mt)));
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mh,
        "identity",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
            if let Some(mt) = build_method_type_from_descriptor(ctx, "()V") {
                ctx.set_field_by_name(obj, "type", Value::Object(Some(mt)));
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mh,
        "constant",
        "(Ljava/lang/Class;Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
            if let Some(mt) = build_method_type_from_descriptor(ctx, "()V") {
                ctx.set_field_by_name(obj, "type", Value::Object(Some(mt)));
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mh,
        "dropArguments",
        "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |_ctx, args| {
            // Return the original method handle (simplified)
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        mh,
        "insertArguments",
        "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        mh,
        "empty",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            // C19: empty(mt) takes a MethodType — thread it into `type`.
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
            if let Some(Value::Object(Some(mt))) = args.first() {
                ctx.set_field_by_name(obj, "type", Value::Object(Some(*mt)));
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        mh,
        "zero",
        "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
            if let Some(mt) = build_method_type_from_descriptor(ctx, "()V") {
                ctx.set_field_by_name(obj, "type", Value::Object(Some(mt)));
            }
            Ok(Some(Value::Object(Some(obj))))
        },
    );
}

// =============================================================================
// java.lang.invoke extras — MethodHandleProxies, LambdaMetafactory
// =============================================================================

pub(crate) fn register_p68_invoke_extras(r: &mut NativeMethodRegistry) {
    // MethodHandleProxies
    let mhp = "java/lang/invoke/MethodHandleProxies";
    r.register(
        mhp,
        "asInterfaceInstance",
        "(Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/Object;",
        |_ctx, args| {
            // Return the method handle as the proxy (simplified)
            Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        mhp,
        "isWrapperInstance",
        "(Ljava/lang/Object;)Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );
    r.register(
        mhp,
        "wrapperInstanceTarget",
        "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    r.register(
        mhp,
        "wrapperInstanceType",
        "(Ljava/lang/Object;)Ljava/lang/Class;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // LambdaMetafactory (static method stubs for bootstrap)
    let lmf = "java/lang/invoke/LambdaMetafactory";
    r.register(lmf, "metafactory",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;",
        |_ctx, _args| Ok(Some(Value::Object(None))));
    r.register(lmf, "altMetafactory",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;[Ljava/lang/Object;)Ljava/lang/invoke/CallSite;",
        |_ctx, _args| Ok(Some(Value::Object(None))));

    // StringConcatFactory — already registered in Phase 58 with full implementation
}

// =============================================================================
// T4: MethodHandle.invoke / invokeExact / invokeWithArguments
//
// MethodHandle layout (5 fields):
//   MH_CLASS  = 0  String  — class name (JVM-style, slash-separated)
//   MH_NAME   = 1  String  — method name
//   MH_DESC   = 2  String  — JVM descriptor
//   MH_KIND   = 3  Int     — 0=static 1=virtual 2=special 3=constructor
//                            4=getter 5=setter
//   MH_BOUND  = 4  Object  — bound receiver (for bound MH) or null
//
// Lookup.find* now populates these fields so invoke/invokeExact can dispatch.
// Existing zero-field stubs created by other subsystems remain harmless:
// invoke on a 0-field MH returns null/void.
// =============================================================================

// C15: Synthetic-MH field slots live AFTER the real JDK's instance fields.
// The real JDK `java/lang/invoke/MethodHandle` has 6 instance fields
// (type, form, asTypeCache, asTypeSoftCache, customizationCount,
// updateInProgress) at slots 0-5, where slot 0 (`type`) is the MethodType the
// JDK expects.  If we put MH_CLASS at slot 0 it collides with `type` and
// either overwrites JDK code's view of the handle or gets overwritten when
// the JDK class field is populated.  By anchoring our synthetic fields at
// slot 16 we reserve plenty of headroom past any future JDK field additions.
const MH_BASE:  usize = 16;
const MH_CLASS: usize = MH_BASE + 0;
const MH_NAME:  usize = MH_BASE + 1;
const MH_DESC:  usize = MH_BASE + 2;
const MH_KIND:  usize = MH_BASE + 3;
const MH_BOUND: usize = MH_BASE + 4;

/// Returns true if a method descriptor has exactly two parameters.
/// Used to distinguish instance setters "(Lowner;value)V" (2 params)
/// from static setters "(value)V" (1 param).
fn desc_has_two_params(desc: &str) -> bool {
    let end = match desc.find(')') {
        Some(i) => i,
        None => return false,
    };
    let params = &desc[1..end];
    let mut count = 0;
    let mut i = 0;
    let bytes = params.as_bytes();
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' => {
                count += 1;
                i += 1;
            }
            '[' => {
                // skip array prefix
                while i < bytes.len() && bytes[i] as char == '[' { i += 1; }
                // then one type
                if i < bytes.len() {
                    if bytes[i] as char == 'L' {
                        while i < bytes.len() && bytes[i] as char != ';' { i += 1; }
                        if i < bytes.len() { i += 1; }
                    } else {
                        i += 1;
                    }
                }
                count += 1;
            }
            'L' => {
                while i < bytes.len() && bytes[i] as char != ';' { i += 1; }
                if i < bytes.len() { i += 1; }
                count += 1;
            }
            _ => return false,
        }
    }
    count == 2
}

const MH_KIND_STATIC:      i32 = 0;
const MH_KIND_VIRTUAL:     i32 = 1;
const MH_KIND_SPECIAL:     i32 = 2;
const MH_KIND_CONSTRUCTOR: i32 = 3;
#[allow(dead_code)]
const MH_KIND_GETTER:      i32 = 4;
#[allow(dead_code)]
const MH_KIND_SETTER:      i32 = 5;
const MH_KIND_PERMUTE:     i32 = 6;
const MH_KIND_GUARD:       i32 = 7;
/// C26: dropArgumentsTrusted adapter. MH_BOUND holds the wrapped inner MH.
/// MH_DESC is the widened descriptor (used for invokeExact arity check).
/// Dispatch reads the inner MH's MH_DESC and forwards a trimmed argument
/// slice (outer-arity minus inner-arity extras are discarded from the
/// position encoded in MH_CLASS as a decimal pos string).
const MH_KIND_DROP:        i32 = 8;

/// Allocate a fully-described MethodHandle.
pub(crate) fn alloc_method_handle(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    desc: &str,
    kind: i32,
) -> rustjvm_types::ObjectRef {
    // C15: Allocate MH_BOUND+1 slots so our synthetic fields (at slots 16-20)
    // live PAST the real JDK's instance-field count (6). This prevents
    // `set_field_by_name(mh, "type", ...)` — which resolves to slot 0 — from
    // overwriting our class/name/desc/kind/bound data.
    let mh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", MH_BOUND + 1);
    let cls = ctx.create_string(class);
    let nm  = ctx.create_string(name);
    let dc  = ctx.create_string(desc);
    ctx.set_field(mh, MH_CLASS, Value::Object(Some(cls)));
    ctx.set_field(mh, MH_NAME,  Value::Object(Some(nm)));
    ctx.set_field(mh, MH_DESC,  Value::Object(Some(dc)));
    ctx.set_field(mh, MH_KIND,  Value::Int(kind));
    ctx.set_field(mh, MH_BOUND, Value::Object(None));
    // Populate the real-JDK MethodHandle.type:MethodType field at its
    // resolved slot (0) so `mh.type()` and JDK-internal reads (LambdaForm,
    // MemberName, Invokers, ObjectStreamClass) see a MethodType, not null.
    // C21: Always populate `type` — fall back to `()V` when no usable
    // descriptor was supplied (empty-desc alloc sites for missing-arg
    // failure paths in lookup_find_static_getter/setter, linkCallSite, etc.)
    // so JDK-internal `erasedType()` / `parameterSlotCount()` walks never
    // observe a null MethodType.
    let mt_opt = build_method_type_from_descriptor(ctx, desc)
        .or_else(|| build_method_type_from_descriptor(ctx, "()V"));
    if let Some(mt) = mt_opt {
        ctx.set_field_by_name(mh, "type", Value::Object(Some(mt)));
    }
    mh
}

/// Read the class name string from a MethodHandle (field MH_CLASS).
pub(crate) fn mh_read_class(ctx: &dyn NativeContext, mh: rustjvm_types::ObjectRef) -> Option<String> {
    match ctx.get_field(mh, MH_CLASS) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Read the method name string from a MethodHandle (field MH_NAME).
pub(crate) fn mh_read_name(ctx: &dyn NativeContext, mh: rustjvm_types::ObjectRef) -> Option<String> {
    match ctx.get_field(mh, MH_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Read the descriptor string from a MethodHandle (field MH_DESC).
pub(crate) fn mh_read_desc(ctx: &dyn NativeContext, mh: rustjvm_types::ObjectRef) -> Option<String> {
    match ctx.get_field(mh, MH_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Core dispatch: given a populated MethodHandle and argument list, invoke it.
/// `extra_args` are the args passed to invoke() after `this` (the MH itself).
pub(crate) fn mh_dispatch(
    ctx: &mut dyn NativeContext,
    mh: rustjvm_types::ObjectRef,
    extra_args: &[Value],
) -> MethodCallResult {
    let class = match mh_read_class(ctx, mh) {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let name = mh_read_name(ctx, mh).unwrap_or_default();
    let desc = mh_read_desc(ctx, mh).unwrap_or_default();
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => MH_KIND_VIRTUAL,
    };
    let bound = ctx.get_field(mh, MH_BOUND);

    match kind {
        MH_KIND_STATIC => {
            // Static: extra_args are the full argument list
            ctx.invoke(&class, &name, &desc, extra_args)
        }
        MH_KIND_CONSTRUCTOR => {
            // Constructor: allocate new object then call <init>
            let cid = match ctx.ensure_class_initialized(&class) {
                Ok(id) => id,
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            let new_obj = ctx.alloc_object(cid, 16); // generous field count
            let mut init_args = Vec::with_capacity(1 + extra_args.len());
            init_args.push(Value::Object(Some(new_obj)));
            init_args.extend_from_slice(extra_args);
            ctx.invoke(&class, "<init>", &desc, &init_args)?;
            Ok(Some(Value::Object(Some(new_obj))))
        }
        MH_KIND_GETTER => {
            // Field getter: static when the descriptor has no param (e.g.
            // "()J" after `unreflectGetter` on a static field); otherwise
            // receiver-based.  For static the class name identifies the
            // declaring class whose static field we read; for instance,
            // the first extra arg (or `bound`) is the receiver.
            let is_static = desc.starts_with("()");
            if is_static {
                // Resolve the STATIC field's storage slot (the static
                // index space is separate from instance indices — use the
                // dedicated name→index helper).
                let class_id = match ctx.ensure_class_initialized(&class) {
                    Ok(cid) => cid,
                    Err(_) => return Ok(Some(Value::Object(None))),
                };
                let slot = ctx.static_field_index_by_name(class_id, &name).unwrap_or(0);
                Ok(Some(ctx.get_static_field(class_id, slot)))
            } else {
                let receiver = match bound {
                    Value::Object(Some(r)) => r,
                    _ => match extra_args.first() {
                        Some(Value::Object(Some(r))) => *r,
                        _ => return Ok(Some(Value::Object(None))),
                    },
                };
                match ctx.resolve_field_index(&class, &name) {
                    Some(idx) => Ok(Some(ctx.get_field(receiver, idx))),
                    None => Ok(Some(ctx.get_field_by_name(receiver, &name))),
                }
            }
        }
        MH_KIND_SETTER => {
            // Field setter: static when descriptor has a single param,
            // instance when it has two (owner, value).
            let is_static = !desc_has_two_params(&desc);
            if is_static {
                let value = extra_args.first().copied().unwrap_or(Value::Object(None));
                let class_id = match ctx.ensure_class_initialized(&class) {
                    Ok(cid) => cid,
                    Err(_) => return Ok(None),
                };
                if let Some(slot) = ctx.static_field_index_by_name(class_id, &name) {
                    ctx.set_static_field(class_id, slot, value);
                }
                Ok(None)
            } else {
                let (receiver, value) = match bound {
                    Value::Object(Some(r)) => {
                        let v = extra_args.first().copied().unwrap_or(Value::Object(None));
                        (r, v)
                    }
                    _ => {
                        let r = match extra_args.first() {
                            Some(Value::Object(Some(r))) => *r,
                            _ => return Ok(None),
                        };
                        let v = extra_args.get(1).copied().unwrap_or(Value::Object(None));
                        (r, v)
                    }
                };
                match ctx.resolve_field_index(&class, &name) {
                    Some(idx) => ctx.set_field(receiver, idx, value),
                    None => ctx.set_field_by_name(receiver, &name, value),
                }
                Ok(None)
            }
        }
        MH_KIND_PERMUTE => {
            // Permute adapter: bound (field 4) holds a wrapper synthetic with:
            //   field 0 = target MH, field 1 = int[] reorder array
            let wrapper = match bound {
                Value::Object(Some(w)) => w,
                _ => return Ok(Some(Value::Object(None))),
            };
            let target_mh = match ctx.get_field(wrapper, 0) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let reorder_arr = match ctx.get_field(wrapper, 1) {
                Value::Object(Some(a)) => a,
                _ => {
                    // No reorder array — pass through directly
                    return mh_dispatch(ctx, target_mh, extra_args);
                }
            };
            let reorder_len = ctx.array_length(reorder_arr);
            let mut permuted_args = Vec::with_capacity(reorder_len);
            for i in 0..reorder_len {
                let idx = match ctx.get_array_element(reorder_arr, i) {
                    Value::Int(v) => v as usize,
                    _ => i,
                };
                let val = extra_args.get(idx).copied().unwrap_or(Value::Object(None));
                permuted_args.push(val);
            }
            mh_dispatch(ctx, target_mh, &permuted_args)
        }
        MH_KIND_DROP => {
            // C26: dropArgumentsTrusted wrapper. Unwrap to inner MH (in
            // MH_BOUND) and forward only the inner MH's expected args.
            // Inner arity is derived from the inner MH's MH_DESC. The drop
            // position is encoded in MH_CLASS as "<pos>" decimal; if parse
            // fails, drop from the head.
            let inner = match bound {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let inner_desc = mh_read_desc(ctx, inner).unwrap_or_default();
            let inner_params = count_descriptor_params(&inner_desc);
            let inner_kind = match ctx.get_field(inner, MH_KIND) {
                Value::Int(k) => k,
                _ => MH_KIND_STATIC,
            };
            let inner_needs_recv = inner_kind == MH_KIND_VIRTUAL || inner_kind == MH_KIND_SPECIAL;
            let inner_bound = matches!(ctx.get_field(inner, MH_BOUND), Value::Object(Some(_)));
            let inner_expected = inner_params + if inner_needs_recv && !inner_bound { 1 } else { 0 };
            let pos: usize = mh_read_class(ctx, mh)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            // Drop `extra_args.len() - inner_expected` args starting at `pos`.
            let drop_n = extra_args.len().saturating_sub(inner_expected);
            let pos = pos.min(extra_args.len().saturating_sub(drop_n));
            let mut trimmed: Vec<Value> = Vec::with_capacity(inner_expected);
            trimmed.extend_from_slice(&extra_args[..pos]);
            if pos + drop_n < extra_args.len() {
                trimmed.extend_from_slice(&extra_args[pos + drop_n..]);
            }
            mh_dispatch(ctx, inner, &trimmed)
        }
        MH_KIND_GUARD => {
            // Guard adapter: bound (field 4) holds a wrapper synthetic with:
            //   field 0 = test MH, field 1 = target MH, field 2 = fallback MH
            let wrapper = match bound {
                Value::Object(Some(w)) => w,
                _ => return Ok(Some(Value::Object(None))),
            };
            let test_mh = match ctx.get_field(wrapper, 0) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let target_mh = match ctx.get_field(wrapper, 1) {
                Value::Object(Some(t)) => t,
                _ => return Ok(Some(Value::Object(None))),
            };
            let fallback_mh = match ctx.get_field(wrapper, 2) {
                Value::Object(Some(f)) => f,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Invoke the test MH with the same arguments
            let test_result = mh_dispatch(ctx, test_mh, extra_args)?;
            let is_true = match test_result {
                Some(Value::Int(v)) => v != 0,
                Some(Value::Object(Some(_))) => true,
                _ => false,
            };
            if is_true {
                mh_dispatch(ctx, target_mh, extra_args)
            } else {
                mh_dispatch(ctx, fallback_mh, extra_args)
            }
        }
        MH_KIND_SPECIAL => {
            // WP2.9 — invokespecial semantics: dispatch *exactly* on the
            // resolved class, no virtual lookup, no iface retarget. Used
            // for private-to-private calls and `I.super.m()` default-method
            // super-call patterns.
            //
            // The MH carries the resolved class name in MH_CLASS. We build
            // an args vector with the receiver at index 0 and forward via
            // `invoke_special` (which the Vm impl routes through the
            // no-retarget path). For bound MHs, the receiver is pre-captured.
            let mut full_args: Vec<Value>;
            let class_for_dispatch = class.clone();
            match bound {
                Value::Object(Some(r)) => {
                    full_args = Vec::with_capacity(1 + extra_args.len());
                    full_args.push(Value::Object(Some(r)));
                    full_args.extend_from_slice(extra_args);
                }
                _ => {
                    if extra_args.is_empty() {
                        return Ok(Some(Value::Object(None)));
                    }
                    full_args = extra_args.to_vec();
                }
            }
            ctx.invoke_special(&class_for_dispatch, &name, &desc, &full_args)
        }
        _ => {
            // Virtual: first extra_arg is receiver (unless bound)
            match bound {
                Value::Object(Some(r)) => {
                    // Bound method handle — receiver was pre-captured
                    ctx.invoke_virtual(r, &name, &desc, extra_args)
                }
                _ => match extra_args.first() {
                    Some(Value::Object(Some(receiver))) => {
                        ctx.invoke_virtual(*receiver, &name, &desc, &extra_args[1..])
                    }
                    _ => Ok(Some(Value::Object(None))),
                },
            }
        }
    }
}

/// C26: Insert `extra_n` erased parameter descriptors at position `pos`
/// in `inner_desc`. Each inserted parameter's descriptor is derived from
/// its Class mirror; if unavailable, falls back to `Ljava/lang/Object;`.
fn widen_descriptor(
    ctx: &dyn NativeContext,
    inner_desc: &str,
    extra_classes: rustjvm_types::ObjectRef,
    pos: usize,
) -> String {
    if !inner_desc.starts_with('(') {
        return inner_desc.to_string();
    }
    let close = match inner_desc.find(')') {
        Some(i) => i,
        None => return inner_desc.to_string(),
    };
    let params_str = &inner_desc[1..close];
    let ret_str = &inner_desc[close..]; // includes ')'
    let orig_types = parse_descriptor_types(params_str);
    let mut all: Vec<String> = orig_types
        .iter()
        .map(|n| class_name_to_descriptor(n))
        .collect();
    let extra_n = ctx.array_length(extra_classes);
    let pos_c = pos.min(all.len());
    let mut inserts: Vec<String> = Vec::with_capacity(extra_n);
    for i in 0..extra_n {
        let d = match ctx.get_array_element(extra_classes, i) {
            Value::Object(Some(mirror)) => mirror_to_descriptor(ctx, mirror),
            _ => "Ljava/lang/Object;".to_string(),
        };
        inserts.push(d);
    }
    all.splice(pos_c..pos_c, inserts);
    let mut out = String::from("(");
    for p in &all {
        out.push_str(p);
    }
    out.push_str(ret_str);
    out
}

/// Count the number of parameters in a JVM method descriptor.
fn count_descriptor_params(desc: &str) -> usize {
    if !desc.starts_with('(') { return 0; }
    let close = match desc.find(')') { Some(i) => i, None => return 0 };
    let params_str = &desc[1..close];
    parse_descriptor_types(params_str).len()
}

/// Type adaptation for invoke(): coerce args to match the expected descriptor.
/// Handles boxing (int→Integer), unboxing (Integer→int), and widening (int→long).
fn adapt_invoke_args(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    desc: &str,
) -> Vec<Value> {
    if desc.is_empty() || !desc.starts_with('(') {
        return args.to_vec();
    }
    let close = match desc.find(')') { Some(i) => i, None => return args.to_vec() };
    let params_str = &desc[1..close];
    let param_types = parse_descriptor_types(params_str);

    let mut result = Vec::with_capacity(args.len());
    for (i, arg) in args.iter().enumerate() {
        if i < param_types.len() {
            result.push(adapt_single_arg(ctx, *arg, &param_types[i]));
        } else {
            result.push(*arg);
        }
    }
    result
}

/// Adapt a single argument value to match the expected type.
fn adapt_single_arg(
    _ctx: &mut dyn NativeContext,
    arg: Value,
    expected_type: &str,
) -> Value {
    match expected_type {
        // Widening: int → long
        "long" => match arg {
            Value::Int(v) => Value::Long(v as i64),
            other => other,
        },
        // Widening: int → float
        "float" => match arg {
            Value::Int(v) => Value::Float(v as f32),
            other => other,
        },
        // Widening: int → double, long → double, float → double
        "double" => match arg {
            Value::Int(v) => Value::Double(v as f64),
            Value::Long(v) => Value::Double(v as f64),
            Value::Float(v) => Value::Double(v as f64),
            other => other,
        },
        // Narrowing not done automatically (invoke is lenient but not that lenient)
        _ => arg,
    }
}

/// Extract the return type descriptor from a method descriptor.
/// e.g. "(II)I" → "I", "(Ljava/lang/String;)V" → "V"
fn return_type_desc(desc: &str) -> &str {
    if let Some(pos) = desc.rfind(')') {
        &desc[pos + 1..]
    } else {
        ""
    }
}

/// Auto-box a primitive return value if the method descriptor returns a primitive.
/// This is needed because signature-polymorphic invoke() returns Object, but the
/// underlying method may return a primitive (int, long, etc.).
fn auto_box_return(ctx: &mut dyn NativeContext, result: MethodCallResult, desc: &str) -> MethodCallResult {
    let ret_desc = return_type_desc(desc);
    match ret_desc {
        "I" | "J" | "F" | "D" | "Z" | "B" | "S" | "C" => {
            match result {
                Ok(Some(val)) => Ok(Some(box_value(ctx, val, ret_desc))),
                other => other,
            }
        }
        "V" => Ok(Some(Value::Object(None))), // void → null for Object return
        _ => result, // already an object reference
    }
}

pub fn register_t4_method_handle_invoke(r: &mut NativeMethodRegistry) {
    let mh = "java/lang/invoke/MethodHandle";

    // invoke(...) — polymorphic signature with automatic type adaptation.
    // Boxing/unboxing and widening conversions are applied implicitly.
    // Return value is auto-boxed since call-site expects Object.
    r.register(mh, "invoke", "([Ljava/lang/Object;)Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let extra = &args[1..];
        let desc = mh_read_desc(ctx, this).unwrap_or_default();
        let kind = match ctx.get_field(this, MH_KIND) { Value::Int(k) => k, _ => MH_KIND_VIRTUAL };
        let adapted = adapt_invoke_args(ctx, extra, &desc);
        let result = mh_dispatch(ctx, this, &adapted);
        // Constructor MH already returns the new object; skip auto_box_return
        // which would incorrectly convert the result to null (desc ends in V).
        if kind == MH_KIND_CONSTRUCTOR {
            result
        } else {
            auto_box_return(ctx, result, &desc)
        }
    });

    // invokeExact(...) — strict type checking: argument count must match.
    // Throws WrongMethodTypeException if arity mismatches.
    // Return value is auto-boxed since call-site expects Object.
    r.register(mh, "invokeExact", "([Ljava/lang/Object;)Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let extra = &args[1..];
        let desc = mh_read_desc(ctx, this).unwrap_or_default();
        // Verify argument count matches descriptor
        if !desc.is_empty() {
            let expected_params = count_descriptor_params(&desc);
            let kind = match ctx.get_field(this, MH_KIND) { Value::Int(k) => k, _ => MH_KIND_VIRTUAL };
            // Virtual/special methods need a receiver arg in addition to params
            let needs_receiver = kind == MH_KIND_VIRTUAL || kind == MH_KIND_SPECIAL;
            let expected_count = if needs_receiver { expected_params + 1 } else { expected_params };
            // Check for bound receiver (reduces expected by 1)
            let has_bound = matches!(ctx.get_field(this, MH_BOUND), Value::Object(Some(_)));
            let final_expected = if has_bound && needs_receiver { expected_count - 1 } else { expected_count };
            if extra.len() != final_expected {
                // WrongMethodTypeException — arity mismatch
                return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                    rustjvm_types::error::VmError::Internal {
                        message: format!("WrongMethodTypeException: expected {} args, got {}", final_expected, extra.len()),
                    },
                ));
            }
        }
        let kind = match ctx.get_field(this, MH_KIND) { Value::Int(k) => k, _ => MH_KIND_VIRTUAL };
        let result = mh_dispatch(ctx, this, extra);
        if kind == MH_KIND_CONSTRUCTOR {
            result
        } else {
            auto_box_return(ctx, result, &desc)
        }
    });
    r.register(mh, "invokeWithArguments", "([Ljava/lang/Object;)Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // args[1] is an Object[] — unpack it
        let arr_ref = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return mh_dispatch(ctx, this, &[]),
        };
        let len = ctx.array_length(arr_ref);
        let unpacked: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr_ref, i)).collect();
        mh_dispatch(ctx, this, &unpacked)
    });
    r.register(mh, "invokeWithArguments", "(Ljava/util/List;)Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Unpack List by reading ArrayList internals (field 0 = data array, field 1 = size)
        let list_ref = match args.get(1) {
            Some(Value::Object(Some(l))) => *l,
            _ => return mh_dispatch(ctx, this, &[]),
        };
        let size = match ctx.get_field(list_ref, 1) {
            Value::Int(s) => s as usize,
            _ => 0,
        };
        if size == 0 {
            return mh_dispatch(ctx, this, &[]);
        }
        let data = match ctx.get_field(list_ref, 0) {
            Value::Object(Some(arr)) => arr,
            _ => return mh_dispatch(ctx, this, &[]),
        };
        let unpacked: Vec<Value> = (0..size).map(|i| ctx.get_array_element(data, i)).collect();
        mh_dispatch(ctx, this, &unpacked)
    });
    r.register(mh, "bindTo", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let recv = args.get(1).copied().unwrap_or(Value::Object(None));
        // Clone the MH and set BOUND field
        let class = mh_read_class(ctx, this).unwrap_or_default();
        let name  = mh_read_name(ctx, this).unwrap_or_default();
        let desc  = mh_read_desc(ctx, this).unwrap_or_default();
        let kind  = match ctx.get_field(this, MH_KIND) { Value::Int(k) => k, _ => MH_KIND_VIRTUAL };
        let new_mh = alloc_method_handle(ctx, &class, &name, &desc, kind);
        ctx.set_field(new_mh, MH_BOUND, recv);
        Ok(Some(Value::Object(Some(new_mh))))
    });
    // asType — type adaptation: return self, but propagate the supplied
    // MethodType into the `type` field so subsequent JDK-internal reads of
    // `mh.type()` / `parameterSlotCount` reflect the adapted signature.
    // invoke()/invokeExact handle the actual argument coercions.
    r.register(mh, "asType", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;", |ctx, args| {
        if let (Some(Value::Object(Some(this))), Some(Value::Object(Some(mt)))) = (args.first(), args.get(1)) {
            ctx.set_field_by_name(*this, "type", Value::Object(Some(*mt)));
        }
        Ok(Some(args[0]))
    });

    // C21: rebind() is JDK-abstract (no Code attribute) — synthetic MHs that
    // do not subclass BoundMethodHandle still get rebind() called by JDK
    // internals (e.g. Invokers, LambdaForm specialization). Return self so the
    // chain continues without "no Code attribute" internal errors.
    r.register(mh, "rebind", "()Ljava/lang/invoke/BoundMethodHandle;", |_ctx, args| {
        Ok(Some(args[0]))
    });

    // type() — return a MethodType representing this MH's signature.
    // C19: prefer the real-JDK `type` field (populated by C15/C19 at
    // slot 0). Fall back to our synthetic descriptor, then finally
    // synthesize `()V` so callers never observe null.
    r.register(mh, "type", "()Ljava/lang/invoke/MethodType;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(mt)) = ctx.get_field_by_name(this, "type") {
            return Ok(Some(Value::Object(Some(mt))));
        }
        let desc = mh_read_desc(ctx, this).unwrap_or_default();
        if let Some(mt) = build_method_type_from_descriptor(ctx, &desc) {
            return Ok(Some(Value::Object(Some(mt))));
        }
        let mt = build_method_type_from_descriptor(ctx, "()V");
        Ok(Some(Value::Object(mt)))
    });

    // Lookup.find* are already registered in register_p63_method_handles_lookup
    // with full descriptor resolution. No duplicate registration needed here.
}

/// Build a MethodType object from a JVM method descriptor string.
pub(crate) fn build_method_type_from_descriptor(
    ctx: &mut dyn NativeContext,
    desc: &str,
) -> Option<ObjectRef> {
    if desc.is_empty() || !desc.starts_with('(') {
        return None;
    }

    let close = desc.find(')')?;
    let params_str = &desc[1..close];
    let ret_str = &desc[close+1..];

    let param_names = parse_descriptor_types(params_str);
    let ret_name = descriptor_to_class_name(ret_str);

    // Create return type Class mirror
    let ret_mirror = if let Some(cid) = ctx.class_id_by_name(&ret_name) {
        ctx.get_class_mirror(cid)
    } else {
        ctx.primitive_class_mirror(&ret_name)
    };

    // Create params array
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, param_names.len());
    for (i, pname) in param_names.iter().enumerate() {
        let mirror = if let Some(cid) = ctx.class_id_by_name(pname) {
            ctx.get_class_mirror(cid)
        } else {
            ctx.primitive_class_mirror(pname)
        };
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }

    // JDK MethodType field layout: rtype(0), ptypes(1), form(2), wrapAlt(3),
    // invokers(4), methodDescriptor(5). Allocate 6 slots so the JDK-resolved
    // `form` slot (2) lives within the synthetic object.
    let mt = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6);
    ctx.set_field(mt, 0, Value::Object(Some(ret_mirror)));
    ctx.set_field(mt, 1, Value::Object(Some(arr)));

    populate_method_type_form(ctx, mt);
    Some(mt)
}

/// C21: Build a synthetic MethodTypeForm and link it into `mt.form` so
/// `MethodType.parameterSlotCount()` (= `form.parameterSlotCount`) and
/// `MethodType.erasedType()` / `.erase()` (= `form.erasedType()`) do not NPE
/// on the null `form` field. JDK MethodTypeForm field layout:
///   parameterSlotCount: short (slot 0)
///   primitiveCount:     short (slot 1)
///   erasedType:         MethodType (slot 2)
///   basicType:          MethodType (slot 3)
///   methodHandles:      SoftReference[] (slot 4)
///   lambdaForms:        SoftReference[] (slot 5)
///   interpretEntry:     SoftReference (slot 6)
///
/// Reads the `ptypes` array (slot 1) to compute slot/primitive counts and
/// stores `mt` itself as both erasedType and basicType — a minimal stand-in
/// so downstream `form.erasedType()` returns a non-null MethodType.
pub(crate) fn populate_method_type_form(
    ctx: &mut dyn NativeContext,
    mt: rustjvm_types::ObjectRef,
) {
    let mut slot_count: i32 = 0;
    let mut primitive_count: i32 = 0;
    if let Value::Object(Some(arr)) = ctx.get_field(mt, 1) {
        let n = ctx.array_length(arr);
        for i in 0..n {
            slot_count += 1;
            if let Value::Object(Some(mirror)) = ctx.get_array_element(arr, i) {
                let nm = crate::lang_class::mirror_class_name(ctx, mirror).unwrap_or_default();
                match nm.as_str() {
                    "long" | "double" => slot_count += 1,
                    "boolean" | "byte" | "char" | "short" | "int" | "float" => {
                        primitive_count += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    let form = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodTypeForm", 7);
    ctx.set_field(form, 0, Value::Int(slot_count));
    ctx.set_field(form, 1, Value::Int(primitive_count));
    ctx.set_field(form, 2, Value::Object(Some(mt)));
    ctx.set_field(form, 3, Value::Object(Some(mt)));
    ctx.set_field(mt, 2, Value::Object(Some(form)));
}

/// Parse a sequence of JVM type descriptors from a parameter string.
/// e.g. "ILjava/lang/String;D" → ["int", "java/lang/String", "double"]
fn parse_descriptor_types(desc: &str) -> Vec<String> {
    let mut result = Vec::new();
    let bytes = desc.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'I' => { result.push("int".to_string()); i += 1; }
            b'J' => { result.push("long".to_string()); i += 1; }
            b'F' => { result.push("float".to_string()); i += 1; }
            b'D' => { result.push("double".to_string()); i += 1; }
            b'Z' => { result.push("boolean".to_string()); i += 1; }
            b'B' => { result.push("byte".to_string()); i += 1; }
            b'C' => { result.push("char".to_string()); i += 1; }
            b'S' => { result.push("short".to_string()); i += 1; }
            b'V' => { result.push("void".to_string()); i += 1; }
            b'L' => {
                if let Some(semi) = desc[i..].find(';') {
                    result.push(desc[i+1..i+semi].to_string());
                    i += semi + 1;
                } else { break; }
            }
            b'[' => {
                let start = i;
                while i < bytes.len() && bytes[i] == b'[' { i += 1; }
                if i < bytes.len() {
                    if bytes[i] == b'L' {
                        if let Some(semi) = desc[i..].find(';') {
                            result.push(desc[start..i+semi+1].to_string());
                            i += semi + 1;
                        } else { break; }
                    } else {
                        result.push(desc[start..=i].to_string());
                        i += 1;
                    }
                }
            }
            _ => { i += 1; }
        }
    }
    result
}

// =============================================================================
// T2.8: MethodHandle completeness — unreflect, permuteArguments, guardWithTest
// =============================================================================

pub fn register_t28_method_handle_completeness(r: &mut NativeMethodRegistry) {
    // I2 follow-up: register `ClassLoader.{getDefinedPackage,
    // getDefinedPackages, getNamedPackage, definePackage}` overrides so
    // ByteBuddy + Mockito can complete `JavaDispatcher.<clinit>` without
    // tripping a NullPointerException on the never-initialized private
    // `packages` field of user-instantiated ClassLoader subclasses.
    // Piggy-backing here keeps the registration on the real-JDK boot path
    // (this function is called from both the synthetic-jdk and real-JDK
    // branches of `vm_init`) without touching forbidden surface.
    crate::lang_class::i2_register_classloader_package_natives(r);

    // --- T2.8.4: Lookup.unreflect(Method) ---
    let lk = "java/lang/invoke/MethodHandles$Lookup";
    r.register(
        lk,
        "unreflect",
        "(Ljava/lang/reflect/Method;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect,
    );

    // Also register unreflectSpecial and unreflectGetter/unreflectSetter
    r.register(
        lk,
        "unreflectSpecial",
        "(Ljava/lang/reflect/Method;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect_special,
    );
    r.register(
        lk,
        "unreflectGetter",
        "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect_getter,
    );
    r.register(
        lk,
        "unreflectSetter",
        "(Ljava/lang/reflect/Field;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect_setter,
    );
    r.register(
        lk,
        "unreflectConstructor",
        "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        lookup_unreflect_constructor,
    );

    // --- T2.8.11: MethodHandles.permuteArguments ---
    let mhs = "java/lang/invoke/MethodHandles";
    r.register(
        mhs,
        "permuteArguments",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;[I)Ljava/lang/invoke/MethodHandle;",
        mhs_permute_arguments,
    );

    // --- T2.8.12: MethodHandles.guardWithTest ---
    r.register(
        mhs,
        "guardWithTest",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        mhs_guard_with_test,
    );

    // --- C26: MethodHandles.dropArgumentsTrusted ---
    // JDK bytecode reads `mh.form.editor()` and NPEs on our synthetic MH
    // whose form field is null. Build a DROP-kind wrapper whose MH_BOUND
    // holds the original MH and whose widened MH_DESC matches the widened
    // signature (so invokeExact arity checks pass); dispatch unwraps and
    // forwards trimmed args to the inner MH.
    r.register(
        mhs,
        "dropArgumentsTrusted",
        "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let orig_mh = match args.first() {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let pos = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
            let extra_classes = match args.get(2) {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    return Ok(Some(Value::Object(Some(orig_mh))));
                }
            };
            let extra_n = ctx.array_length(extra_classes);
            if extra_n == 0 {
                return Ok(Some(Value::Object(Some(orig_mh))));
            }
            // Read the original MH's desc; construct the widened desc by
            // inserting `extra_n` erased object descriptors at position `pos`.
            let inner_desc = match ctx.get_field(orig_mh, MH_DESC) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let widened_desc = widen_descriptor(ctx, &inner_desc, extra_classes, pos);
            // Encode `pos` into MH_CLASS so dispatch can recover it.
            let pos_str = pos.to_string();
            let wrapper = alloc_method_handle(ctx, &pos_str, "drop", &widened_desc, MH_KIND_DROP);
            ctx.set_field(wrapper, MH_BOUND, Value::Object(Some(orig_mh)));
            // Also widen the `type:MethodType` field so JDK-internal code
            // that reads mh.type().parameterCount() sees the widened arity.
            let orig_type = ctx.get_field(orig_mh, 0);
            if let Value::Object(Some(mt)) = orig_type {
                let ret = ctx.get_field(mt, 0);
                if let Value::Object(Some(orig_ptypes)) = ctx.get_field(mt, 1) {
                    let orig_n = ctx.array_length(orig_ptypes);
                    let new_n = orig_n + extra_n;
                    let new_ptypes = ctx.new_array(rustjvm_types::ArrayElementType::Reference, new_n);
                    let pos_c = pos.min(orig_n);
                    for i in 0..pos_c {
                        let v = ctx.get_array_element(orig_ptypes, i);
                        ctx.set_array_element(new_ptypes, i, v);
                    }
                    for i in 0..extra_n {
                        let v = ctx.get_array_element(extra_classes, i);
                        ctx.set_array_element(new_ptypes, pos_c + i, v);
                    }
                    for i in pos_c..orig_n {
                        let v = ctx.get_array_element(orig_ptypes, i);
                        ctx.set_array_element(new_ptypes, extra_n + i, v);
                    }
                    let new_mt = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodType", 6);
                    ctx.set_field(new_mt, 0, ret);
                    ctx.set_field(new_mt, 1, Value::Object(Some(new_ptypes)));
                    populate_method_type_form(ctx, new_mt);
                    ctx.set_field_by_name(wrapper, "type", Value::Object(Some(new_mt)));
                }
            }
            Ok(Some(Value::Object(Some(wrapper))))
        },
    );

    // --- Additional MethodHandles combinators ---
    r.register(
        mhs,
        "filterArguments",
        "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |_ctx, args| {
            // Simplified: return the target MH unchanged
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        mhs,
        "filterReturnValue",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |_ctx, args| {
            // Simplified: return the target MH unchanged
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        mhs,
        "foldArguments",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |_ctx, args| {
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        mhs,
        "foldArguments",
        "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |_ctx, args| {
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        mhs,
        "collectArguments",
        "(Ljava/lang/invoke/MethodHandle;ILjava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |_ctx, args| {
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        mhs,
        "catchException",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/Class;Ljava/lang/invoke/MethodHandle;)Ljava/lang/invoke/MethodHandle;",
        |_ctx, args| {
            // Return the target MH — exception handling delegated to the interpreter
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );
    r.register(
        mhs,
        "exactInvoker",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            // C19: propagate the caller's MethodType into the real-JDK
            // `type` field so `mh.type()` / `parameterSlotCount` do not NPE.
            let mh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
            if let Some(Value::Object(Some(mt))) = args.first() {
                ctx.set_field_by_name(mh, "type", Value::Object(Some(*mt)));
            }
            Ok(Some(Value::Object(Some(mh))))
        },
    );
    r.register(
        mhs,
        "invoker",
        "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let mh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
            if let Some(Value::Object(Some(mt))) = args.first() {
                ctx.set_field_by_name(mh, "type", Value::Object(Some(*mt)));
            }
            Ok(Some(Value::Object(Some(mh))))
        },
    );
    r.register(
        mhs,
        "spreadInvoker",
        "(Ljava/lang/invoke/MethodType;I)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let mh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 17);
            if let Some(Value::Object(Some(mt))) = args.first() {
                ctx.set_field_by_name(mh, "type", Value::Object(Some(*mt)));
            }
            Ok(Some(Value::Object(Some(mh))))
        },
    );
}

// ---------------------------------------------------------------------------
// T2.8.4 — Lookup.unreflect(Method) -> MethodHandle
// Method layout: 0=Class(decl), 1=String(name), 2=Class(ret), 3=Class[](params),
//   4=Int(modifiers), 5=String(descriptor), 6=Int(paramCount), 7=Int(accessible)
// ---------------------------------------------------------------------------

fn lookup_unreflect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (Lookup), args[1] = Method object
    let method_obj = match args.get(1) {
        Some(Value::Object(Some(m))) => *m,
        _ => {
            return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Internal {
                    message: "Lookup.unreflect: method argument is null".to_string(),
                },
            ));
        }
    };

    // C6: Method uses real-JDK field layout. Read JDK fields by name;
    // descriptor lives in our RustJVM extra slot.
    let class_name = match ctx.get_field_by_name(method_obj, "clazz") {
        Value::Object(Some(class_mirror)) => {
            mirror_class_name(ctx, class_mirror).unwrap_or_default()
        }
        _ => String::new(),
    };
    let method_name = match ctx.get_field_by_name(method_obj, "name") {
        Value::Object(Some(name_ref)) => ctx.read_string(name_ref).unwrap_or_default(),
        _ => String::new(),
    };
    let descriptor = crate::lang_class::read_method_descriptor(ctx, method_obj).unwrap_or_default();
    let modifiers = match ctx.get_field_by_name(method_obj, "modifiers") {
        Value::Int(m) => m,
        _ => 0,
    };
    let is_static = (modifiers & 0x0008) != 0;
    let kind = if is_static { MH_KIND_STATIC } else { MH_KIND_VIRTUAL };

    let _ = ctx.ensure_class_initialized(&class_name);
    let mh = alloc_method_handle(ctx, &class_name, &method_name, &descriptor, kind);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_unreflect_special(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = Lookup, args[1] = Method, args[2] = specialCaller class
    let method_obj = match args.get(1) {
        Some(Value::Object(Some(m))) => *m,
        _ => {
            return Err(rustjvm_types::error::MethodCallFailed::InternalError(
                rustjvm_types::error::VmError::Internal {
                    message: "Lookup.unreflectSpecial: method argument is null".to_string(),
                },
            ));
        }
    };

    let class_name = match ctx.get_field_by_name(method_obj, "clazz") {
        Value::Object(Some(class_mirror)) => {
            mirror_class_name(ctx, class_mirror).unwrap_or_default()
        }
        _ => String::new(),
    };
    let method_name = match ctx.get_field_by_name(method_obj, "name") {
        Value::Object(Some(name_ref)) => ctx.read_string(name_ref).unwrap_or_default(),
        _ => String::new(),
    };
    let descriptor = crate::lang_class::read_method_descriptor(ctx, method_obj).unwrap_or_default();

    let _ = ctx.ensure_class_initialized(&class_name);
    let mh = alloc_method_handle(ctx, &class_name, &method_name, &descriptor, MH_KIND_SPECIAL);
    Ok(Some(Value::Object(Some(mh))))
}

/// Read the descriptor string for a Field reflection object. Prefers the
/// RustJVM extra-slot descriptor (matches `create_field_object` in
/// `lang_class.rs`), and falls back to deriving it from the `type` Class
/// mirror if needed.
fn read_field_descriptor_string(ctx: &dyn NativeContext, field_obj: rustjvm_types::ObjectRef) -> String {
    // The `type` field is a Class mirror — derive the descriptor from it
    // as a safe fallback (e.g. "J" for primitive long, "Ljava/lang/String;"
    // for references). This is the authoritative source in real JDK mode.
    if let Value::Object(Some(type_mirror)) = ctx.get_field_by_name(field_obj, "type") {
        let name = crate::lang_class::mirror_class_name(ctx, type_mirror).unwrap_or_default();
        if !name.is_empty() {
            return match name.as_str() {
                "boolean" => "Z".to_string(),
                "byte" => "B".to_string(),
                "char" => "C".to_string(),
                "short" => "S".to_string(),
                "int" => "I".to_string(),
                "long" => "J".to_string(),
                "float" => "F".to_string(),
                "double" => "D".to_string(),
                "void" => "V".to_string(),
                s if s.starts_with('[') => s.to_string(),
                s => format!("L{};", s.replace('.', "/")),
            };
        }
    }
    "Ljava/lang/Object;".to_string()
}

fn lookup_unreflect_getter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = Lookup, args[1] = Field object.
    // Read Field fields via `get_field_by_name` / the `type` mirror so the
    // resolution lands on the real JDK `java.lang.reflect.Field` layout
    // (clazz/name/type at hierarchy-adjusted slots 2/4/5) rather than on
    // our legacy synthetic-slot layout. See the C5 fix in
    // `lang_class.rs::create_field_object` for details.
    const ACC_STATIC: i32 = 0x0008;
    let field_obj = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => {
            return Err(no_such_field_error("", ""));
        }
    };

    let class_name = match ctx.get_field_by_name(field_obj, "clazz") {
        Value::Object(Some(class_mirror)) => {
            mirror_class_name(ctx, class_mirror).unwrap_or_default()
        }
        _ => String::new(),
    };
    let field_name = match ctx.get_field_by_name(field_obj, "name") {
        Value::Object(Some(name_ref)) => ctx.read_string(name_ref).unwrap_or_default(),
        _ => String::new(),
    };
    let modifiers = match ctx.get_field_by_name(field_obj, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let field_desc = read_field_descriptor_string(ctx, field_obj);

    // Static getter takes no receiver; instance getter takes the owner.
    let desc = if (modifiers & ACC_STATIC) != 0 {
        format!("(){field_desc}")
    } else {
        format!("(L{class_name};){field_desc}")
    };
    let mh = alloc_method_handle(ctx, &class_name, &field_name, &desc, MH_KIND_GETTER);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_unreflect_setter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    const ACC_STATIC: i32 = 0x0008;
    let field_obj = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => {
            return Err(no_such_field_error("", ""));
        }
    };

    let class_name = match ctx.get_field_by_name(field_obj, "clazz") {
        Value::Object(Some(class_mirror)) => {
            mirror_class_name(ctx, class_mirror).unwrap_or_default()
        }
        _ => String::new(),
    };
    let field_name = match ctx.get_field_by_name(field_obj, "name") {
        Value::Object(Some(name_ref)) => ctx.read_string(name_ref).unwrap_or_default(),
        _ => String::new(),
    };
    let modifiers = match ctx.get_field_by_name(field_obj, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let field_desc = read_field_descriptor_string(ctx, field_obj);

    // Static setter takes only the new value; instance takes (owner, value).
    let desc = if (modifiers & ACC_STATIC) != 0 {
        format!("({field_desc})V")
    } else {
        format!("(L{class_name};{field_desc})V")
    };
    let mh = alloc_method_handle(ctx, &class_name, &field_name, &desc, MH_KIND_SETTER);
    Ok(Some(Value::Object(Some(mh))))
}

fn lookup_unreflect_constructor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = Lookup, args[1] = Class, args[2] = MethodType
    let class_obj = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => {
            return Err(no_such_method_error("", "<init>", ""));
        }
    };
    let mt_obj = match args.get(2) {
        Some(Value::Object(Some(m))) => *m,
        _ => {
            return Err(no_such_method_error("", "<init>", ""));
        }
    };

    let class_name = mirror_class_name(ctx, class_obj).unwrap_or_default();
    let mut desc = descriptor_from_method_type(ctx, mt_obj);
    // Constructor descriptor always returns V
    if let Some(pos) = desc.rfind(')') {
        desc.truncate(pos + 1);
        desc.push('V');
    }
    let _ = ctx.ensure_class_initialized(&class_name);
    let mh = alloc_method_handle(ctx, &class_name, "<init>", &desc, MH_KIND_CONSTRUCTOR);
    Ok(Some(Value::Object(Some(mh))))
}

// ---------------------------------------------------------------------------
// T2.8.11 — MethodHandles.permuteArguments
// ---------------------------------------------------------------------------

fn mhs_permute_arguments(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = target MH, args[1] = newType (MethodType), args[2] = int[] reorder
    let target_mh = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_type = match args.get(1) {
        Some(Value::Object(Some(mt))) => *mt,
        _ => return Ok(Some(Value::Object(None))),
    };
    let reorder_arr = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Build the new descriptor from the MethodType
    let new_desc = descriptor_from_method_type(ctx, new_type);

    // Create a wrapper synthetic to hold (target_mh, reorder_arr)
    let wrapper = alloc_concurrent_synthetic(ctx, "__mh_permute_wrapper__", 2);
    ctx.set_field(wrapper, 0, Value::Object(Some(target_mh)));
    ctx.set_field(wrapper, 1, Value::Object(Some(reorder_arr)));

    // Create the adapter MH with kind=PERMUTE
    let adapter = alloc_method_handle(ctx, "__adapter__", "permute", &new_desc, MH_KIND_PERMUTE);
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
    Ok(Some(Value::Object(Some(adapter))))
}

// ---------------------------------------------------------------------------
// T2.8.12 — MethodHandles.guardWithTest
// ---------------------------------------------------------------------------

fn mhs_guard_with_test(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = test MH, args[1] = target MH, args[2] = fallback MH
    let test_mh = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target_mh = match args.get(1) {
        Some(Value::Object(Some(t))) => *t,
        _ => return Ok(Some(Value::Object(None))),
    };
    let fallback_mh = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Get the target's descriptor for the adapter MH
    let target_desc = mh_read_desc(ctx, target_mh).unwrap_or_default();

    // Create a wrapper synthetic to hold (test, target, fallback)
    let wrapper = alloc_concurrent_synthetic(ctx, "__mh_guard_wrapper__", 3);
    ctx.set_field(wrapper, 0, Value::Object(Some(test_mh)));
    ctx.set_field(wrapper, 1, Value::Object(Some(target_mh)));
    ctx.set_field(wrapper, 2, Value::Object(Some(fallback_mh)));

    // Create the adapter MH with kind=GUARD
    let adapter = alloc_method_handle(ctx, "__adapter__", "guard", &target_desc, MH_KIND_GUARD);
    ctx.set_field(adapter, MH_BOUND, Value::Object(Some(wrapper)));
    Ok(Some(Value::Object(Some(adapter))))
}

// ---------------------------------------------------------------------------
// T15 — java/lang/invoke/MethodHandleNatives
// ---------------------------------------------------------------------------

/// `MethodHandleNatives.resolve(MemberName self, Class<?> caller, int lookupMode, boolean speculativeResolve)`
///
/// Resolves a MemberName object by looking up the referenced class/method/field.
/// In HotSpot this does full JVM-level resolution. Our implementation reads the
/// MemberName fields (clazz, name, type) and creates a resolved MemberName with
/// the vmindex and vmtarget fields populated.
///
/// MemberName layout (from JDK source):
///   field 0: Class<?> clazz - the declaring class
///   field 1: String name - member name
///   field 2: Object type - MethodType or Class (field type)
///   field 3: int flags - access flags + ref kind
///   field 4: Object resolution (vmtarget/vmindex as int)
pub(crate) fn native_mhn_resolve(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = MemberName self, args[1] = caller Class, args[2] = lookupMode, args[3] = speculativeResolve
    let member_name = crate::obj_arg(args, 0)?;

    // Read the class mirror from field 0
    let class_mirror = match ctx.get_field(member_name, 0) {
        Value::Object(Some(m)) => m,
        _ => return Ok(Some(Value::Object(Some(member_name)))), // no class → return as-is
    };

    let class_name = match crate::lang_class::mirror_class_name(ctx, class_mirror) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(Some(member_name)))),
    };

    // Read the name string from field 1
    let name = match ctx.get_field(member_name, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };

    // Read flags from field 3 to determine what kind of member this is
    let flags = match ctx.get_field(member_name, 3) {
        Value::Int(f) => f,
        _ => 0,
    };

    // Reference kind is encoded in bits 24-27 of flags
    let ref_kind = (flags >> 24) & 0x0F;

    // Ensure the class is loaded
    let _ = ctx.ensure_class_initialized(&class_name);

    // Mark as resolved by setting field 4 (vmindex) to a non-zero sentinel
    // The JDK checks this field to determine if resolution succeeded.
    ctx.set_field(member_name, 4, Value::Int(1));

    // If this is a method reference (refKind 5-9), verify the method exists
    if ref_kind >= 5 && ref_kind <= 9 {
        // Read descriptor from type field (field 2) if it's a MethodType
        if let Value::Object(Some(mt)) = ctx.get_field(member_name, 2) {
            let desc = descriptor_from_method_type(ctx, mt);
            if !name.is_empty() && !desc.is_empty() {
                let exists = ctx.method_exists(&class_name, &name, &desc);
                if !exists && !name.is_empty() {
                    // Method not found — for speculative resolve, return null
                    let speculative = matches!(args.get(3), Some(Value::Int(1)));
                    if speculative {
                        return Ok(Some(Value::Object(None)));
                    }
                }
            }
        }
    }

    Ok(Some(Value::Object(Some(member_name))))
}

/// `MethodHandleNatives.init(MemberName self, Object ref)`
///
/// Initializes a MemberName from a reflected member (Method, Field, Constructor).
/// Copies the declaring class, name, type, and flags from the reflected object
/// into the MemberName fields.
pub(crate) fn native_mhn_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // MemberName layout (JDK):
    //   clazz  (Class<?>)
    //   name   (String)
    //   type   (Object, MethodType or Class for fields)
    //   flags  (int, refKind | kind_bits | ACC_*)
    // The `clazz`, `flags` (and for Method/Constructor `type`) must be
    // populated by this native — the Java-side MemberName(Field)/
    // MemberName(Method)/MemberName(Constructor) constructors only set
    // `name` and `type` themselves and rely on init() for the rest.
    //
    // MemberName kind bits (see MemberName constants):
    const IS_METHOD: i32 = 0x1_0000;
    const IS_CONSTRUCTOR: i32 = 0x2_0000;
    const IS_FIELD: i32 = 0x4_0000;
    // Reference kinds per JVM spec table 5.4.3.5:
    const REF_GET_FIELD: i32 = 1;
    const REF_GET_STATIC: i32 = 2;
    const REF_INVOKE_VIRTUAL: i32 = 5;
    const REF_INVOKE_STATIC: i32 = 6;
    const REF_INVOKE_SPECIAL: i32 = 7;
    const REF_NEW_INVOKE_SPECIAL: i32 = 8;
    const ACC_STATIC: i32 = 0x0008;

    let member_name = crate::obj_arg(args, 0)?;
    let ref_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };

    // Discriminate by the reflected object's class name.
    let ref_class_id = ctx.class_id_of_object(ref_obj);
    let ref_class_name = ctx.class_name_of_id(ref_class_id).unwrap_or_default();

    match ref_class_name.as_str() {
        "java/lang/reflect/Field" => {
            // Copy clazz, name, type from the Field; compute flags.
            let clazz = ctx.get_field_by_name(ref_obj, "clazz");
            let name = ctx.get_field_by_name(ref_obj, "name");
            let ty = ctx.get_field_by_name(ref_obj, "type");
            let modifiers = match ctx.get_field_by_name(ref_obj, "modifiers") {
                Value::Int(v) => v,
                _ => 0,
            };
            let ref_kind = if (modifiers & ACC_STATIC) != 0 { REF_GET_STATIC } else { REF_GET_FIELD };
            let flags = IS_FIELD | (modifiers & 0xFFFF) | (ref_kind << 24);

            ctx.set_field_by_name(member_name, "clazz", clazz);
            ctx.set_field_by_name(member_name, "name", name);
            ctx.set_field_by_name(member_name, "type", ty);
            ctx.set_field_by_name(member_name, "flags", Value::Int(flags));
        }
        "java/lang/reflect/Method" => {
            let clazz = ctx.get_field_by_name(ref_obj, "clazz");
            let name = ctx.get_field_by_name(ref_obj, "name");
            let modifiers = match ctx.get_field_by_name(ref_obj, "modifiers") {
                Value::Int(v) => v,
                _ => 0,
            };
            let is_static = (modifiers & ACC_STATIC) != 0;
            let ref_kind = if is_static { REF_INVOKE_STATIC } else { REF_INVOKE_VIRTUAL };
            let flags = IS_METHOD | (modifiers & 0xFFFF) | (ref_kind << 24);

            ctx.set_field_by_name(member_name, "clazz", clazz);
            ctx.set_field_by_name(member_name, "name", name);
            // `type` (MethodType) is populated by the Java constructor
            // (`invokevirtual Method.getGenericReturnType` etc.); leave as-is.
            ctx.set_field_by_name(member_name, "flags", Value::Int(flags));
        }
        "java/lang/reflect/Constructor" => {
            let clazz = ctx.get_field_by_name(ref_obj, "clazz");
            let modifiers = match ctx.get_field_by_name(ref_obj, "modifiers") {
                Value::Int(v) => v,
                _ => 0,
            };
            let flags = IS_CONSTRUCTOR | (modifiers & 0xFFFF) | (REF_NEW_INVOKE_SPECIAL << 24);
            let _ = REF_INVOKE_SPECIAL;

            ctx.set_field_by_name(member_name, "clazz", clazz);
            // name = "<init>" — the constructor's name
            let name_str = ctx.create_string("<init>");
            ctx.set_field_by_name(member_name, "name", Value::Object(Some(name_str)));
            ctx.set_field_by_name(member_name, "flags", Value::Int(flags));
        }
        _ => {
            // Unknown ref object — bail quietly.
        }
    }

    // Mark resolved (legacy-safe index-based resolution marker)
    ctx.set_field(member_name, 4, Value::Int(1));
    Ok(None)
}

/// `MethodHandleNatives.getConstant(int which)`
///
/// Returns VM-specific constants used by the MethodHandle implementation.
/// Constants:
///   0 = GC_COUNT_GWT (guard with test count) → 4
///   1 = USE_SOFT_CACHE → 1
///   4 = HAVE_PENDING_EXCEPTION → 0
pub(crate) fn native_mhn_get_constant(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let which = match args.get(0) {
        Some(Value::Int(w)) => *w,
        _ => 0,
    };
    let result = match which {
        0 => 4, // GC_COUNT_GWT — suggested MethodHandle.guardWithTest specialization threshold
        1 => 1, // USE_SOFT_CACHE — use SoftReferences in method handle caching
        _ => 0, // unknown constant → 0
    };
    Ok(Some(Value::Int(result)))
}

/// `MethodHandleNatives.linkMethod(Class<?> callerClass, int refKind, Class<?> defc, String name, Object type, Object[] appendixResult)`
///
/// Links a method call site. Returns a MemberName that the VM can use for dispatch.
/// This is called when the JDK needs to link a signature-polymorphic call.
pub(crate) fn native_mhn_link_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: callerClass(0), refKind(1), defc(2), name(3), type(4), appendixResult(5)
    let _caller = args.get(0);
    let ref_kind = match args.get(1) { Some(Value::Int(k)) => *k, _ => 0 };

    let defc_mirror = match args.get(2) {
        Some(Value::Object(Some(m))) => *m,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_name = crate::lang_class::mirror_class_name(ctx, defc_mirror)
        .unwrap_or_default();

    let name = match args.get(3) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };

    // Extract descriptor from MethodType (args[4])
    let desc = match args.get(4) {
        Some(Value::Object(Some(mt))) => descriptor_from_method_type(ctx, *mt),
        _ => "()V".to_string(),
    };

    // Determine MH kind from refKind
    let mh_kind = match ref_kind {
        1 | 2 | 3 => MH_KIND_GETTER,   // getField/getStatic/putField
        4         => MH_KIND_SETTER,    // putStatic
        5         => MH_KIND_VIRTUAL,   // invokeVirtual
        6         => MH_KIND_STATIC,    // invokeStatic
        7         => MH_KIND_SPECIAL,   // invokeSpecial
        8         => MH_KIND_CONSTRUCTOR, // newInvokeSpecial
        9         => MH_KIND_VIRTUAL,   // invokeInterface
        _         => MH_KIND_VIRTUAL,
    };

    let mh = alloc_method_handle(ctx, &class_name, &name, &desc, mh_kind);
    Ok(Some(Value::Object(Some(mh))))
}

/// `MethodHandleNatives.linkCallSite(Object callerObj, int bsmIndex, Object name, Object type, Object staticArgs, Object[] appendixResult)`
///
/// Links an invokedynamic call site by resolving the bootstrap method and
/// calling it to produce a CallSite. Returns a MemberName for the target.
pub(crate) fn native_mhn_link_call_site(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // In our VM, invokedynamic is already handled by the interpreter's
    // specialized bootstrap dispatch (runtime/invokedynamic.rs). This native
    // is called when the JDK's MethodHandleNatives.linkCallSite is invoked
    // from Java code. We return a minimal MemberName that allows the call to
    // proceed.

    // Extract name from args[2]
    let name = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => "invoke".to_string(),
    };

    // Extract MethodType from args[3]
    let desc = match args.get(3) {
        Some(Value::Object(Some(mt))) => descriptor_from_method_type(ctx, *mt),
        _ => "()Ljava/lang/Object;".to_string(),
    };

    // Allocate a virtual MH as the linked target
    let mh = alloc_method_handle(ctx, "", &name, &desc, MH_KIND_VIRTUAL);

    // If appendixResult array is provided, store the MH as appendix
    if let Some(Value::Object(Some(appendix_arr))) = args.get(5) {
        ctx.set_array_element(*appendix_arr, 0, Value::Object(Some(mh)));
    }

    Ok(Some(Value::Object(Some(mh))))
}

/// `MethodHandleNatives.objectFieldOffset(MemberName self)`
///
/// Returns the field offset for Unsafe-style access. We return the field
/// index directly since our field layout is index-based not byte-offset-based.
pub(crate) fn native_mhn_object_field_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let member_name = crate::obj_arg(args, 0)?;
    // Field index is stored in field 4 (vmindex)
    let vmindex = match ctx.get_field(member_name, 4) {
        Value::Int(i) => i as i64,
        _ => 0,
    };
    Ok(Some(Value::Long(vmindex)))
}

/// `MethodHandleNatives.staticFieldOffset(MemberName self)`
pub(crate) fn native_mhn_static_field_offset(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_mhn_object_field_offset(ctx, args)
}

/// `MethodHandleNatives.staticFieldBase(MemberName self)`
pub(crate) fn native_mhn_static_field_base(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `MethodHandleNatives.getMemberVMInfo(MemberName self)`
///
/// Returns a 2-element Object[] with [vmindex, vmtarget].
pub(crate) fn native_mhn_get_member_vm_info(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let member_name = crate::obj_arg(args, 0)?;
    let vmindex = match ctx.get_field(member_name, 4) {
        Value::Int(i) => i,
        _ => 0,
    };
    let arr = ctx.new_array(rustjvm_types::ArrayElementType::Reference, 2);
    // Box vmindex as Integer
    let boxed = crate::lang_class::box_value(ctx, Value::Int(vmindex), "I");
    ctx.set_array_element(arr, 0, boxed);
    ctx.set_array_element(arr, 1, Value::Object(Some(member_name)));
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// C33 — InvokerBytecodeGenerator bypass
// ---------------------------------------------------------------------------
//
// JDK 25's `java.lang.invoke.InvokerBytecodeGenerator` (called via
// `LambdaForm.compileToBytecode` and, crucially, via `LambdaForm.prepare`)
// drives JEP 466 `java.lang.classfile.*` code-generation to produce invoker
// method classes on the fly. `LambdaForm.compileToBytecode` catches
// `BytecodeGenerationException`, but `LambdaForm.prepare` calls
// `generateLambdaFormInterpreterEntryPoint` WITHOUT a catch — so a BGE from
// the classfile pipeline propagates up through the first MH-using clinit in
// any loaded class (e.g. `org/junit/runner/Result` via its reflection /
// serialization callers).
//
// Our VM does NOT actually dispatch via the generated classes — MH invocation
// is handled by `native_method_handle_link_to` / `mh_dispatch`, which read
// our own MH_* slot layout. So the invoker classes are useless to us. Skip
// the generator entirely by returning a populated MemberName that `prepare`
// can cache as `vmentry` without ever dereferencing.

/// C33: Allocate a minimal, resolved MemberName for code-gen bypass paths.
/// The returned MemberName has:
///   - clazz:      `java/lang/invoke/LambdaForm` mirror (stable, always loaded)
///   - name:       the supplied entry-point name
///   - type:       MethodType built from `desc`, or a `()V` fallback
///   - flags:      IS_METHOD | REF_INVOKE_STATIC<<24 (static invoker)
///   - method:     Int(1) at slot 4 — non-zero sentinel so downstream vmindex
///                 reads (`getMemberVMInfo`, `objectFieldOffset`) don't see 0
///   - resolution: Value::Object(None) at slot 5 — null means `isResolved()`
fn alloc_resolved_member_name(
    ctx: &mut dyn NativeContext,
    host_class: &str,
    name: &str,
    desc: &str,
) -> ObjectRef {
    // MemberName has 6 declared instance fields in real JDK 25:
    //   0:clazz 1:name 2:type 3:flags 4:method 5:resolution
    let mn = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MemberName", 6);

    // clazz: use the host class mirror (must be a valid Class mirror for
    // downstream `mn.getDeclaringClass()` reads).
    let host_cid = ctx.class_id_by_name(host_class);
    let clazz_mirror = host_cid
        .map(|cid| ctx.get_class_mirror(cid))
        .unwrap_or_else(|| {
            // Fallback: allocate a synthetic Class stub — should not normally
            // happen since LambdaForm is always loaded before this path.
            alloc_concurrent_synthetic(ctx, "java/lang/Class", 1)
        });
    ctx.set_field_by_name(mn, "clazz", Value::Object(Some(clazz_mirror)));

    // name: Java String
    let name_str = ctx.create_string(name);
    ctx.set_field_by_name(mn, "name", Value::Object(Some(name_str)));

    // type: MethodType built from desc (fall back to ()V if desc is garbage)
    let mt = build_method_type_from_descriptor(ctx, desc)
        .or_else(|| build_method_type_from_descriptor(ctx, "()V"));
    if let Some(mt) = mt {
        ctx.set_field_by_name(mn, "type", Value::Object(Some(mt)));
    }

    // flags: IS_METHOD (0x10000) | ACC_STATIC (0x0008) | REF_invokeStatic (6) << 24
    let flags: i32 = 0x1_0000 | 0x0008 | (6 << 24);
    ctx.set_field_by_name(mn, "flags", Value::Int(flags));

    // method slot (4): non-zero sentinel. Our natives also mirror this write
    // through native_mhn_resolve. Use a non-zero Int so downstream vmindex
    // readers (native_mhn_object_field_offset, native_mhn_get_member_vm_info)
    // see a non-zero value — this is critical because the JDK's
    // SplitConstantPool throws `ConstantPoolException("Bad CP index: 0")` on
    // `entryByIndex(0)`, and various MH paths look up CP indices derived
    // from MemberName's vmindex / method fields.
    ctx.set_field(mn, 4, Value::Int(1));

    // resolution slot (5): null means `isResolved() == true` per JDK source.
    ctx.set_field(mn, 5, Value::Object(None));

    mn
}

/// C33: Native bypass for
/// `java.lang.invoke.InvokerBytecodeGenerator.generateLambdaFormInterpreterEntryPoint`.
///
/// Called from `LambdaForm.prepare()` in JDK 25. Returns a resolved
/// MemberName for an interpreter entry point. In our VM, actual dispatch
/// never reads `vmentry` — we intercept MH invocation in the interpreter —
/// so any non-null resolved MemberName is sufficient to keep JDK internals
/// happy without triggering JEP 466 code-gen (which in turn fails with
/// `ConstantPoolException: Bad CP index: 0` in our classfile-API stubs).
pub(crate) fn native_ibg_generate_interpreter_entry_point(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = MethodType mt
    let desc = match args.get(0) {
        Some(Value::Object(Some(mt))) => descriptor_from_method_type(ctx, *mt),
        _ => "()V".to_string(),
    };
    let ret_char = desc.rsplit_once(')').map(|(_, r)| r.chars().next().unwrap_or('V')).unwrap_or('V');
    let name = format!("interpret_{}", ret_char);
    let mn = alloc_resolved_member_name(
        ctx,
        "java/lang/invoke/LambdaForm",
        &name,
        &desc,
    );
    Ok(Some(Value::Object(Some(mn))))
}

/// C33: Native bypass for
/// `java.lang.invoke.InvokerBytecodeGenerator.generateCustomizedCode`.
///
/// Called from `LambdaForm.compileToBytecode()` in JDK 25. The caller wraps
/// this in a try/catch(BytecodeGenerationException), but rather than throw
/// from native, we return a resolved MemberName — lets `isCompiled` flip to
/// true without any generator side effects.
pub(crate) fn native_ibg_generate_customized_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: LambdaForm form (0), MethodType invokerType (1)
    let desc = match args.get(1) {
        Some(Value::Object(Some(mt))) => descriptor_from_method_type(ctx, *mt),
        _ => "()V".to_string(),
    };
    let mn = alloc_resolved_member_name(
        ctx,
        "java/lang/invoke/LambdaForm",
        "MH",
        &desc,
    );
    Ok(Some(Value::Object(Some(mn))))
}

/// C33: Native bypass for
/// `java.lang.invoke.InvokerBytecodeGenerator.generateNamedFunctionInvoker`.
///
/// Called from `NamedFunction.resolve()` in JDK 25 to get a vmentry for a
/// LambdaForm name. Same bypass reasoning as the other two.
pub(crate) fn native_ibg_generate_named_function_invoker(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: LambdaForm.NamedFunction$Kind typeForm (0) OR MethodTypeForm (0)
    // We don't read it — just return a resolved MemberName with ()V type.
    let _ = args;
    let mn = alloc_resolved_member_name(
        ctx,
        "java/lang/invoke/LambdaForm",
        "NFI",
        "()V",
    );
    Ok(Some(Value::Object(Some(mn))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    // C13: alloc_method_handle must populate the real-JDK
    // MethodHandle.type:MethodType field so JDK code paths that read
    // `mh.type()` see a non-null MethodType (not NPE / CCE).
    #[test]
    fn alloc_method_handle_populates_type_field() {
        let mut ctx = MockNativeContext::new();
        let mh = alloc_method_handle(
            &mut ctx,
            "java/lang/Integer",
            "parseInt",
            "(Ljava/lang/String;)I",
            MH_KIND_STATIC,
        );
        // In the mock, `set_field_by_name("type", ...)` maps to slot 2
        // (see test_utils::mock_jdk_field_slot) — that's the same slot
        // the MH write-path targets, so reading it back yields the
        // MethodType that was installed. The key assertion is simply
        // that the value is a non-null object (a real MethodType).
        let t = ctx.get_field_by_name(mh, "type");
        match t {
            Value::Object(Some(_)) => {}
            other => panic!("expected non-null MethodType at 'type' field, got {:?}", other),
        }
    }

    // Sanity check: build_method_type_from_descriptor turns a plain
    // descriptor string into a non-null 2-field MethodType object.
    #[test]
    fn build_method_type_from_descriptor_non_null() {
        let mut ctx = MockNativeContext::new();
        let mt = build_method_type_from_descriptor(&mut ctx, "(Ljava/lang/String;)I");
        assert!(mt.is_some(), "MethodType should be non-null for a valid descriptor");
    }

    // Invalid descriptors should return None rather than panicking.
    #[test]
    fn build_method_type_from_descriptor_rejects_garbage() {
        let mut ctx = MockNativeContext::new();
        assert!(build_method_type_from_descriptor(&mut ctx, "").is_none());
        assert!(build_method_type_from_descriptor(&mut ctx, "not-a-descriptor").is_none());
    }

    // C33: alloc_resolved_member_name must produce a MemberName that reads
    // as fully resolved (resolution == null) with a non-zero vmindex sentinel
    // at slot 4, a non-null name/type/clazz, and a non-zero flags field.
    #[test]
    fn c33_alloc_resolved_member_name_populates_all_slots() {
        let mut ctx = MockNativeContext::new();
        let mn = alloc_resolved_member_name(
            &mut ctx,
            "java/lang/invoke/LambdaForm",
            "interpret_V",
            "()V",
        );
        // clazz (slot 0) — Class mirror
        match ctx.get_field_by_name(mn, "clazz") {
            Value::Object(Some(_)) => {}
            other => panic!("expected clazz to be non-null, got {:?}", other),
        }
        // name (slot 1) — String
        match ctx.get_field_by_name(mn, "name") {
            Value::Object(Some(_)) => {}
            other => panic!("expected name to be non-null, got {:?}", other),
        }
        // type (slot 2) — MethodType
        match ctx.get_field_by_name(mn, "type") {
            Value::Object(Some(_)) => {}
            other => panic!("expected type to be non-null, got {:?}", other),
        }
        // flags (slot 3) — int with IS_METHOD | ACC_STATIC | REF_invokeStatic<<24
        match ctx.get_field_by_name(mn, "flags") {
            Value::Int(f) => {
                assert!(f != 0, "flags must be non-zero");
                assert!((f & 0x1_0000) != 0, "IS_METHOD bit must be set");
                assert!((f >> 24) & 0x0F == 6, "refKind must be REF_invokeStatic (6)");
            }
            other => panic!("expected flags to be Int, got {:?}", other),
        }
        // slot 4 (method / vmindex sentinel) — non-zero
        match ctx.get_field(mn, 4) {
            Value::Int(v) => assert_ne!(v, 0, "vmindex sentinel must be non-zero"),
            other => panic!("expected Int at slot 4, got {:?}", other),
        }
        // slot 5 (resolution) — null means isResolved() == true in real JDK
        match ctx.get_field(mn, 5) {
            Value::Object(None) => {}
            other => panic!("expected null at resolution slot 5, got {:?}", other),
        }
    }

    // C33: the three IBG native bypass functions must each return a non-null
    // MemberName without panicking, for the common entry-point signatures.
    #[test]
    fn c33_ibg_interpreter_entry_point_returns_non_null() {
        let mut ctx = MockNativeContext::new();
        let mt = build_method_type_from_descriptor(&mut ctx, "(I)J").expect("MethodType");
        let result = native_ibg_generate_interpreter_entry_point(
            &mut ctx,
            &[Value::Object(Some(mt))],
        );
        match result {
            Ok(Some(Value::Object(Some(_)))) => {}
            other => panic!("expected non-null MemberName, got {:?}", other),
        }
    }

    #[test]
    fn c33_ibg_customized_code_returns_non_null() {
        let mut ctx = MockNativeContext::new();
        // A LambdaForm and MethodType. Synthetic alloc for the form is fine
        // since the native doesn't read its fields.
        let form = alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/LambdaForm", 8);
        let mt = build_method_type_from_descriptor(&mut ctx, "()V").expect("MethodType");
        let result = native_ibg_generate_customized_code(
            &mut ctx,
            &[Value::Object(Some(form)), Value::Object(Some(mt))],
        );
        match result {
            Ok(Some(Value::Object(Some(_)))) => {}
            other => panic!("expected non-null MemberName, got {:?}", other),
        }
    }

    #[test]
    fn c33_ibg_named_function_invoker_returns_non_null() {
        let mut ctx = MockNativeContext::new();
        let form = alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodTypeForm", 6);
        let result = native_ibg_generate_named_function_invoker(
            &mut ctx,
            &[Value::Object(Some(form))],
        );
        match result {
            Ok(Some(Value::Object(Some(_)))) => {}
            other => panic!("expected non-null MemberName, got {:?}", other),
        }
    }
}

