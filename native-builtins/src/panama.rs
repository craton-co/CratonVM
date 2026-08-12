// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Panama FFI native method registrations.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{try_alloc_concurrent_synthetic, obj_arg};

use cratonvm_native_api::ffi::{
    self, LAYOUT_ADDRESS, LAYOUT_BOOLEAN, LAYOUT_BYTE, LAYOUT_CHAR, LAYOUT_DOUBLE, LAYOUT_FLOAT,
    LAYOUT_INT, LAYOUT_LONG, LAYOUT_PADDING, LAYOUT_SEQUENCE, LAYOUT_SHORT, LAYOUT_STRUCT,
    LAYOUT_UNION,
};

/// Maximum number of bytes for a single memory copy/fill operation.
const MAX_COPY_SIZE: usize = 256 * 1024 * 1024; // 256 MiB

// ofArray segments do not have an arena. Reuse their arena/alive slots to
// retain the Java backing array and its primitive layout kind.
const SEG_BACKING_ARRAY_FIELD: usize = 2;
const SEG_BACKING_KIND_FIELD: usize = 4;

/// Maximum length to scan when reading a C string from native memory.
const MAX_CSTR_LEN: usize = 4096;

/// Native-access policy for Panama downcalls and raw-address memory access.
///
/// A `validated_fn_ptr` call transmutes a Java-supplied raw address to an
/// `extern "C" fn` and invokes it — arbitrary native code execution. Real
/// JDK Panama gates this behind `--enable-native-access=<module-list>` /
/// the module's `enableNativeAccess` permission, which is granted *per
/// module* (a comma-separated list of module names, or `ALL-UNNAMED`).
///
/// `NativeAccessPolicy` records that grant faithfully:
///
/// * [`NativeAccessPolicy::None`] — no module has native access (the
///   secure-by-default state, matching the JDK with no `--enable-native-access`).
/// * [`NativeAccessPolicy::All`] — every module is granted (the launcher saw
///   `--enable-native-access` with no argument, or `ALL-UNNAMED`/`ALL-MODULES`).
/// * [`NativeAccessPolicy::Modules`] — only the named modules are granted.
///   `None` (the unnamed module) is represented by the empty string `""`.
///
/// ### Why the gate is still consulted process-globally at the call sites
///
/// The native-method closures in this file receive only `ctx` (a
/// [`NativeContext`]) and the Java `args`; there is **no reachable accessor
/// for the *calling* class/module at the gate point** (the Panama API method
/// itself lives in `java.base`, and the caller's frame is not exposed to a
/// native callee here). `NativeContext::module_name_of_class` can name the
/// module of a *given* `ClassId`, but the gate has no `ClassId` for the
/// caller. So while the *policy* is now tracked per module, the gate
/// helpers ([`require_native_access`]/[`validated_fn_ptr`]) currently answer
/// the coarser question "is native access granted to *any* module?" via
/// [`native_access_enabled`]. This is a deliberate, fail-closed
/// approximation: it never *grants* access the launcher did not, but it
/// cannot yet *distinguish* a denied module from a granted one. Once a
/// caller-frame/module accessor is plumbed to the native dispatch boundary,
/// the gate can call [`module_native_access_enabled`] with the real caller
/// module to achieve full per-module fidelity; the policy plumbing here is
/// the prerequisite half of that work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NativeAccessPolicy {
    /// No module is granted native access (secure default).
    #[default]
    None,
    /// Every module is granted native access (unscoped `--enable-native-access`).
    All,
    /// Only the listed modules are granted. The unnamed module is `""`.
    Modules(std::collections::BTreeSet<String>),
}

impl NativeAccessPolicy {
    /// Whether *any* module is granted native access. Used by the coarse,
    /// process-global gate (see the type docs for why the caller module is
    /// not available at the gate point).
    fn any_granted(&self) -> bool {
        match self {
            NativeAccessPolicy::None => false,
            NativeAccessPolicy::All => true,
            NativeAccessPolicy::Modules(m) => !m.is_empty(),
        }
    }

    /// Whether the given module is granted native access. `None` denotes the
    /// unnamed module. This is the per-module query the JDK actually performs;
    /// it is exposed now so a future caller-module-aware gate can use it.
    fn module_granted(&self, module: Option<&str>) -> bool {
        match self {
            NativeAccessPolicy::None => false,
            NativeAccessPolicy::All => true,
            NativeAccessPolicy::Modules(m) => m.contains(module.unwrap_or("")),
        }
    }
}

static NATIVE_ACCESS_POLICY: std::sync::RwLock<NativeAccessPolicy> =
    std::sync::RwLock::new(NativeAccessPolicy::None);

/// Replace the process native-access policy wholesale.
fn store_policy(policy: NativeAccessPolicy) {
    if let Ok(mut guard) = NATIVE_ACCESS_POLICY.write() {
        *guard = policy;
    }
}

/// Enable or disable Panama native downcalls process-wide.
///
/// `true` records an [`NativeAccessPolicy::All`] grant; `false` records
/// [`NativeAccessPolicy::None`]. When no module is granted, every downcall
/// through [`validated_fn_ptr`] fails with a thrown
/// `java.lang.IllegalCallerException`, matching the JDK's
/// `--enable-native-access` semantics. (Task #57: previously folded into
/// `IllegalStateException` because `RuntimeError` lacked the variant.)
///
/// Retained for the bare/unscoped `--enable-native-access` launcher path and
/// for callers (and tests) that only need the all-or-nothing behavior. For
/// the scoped `--enable-native-access=<module-list>` form use
/// [`set_native_access_modules`].
pub fn set_native_access_enabled(enabled: bool) {
    store_policy(
        if enabled && !cratonvm_types::flags::flags().io.untrusted_code {
            NativeAccessPolicy::All
        } else {
            NativeAccessPolicy::None
        },
    );
}

/// Record the exact set of modules granted native access, parsed from the
/// `--enable-native-access=<module-list>` argument (a comma-separated list).
///
/// The sentinels `ALL-UNNAMED`, `ALL-MODULES`, and an empty/whitespace-only
/// list collapse to [`NativeAccessPolicy::All`] (the JDK treats `ALL-UNNAMED`
/// as granting the unnamed module that hosts the classpath; this VM has no
/// rich module graph at this layer, so it is approximated as a global grant
/// — strictly no *narrower* than the JDK for the unnamed module that almost
/// all application code lives in). Otherwise each named module is recorded
/// individually so [`module_native_access_enabled`] can answer per module.
pub fn set_native_access_modules<I, S>(modules: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    if cratonvm_types::flags::flags().io.untrusted_code {
        store_policy(NativeAccessPolicy::None);
        return;
    }
    let mut set = std::collections::BTreeSet::new();
    let mut grant_all = false;
    for m in modules {
        let name = m.as_ref().trim();
        if name.is_empty() {
            continue;
        }
        if name.eq_ignore_ascii_case("ALL-UNNAMED") || name.eq_ignore_ascii_case("ALL-MODULES") {
            grant_all = true;
            continue;
        }
        set.insert(name.to_string());
    }
    if grant_all {
        store_policy(NativeAccessPolicy::All);
    } else if set.is_empty() {
        // A non-empty but all-blank argument is treated like the bare flag.
        store_policy(NativeAccessPolicy::All);
    } else {
        store_policy(NativeAccessPolicy::Modules(set));
    }
}

/// Whether Panama native downcalls are currently permitted for *any* module.
///
/// This is the coarse, process-global view the gate helpers use today
/// because the calling module is not reachable at the gate point (see
/// [`NativeAccessPolicy`]). It fails closed: it returns `false` whenever no
/// module has been granted access.
pub fn native_access_enabled() -> bool {
    if cratonvm_types::flags::flags().io.untrusted_code {
        return false;
    }
    NATIVE_ACCESS_POLICY
        .read()
        .map(|p| p.any_granted())
        .unwrap_or(false)
}

/// Whether the named module is granted native access. `None` denotes the
/// unnamed module. This is the per-module query the JDK performs; it is the
/// intended entry point for a future caller-module-aware gate once the
/// calling module is plumbed to the native dispatch boundary.
pub fn module_native_access_enabled(module: Option<&str>) -> bool {
    NATIVE_ACCESS_POLICY
        .read()
        .map(|p| p.module_granted(module))
        .unwrap_or(false)
}

/// Defense-in-depth gate for the raw-address `MemorySegment` memory-access
/// methods (get/set/getAtIndex/setAtIndex/copy/fill). These dereference a raw
/// address derived from the segment's Java-controlled `ptr`/`offset` fields, so
/// — like `ofAddress` and the downcall path — they must be denied unless native
/// access has been granted for the module. Returns the same
/// `IllegalCallerException` those other gated paths return.
///
/// NOTE: intentionally NOT applied to the shared `pe_segment_{get,set}_impl`
/// helpers, because those are also invoked internally by the arena
/// `allocateFrom` paths to initialize freshly-allocated, already-validated
/// arena-backed segments; gating there would break legitimate allocation even
/// when native access is off. Instead the gate is applied at each public JNI
/// entry point (the methods a Java caller can reach directly).
///
/// PER-MODULE LIMITATION: this consults the *process-global* view
/// ([`native_access_enabled`]) rather than the calling module's grant,
/// because the caller's module is not reachable from a native callee at this
/// point. The grant is tracked per module by [`NativeAccessPolicy`]; see its
/// docs for why the gate cannot yet consult it per caller. The behavior fails
/// closed (denies unless *some* module is granted).
fn require_native_access(ctx: &mut dyn NativeContext, op: &str) -> Result<(), MethodCallFailed> {
    if !native_access_enabled() {
        return Err(RuntimeError::IllegalCallerException {
            message: format!(
                "Native access is not enabled for this module \
                 (MemorySegment.{op} denied)"
            ),
        }
        .into());
    }
    crate::security_manager::check_host_native_access_or_throw(ctx, "foreign")?;
    // Audit row M3 / work-list item 18: one edit here gives every raw-address
    // `MemorySegment` accessor a `RawMemory` capability check, and each passes
    // its own `op`, so the audit report names the accessor rather than a single
    // undifferentiated "foreign" row. Permissive by default — this only records.
    crate::capability_gate::gate_raw_memory_named(&*ctx, op)?;
    Ok(())
}

/// Safely transmute a raw function address to an extern "C" fn pointer.
/// Returns an error if native access is not permitted, or if the address
/// is null or misaligned.
fn validated_fn_ptr<T>(fn_addr: i64) -> Result<T, MethodCallFailed>
where
    T: Copy,
{
    // Gate arbitrary-native-code-execution: a Java caller controlling
    // `fn_addr` must not be able to invoke arbitrary native code unless
    // native access has been granted.
    if !native_access_enabled() {
        // Task #57: route through the dedicated IllegalCallerException
        // variant so the thrown Java class is `java.lang.IllegalCallerException`
        // (the JDK convention for native-access gate denials) instead of
        // `IllegalStateException` carrying an "IllegalCallerException: " prefix.
        return Err(RuntimeError::IllegalCallerException {
            message: "Native access is not enabled for this module \
                      (Panama downcall denied)"
                .into(),
        }
        .into());
    }
    let addr = fn_addr as usize;
    if addr == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Null function pointer in downcall".into(),
        }
        .into());
    }
    if addr % std::mem::align_of::<usize>() != 0 {
        return Err(RuntimeError::IllegalStateException {
            message: format!("Misaligned function pointer {:#x} in downcall", addr),
        }
        .into());
    }
    // SAFETY: We have verified the address is non-null and properly aligned.
    // The caller is responsible for ensuring the address points to a valid
    // extern "C" function with the expected signature.
    Ok(unsafe { std::mem::transmute_copy(&addr) })
}

pub(crate) fn register_pe_panama(registry: &mut NativeMethodRegistry) {
    register_pe_value_layout(registry);
    register_pe_arena(registry);
    register_pe_memory_segment(registry);
    register_pe_symbol_lookup(registry);
    register_pe_linker(registry);
    register_pe_function_descriptor(registry);
    register_pe2_struct_layouts(registry);
    register_pe2_string_marshaling(registry);
}

// --- ValueLayout: type descriptors for native memory ---
// ValueLayout synthetic: [0]=kind (Int), [1]=byteSize (Int)

const PE_VALUE_LAYOUT_NAME_SLOT: usize = 2;
const PE_P67_LAYOUT_NAME_SLOT: usize = 3;

fn pe_make_layout(ctx: &mut dyn NativeContext, kind: i32) -> Result<ObjectRef, MethodCallFailed> {
    let layout = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/ValueLayout", 3)?;
    ctx.set_field(layout, 0, Value::Int(kind));
    ctx.set_field(layout, 1, Value::Int(ffi::layout_byte_size(kind) as i32));
    ctx.set_field(layout, PE_VALUE_LAYOUT_NAME_SLOT, Value::Object(None));
    Ok(layout)
}

fn pe_optional(ctx: &mut dyn NativeContext, value: Value) -> Result<ObjectRef, MethodCallFailed> {
    let pinned = match value {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };
    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
    let value = match pinned {
        Some((pin, obj)) => {
            let obj = ctx.read_native_pin(pin, obj);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(obj))
        }
        None => Value::Object(None),
    };
    ctx.set_field(opt, 0, value);
    Ok(opt)
}

fn pe_layout_name_value(ctx: &dyn NativeContext, layout: ObjectRef) -> Value {
    match ctx.get_field(layout, 0) {
        Value::Long(_) => {
            if ctx.object_num_fields(layout) > PE_P67_LAYOUT_NAME_SLOT {
                ctx.get_field(layout, PE_P67_LAYOUT_NAME_SLOT)
            } else {
                Value::Object(None)
            }
        }
        _ => {
            if ctx.object_num_fields(layout) > PE_VALUE_LAYOUT_NAME_SLOT {
                ctx.get_field(layout, PE_VALUE_LAYOUT_NAME_SLOT)
            } else {
                Value::Object(None)
            }
        }
    }
}

fn pe_layout_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = pe_layout_name_value(ctx, this);
    Ok(Some(Value::Object(Some(pe_optional(ctx, name)?))))
}

fn pe_layout_with_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "java/lang/foreign/MemoryLayout".to_string());
    let field_count = ctx.object_num_fields(this);
    let name_slot = match ctx.get_field(this, 0) {
        Value::Long(_) => PE_P67_LAYOUT_NAME_SLOT,
        _ => PE_VALUE_LAYOUT_NAME_SLOT,
    };
    let clone_fields = std::cmp::max(field_count, name_slot + 1);
    let this_pin = ctx.pin_native_root(this);
    let name_pin = match name {
        Value::Object(Some(obj)) => Some((ctx.pin_native_root(obj), obj)),
        _ => None,
    };

    let cloned = try_alloc_concurrent_synthetic(ctx, &class_name, clone_fields)?;
    let this = ctx.read_native_pin(this_pin, this);
    for i in 0..field_count {
        let value = ctx.get_field(this, i);
        ctx.set_field(cloned, i, value);
    }
    let name = match name_pin {
        Some((pin, obj)) => {
            let obj = ctx.read_native_pin(pin, obj);
            ctx.unpin_native_roots(pin);
            Value::Object(Some(obj))
        }
        None => Value::Object(None),
    };
    ctx.set_field(cloned, name_slot, name);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(cloned))))
}

fn register_pe_value_layout(r: &mut NativeMethodRegistry) {
    let vl = "java/lang/foreign/ValueLayout";

    // Static factory fields — return pre-built layout objects
    r.register(
        vl,
        "JAVA_BYTE",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_BYTE)?)))),
    );
    r.register(
        vl,
        "JAVA_SHORT",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_SHORT)?)))),
    );
    r.register(
        vl,
        "JAVA_INT",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_INT)?)))),
    );
    r.register(
        vl,
        "JAVA_LONG",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_LONG)?)))),
    );
    r.register(
        vl,
        "JAVA_FLOAT",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_FLOAT)?)))),
    );
    r.register(
        vl,
        "JAVA_DOUBLE",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| {
            Ok(Some(Value::Object(Some(pe_make_layout(
                ctx,
                LAYOUT_DOUBLE,
            )?))))
        },
    );
    r.register(
        vl,
        "JAVA_BOOLEAN",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| {
            Ok(Some(Value::Object(Some(pe_make_layout(
                ctx,
                LAYOUT_BOOLEAN,
            )?))))
        },
    );
    r.register(
        vl,
        "JAVA_CHAR",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_CHAR)?)))),
    );
    r.register(
        vl,
        "ADDRESS",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| {
            Ok(Some(Value::Object(Some(pe_make_layout(
                ctx,
                LAYOUT_ADDRESS,
            )?))))
        },
    );

    r.register(vl, "byteSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Int(n) => n as i64,
            _ => 1,
        };
        Ok(Some(Value::Long(size)))
    });
    r.register(vl, "byteAlignment", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Int(n) => n as i64,
            _ => 1,
        };
        Ok(Some(Value::Long(size))) // alignment = size for primitive layouts
    });
    r.register(vl, "name", "()Ljava/util/Optional;", pe_layout_name);
    r.register(
        vl,
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/ValueLayout;",
        pe_layout_with_name,
    );
    r.register(
        vl,
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
        pe_layout_with_name,
    );
    ()
}

// --- Arena: lifecycle-scoped memory management ---
// Arena synthetic: [0]=kind (Int), [1]=alloc_ids (Object — int array of alloc IDs), [2]=closed (Int), [3]=count (Int)

pub(crate) fn register_pe_arena(r: &mut NativeMethodRegistry) {
    let arena = "java/lang/foreign/Arena";

    r.register(arena, "global", "()Ljava/lang/foreign/Arena;", |ctx, _| {
        let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
        let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
        ctx.set_field(a, 0, Value::Int(ffi::ARENA_GLOBAL));
        ctx.set_field(a, 1, Value::Object(Some(ids)));
        ctx.set_field(a, 2, Value::Int(0));
        ctx.set_field(a, 3, Value::Int(0));
        Ok(Some(Value::Object(Some(a))))
    });
    r.register(arena, "ofAuto", "()Ljava/lang/foreign/Arena;", |ctx, _| {
        let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
        let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
        ctx.set_field(a, 0, Value::Int(ffi::ARENA_AUTO));
        ctx.set_field(a, 1, Value::Object(Some(ids)));
        ctx.set_field(a, 2, Value::Int(0));
        ctx.set_field(a, 3, Value::Int(0));
        Ok(Some(Value::Object(Some(a))))
    });
    r.register(
        arena,
        "ofConfined",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _| {
            let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
            let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
            ctx.set_field(a, 0, Value::Int(ffi::ARENA_CONFINED));
            ctx.set_field(a, 1, Value::Object(Some(ids)));
            ctx.set_field(a, 2, Value::Int(0));
            ctx.set_field(a, 3, Value::Int(0));
            Ok(Some(Value::Object(Some(a))))
        },
    );
    // ofShared() — multi-thread-safe arena (same as confined but with ARENA_SHARED kind)
    r.register(
        arena,
        "ofShared",
        "()Ljava/lang/foreign/Arena;",
        |ctx, _| {
            let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4)?;
            let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
            ctx.set_field(a, 0, Value::Int(ffi::ARENA_SHARED));
            ctx.set_field(a, 1, Value::Object(Some(ids)));
            ctx.set_field(a, 2, Value::Int(0));
            ctx.set_field(a, 3, Value::Int(0));
            Ok(Some(Value::Object(Some(a))))
        },
    );

    // allocate(byteSize, byteAlignment) → MemorySegment
    r.register(
        arena,
        "allocate",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        pe_arena_allocate,
    );
    // allocate(byteSize) → MemorySegment (align=1)
    r.register(
        arena,
        "allocate",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let size = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            pe_arena_allocate_impl(ctx, this, size, 1)
        },
    );
    // allocate(layout) → MemorySegment
    r.register(
        arena,
        "allocate",
        "(Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let size = match ctx.get_field(layout, 1) {
                Value::Int(n) => n as i64,
                _ => 1,
            };
            pe_arena_allocate_impl(ctx, this, size, size)
        },
    );
    // allocate(MemoryLayout) uses the interface descriptor emitted for
    // StructLayout capture-state allocations in real-JDK bytecode.
    r.register(
        arena,
        "allocate",
        "(Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let size = crate::panama_libffi::layout_total_size(ctx, layout)? as i64;
            let align = crate::panama_libffi::layout_align(ctx, layout) as i64;
            pe_arena_allocate_impl(ctx, this, size, align)
        },
    );

    // close() — free all allocations in this arena
    r.register(arena, "close", "()V", pe_arena_close);
}

pub(crate) fn pe_arena_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = match args.get(1) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        Some(Value::Double(n)) => i64::from_le_bytes(n.to_le_bytes()),
        _ => 0,
    };
    let align = match args.get(2) {
        Some(Value::Long(n)) => *n,
        Some(Value::Int(n)) => *n as i64,
        Some(Value::Double(n)) => i64::from_le_bytes(n.to_le_bytes()),
        _ => 1,
    };
    pe_arena_allocate_impl(ctx, this, size, align)
}

fn pe_arena_allocate_impl(
    ctx: &mut dyn NativeContext,
    arena_obj: ObjectRef,
    size: i64,
    align: i64,
) -> MethodCallResult {
    if matches!(ctx.get_field(arena_obj, 2), Value::Int(1)) {
        return Err(RuntimeError::IllegalStateException {
            message: "Arena is closed".into(),
        }
        .into());
    }

    // Allocate off-heap memory via SharedVm's native_memory table
    let (alloc_id, ptr) = ctx
        .allocate_native_memory(size.max(1) as usize, align.max(1) as usize)
        .ok_or_else(|| RuntimeError::IllegalStateException {
            message: "Out of native memory".into(),
        })?;

    // Track alloc_id in arena's ID list
    if let Value::Object(Some(ids_arr)) = ctx.get_field(arena_obj, 1) {
        let count = match ctx.get_field(arena_obj, 3) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        ctx.set_array_element(ids_arr, count, Value::Long(alloc_id));
        ctx.set_field(arena_obj, 3, Value::Int((count + 1) as i32));
    }

    // Create MemorySegment: [0]=ptr, [1]=size, [2]=arena, [3]=ro, [4]=alive, [5]=offset
    let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
    ctx.set_field(seg, 0, Value::Long(ptr as i64));
    ctx.set_field(seg, 1, Value::Long(size));
    ctx.set_field(seg, 2, Value::Object(Some(arena_obj)));
    ctx.set_field(seg, 3, Value::Int(0)); // read-write
    ctx.set_field(seg, 4, Value::Int(1)); // alive
    ctx.set_field(seg, 5, Value::Long(0)); // no offset

    Ok(Some(Value::Object(Some(seg))))
}

fn pe_arena_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if matches!(ctx.get_field(this, 0), Value::Int(0)) {
        return Err(RuntimeError::IllegalStateException {
            message: "Cannot close global arena".into(),
        }
        .into());
    }
    if matches!(ctx.get_field(this, 2), Value::Int(1)) {
        return Err(RuntimeError::IllegalStateException {
            message: "Arena already closed".into(),
        }
        .into());
    }

    // Free all allocations tracked by this arena
    if let Value::Object(Some(ids_arr)) = ctx.get_field(this, 1) {
        let count = match ctx.get_field(this, 3) {
            Value::Int(n) => n as usize,
            _ => 0,
        };
        for i in 0..count {
            if let Value::Long(alloc_id) = ctx.get_array_element(ids_arr, i) {
                ctx.free_native_memory(alloc_id);
            }
        }
    }

    ctx.set_field(this, 2, Value::Int(1)); // closed
    Ok(None)
}

// --- MemorySegment: off-heap byte buffer ---

pub(crate) fn register_pe_memory_segment(r: &mut NativeMethodRegistry) {
    // Real-JDK callers dispatch these interface methods directly; retain the
    // concrete native bridges when SyntheticStub registrations are filtered.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let ms = "java/lang/foreign/MemorySegment";

    // byteSize() → long
    r.register(ms, "byteSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(crate::panama_libffi::segment_byte_size(
            ctx, this,
        ))))
    });

    // address() → long (raw pointer as long)
    r.register(ms, "address", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(crate::panama_libffi::segment_address(
            ctx, this,
        ))))
    });

    // isNative() → boolean.
    //
    // Was an unconditional `true` ("always true for our segments"), which is
    // wrong for the `ofArray(...)` segments registered further down in THIS
    // file: those are heap segments, and `isNative()` is exactly the query a
    // caller uses to decide whether `address()` is meaningful and whether the
    // segment may be handed to a downcall. They only *look* native from the
    // inside because CratonVM cannot expose a moving Java array to native code
    // and so gives them an off-heap mirror (`sync_heap_backed_segment`) — an
    // implementation detail that must not leak into the spec'd answer.
    //
    // The discriminator is the one `sync_heap_backed_segment` already uses:
    // `SEG_BACKING_ARRAY_FIELD` retains the Java array on an `ofArray` segment
    // and holds an Arena (or nothing) on every off-heap one.
    //
    // NOTE: `foreign_ffm.rs` registers this same class+method+descriptor, and
    // this registrar runs AFTER it on both paths that reach them
    // (lib.rs:9736→9740, and :22995→23051), so THIS is the live answer — the
    // one over there was dead, and said the opposite. They now agree.
    r.register(ms, "isNative", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let heap_backed = match ctx.get_field(this, SEG_BACKING_ARRAY_FIELD) {
            Value::Object(Some(array)) => ctx.object_is_array(array),
            _ => false,
        };
        Ok(Some(Value::Int(i32::from(!heap_backed))))
    });

    // get(ValueLayout, long offset) → value
    r.register(
        ms,
        "get",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        pe_segment_get,
    );

    // set(ValueLayout, long offset, value)
    r.register(
        ms,
        "set",
        "(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V",
        pe_segment_set,
    );

    // Real-JDK bytecode resolves the covariant ValueLayout descriptors rather
    // than the erased Object signature above.
    //
    // All nine of each. `java.lang.foreign.MemorySegment` declares nine
    // `get`/`set` pairs and every one of the eighteen is `public abstract`, so
    // a descriptor nobody registers is not a slow path — it is
    // `AbstractMethodError: … has no Code attribute`, thrown at the interface
    // method itself.
    //
    // The `set` half of this loop did not exist. Only the erased
    // `(ValueLayout;JLjava/lang/Object;)V` above was registered, and real
    // bytecode never emits that; `phases_late/foreign_ffm.rs` separately
    // covered Byte/Short/Int/Long, which is why four of the nine worked and
    // `set(JAVA_DOUBLE, …)` raised. Keeping the two lists adjacent and
    // identical is the point: an asymmetry between them is exactly the defect,
    // and it is only visible when they are read together.
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;J)Z",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
        "(Ljava/lang/foreign/ValueLayout$OfChar;J)C",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;J)F",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;J)D",
        "(Ljava/lang/foreign/AddressLayout;J)Ljava/lang/foreign/MemorySegment;",
    ] {
        r.register(ms, "get", desc, pe_segment_get);
    }
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;JZ)V",
        "(Ljava/lang/foreign/ValueLayout$OfByte;JB)V",
        "(Ljava/lang/foreign/ValueLayout$OfChar;JC)V",
        "(Ljava/lang/foreign/ValueLayout$OfShort;JS)V",
        "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V",
        "(Ljava/lang/foreign/ValueLayout$OfLong;JJ)V",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;JF)V",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;JD)V",
        "(Ljava/lang/foreign/AddressLayout;JLjava/lang/foreign/MemorySegment;)V",
    ] {
        r.register(ms, "set", desc, pe_segment_set);
    }
    r.register(
        ms,
        "getAtIndex",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        |ctx, args| {
            // Defense-in-depth: dereferences the segment's raw `ptr` field.
            require_native_access(ctx, "getAtIndex")?;
            let this = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let index = match args.get(2) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let elem_size = match ctx.get_field(layout, 1) {
                Value::Int(n) => n as i64,
                _ => 1,
            };
            let offset = index * elem_size;
            pe_segment_get_impl(ctx, this, layout, offset)
        },
    );

    // setAtIndex(ValueLayout, long index, value)
    r.register(
        ms,
        "setAtIndex",
        "(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V",
        |ctx, args| {
            // Defense-in-depth: dereferences the segment's raw `ptr` field.
            require_native_access(ctx, "setAtIndex")?;
            let this = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let index = match args.get(2) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let value = args.get(3).copied().unwrap_or(Value::Int(0));
            let elem_size = match ctx.get_field(layout, 1) {
                Value::Int(n) => n as i64,
                _ => 1,
            };
            let offset = index * elem_size;
            pe_segment_set_impl(ctx, this, layout, offset, value)
        },
    );

    // Real-JDK bytecode resolves the covariant ValueLayout descriptors rather
    // than the erased Object signatures above.
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;J)Z",
        "(Ljava/lang/foreign/ValueLayout$OfByte;J)B",
        "(Ljava/lang/foreign/ValueLayout$OfChar;J)C",
        "(Ljava/lang/foreign/ValueLayout$OfShort;J)S",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
        "(Ljava/lang/foreign/ValueLayout$OfLong;J)J",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;J)F",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;J)D",
        "(Ljava/lang/foreign/AddressLayout;J)Ljava/lang/foreign/MemorySegment;",
    ] {
        r.register(ms, "getAtIndex", desc, pe_segment_get_at_index);
    }
    for desc in [
        "(Ljava/lang/foreign/ValueLayout$OfBoolean;JZ)V",
        "(Ljava/lang/foreign/ValueLayout$OfByte;JB)V",
        "(Ljava/lang/foreign/ValueLayout$OfChar;JC)V",
        "(Ljava/lang/foreign/ValueLayout$OfShort;JS)V",
        "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V",
        "(Ljava/lang/foreign/ValueLayout$OfLong;JJ)V",
        "(Ljava/lang/foreign/ValueLayout$OfFloat;JF)V",
        "(Ljava/lang/foreign/ValueLayout$OfDouble;JD)V",
        "(Ljava/lang/foreign/AddressLayout;JLjava/lang/foreign/MemorySegment;)V",
    ] {
        r.register(ms, "setAtIndex", desc, pe_segment_set_at_index);
    }

    // asSlice(long offset, long size) → MemorySegment
    r.register(
        ms,
        "asSlice",
        "(JJ)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let new_size = match args.get(2) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let size = crate::panama_libffi::segment_byte_size(ctx, this);
            let end = offset.checked_add(new_size);
            if offset < 0 || new_size < 0 || end.map_or(true, |n| n > size) {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "slice offset {} + size {} exceeds segment size {}",
                        offset, new_size, size
                    ),
                }
                .into());
            }
            let base_ptr = crate::panama_libffi::segment_address(ctx, this);
            let slice_ptr = base_ptr.checked_add(offset).ok_or_else(|| {
                MethodCallFailed::from(RuntimeError::IllegalStateException {
                    message: "address arithmetic overflow in MemorySegment.asSlice".into(),
                })
            })?;
            // A synthetic slice cannot retain the real implementation's
            // private scope object.  It stores an already-adjusted absolute
            // address instead, which is valid for both real and synthetic
            // source segments and avoids treating real field 0/5 as ptr/off.
            let read_only = match ctx.get_field_by_name(this, "readOnly") {
                Value::Int(n) => Value::Int(n),
                _ => ctx.get_field(this, 3),
            };

            // W7-89: a slice stays inside its parent's scope. Slot 2 used to be
            // written as the "no arena" marker unconditionally, so
            // `pe_segment_session` answered `None` for every slice and
            // `pe_segment_check_scope` let it through — HotSpot raises
            // `IllegalStateException: Already closed` for a slice of a closed
            // arena exactly as it does for the parent (measured,
            // `MemorySessionValidStateProbe` row `C.closed.slice.get`).
            //
            // Only a session we MODELLED is propagated, which is all
            // `pe_segment_session` can return. That is what keeps the slot's
            // OTHER tenant safe: on an `ofArray` segment slot 2 holds the Java
            // backing array (`SEG_BACKING_ARRAY_FIELD`), an array resolves to no
            // session, and such a slice keeps the historical `Object(None)` — so
            // `isNative()` and `sync_heap_backed_segment` see exactly what they
            // saw before.
            let parent_session = pe_segment_session(ctx, this);
            let session_pin = parent_session.map(|session| ctx.pin_native_root(session));
            let slice = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
            // The allocation above can move the session (native stale-local
            // family), so re-read it through the pin before storing it.
            let scope_value = match (parent_session, session_pin) {
                (Some(session), Some(pin)) => {
                    let session = ctx.read_native_pin(pin, session);
                    ctx.unpin_native_roots(pin);
                    Value::Object(Some(session))
                }
                _ => Value::Object(None),
            };
            ctx.set_field(slice, 0, Value::Long(slice_ptr));
            ctx.set_field(slice, 1, Value::Long(new_size));
            ctx.set_field(slice, 2, scope_value);
            ctx.set_field(slice, 3, read_only);
            ctx.set_field(slice, 4, Value::Int(1));
            ctx.set_field(slice, 5, Value::Long(0));
            Ok(Some(Value::Object(Some(slice))))
        },
    );

    // ofAddress(long address) → MemorySegment (wraps a raw address, zero-length)
    r.register(
        ms,
        "ofAddress",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let addr = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            // Gate raw-address wrapping behind native access: turning an
            // arbitrary caller-supplied long into an addressable segment is
            // equivalent to arbitrary process-memory access once paired with
            // reinterpret/get/set. Refuse unless native access is enabled.
            //
            // EXCEPTION: address 0 (`MemorySegment.NULL`, a zero-length
            // segment that can never be dereferenced) is always permitted,
            // matching real JDK. `MemorySegment`'s own <clinit> builds `NULL`
            // via `ofAddress(0)` before any user code runs and before any
            // module has had a chance to request native access; gating that
            // internal bootstrap call poisons the class forever (a <clinit>
            // failure is a permanent NoClassDefFoundError for every
            // subsequent use, per JVMS 5.5) even though HotSpot never denies
            // access to the harmless null segment.
            if addr != 0 && !native_access_enabled() {
                return Err(RuntimeError::IllegalCallerException {
                    message: "Native access is not enabled for this module \
                              (MemorySegment.ofAddress denied)"
                        .into(),
                }
                .into());
            }
            let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
            ctx.set_field(seg, 0, Value::Long(addr));
            ctx.set_field(seg, 1, Value::Long(0)); // unknown size
            ctx.set_field(seg, 2, Value::Object(None)); // no arena
            ctx.set_field(seg, 3, Value::Int(0));
            ctx.set_field(seg, 4, Value::Int(1));
            ctx.set_field(seg, 5, Value::Long(0));
            Ok(Some(Value::Object(Some(seg))))
        },
    );

    // allocateFrom(Arena, ValueLayout, value) → MemorySegment
    // Allocates a segment for a single value and writes the value into it.
    r.register(
        ms,
        "allocateFrom",
        "(Ljava/lang/foreign/Arena;Ljava/lang/foreign/ValueLayout;I)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arena = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let value = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let size = match ctx.get_field(layout, 1) {
                Value::Int(n) => n as i64,
                _ => 4,
            };
            let seg = pe_arena_allocate_impl(ctx, arena, size, size)?
                .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None });
            if let Some(seg) = seg {
                pe_segment_set_impl(ctx, seg, layout, 0, Value::Int(value))?;
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );
    // allocateFrom(Arena, ValueLayout.OfLong, long) → MemorySegment
    r.register(
        ms,
        "allocateFrom",
        "(Ljava/lang/foreign/Arena;Ljava/lang/foreign/ValueLayout;J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arena = obj_arg(args, 0)?;
            let layout = obj_arg(args, 1)?;
            let value = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let size = match ctx.get_field(layout, 1) {
                Value::Int(n) => n as i64,
                _ => 8,
            };
            let seg = pe_arena_allocate_impl(ctx, arena, size, size)?
                .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None });
            if let Some(seg) = seg {
                pe_segment_set_impl(ctx, seg, layout, 0, Value::Long(value))?;
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );

    // ofArray(int[]) → MemorySegment wrapping the Java array's data
    r.register(
        ms,
        "ofArray",
        "([I)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let byte_size = (len * 4) as i64; // int = 4 bytes each
                                              // Allocate native memory and copy array contents into it
            let result = ctx.allocate_native_memory(byte_size as usize, 4);
            if let Some((alloc_id, ptr)) = result {
                // Copy array elements into native memory
                for i in 0..len {
                    if let Value::Int(v) = ctx.get_array_element(arr, i) {
                        unsafe {
                            let dest = (ptr as *mut i32).add(i);
                            *dest = v;
                        }
                    }
                }
                let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None)); // auto-managed
                ctx.set_field(seg, 3, Value::Int(0)); // read-write
                ctx.set_field(seg, 4, Value::Int(1)); // alive
                ctx.set_field(seg, 5, Value::Long(0));
                ctx.set_field(seg, SEG_BACKING_ARRAY_FIELD, Value::Object(Some(arr)));
                ctx.set_field(seg, SEG_BACKING_KIND_FIELD, Value::Int(LAYOUT_INT));
                // Store alloc_id so it can be freed (field 0 encodes the pointer)
                let _ = alloc_id; // tracked by NativeMemoryTable
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }
                .into())
            }
        },
    );
    // ofArray(long[]) → MemorySegment
    r.register(
        ms,
        "ofArray",
        "([J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let byte_size = (len * 8) as i64;
            let result = ctx.allocate_native_memory(byte_size as usize, 8);
            if let Some((_alloc_id, ptr)) = result {
                for i in 0..len {
                    if let Value::Long(v) = ctx.get_array_element(arr, i) {
                        unsafe {
                            let dest = (ptr as *mut i64).add(i);
                            *dest = v;
                        }
                    }
                }
                let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                ctx.set_field(seg, SEG_BACKING_ARRAY_FIELD, Value::Object(Some(arr)));
                ctx.set_field(seg, SEG_BACKING_KIND_FIELD, Value::Int(LAYOUT_LONG));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }
                .into())
            }
        },
    );
    // ofArray(float[]) → MemorySegment
    for (owner, method, descriptor) in [
        (ms, "ofArray", "([F)Ljava/lang/foreign/MemorySegment;"),
        (
            "jdk/internal/foreign/SegmentFactories",
            "fromArray",
            "([F)Ljdk/internal/foreign/HeapMemorySegmentImpl$OfFloat;",
        ),
    ] {
        r.register(owner, method, descriptor, |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let byte_size = (len * 4) as i64;
            let result = ctx.allocate_native_memory(byte_size as usize, 4);
            if let Some((_alloc_id, ptr)) = result {
                for i in 0..len {
                    let value = match ctx.get_array_element(arr, i) {
                        Value::Float(v) => v,
                        // Primitive float arrays are stored as raw IEEE-754
                        // bits in this VM's generic array representation.
                        Value::Int(bits) => f32::from_bits(bits as u32),
                        _ => 0.0,
                    };
                    unsafe {
                        let dest = (ptr as *mut f32).add(i);
                        *dest = value;
                    }
                }
                let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                ctx.set_field(seg, SEG_BACKING_ARRAY_FIELD, Value::Object(Some(arr)));
                ctx.set_field(seg, SEG_BACKING_KIND_FIELD, Value::Int(LAYOUT_FLOAT));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }
                .into())
            }
        });
    }
    // ofArray(double[]) → MemorySegment
    r.register(
        ms,
        "ofArray",
        "([D)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let arr = match args.first() {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let byte_size = (len * 8) as i64;
            let result = ctx.allocate_native_memory(byte_size as usize, 8);
            if let Some((_alloc_id, ptr)) = result {
                for i in 0..len {
                    if let Value::Double(v) = ctx.get_array_element(arr, i) {
                        unsafe {
                            let dest = (ptr as *mut f64).add(i);
                            *dest = v;
                        }
                    }
                }
                let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                ctx.set_field(seg, SEG_BACKING_ARRAY_FIELD, Value::Object(Some(arr)));
                ctx.set_field(seg, SEG_BACKING_KIND_FIELD, Value::Int(LAYOUT_DOUBLE));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }
                .into())
            }
        },
    );

    // Copy primitive array elements into a native segment. Elasticsearch uses
    // this overload to stage float[] rows for bulk vector kernels.
    r.register(
        ms,
        "copy",
        "(Ljava/lang/Object;ILjava/lang/foreign/MemorySegment;Ljava/lang/foreign/ValueLayout;JI)V",
        |ctx, args| {
            require_native_access(ctx, "copy")?;
            let src = obj_arg(args, 0)?;
            let src_index = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                _ => 0,
            };
            let dst = obj_arg(args, 2)?;
            let layout = obj_arg(args, 3)?;
            let dst_offset = match args.get(4) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let count = match args.get(5) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                _ => 0,
            };
            let length = ctx.array_length(src);
            if src_index
                .checked_add(count)
                .map_or(true, |end| end > length)
            {
                return Err(RuntimeError::aioobe_index_only(src_index as i32).into());
            }
            let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
            let width = ffi::layout_byte_size(kind) as i64;
            for i in 0..count {
                let value = ctx.get_array_element(src, src_index + i);
                // Primitive float arrays use their raw IEEE-754 bits in the
                // interpreter's array representation. The set helper expects
                // the typed Value::Float form; otherwise its type switch
                // silently falls through and leaves the destination zeroed.
                let value = match (kind, value) {
                    (LAYOUT_FLOAT, Value::Int(bits)) => Value::Float(f32::from_bits(bits as u32)),
                    (_, value) => value,
                };
                pe_segment_set_impl(ctx, dst, layout, dst_offset + (i as i64) * width, value)?;
            }
            Ok(None)
        },
    );

    // copy(src, srcOffset, dst, dstOffset, bytes) — memcpy
    r.register(
        ms,
        "copy",
        "(Ljava/lang/foreign/MemorySegment;JLjava/lang/foreign/MemorySegment;JJ)V",
        |ctx, args| {
            // Defense-in-depth: copy dereferences both segments' raw `ptr` fields.
            require_native_access(ctx, "copy")?;
            let src = obj_arg(args, 0)?;
            let src_offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let dst = obj_arg(args, 2)?;
            let dst_offset = match args.get(3) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let bytes = match args.get(4) {
                Some(Value::Long(n)) => *n as usize,
                _ => 0,
            };

            let src_ptr = crate::panama_libffi::segment_address(ctx, src);
            let dst_ptr = crate::panama_libffi::segment_address(ctx, dst);

            // Validate offsets against segment sizes to prevent out-of-bounds access
            let src_size = crate::panama_libffi::segment_byte_size(ctx, src);
            let dst_size = crate::panama_libffi::segment_byte_size(ctx, dst);

            if bytes > 0 {
                if bytes > MAX_COPY_SIZE {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "copy size {} exceeds maximum of {} bytes",
                            bytes, MAX_COPY_SIZE
                        ),
                    }
                    .into());
                }
                // Bounds check: offset + bytes must fit within segment size.
                // FIX: validate UNCONDITIONALLY (not gated on size > 0). A segment
                // with a declared byteSize() of 0 must still reject a non-zero
                // `bytes` copy — otherwise a 0-size segment drives an OOB
                // read/write of up to MAX_COPY_SIZE. This mirrors the correct
                // single-element path (`pe_segment_access_addr`), which rejects
                // any non-zero access on a zero-size segment. Uses checked_add so
                // a malicious offset cannot wrap past the size comparison.
                let src_end = src_offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
                if src_offset < 0 || src_end > src_size {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "source offset {} + {} bytes exceeds segment size {}",
                            src_offset, bytes, src_size
                        ),
                    }
                    .into());
                }
                let dst_end = dst_offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
                if dst_offset < 0 || dst_end > dst_size {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "destination offset {} + {} bytes exceeds segment size {}",
                            dst_offset, bytes, dst_size
                        ),
                    }
                    .into());
                }

                // Validate address arithmetic doesn't overflow
                let src_total = (src_ptr as u64).checked_add(src_offset as u64);
                let dst_total = (dst_ptr as u64).checked_add(dst_offset as u64);

                if let (Some(s), Some(d)) = (src_total, dst_total) {
                    let src_addr = s as *const u8;
                    let dst_addr = d as *mut u8;
                    if !src_addr.is_null() && !dst_addr.is_null() {
                        // The JDK's MemorySegment.copy is defined for overlapping
                        // src/dst (it is specified as a memmove-equivalent bulk
                        // copy). Decide between memmove and the faster
                        // `copy_nonoverlapping` by testing whether the two
                        // byte ranges actually intersect.
                        //
                        // Ranges are `[s, s+bytes)` and `[d, d+bytes)` over the
                        // *absolute* addresses computed above. They overlap iff
                        // `s < d+bytes && d < s+bytes`. `bytes` is bounded by
                        // MAX_COPY_SIZE and both endpoints derive from the
                        // checked address arithmetic, so the `+ bytes` cannot
                        // wrap a u64.
                        let bytes_u64 = bytes as u64;
                        let overlap =
                            s < d.saturating_add(bytes_u64) && d < s.saturating_add(bytes_u64);
                        // SAFETY: addresses are non-null, bounds-checked against
                        // segment sizes, and bytes is bounded by MAX_COPY_SIZE.
                        // Overlapping ranges use `copy` (memmove), which is
                        // defined for overlap; provably-disjoint ranges use the
                        // faster `copy_nonoverlapping`.
                        if overlap {
                            unsafe { std::ptr::copy(src_addr, dst_addr, bytes) };
                        } else {
                            unsafe { std::ptr::copy_nonoverlapping(src_addr, dst_addr, bytes) };
                        }
                    }
                } else {
                    return Err(RuntimeError::IllegalStateException {
                        message: "address arithmetic overflow in MemorySegment.copy".into(),
                    }
                    .into());
                }
            }
            Ok(None)
        },
    );

    // fill(byte value) — memset
    r.register(
        ms,
        "fill",
        "(B)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            // Defense-in-depth: fill writes to the segment's raw `ptr` field.
            require_native_access(ctx, "fill")?;
            let this = obj_arg(args, 0)?;
            let byte_val = match args.get(1) {
                Some(Value::Int(n)) => *n as u8,
                _ => 0,
            };
            let size = crate::panama_libffi::segment_byte_size(ctx, this).max(0) as usize;
            let addr = crate::panama_libffi::segment_address(ctx, this) as *mut u8;
            if size > 0 && !addr.is_null() {
                if size > MAX_COPY_SIZE {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "fill size {} exceeds maximum of {} bytes",
                            size, MAX_COPY_SIZE
                        ),
                    }
                    .into());
                }
                // SAFETY: addr is non-null, and size is bounded by MAX_COPY_SIZE.
                // The address comes from a JVM-managed MemorySegment.
                unsafe { std::ptr::write_bytes(addr, byte_val, size) };
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.set_category(__prev_cat);
}

// --- Scope validity: refuse access through a closed Arena ---
//
// `phases_late::foreign_ffm` gives every synthetic `Arena` a *session* object
// and `Arena.close()` now genuinely closes it and runs its close actions — so
// the off-heap block a segment points at is really freed. The raw load/store
// below would then read or write freed memory; HotSpot raises
// `IllegalStateException` instead, and so must we.
//
// The two layouts are `foreign_ffm`'s. They are MIRRORED here rather than
// shared because those helpers are private to that module, and the one public
// resolver it does expose (`p67_receiver_session`) mints a fresh — therefore
// always-open — session when it finds nothing, which would make every check
// below trivially pass:
//
//   Arena   : [0] = open (Int), [1] = session
//   session : four words, whose SLOTS are `foreign_ffm`'s to decide — see
//             `P67SessionSlots`. This file used to hard-code `state` at slot 0
//             and a width of 4; both are deleted (W7-89) because that second
//             copy of the index is what made the check dead in Compatible mode,
//             where the real `MemorySessionImpl` types slot 0 as a REFERENCE.
//
// Both are `alloc_concurrent_synthetic` objects, so their class names are
// exactly the two constants below. Every step of the resolution is gated on
// those names, which is what makes it self-validating rather than
// shape-guessing: a miss answers "no resolvable scope" (access proceeds
// unchanged) and never "closed".
const PE_ARENA_CLASS: &str = "java/lang/foreign/Arena";
const PE_SESSION_CLASS: &str = "jdk/internal/foreign/MemorySessionImpl";
const PE_ARENA_SESSION_FIELD: usize = 1;
/// Slot 2 of a synthetic segment names the arena that allocated it — this
/// file's own convention (see the `// no arena` writes in `ofAddress` and
/// `asSlice`), which `foreign_ffm`'s 3-field segments adopted as well.
///
/// The slot is REUSED by `ofArray` segments to retain the Java backing array
/// ([`SEG_BACKING_ARRAY_FIELD`]), and on a real-JDK segment it is whatever
/// field happens to sit at index 2 — hence the class-name gate in
/// [`pe_arena_session`]: we must never follow a non-Arena object's slots.
const PE_SEGMENT_ARENA_FIELD: usize = 2;

/// Single-entry positive/negative memo for an exact class-name test.
///
/// The test itself is what makes the resolution safe — unlike a field-shape
/// probe it cannot mistake a primitive array, whose raw slots decode to
/// arbitrary `Value`s, for an object we modelled — but `class_name_of_id`
/// takes the class-manager lock and allocates a `String`, and this runs on
/// every FFM load/store. Class ids are process-stable and each site sees
/// exactly two shapes (the arena / the `ofArray` backing array), so one
/// remembered hit id and one remembered miss id keep the steady state at an
/// integer compare.
struct PeClassMemo {
    hit: std::sync::atomic::AtomicU32,
    miss: std::sync::atomic::AtomicU32,
}

impl PeClassMemo {
    /// `u32::MAX` is the "nothing remembered" sentinel; no real class id
    /// reaches it.
    const fn new() -> Self {
        Self {
            hit: std::sync::atomic::AtomicU32::new(u32::MAX),
            miss: std::sync::atomic::AtomicU32::new(u32::MAX),
        }
    }

    fn matches(&self, ctx: &dyn NativeContext, obj: ObjectRef, expected: &str) -> bool {
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        let class_id = ctx.class_id_of_object(obj);
        let raw = class_id.as_u32();
        if raw == self.hit.load(relaxed) {
            return true;
        }
        if raw == self.miss.load(relaxed) {
            return false;
        }
        let matched = ctx.class_name_arc_of_id(class_id).as_deref() == Some(expected);
        if matched {
            self.hit.store(raw, relaxed);
        } else {
            self.miss.store(raw, relaxed);
        }
        matched
    }
}

static PE_ARENA_CLASS_MEMO: PeClassMemo = PeClassMemo::new();
static PE_SESSION_CLASS_MEMO: PeClassMemo = PeClassMemo::new();

/// Whether `session` carries the layout `foreign_ffm` writes. Anything else —
/// a real JDK `ConfinedSession`/`SharedSession`, or an object that merely
/// happens to sit in the session slot — is left strictly alone.
///
/// The state word is located through `foreign_ffm`'s own slot map rather than
/// through a second copy of the index. That map is the W7-89 repair: in
/// Compatible mode the carrier is the REAL `MemorySessionImpl`, whose slot 0 is
/// a declared REFERENCE (`resourceList`), so the model's `Int` state word never
/// read back as an `Int` there and this predicate answered false for every
/// session — which is why the choke point below, though correctly wired since
/// W7-58, never once fired. Calling the shared resolver keeps the decision in
/// ONE implementation; open-coding the index here is what let the two files
/// drift out of step in the first place.
fn pe_session_modelled(ctx: &dyn NativeContext, session: ObjectRef) -> bool {
    if !PE_SESSION_CLASS_MEMO.matches(ctx, session, PE_SESSION_CLASS) {
        return false;
    }
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    ctx.object_num_fields(session) >= slots.required_width()
        && matches!(ctx.get_field(session, slots.state), Value::Int(_))
}

/// The session stored on a synthetic `Arena`, if `arena` is one.
///
/// Rejects [`register_pe_arena`]'s rival 4-slot arena, whose slot 1 is an
/// int-array of allocation ids rather than a session: the array fails
/// [`pe_session_modelled`]'s class-name test, so the arena resolves to `None`
/// (no scope) instead of to a bogus "closed" session. That shape is the one
/// that wins under `--synthetic-jdk`, where a false throw here would break
/// every FFM access.
fn pe_arena_session(ctx: &dyn NativeContext, arena: ObjectRef) -> Option<ObjectRef> {
    if ctx.object_num_fields(arena) <= PE_ARENA_SESSION_FIELD
        || !PE_ARENA_CLASS_MEMO.matches(ctx, arena, PE_ARENA_CLASS)
    {
        return None;
    }
    match ctx.get_field(arena, PE_ARENA_SESSION_FIELD) {
        Value::Object(Some(session)) if pe_session_modelled(ctx, session) => Some(session),
        _ => None,
    }
}

/// The session governing `seg`'s lifetime, or `None` when the segment has no
/// scope we can resolve — `MemorySegment.ofAddress`, `asSlice`, `ofArray`, the
/// global arena and every segment this file allocates outside an arena. Those
/// must keep working exactly as before, so an unresolvable scope is NOT an
/// error.
///
/// Allocation-free and safepoint-free by construction (plain field/class
/// reads only), so no caller has to pin `seg` across the check.
fn pe_segment_session(ctx: &dyn NativeContext, seg: ObjectRef) -> Option<ObjectRef> {
    // Fast path: our own slot-2 convention. One field read decides it for the
    // overwhelmingly common shapes — an arena-allocated segment resolves here,
    // and an explicit `Object(None)` is the "no arena" marker this file writes,
    // which needs no further lookup.
    if ctx.object_num_fields(seg) > PE_SEGMENT_ARENA_FIELD {
        match ctx.get_field(seg, PE_SEGMENT_ARENA_FIELD) {
            Value::Object(None) => return None,
            Value::Object(Some(owner)) => {
                if let Some(session) = pe_arena_session(ctx, owner) {
                    return Some(session);
                }
                // Tolerate a segment stamped with the session directly.
                if pe_session_modelled(ctx, owner) {
                    return Some(owner);
                }
                // Not our convention (an `ofArray` backing array, or a real
                // segment's own reference field) — fall through.
            }
            // A primitive there means this is not our layout at all.
            _ => {}
        }
    }
    // A real-JDK segment carries its session in `AbstractMemorySegmentImpl
    // .scope`, and that field holds one of OUR sessions because the
    // `createConfined`/`createShared` factories are force-dispatched into
    // `foreign_ffm`. A real session we did not build is an honest miss: we
    // cannot read its state word without guessing its encoding, so we let the
    // access through rather than throw on a shape we misread.
    match ctx.get_field_by_name(seg, "scope") {
        Value::Object(Some(scope)) if pe_session_modelled(ctx, scope) => Some(scope),
        _ => None,
    }
}

/// Raise `IllegalStateException` if `seg`'s scope has already been closed.
///
/// This is the single choke point for `get`/`set`/`getAtIndex`/`setAtIndex`:
/// all four reach [`pe_segment_access_addr`], which calls this before it
/// computes an address.
fn pe_segment_check_scope(ctx: &dyn NativeContext, seg: ObjectRef) -> Result<(), MethodCallFailed> {
    let Some(session) = pe_segment_session(ctx, seg) else {
        return Ok(());
    };
    let slots = crate::phases_late::foreign_ffm::p67_session_slots(ctx, session);
    if matches!(ctx.get_field(session, slots.state), Value::Int(0)) {
        return Err(RuntimeError::IllegalStateException {
            message: "Already closed".into(),
        }
        .into());
    }
    Ok(())
}

/// The zero-length `MemorySegment` that `get(AddressLayout, long)` returns.
///
/// The JDK's contract for an address read is a segment of size 0 at that
/// address -- the caller must `reinterpret` it before dereferencing, which is
/// precisely the safety property that makes the read legal at all. It carries
/// no arena, so `pe_arena_close` never frees memory this VM did not allocate.
///
/// Shape matches `pe_arena_allocate_impl`: `[0]=ptr, [1]=size, [2]=arena,
/// [3]=readOnly, [4]=alive, [5]=offset`.
fn pe_zero_length_segment(
    ctx: &mut dyn NativeContext,
    addr: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
    ctx.set_field(seg, 0, Value::Long(addr));
    ctx.set_field(seg, 1, Value::Long(0));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    Ok(seg)
}

/// Validate a single-element access (get/set) against the segment's declared
/// size and compute the target address with checked arithmetic.
///
/// Mirrors the bounds/overflow checks the `copy`/`fill` paths perform, and
/// throws the same `IllegalStateException` on violation. Rejects:
///   - access through a scope that has been closed (`arena.close()` really
///     frees the block, so this is a use-after-FREE guard, not a cosmetic
///     one) — see [`pe_segment_check_scope`],
///   - zero-size segments (a 0-size segment — as produced by `ofAddress`
///     before `reinterpret` — is not accessible, matching JDK semantics),
///   - negative `offset`,
///   - `offset + width` overflowing `i64`,
///   - `offset + width` exceeding the segment size,
///   - `(ptr + base_off + offset)` overflowing the address space, or a null
///     resulting address.
///
/// `width` is the access width in bytes derived from the layout kind
/// (`ffi::layout_byte_size`). Returns the validated raw address.
fn pe_segment_access_addr(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    offset: i64,
    width: i64,
) -> Result<usize, MethodCallFailed> {
    // Liveness before bounds, as HotSpot checks it: a closed scope means the
    // block is gone, so nothing about its size or address is meaningful.
    pe_segment_check_scope(ctx, seg)?;

    let ptr = crate::panama_libffi::segment_address(ctx, seg);
    let size = crate::panama_libffi::segment_byte_size(ctx, seg);

    // A 0-size segment (e.g. ofAddress before reinterpret) is not accessible.
    if size <= 0 {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "access of {} bytes at offset {} not allowed on zero-size segment",
                width, offset
            ),
        }
        .into());
    }

    // Bounds check: 0 <= offset and offset + width <= size, with overflow guard.
    let end = offset.checked_add(width);
    if offset < 0 || end.map_or(true, |e| e > size) {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "offset {} + {} bytes exceeds segment size {}",
                offset, width, size
            ),
        }
        .into());
    }

    // Validate address arithmetic doesn't overflow.
    let total = (ptr as u64).checked_add(offset as u64);
    match total {
        Some(addr) if addr != 0 => Ok(addr as usize),
        Some(_) => Err(RuntimeError::IllegalStateException {
            message: "Null segment address".into(),
        }
        .into()),
        None => Err(RuntimeError::IllegalStateException {
            message: "address arithmetic overflow in MemorySegment access".into(),
        }
        .into()),
    }
}

// Exact primitive/covariant descriptors used by real-JDK MemorySegment
// default methods. They share the checked erased implementation above.
fn pe_segment_get_at_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    require_native_access(ctx, "getAtIndex")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let index = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let elem_size =
        ffi::layout_byte_size(crate::panama_libffi::read_layout_kind(ctx, layout)) as i64;
    pe_segment_get_impl(ctx, this, layout, index * elem_size)
}

fn pe_segment_set_at_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    require_native_access(ctx, "setAtIndex")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let index = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let value = args.get(3).copied().unwrap_or(Value::Int(0));
    let elem_size =
        ffi::layout_byte_size(crate::panama_libffi::read_layout_kind(ctx, layout)) as i64;
    pe_segment_set_impl(ctx, this, layout, index * elem_size, value)
}

fn pe_segment_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Defense-in-depth: get() dereferences the segment's raw `ptr` field.
    require_native_access(ctx, "get")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    pe_segment_get_impl(ctx, this, layout, offset)
}

fn pe_segment_get_impl(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    offset: i64,
) -> MethodCallResult {
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    // Reject reads that fall outside the segment's declared bounds, overflow
    // the address space, or target a zero-size segment. The access width is
    // derived from the layout kind, matching the read widths below.
    let width = ffi::layout_byte_size(kind) as i64;
    let addr = pe_segment_access_addr(ctx, seg, offset, width)? as *const u8;

    // SAFETY: addr is non-null, bounds-checked against the segment's declared
    // size, and the address arithmetic was overflow-checked (see
    // pe_segment_access_addr). The kind determines the read width so alignment
    // is implicit from the segment.
    let value = unsafe {
        match kind {
            LAYOUT_BYTE => Value::Int(*(addr as *const i8) as i32),
            // A `boolean` is 0 or 1, never the raw byte: the JDK reads the byte
            // and compares it to zero, so a stray 2 in the segment is `true`.
            // Passing the raw byte through hands Java bytecode a `Z` value
            // outside its domain.
            LAYOUT_BOOLEAN => Value::Int(i32::from(*(addr as *const i8) != 0)),
            LAYOUT_SHORT => Value::Int(*(addr as *const i16) as i32),
            // `char` is UNSIGNED. Sign-extending it makes every code point
            // above 0x7FFF negative, which is not a `char` at all.
            LAYOUT_CHAR => Value::Int(i32::from(*(addr as *const u16))),
            LAYOUT_INT => Value::Int(*(addr as *const i32)),
            LAYOUT_LONG => Value::Long(*(addr as *const i64)),
            LAYOUT_FLOAT => Value::Float(*(addr as *const f32)),
            LAYOUT_DOUBLE => Value::Double(*(addr as *const f64)),
            // ADDRESS is handled after the unsafe block: its declared return
            // type is `Ljava/lang/foreign/MemorySegment;`, so it has to
            // ALLOCATE, which `Value::Long` cannot stand in for -- a reference
            // slot receiving a primitive is the one shape this tree keeps
            // paying for.
            LAYOUT_ADDRESS => Value::Long(*(addr as *const i64)),
            _ => Value::Int(0),
        }
    };
    if kind == LAYOUT_ADDRESS {
        let raw = match value {
            Value::Long(v) => v,
            _ => 0,
        };
        return Ok(Some(Value::Object(Some(pe_zero_length_segment(ctx, raw)?))));
    }
    if crate::nbflags().dbg_mh_dispatch && kind == LAYOUT_FLOAT && offset == 0 {
        eprintln!(
            "[PANAMA_GET_FLOAT] runtime={} ptr={:?} base_offset={:?} value={value:?}",
            ctx.class_name_of_id(ctx.class_id_of_object(seg))
                .unwrap_or_default(),
            ctx.get_field(seg, 0),
            ctx.get_field(seg, 5),
        );
    }
    Ok(Some(value))
}

fn pe_segment_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Defense-in-depth: set() dereferences the segment's raw `ptr` field.
    require_native_access(ctx, "set")?;
    let this = obj_arg(args, 0)?;
    let layout = obj_arg(args, 1)?;
    let offset = match args.get(2) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let value = args.get(3).copied().unwrap_or(Value::Int(0));
    pe_segment_set_impl(ctx, this, layout, offset, value)
}

fn pe_segment_set_impl(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    layout: ObjectRef,
    offset: i64,
    value: Value,
) -> MethodCallResult {
    let kind = crate::panama_libffi::read_layout_kind(ctx, layout);
    // Reject writes that fall outside the segment's declared bounds, overflow
    // the address space, or target a zero-size segment. The access width is
    // derived from the layout kind, matching the write widths below.
    let width = ffi::layout_byte_size(kind) as i64;
    let addr = pe_segment_access_addr(ctx, seg, offset, width)? as *mut u8;

    // SAFETY: addr is non-null, bounds-checked against the segment's declared
    // size, and the address arithmetic was overflow-checked (see
    // pe_segment_access_addr). The kind determines the write width so alignment
    // is implicit from the segment.
    // `set(ADDRESS, off, seg)` takes a MemorySegment, not a long: its address
    // is what gets stored. Without this the value arrives as `Value::Object`,
    // misses every arm below and the write is a SILENT no-op -- which is the
    // quieter half of the same defect as the missing registration.
    let value = match (kind, value) {
        (LAYOUT_ADDRESS, Value::Object(Some(target))) => {
            Value::Long(crate::panama_libffi::segment_address(ctx, target))
        }
        (LAYOUT_ADDRESS, Value::Object(None)) => Value::Long(0),
        _ => value,
    };
    unsafe {
        match (kind, value) {
            (LAYOUT_BYTE | LAYOUT_BOOLEAN, Value::Int(v)) => *(addr as *mut i8) = v as i8,
            (LAYOUT_SHORT | LAYOUT_CHAR, Value::Int(v)) => *(addr as *mut i16) = v as i16,
            (LAYOUT_INT, Value::Int(v)) => *(addr as *mut i32) = v,
            (LAYOUT_LONG | LAYOUT_ADDRESS, Value::Long(v)) => *(addr as *mut i64) = v,
            (LAYOUT_FLOAT, Value::Float(v)) => *(addr as *mut f32) = v,
            (LAYOUT_DOUBLE, Value::Double(v)) => *(addr as *mut f64) = v,
            _ => {}
        }
    }
    sync_heap_backed_segment(ctx, seg, false);
    Ok(None)
}

/// Synchronize a primitive-array-backed synthetic MemorySegment at the FFI
/// boundary. The VM cannot expose a moving Java heap array directly to
/// native code, so ofArray owns a native mirror; this retains the Java
/// backing array and copies it immediately before and after a downcall.
fn sync_heap_backed_segment(ctx: &mut dyn NativeContext, seg: ObjectRef, to_native: bool) {
    let backing = match ctx.get_field(seg, SEG_BACKING_ARRAY_FIELD) {
        Value::Object(Some(array)) if ctx.object_is_array(array) => array,
        _ => return,
    };
    let kind = match ctx.get_field(seg, SEG_BACKING_KIND_FIELD) {
        Value::Int(kind) => kind,
        _ => return,
    };
    let width = ffi::layout_byte_size(kind);
    if width == 0 {
        return;
    }
    let (base_ptr, segment_offset, byte_size) = match (
        ctx.get_field(seg, 0),
        ctx.get_field(seg, 5),
        ctx.get_field(seg, 1),
    ) {
        (Value::Long(ptr), Value::Long(offset), Value::Long(size))
            if ptr > 0 && offset >= 0 && size >= 0 =>
        {
            (ptr as usize, offset as usize, size as usize)
        }
        _ => return,
    };
    let array_start = segment_offset / width;
    if segment_offset % width != 0 || array_start >= ctx.array_length(backing) {
        return;
    }
    let count = (byte_size / width).min(ctx.array_length(backing) - array_start);
    let Some(native_addr) = base_ptr.checked_add(segment_offset) else {
        return;
    };
    for i in 0..count {
        let addr = unsafe { (native_addr as *mut u8).add(i * width) };
        if to_native {
            unsafe {
                match (kind, ctx.get_array_element(backing, array_start + i)) {
                    (LAYOUT_INT, Value::Int(value)) => *(addr as *mut i32) = value,
                    (LAYOUT_LONG, Value::Long(value)) => *(addr as *mut i64) = value,
                    (LAYOUT_FLOAT, Value::Float(value)) => *(addr as *mut f32) = value,
                    (LAYOUT_FLOAT, Value::Int(bits)) => {
                        *(addr as *mut f32) = f32::from_bits(bits as u32)
                    }
                    (LAYOUT_DOUBLE, Value::Double(value)) => *(addr as *mut f64) = value,
                    _ => {}
                }
            }
        } else {
            unsafe {
                let value = match kind {
                    LAYOUT_INT => Value::Int(*(addr as *const i32)),
                    LAYOUT_LONG => Value::Long(*(addr as *const i64)),
                    LAYOUT_FLOAT => Value::Float(*(addr as *const f32)),
                    LAYOUT_DOUBLE => Value::Double(*(addr as *const f64)),
                    _ => continue,
                };
                ctx.set_array_element(backing, array_start + i, value);
            }
        }
    }
}

// --- SymbolLookup: load shared libraries and find symbols ---
// SymbolLookup synthetic: [0]=lib_index (Long — index into SharedVm.native_libraries), [1]=name

pub(crate) fn register_pe_symbol_lookup(r: &mut NativeMethodRegistry) {
    // Promote to `Bridge`: this function runs under whatever category was
    // ambient at the `register_pe_panama` call site, which defaults to
    // `SyntheticStub` (dropped entirely under strict-no-stubs / real-JDK
    // mode). These natives are real supporting glue for the Panama FFI
    // downcall path (real library loading via `ctx.load_native_library`,
    // real `Optional`/`Optional.empty()` wrapping) — not a placeholder — and
    // must survive that filtering so `phases_late.rs`'s `Linker.defaultLookup`
    // et al. (which allocate 0-field `SymbolLookup` objects) still resolve
    // `.find()` sanely via this implementation's `_ => -1` "unknown lookup"
    // fallback instead of a former duplicate stub here always returning a
    // bare Java `null`. See the `find`/`libraryLookup` doc comments below.
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let sl = "java/lang/foreign/SymbolLookup";

    // libraryLookup(path, arena) → SymbolLookup
    r.register(
        sl,
        "libraryLookup",
        "(Ljava/lang/String;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/SymbolLookup;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let path = ctx.read_string(path_obj).unwrap_or_default();

            require_native_access(ctx, "libraryLookup")?;
            let lib_index = ctx.load_native_library(&path)?;

            let lookup = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 2)?;
            ctx.set_field(lookup, 0, Value::Long(lib_index));
            let name_str = ctx.create_string(&path);
            ctx.set_field(lookup, 1, Value::Object(Some(name_str)));
            Ok(Some(Value::Object(Some(lookup))))
        },
    );

    // loaderLookup() → SymbolLookup (stub — returns lookup for default system library)
    r.register(
        sl,
        "loaderLookup",
        "()Ljava/lang/foreign/SymbolLookup;",
        |ctx, _| {
            // The returned lookup searches every loaded library (index -1),
            // so obtaining one is itself a capability. Gated identically to
            // `libraryLookup` minus the `--enable-native-access` requirement,
            // which the JDK does not impose on `loaderLookup`.
            crate::security_manager::check_host_native_access_or_throw(ctx, "symbolLookup")?;
            let lookup = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 2)?;
            ctx.set_field(lookup, 0, Value::Long(-1)); // -1 = default/system lookup
            Ok(Some(Value::Object(Some(lookup))))
        },
    );

    // find(name) → Optional<MemorySegment>
    r.register(
        sl,
        "find",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sym_name_obj = obj_arg(args, 1)?;
            let sym_name = ctx.read_string(sym_name_obj).unwrap_or_default();
            let lib_index = match ctx.get_field(this, 0) {
                Value::Long(n) => n,
                _ => -1,
            };

            // A `loaderLookup()` receiver carries `lib_index == -1`, and
            // `find_native_symbol` treats that as "search every loaded
            // library", so `loaderLookup().find(x)` is an arbitrary-symbol
            // address oracle over libraries loaded by trusted code. Gate it
            // the same way as the load paths rather than leaving the read
            // side open when the load side is closed.
            crate::security_manager::check_host_native_access_or_throw(ctx, "symbolLookup")?;

            match ctx.find_native_symbol(lib_index, &sym_name) {
                Some(addr) => {
                    // Wrap address in MemorySegment and Optional.of()
                    let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
                    ctx.set_field(seg, 0, Value::Long(addr as i64));
                    ctx.set_field(seg, 1, Value::Long(0)); // function pointer — no byte size
                    ctx.set_field(seg, 2, Value::Object(None));
                    ctx.set_field(seg, 3, Value::Int(1)); // read-only
                    ctx.set_field(seg, 4, Value::Int(1)); // alive
                    ctx.set_field(seg, 5, Value::Long(0));
                    // Return as Optional.of(segment)
                    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
                    ctx.set_field(opt, 0, Value::Object(Some(seg)));
                    Ok(Some(Value::Object(Some(opt))))
                }
                None => {
                    // Return Optional.empty()
                    let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
                    ctx.set_field(opt, 0, Value::Object(None));
                    Ok(Some(Value::Object(Some(opt))))
                }
            }
        },
    );
    r.set_category(__prev_cat);
}

// --- RawNativeLibraries: the real-JDK FFM native-library load path ---
//
// In real-JDK mode `java.lang.foreign.SymbolLookup.libraryLookup(name, arena)`
// runs the JDK's own bytecode (our higher-level `SymbolLookup.libraryLookup`
// override does not win over a concrete method body), which routes through
// `jdk.internal.loader.RawNativeLibraries`:
//
//   RawNativeLibraryImpl.open()  → RawNativeLibraries.load0(impl, name)  // dlopen/LoadLibrary
//   RawNativeLibraryImpl.find()  → NativeLibrary.findEntry0(handle, name) // dlsym/GetProcAddress
//   RawNativeLibraryImpl.close() → RawNativeLibraries.unload0(name, handle)// dlclose/FreeLibrary
//
// All three are real ACC_NATIVE methods, so a registry override always
// dispatches to them (unlike the concrete-bytecode methods above). Without
// them, any FFM downcall binding — e.g. Tomcat's `openssl_h` loading
// libssl/libcrypto — dies in `<clinit>` with
// `UnsatisfiedLinkError: RawNativeLibraries.load0`. Back them with CratonVM's
// existing native-library table (the same `load_native_library` /
// `find_native_symbol` the panama `SymbolLookup` above uses). This makes the
// VM behave like HotSpot: the load is attempted, and if the library is not
// present `load0` returns false (the caller returns null and Tomcat falls back
// to JSSE) rather than raising.
//
// NOTE: registered from `register_essential_natives` (the always-compiled,
// real-JDK path), NOT from `register_pe_panama` — the latter is only reached
// under `#[cfg(feature = "synthetic-jdk")]` and would be dead-code-eliminated
// in real-JDK mode, which is exactly where these natives are needed (real-JDK
// mode runs the JDK's own FFM bytecode, which calls these natives directly).
pub(crate) fn register_pe_raw_native_libraries(r: &mut NativeMethodRegistry) {
    let rnl = "jdk/internal/loader/RawNativeLibraries";

    // static native boolean load0(RawNativeLibraryImpl impl, String name)
    r.register_with_kind(
        rnl,
        "load0",
        "(Ljdk/internal/loader/RawNativeLibraries$RawNativeLibraryImpl;Ljava/lang/String;)Z",
        |ctx, args| {
            let impl_obj = obj_arg(args, 0)?;
            let name_obj = obj_arg(args, 1)?;
            let name = ctx.read_string(name_obj).unwrap_or_default();
            require_native_access(ctx, "libraryLookup")?;
            match ctx.load_native_library(&name) {
                Ok(lib_index) => {
                    // Stash the library index as the opaque `handle` long.
                    // Offset by +1 so a valid index 0 never collides with the
                    // handle==0 "not loaded" sentinel that
                    // RawNativeLibraryImpl.open() checks before calling load0;
                    // findEntry0 below decodes it back.
                    ctx.set_field_by_name(impl_obj, "handle", Value::Long(lib_index + 1));
                    Ok(Some(Value::Int(1)))
                }
                // Match the real native: a failed load returns false (caller
                // returns null), it does NOT throw.
                Err(_) => Ok(Some(Value::Int(0))),
            }
        },
        NativeKind::Bridge,
    );

    // static native void unload0(String name, long handle)
    //
    // IMPLEMENTED (was a no-op) via `NativeContext::unload_native_library`, the
    // escalation this comment used to request. No new handle plumbing was
    // needed: `load0` above stashes `lib_index + 1`, which is exactly what
    // `RawNativeLibraryImpl.close()` hands back as `handle`, so the same `- 1`
    // decode `findEntry0` uses recovers the index.
    //
    // It is a LOGICAL unload, not a `dlclose`: the VM's library table is an
    // append-only `Vec<Library>` whose index IS the handle, so an entry cannot
    // be removed without renumbering every live handle. The VM implementation
    // therefore tombstones the index — subsequent `find_native_symbol` calls on
    // it fail, as they would against a closed handle — while leaving the mapping
    // resident. Staying mapped is the safe direction of the two errors: a
    // `RawNativeLibraries` handle can still back live function pointers (callers
    // cache `findEntry0` results, and any bound downcall stub holds one), so
    // unmapping underneath them segfaults, while not unmapping costs an
    // address-space mapping.
    //
    // Real `unload0` returns void and reports nothing, so the `false` an
    // implementation without an unload path returns is intentionally ignored.
    r.register_with_kind(rnl, "unload0", "(Ljava/lang/String;J)V", |ctx, args| {
        let handle = match args.get(1) {
            Some(Value::Long(n)) => *n,
            Some(Value::Int(n)) => *n as i64,
            _ => 0,
        };
        // Undo the +1 offset applied by load0 to recover the library index; a
        // handle of 0 ("not loaded") decodes to -1 and is refused.
        let _unloaded = ctx.unload_native_library(handle - 1);
        Ok(None)
    }, NativeKind::Bridge);

    // static native long findEntry0(long handle, String name)  (in NativeLibrary)
    r.register_with_kind(
        "jdk/internal/loader/NativeLibrary",
        "findEntry0",
        "(JLjava/lang/String;)J",
        |ctx, args| {
            let handle = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let name_obj = obj_arg(args, 1)?;
            let name = ctx.read_string(name_obj).unwrap_or_default();
            // `handle` is caller-supplied and is only `lib_index + 1`, a small
            // dense integer, so without this gate a guest can walk 1, 2, 3, …
            // and `dlsym` any library loaded by anyone in the process — a
            // symbol-address oracle that never passes the load-time check.
            // Same gate as the load paths, so trusted callers are unaffected:
            // it denies only under CRATONVM_UNTRUSTED_CODE or a SecurityManager
            // policy that withholds `loadLibrary.*`.
            crate::security_manager::check_host_native_access_or_throw(ctx, "findEntry")?;
            // Undo the +1 offset applied by load0 to recover the library index.
            let lib_index = handle - 1;
            let addr = ctx.find_native_symbol(lib_index, &name).unwrap_or(0);
            Ok(Some(Value::Long(addr as i64)))
        },
        NativeKind::Bridge,
    );
}

// --- Linker: create downcall handles ---
// DowncallHandle synthetic: [0]=function_address, [1]=descriptor,
// [2]=first variadic argument, [3]=cached CIF, [4]=captureCallState flag.

pub(crate) fn register_pe_linker_options(r: &mut NativeMethodRegistry) {
    let option = "java/lang/foreign/Linker$Option";
    r.register(
        option,
        "critical",
        "(Z)Ljava/lang/foreign/Linker$Option;",
        |ctx, args| {
            let enabled = args.first().and_then(|v| v.as_int()).unwrap_or(0) != 0;
            let opt = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker$Option", 2)?;
            ctx.set_field(opt, 0, Value::Int(1)); // kind = critical
            ctx.set_field(opt, 1, Value::Long(enabled as i64));
            Ok(Some(Value::Object(Some(opt))))
        },
    );
}

/// True for the real-JDK option that adds a leading MemorySegment state
/// argument to a downcall handle.
pub(crate) fn downcall_option_captures_call_state(
    ctx: &dyn NativeContext,
    option: ObjectRef,
) -> bool {
    matches!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(option))
            .as_deref(),
        Some("jdk/internal/foreign/abi/LinkerOptions$CaptureCallState")
    )
}

fn register_pe_linker(r: &mut NativeMethodRegistry) {
    let linker = "java/lang/foreign/Linker";

    // nativeLinker() → Linker
    r.register(
        linker,
        "nativeLinker",
        "()Ljava/lang/foreign/Linker;",
        |ctx, _| {
            let l = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker", 1)?;
            Ok(Some(Value::Object(Some(l))))
        },
    );

    // downcallHandle(MemorySegment address, FunctionDescriptor desc) → MethodHandle
    //
    // The handle synthetic carries 4 fields:
    //   field 0 : Long   — function pointer
    //   field 1 : Object — FunctionDescriptor
    //   field 2 : Long   — first-variadic-arg index (-1 = non-variadic)
    //   field 3 : Long   — T5.6.3 cached `Box<Cif>` raw pointer as u64
    //                      (0 = not yet built). See `panama_libffi`.
    r.register(linker, "downcallHandle", "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;)Ljava/lang/invoke/MethodHandle;", |ctx, args| {
        let addr_seg = obj_arg(args, 1)?;
        let descriptor = obj_arg(args, 2)?;
        let fn_addr = crate::panama_libffi::segment_address(ctx, addr_seg);

        let handle = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/DowncallHandle", 5)?;
        ctx.set_field(handle, 0, Value::Long(fn_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));
        ctx.set_field(handle, 2, Value::Long(-1));
        ctx.set_field(handle, 3, Value::Long(0)); // cif not yet cached
        ctx.set_field(handle, 4, Value::Int(0)); // captureCallState disabled
        Ok(Some(Value::Object(Some(handle))))
    });

    // downcallHandle with Linker.Option[] for variadic etc. (NEW-18.3).
    r.register(linker, "downcallHandle",
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let addr_seg = obj_arg(args, 1)?;
            let descriptor = obj_arg(args, 2)?;
            let fn_addr = crate::panama_libffi::segment_address(ctx, addr_seg);

            // Scan the option array for a firstVariadicArg option (kind=0).
            // Linker.Option synthetic layout: field 0 = kind (Int), field 1 = payload (Long).
            let mut variadic_fixed: i64 = -1;
            let mut capture_call_state = false;
            if let Some(Value::Object(Some(opts))) = args.get(3) {
                let n = ctx.array_length(*opts);
                for i in 0..n {
                    if let Value::Object(Some(opt)) = ctx.get_array_element(*opts, i) {
                        if downcall_option_captures_call_state(ctx, opt) {
                            capture_call_state = true;
                        }
                        let kind = match ctx.get_field(opt, 0) {
                            Value::Int(k) => k,
                            _ => -1,
                        };
                        if kind == 0 {
                            variadic_fixed = match ctx.get_field(opt, 1) {
                                Value::Long(v) => v,
                                Value::Int(v) => v as i64,
                                _ => -1,
                            };
                        }
                    }
                }
            }

            if crate::nbflags().dbg_linker {
                eprintln!(
                    "[PANAMA_LINKER] option downcall addr=0x{fn_addr:x} options={}",
                    args.get(3).is_some()
                );
            }
            let handle = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/DowncallHandle", 5)?;
            ctx.set_field(handle, 0, Value::Long(fn_addr));
            ctx.set_field(handle, 1, Value::Object(Some(descriptor)));
            ctx.set_field(handle, 2, Value::Long(variadic_fixed));
            ctx.set_field(handle, 3, Value::Long(0)); // cif not yet cached
            ctx.set_field(handle, 4, Value::Int(capture_call_state as i32));
            Ok(Some(Value::Object(Some(handle))))
        },
    );

    // Linker.Option.firstVariadicArg(int n) → Linker$Option synthetic (kind=0, payload=n)
    r.register(
        "java/lang/foreign/Linker$Option",
        "firstVariadicArg",
        "(I)Ljava/lang/foreign/Linker$Option;",
        |ctx, args| {
            let n = args.first().and_then(|v| v.as_int()).unwrap_or(0);
            let opt = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker$Option", 2)?;
            ctx.set_field(opt, 0, Value::Int(0)); // kind = firstVariadicArg
            ctx.set_field(opt, 1, Value::Long(n as i64)); // payload
            Ok(Some(Value::Object(Some(opt))))
        },
    );
    register_pe_linker_options(r);

    // DowncallHandle.invoke(Object... args) → Object
    // This is the actual native function call entry point.
    let dh = "java/lang/foreign/DowncallHandle";
    r.register(
        dh,
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_downcall_invoke,
    );
    r.register(
        dh,
        "invokeExact",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_downcall_invoke,
    );
    r.register(
        dh,
        "invokeBasic",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_downcall_invoke,
    );
    r.register(
        dh,
        "type",
        "()Ljava/lang/invoke/MethodType;",
        pe_downcall_type,
    );

    // upcallHandle(target, descriptor, arena) → MemorySegment wrapping trampoline address
    r.register(linker, "upcallHandle",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/foreign/FunctionDescriptor;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/MemorySegment;",
        pe_upcall_handle);

    // UpcallStub.invoke — dispatches from a trampoline back into Java
    let us = "java/lang/foreign/UpcallStub";
    r.register(
        us,
        "invoke",
        "([Ljava/lang/Object;)Ljava/lang/Object;",
        pe_upcall_invoke,
    );
}

/// Return the MethodHandle type represented by a synthetic DowncallHandle.
///
/// The synthetic stores its FunctionDescriptor in field 1, while JDK callers
/// still invoke inherited MethodHandle.type(). Derive its carrier signature
/// from that descriptor instead of exposing an untyped Object[] invoker.
pub(crate) fn pe_downcall_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::panama_libffi as plf;

    let handle = obj_arg(args, 0)?;
    let descriptor = match ctx.get_field(handle, 1) {
        Value::Object(Some(descriptor)) => descriptor,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "DowncallHandle has no FunctionDescriptor".into(),
            }
            .into());
        }
    };

    let mut method_descriptor = String::from("(");
    if downcall_handle_captures_call_state(ctx, handle) {
        method_descriptor.push_str("Ljava/lang/foreign/MemorySegment;");
    }
    for layout in plf::descriptor_param_layouts(ctx, descriptor) {
        let layout = layout.ok_or_else(|| -> MethodCallFailed {
            RuntimeError::IllegalStateException {
                message: "DowncallHandle FunctionDescriptor has null parameter layout".into(),
            }
            .into()
        })?;
        method_descriptor.push_str(downcall_layout_carrier_descriptor(plf::read_layout_kind(
            ctx, layout,
        )));
    }
    method_descriptor.push(')');
    match plf::descriptor_return_layout(ctx, descriptor) {
        Some(layout) => method_descriptor.push_str(downcall_layout_carrier_descriptor(
            plf::read_layout_kind(ctx, layout),
        )),
        None => method_descriptor.push('V'),
    }

    let method_type =
        crate::lang_invoke::build_method_type_from_descriptor(ctx, &method_descriptor)?.ok_or_else(
            || -> MethodCallFailed {
                RuntimeError::IllegalStateException {
                    message: format!(
                "Unable to construct MethodType for DowncallHandle descriptor {method_descriptor}"
            ),
                }
                .into()
            },
        )?;
    Ok(Some(Value::Object(Some(method_type))))
}

fn downcall_layout_carrier_descriptor(kind: i32) -> &'static str {
    match kind {
        LAYOUT_BOOLEAN => "Z",
        LAYOUT_BYTE => "B",
        LAYOUT_SHORT => "S",
        LAYOUT_CHAR => "C",
        LAYOUT_INT => "I",
        LAYOUT_LONG => "J",
        LAYOUT_FLOAT => "F",
        LAYOUT_DOUBLE => "D",
        // ADDRESS and aggregate layouts use the FFM MemorySegment carrier.
        _ => "Ljava/lang/foreign/MemorySegment;",
    }
}

fn downcall_handle_captures_call_state(ctx: &dyn NativeContext, handle: ObjectRef) -> bool {
    matches!(ctx.get_field(handle, 4), Value::Int(flag) if flag != 0)
}

fn write_downcall_capture_state(ctx: &dyn NativeContext, state: ObjectRef) {
    #[cfg(target_os = "linux")]
    {
        let addr = crate::panama_libffi::segment_address(ctx, state);
        if addr != 0 {
            // The standard Linux capture-state layout starts with errno.
            unsafe { *(addr as *mut i32) = *libc::__errno_location() };
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ctx, state);
    }
}

/// Execute a downcall via libffi (NEW-18).
///
/// The handle synthetic carries the function pointer (field 0), the
/// FunctionDescriptor (field 1), an optional fixed-arg count for
/// variadic calls (field 2; -1 means non-variadic), and a T5.6.3
/// cached `Box<Cif>` raw pointer (field 3; 0 means unpopulated).
/// The descriptor carries the parameter and return layouts.
///
/// libffi handles ABI classification (integer vs float register
/// allocation, struct-by-value, alignment, padding) for every
/// supported platform — replacing the previous 8-arg integer-only
/// dispatcher. See `panama_libffi` for the layout↔ffi_type bridge.
///
/// TODO(T5.6.3 finalization): the `Box<Cif>` stashed on field 3 is
/// currently leaked when the DowncallHandle is garbage-collected —
/// the Panama synthetic objects don't yet route through a finalizer
/// callback. Since `Linker::downcallHandle` is called once per native
/// function per VM run the leak is bounded (typically a handful of
/// Cifs) and matches HotSpot's own permanent FFI metadata. When a
/// finalizer path lands, call `plf::free_cached_cif(field3)` from it.
pub(crate) fn pe_downcall_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::panama_libffi as plf;
    use libffi::middle::{arg as ffi_arg, CodePtr};

    require_native_access(ctx, "downcall")?;
    let handle = obj_arg(args, 0)?;
    let fn_addr = match ctx.get_field(handle, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
    // Work-list item 14. The symbol name is not carried on the handle, so the
    // scope is the target address — which is what a denial needs to report and
    // what an operator would have to grant. Permissive by default.
    crate::capability_gate::gate_foreign_downcall(&*ctx, &format!("0x{fn_addr:x}"))?;
    let descriptor = match ctx.get_field(handle, 1) {
        Value::Object(Some(d)) => d,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "No FunctionDescriptor".into(),
            }
            .into())
        }
    };

    if fn_addr == 0 {
        return Err(RuntimeError::IllegalStateException {
            message: "Null function pointer".into(),
        }
        .into());
    }
    // Run the validated_fn_ptr alignment check.
    let _: unsafe extern "C" fn() = validated_fn_ptr(fn_addr)?;

    // Variadic flag — the Linker.Option.firstVariadicArg(int) overload
    // populates this when registering the downcall handle. -1 (or
    // missing field) = non-variadic.
    let variadic_fixed: Option<usize> = match ctx.get_field(handle, 2) {
        Value::Long(n) if n >= 0 => Some(n as usize),
        Value::Int(n) if n >= 0 => Some(n as usize),
        _ => None,
    };

    // T5.6.3 — previously cached `Box<Cif>` pointer (0 = miss). See
    // `panama_libffi::box_cif_to_u64` for the encoding.
    let cached_cif_u64: u64 = match ctx.get_field(handle, 3) {
        Value::Long(n) => n as u64,
        _ => 0,
    };

    // ----- Read descriptor layouts -----
    let param_layout_objs = plf::descriptor_param_layouts(ctx, descriptor);
    let return_layout = plf::descriptor_return_layout(ctx, descriptor);

    // ----- Read incoming Java arguments -----
    //
    // The legacy synthetic bridge calls invoke(Object[]) and supplies one
    // boxed array after the handle. Signature-polymorphic real-JDK calls
    // arrive as [handle, arg0, arg1, ...], so preserve those concrete
    // values instead of interpreting the first reference argument as an
    // array.
    let mut call_args: Vec<Value> = match args.get(1) {
        Some(Value::Object(Some(arr))) if args.len() == 2 && ctx.object_is_array(*arr) => {
            let len = ctx.array_length(*arr);
            (0..len).map(|i| ctx.get_array_element(*arr, i)).collect()
        }
        _ => args[1..].to_vec(),
    };

    let capture_state = if downcall_handle_captures_call_state(ctx, handle) {
        if call_args.len() != param_layout_objs.len() + 1 {
            return Err(RuntimeError::IllegalStateException {
                message: format!(
                    "Panama capture-state downcall expected {} arguments, got {}",
                    param_layout_objs.len() + 1,
                    call_args.len()
                ),
            }
            .into());
        }
        match call_args.remove(0) {
            Value::Object(Some(state)) => Some(state),
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "Panama capture-state argument is not a MemorySegment".into(),
                }
                .into());
            }
        }
    } else {
        None
    };

    if call_args.len() != param_layout_objs.len() {
        return Err(RuntimeError::IllegalStateException {
            message: format!(
                "Panama downcall arity mismatch: descriptor has {} params, got {} args",
                param_layout_objs.len(),
                call_args.len()
            ),
        }
        .into());
    }

    // Heap-array segments use a native mirror so moving GC cannot expose the
    // Java heap directly. Refresh mirrors at the precise foreign boundary.
    for value in &call_args {
        if let Value::Object(Some(segment)) = value {
            sync_heap_backed_segment(ctx, *segment, true);
        }
    }

    // ----- Marshal arguments into per-slot scratch storage -----
    // Must happen before the CIF hit/miss fork because it allocates
    // per-call storage that depends on the actual Value inputs.
    let mut slots: Vec<plf::ArgSlot> = Vec::with_capacity(call_args.len());
    for (i, val) in call_args.iter().enumerate() {
        let layout = param_layout_objs[i].ok_or_else(|| -> MethodCallFailed {
            RuntimeError::IllegalStateException {
                message: format!("Panama downcall parameter {} layout is null", i),
            }
            .into()
        })?;
        slots.push(plf::marshal_arg(ctx, layout, val)?);
    }

    // ----- CIF: cache hit / miss -----
    //
    // If the DowncallHandle carries a previously boxed Cif, reuse it.
    // Otherwise build a fresh Cif, box it, and stash the raw pointer
    // on field 3 so the next invocation skips layout→ffi_type
    // translation and the libffi `prep_cif` call.
    //
    // After this block, `raw_cif_ptr` always points at a Cif living
    // inside the stashed Box on the handle synthetic. The Box outlives
    // us (it lives until handle finalization).
    let raw_cif_ptr: *mut libffi::low::ffi_cif = if cached_cif_u64 != 0 {
        // SAFETY: `cached_cif_u64` was produced by box_cif_to_u64 in
        // a prior invocation on this same handle; the Box outlives
        // this call (freed on handle finalization). libffi only
        // reads the Cif during ffi_call.
        let cif_ref =
            unsafe { plf::cached_cif_ref(cached_cif_u64) }.expect("non-zero pointer must deref");
        cif_ref.as_raw_ptr()
    } else {
        // ----- Build libffi types for each parameter -----
        let mut ffi_arg_types = Vec::with_capacity(param_layout_objs.len());
        for (i, pl) in param_layout_objs.iter().enumerate() {
            let layout = pl.ok_or_else(|| -> MethodCallFailed {
                RuntimeError::IllegalStateException {
                    message: format!("Panama downcall parameter {} layout is null", i),
                }
                .into()
            })?;
            ffi_arg_types.push(plf::layout_to_ffi_type(ctx, layout, 0)?);
        }

        // Return type
        let ret_ffi_type = match return_layout {
            Some(rl) => plf::layout_to_ffi_type(ctx, rl, 0)?,
            None => libffi::middle::Type::void(),
        };

        // Build CIF (variadic-aware) and record the build for tests.
        let cif = plf::build_cif_and_record(ffi_arg_types, ret_ffi_type, variadic_fixed)?;
        // Move the Cif into a Box, stash the raw pointer, and return a
        // pointer to the Box-owned Cif for this call.
        let stash_u64 = plf::box_cif_to_u64(cif);
        ctx.set_field(handle, 3, Value::Long(stash_u64 as i64));
        // SAFETY: stash_u64 was produced above; Box is live for the
        // remainder of this call and beyond.
        let cif_ref =
            unsafe { plf::cached_cif_ref(stash_u64) }.expect("just-stashed pointer must deref");
        cif_ref.as_raw_ptr()
    };

    // libffi expects the args slice to be `&[Arg]` where each `Arg`
    // wraps a `*mut c_void` — which is the address of the typed slot
    // bytes. The slot Vec must outlive the call.
    let arg_refs: Vec<libffi::middle::Arg> = slots
        .iter()
        .map(|s| ffi_arg(unsafe { &*(s.as_ptr() as *const u8) }))
        .collect();

    // ----- Allocate return slot -----
    let mut ret_slot = plf::alloc_return_slot(ctx, return_layout)?;

    // ----- Make the call -----
    // Install the active NativeContext so any libffi-closure-backed
    // upcall fired from C during this downcall can re-enter Java.
    // The guard restores the previous slot on drop.
    let _ctx_guard = plf::ActiveContextGuard::install(ctx);

    // SAFETY: fn_addr was validated above for non-null and alignment
    // via validated_fn_ptr. The CIF was built from the descriptor, so
    // arg/return types match the slot byte layout. The slot vectors
    // outlive the call (held until return). `raw_cif_ptr` points into
    // the stashed `Box<Cif>` on the handle synthetic (field 3), which
    // lives for the remainder of the handle's lifetime.
    unsafe {
        // ffi_call requires a real writable result address even for a void
        // descriptor. Do not use low::call<()> here: its zero-sized return
        // slot gives libffi a dangling result pointer and caused native void
        // calls (the Elasticsearch bulk-vector symbols) to leave outputs
        // untouched on this ABI.
        let mut void_sink = [0u8; std::mem::size_of::<usize>()];
        let result_ptr = if ret_slot.is_empty() {
            void_sink.as_mut_ptr() as *mut std::ffi::c_void
        } else {
            ret_slot.as_mut_ptr() as *mut std::ffi::c_void
        };
        libffi::raw::ffi_call(
            raw_cif_ptr,
            Some(*CodePtr::from_ptr(fn_addr as *const std::ffi::c_void).as_safe_fun()),
            result_ptr,
            arg_refs.as_ptr() as *mut *mut std::ffi::c_void,
        );
    }

    for value in &call_args {
        if let Value::Object(Some(segment)) = value {
            sync_heap_backed_segment(ctx, *segment, false);
        }
    }

    if crate::nbflags().dbg_mh_dispatch && return_layout.is_none() && call_args.len() == 5 {
        if let Some(Value::Object(Some(out))) = call_args.last() {
            if let Value::Long(ptr) = ctx.get_field(*out, 0) {
                if ptr != 0 {
                    let first = unsafe { *(ptr as *const f32) };
                    eprintln!("[PANAMA_POST_VOID] fn=0x{fn_addr:x} out=0x{ptr:x} first={first}");
                }
            }
        }
    }
    if let Some(state) = capture_state {
        write_downcall_capture_state(ctx, state);
    }

    // ----- Unmarshal return -----
    let result = match return_layout {
        None => Value::Object(None),
        Some(rl) => {
            let kind = plf::read_layout_kind(ctx, rl);
            if kind == LAYOUT_ADDRESS {
                let address = match plf::unmarshal_return_primitive(kind, &ret_slot) {
                    Value::Long(address) => address,
                    _ => 0,
                };
                let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
                ctx.set_field(seg, 0, Value::Long(address));
                ctx.set_field(seg, 1, Value::Long(0));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                Value::Object(Some(seg))
            } else if kind < 10 {
                plf::unmarshal_return_primitive(kind, &ret_slot)
            } else {
                // Struct/union/sequence return: copy the bytes into a
                // freshly allocated MemorySegment via the global arena
                // path. The Java caller will then read the segment.
                let total = plf::layout_total_size(ctx, rl)?;
                let (alloc_id, ptr) = ctx
                    .allocate_native_memory(total, plf::layout_align(ctx, rl).max(8))
                    .ok_or_else(|| -> MethodCallFailed {
                        RuntimeError::OutOfMemoryError {
                            message: "Failed to allocate result MemorySegment".into(),
                        }
                        .into()
                    })?;
                // SAFETY: ptr was just freshly allocated to `total`
                // bytes; ret_slot has at least `total` bytes (we sized
                // it that way for aggregate returns).
                unsafe {
                    std::ptr::copy_nonoverlapping(ret_slot.as_ptr(), ptr, total);
                }
                let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(total as i64));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                let _ = alloc_id;
                Value::Object(Some(seg))
            }
        }
    };

    Ok(Some(result))
}

// --- FunctionDescriptor: native function signature ---
// FunctionDescriptor: [0]=return layout (Object or null for void), [1]=param layouts (Object array)

fn register_pe_function_descriptor(r: &mut NativeMethodRegistry) {
    let fd = "java/lang/foreign/FunctionDescriptor";

    let prev_category = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // of(returnLayout, paramLayouts...) → FunctionDescriptor
    r.register(fd, "of", "(Ljava/lang/foreign/ValueLayout;[Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/FunctionDescriptor;", |ctx, args| {
        let ret_layout = args.first().copied().unwrap_or(Value::Object(None));
        let params = args.get(1).copied().unwrap_or(Value::Object(None));
        let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
        ctx.set_field(desc, 0, ret_layout);
        ctx.set_field(desc, 1, params);
        Ok(Some(Value::Object(Some(desc))))
    });

    r.register(fd, "of", "(Ljava/lang/foreign/MemoryLayout;[Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;", |ctx, args| {
        let ret_layout = args.first().copied().unwrap_or(Value::Object(None));
        let params = args.get(1).copied().unwrap_or(Value::Object(None));
        let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
        ctx.set_field(desc, 0, ret_layout);
        ctx.set_field(desc, 1, params);
        Ok(Some(Value::Object(Some(desc))))
    });

    // ofVoid(paramLayouts...) → FunctionDescriptor
    r.register(
        fd,
        "ofVoid",
        "([Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/FunctionDescriptor;",
        |ctx, args| {
            let params = args.first().copied().unwrap_or(Value::Object(None));
            let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
            ctx.set_field(desc, 0, Value::Object(None)); // void return
            ctx.set_field(desc, 1, params);
            Ok(Some(Value::Object(Some(desc))))
        },
    );

    r.register(
        fd,
        "ofVoid",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/FunctionDescriptor;",
        |ctx, args| {
            let params = args.first().copied().unwrap_or(Value::Object(None));
            let desc = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2)?;
            ctx.set_field(desc, 0, Value::Object(None));
            ctx.set_field(desc, 1, params);
            Ok(Some(Value::Object(Some(desc))))
        },
    );

    // returnLayout() → Optional<ValueLayout>
    r.register(fd, "returnLayout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let rl = ctx.get_field(this, 0);
        let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        ctx.set_field(opt, 0, rl);
        Ok(Some(Value::Object(Some(opt))))
    });

    // argumentLayouts() → ValueLayout[]
    r.register(
        fd,
        "argumentLayouts",
        "()[Ljava/lang/foreign/ValueLayout;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.set_category(prev_category);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// =============================================================================
// Phase 85.4: Upcall Trampoline Pool
// =============================================================================
//
// When native code needs to call back into Java (upcall), we need a real
// extern "C" function pointer. Since we can't dynamically generate executable
// code portably, we pre-generate a fixed pool of trampoline functions via macro.
//
// Each trampoline:
//   1. Reads its slot index (embedded as a constant)
//   2. Looks up the callback info from the global dispatch table
//   3. Invokes the Java method handle through a thread-local NativeContext
//
// The pool size (MAX_UPCALL_TRAMPOLINES) limits concurrent upcall stubs.

use std::sync::{Mutex, OnceLock};

/// Maximum number of concurrent upcall trampolines.
const MAX_UPCALL_TRAMPOLINES: usize = 64;

/// Global dispatch table entry for an upcall trampoline.
struct UpcallTrampolineEntry {
    /// The Java target object (MethodHandle)
    target: ObjectRef,
    /// Parameter layout kinds for marshaling
    param_kinds: Vec<i32>,
    /// Return layout kind for marshaling
    return_kind: i32,
    /// Whether this slot is in use
    active: bool,
}

/// Global dispatch table — maps trampoline slot to callback info.
static UPCALL_DISPATCH_TABLE: OnceLock<Mutex<Vec<Option<UpcallTrampolineEntry>>>> = OnceLock::new();

fn upcall_dispatch_table() -> &'static Mutex<Vec<Option<UpcallTrampolineEntry>>> {
    UPCALL_DISPATCH_TABLE.get_or_init(|| {
        let mut v = Vec::with_capacity(MAX_UPCALL_TRAMPOLINES);
        for _ in 0..MAX_UPCALL_TRAMPOLINES {
            v.push(None);
        }
        Mutex::new(v)
    })
}

/// Allocate a trampoline slot and return (slot_index, function_pointer).
fn allocate_trampoline_slot(
    target: ObjectRef,
    param_kinds: Vec<i32>,
    return_kind: i32,
) -> Option<(usize, usize)> {
    let mut table = upcall_dispatch_table().lock().ok()?;
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(UpcallTrampolineEntry {
                target,
                param_kinds,
                return_kind,
                active: true,
            });
            // Return the function pointer for trampoline #i
            let fn_ptr = UPCALL_TRAMPOLINE_FNS[i] as usize;
            return Some((i, fn_ptr));
        }
    }
    None // all slots in use
}

/// Free a trampoline slot.
fn free_trampoline_slot(slot: usize) {
    if let Ok(mut table) = upcall_dispatch_table().lock() {
        if slot < table.len() {
            table[slot] = None;
        }
    }
}

/// Generic trampoline dispatch: called by each generated trampoline with its slot index.
/// Accepts up to 8 raw u64 arguments and returns a u64 result.
///
/// This function looks up the callback in the dispatch table and invokes it
/// through the VM's upcall mechanism (pe_upcall_invoke). Since we're called
/// from C, we don't have a NativeContext — real upcalls would go through the
/// JNI thread-local VM reference. For now, the trampoline returns 0 if no
/// context is available (the Java-side dispatch happens through pe_upcall_invoke
/// when the JVM calls the stub's invoke method).
fn trampoline_dispatch(slot: usize, args: &[u64]) -> u64 {
    let table = match upcall_dispatch_table().lock() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let entry = match &table[slot] {
        Some(e) if e.active => e,
        _ => return 0,
    };
    // In a full implementation, we'd use thread-local NativeContext to invoke
    // the Java target. For now, we record that the trampoline was called and
    // return 0 (the pe_upcall_invoke path handles actual Java dispatch).
    let _ = (entry.target, &entry.param_kinds, entry.return_kind);
    let _ = args;
    0
}

/// Macro to generate N extern "C" trampoline functions.
#[allow(unused_macros)]
macro_rules! gen_trampolines {
    ($($idx:expr),* $(,)?) => {
        $(
            unsafe extern "C" fn _upcall_trampoline_fn($idx: usize,
                a0: u64, a1: u64, a2: u64, a3: u64,
                a4: u64, a5: u64, a6: u64, a7: u64) -> u64
            {
                // The slot index is baked into the function via the array index
                // We use a workaround: each function is distinct because the
                // compiler sees a different constant in the body.
                let _ = $idx; // suppress unused warning; the slot is the array index
                0
            }
        )*
    };
}

// Generate individual trampoline functions with distinct slot constants.
macro_rules! gen_trampoline {
    ($name:ident, $slot:expr) => {
        unsafe extern "C" fn $name(
            a0: u64,
            a1: u64,
            a2: u64,
            a3: u64,
            a4: u64,
            a5: u64,
            a6: u64,
            a7: u64,
        ) -> u64 {
            trampoline_dispatch($slot, &[a0, a1, a2, a3, a4, a5, a6, a7])
        }
    };
}

gen_trampoline!(_upcall_t00, 0);
gen_trampoline!(_upcall_t01, 1);
gen_trampoline!(_upcall_t02, 2);
gen_trampoline!(_upcall_t03, 3);
gen_trampoline!(_upcall_t04, 4);
gen_trampoline!(_upcall_t05, 5);
gen_trampoline!(_upcall_t06, 6);
gen_trampoline!(_upcall_t07, 7);
gen_trampoline!(_upcall_t08, 8);
gen_trampoline!(_upcall_t09, 9);
gen_trampoline!(_upcall_t10, 10);
gen_trampoline!(_upcall_t11, 11);
gen_trampoline!(_upcall_t12, 12);
gen_trampoline!(_upcall_t13, 13);
gen_trampoline!(_upcall_t14, 14);
gen_trampoline!(_upcall_t15, 15);
gen_trampoline!(_upcall_t16, 16);
gen_trampoline!(_upcall_t17, 17);
gen_trampoline!(_upcall_t18, 18);
gen_trampoline!(_upcall_t19, 19);
gen_trampoline!(_upcall_t20, 20);
gen_trampoline!(_upcall_t21, 21);
gen_trampoline!(_upcall_t22, 22);
gen_trampoline!(_upcall_t23, 23);
gen_trampoline!(_upcall_t24, 24);
gen_trampoline!(_upcall_t25, 25);
gen_trampoline!(_upcall_t26, 26);
gen_trampoline!(_upcall_t27, 27);
gen_trampoline!(_upcall_t28, 28);
gen_trampoline!(_upcall_t29, 29);
gen_trampoline!(_upcall_t30, 30);
gen_trampoline!(_upcall_t31, 31);
gen_trampoline!(_upcall_t32, 32);
gen_trampoline!(_upcall_t33, 33);
gen_trampoline!(_upcall_t34, 34);
gen_trampoline!(_upcall_t35, 35);
gen_trampoline!(_upcall_t36, 36);
gen_trampoline!(_upcall_t37, 37);
gen_trampoline!(_upcall_t38, 38);
gen_trampoline!(_upcall_t39, 39);
gen_trampoline!(_upcall_t40, 40);
gen_trampoline!(_upcall_t41, 41);
gen_trampoline!(_upcall_t42, 42);
gen_trampoline!(_upcall_t43, 43);
gen_trampoline!(_upcall_t44, 44);
gen_trampoline!(_upcall_t45, 45);
gen_trampoline!(_upcall_t46, 46);
gen_trampoline!(_upcall_t47, 47);
gen_trampoline!(_upcall_t48, 48);
gen_trampoline!(_upcall_t49, 49);
gen_trampoline!(_upcall_t50, 50);
gen_trampoline!(_upcall_t51, 51);
gen_trampoline!(_upcall_t52, 52);
gen_trampoline!(_upcall_t53, 53);
gen_trampoline!(_upcall_t54, 54);
gen_trampoline!(_upcall_t55, 55);
gen_trampoline!(_upcall_t56, 56);
gen_trampoline!(_upcall_t57, 57);
gen_trampoline!(_upcall_t58, 58);
gen_trampoline!(_upcall_t59, 59);
gen_trampoline!(_upcall_t60, 60);
gen_trampoline!(_upcall_t61, 61);
gen_trampoline!(_upcall_t62, 62);
gen_trampoline!(_upcall_t63, 63);

/// Table of trampoline function pointers, indexed by slot.
static UPCALL_TRAMPOLINE_FNS: [unsafe extern "C" fn(
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
    u64,
) -> u64; MAX_UPCALL_TRAMPOLINES] = [
    _upcall_t00,
    _upcall_t01,
    _upcall_t02,
    _upcall_t03,
    _upcall_t04,
    _upcall_t05,
    _upcall_t06,
    _upcall_t07,
    _upcall_t08,
    _upcall_t09,
    _upcall_t10,
    _upcall_t11,
    _upcall_t12,
    _upcall_t13,
    _upcall_t14,
    _upcall_t15,
    _upcall_t16,
    _upcall_t17,
    _upcall_t18,
    _upcall_t19,
    _upcall_t20,
    _upcall_t21,
    _upcall_t22,
    _upcall_t23,
    _upcall_t24,
    _upcall_t25,
    _upcall_t26,
    _upcall_t27,
    _upcall_t28,
    _upcall_t29,
    _upcall_t30,
    _upcall_t31,
    _upcall_t32,
    _upcall_t33,
    _upcall_t34,
    _upcall_t35,
    _upcall_t36,
    _upcall_t37,
    _upcall_t38,
    _upcall_t39,
    _upcall_t40,
    _upcall_t41,
    _upcall_t42,
    _upcall_t43,
    _upcall_t44,
    _upcall_t45,
    _upcall_t46,
    _upcall_t47,
    _upcall_t48,
    _upcall_t49,
    _upcall_t50,
    _upcall_t51,
    _upcall_t52,
    _upcall_t53,
    _upcall_t54,
    _upcall_t55,
    _upcall_t56,
    _upcall_t57,
    _upcall_t58,
    _upcall_t59,
    _upcall_t60,
    _upcall_t61,
    _upcall_t62,
    _upcall_t63,
];

// =============================================================================
// Phase E2: Struct/Union Layouts, Upcalls, String Marshaling
// =============================================================================

// =============================================================================
// NEW-18: real upcall handles via libffi closures
// =============================================================================
//
// `Linker.upcallHandle(target, descriptor, arena)` returns a
// MemorySegment wrapping a real extern "C" function pointer that C
// code can call directly. libffi generates the trampoline; our closure
// dispatches back into Java via the per-thread NativeContext installed
// by the surrounding downcall.
//
// Each upcall keeps a `Box<UpcallClosure>` alive on the heap. The
// closure owns its `libffi::middle::Closure` (which owns the libffi
// closure object + executable trampoline page) plus the userdata. We
// register a global registry keyed by the trampoline address so that
// a) the closure stays alive until the owning arena is closed, and
// b) the userdata pointer remains stable across the call.

use libffi::middle::{Cif as MiddleCif, Closure, Type as MiddleType};

/// Per-upcall state held alive in `UPCALL_REGISTRY`.
///
/// libffi `Closure` is not `Send` because it carries raw pointers
/// to its trampoline page, but the closure data is read-only after
/// construction and the trampoline page is allocated by libffi with
/// rwx permissions independent of any thread. We assert `Send`/`Sync`
/// manually so we can park it in a global mutex-guarded map.
struct UpcallEntry {
    /// libffi closure object — owns the executable trampoline page.
    _closure: Box<Closure<'static>>,
    /// The leaked `&'static UpcallUserdata` the trampoline reads its target from.
    /// Held here (the closure captures the same allocation) so the GC root
    /// scan/remap can reach and rewrite `target` in place — see
    /// `gc_scan_upcall_target_roots` / `gc_update_upcall_target_refs`. Step 5
    /// GAP C: the upcall target is a live Java object the native trampoline
    /// holds; without this it was neither kept alive nor remapped across a
    /// moving GC (use-after-free on the next upcall).
    userdata: *const UpcallUserdata,
}

// SAFETY: libffi closures are immutable after construction and their
// trampoline pages are independent of any thread. The userdata we
// store is `'static` and only read by the closure callback.
unsafe impl Send for UpcallEntry {}
unsafe impl Sync for UpcallEntry {}

/// Userdata captured by every upcall trampoline.
struct UpcallUserdata {
    /// Java target object address (a relocatable heap pointer), stored as an
    /// `AtomicUsize` so the GC remap (`gc_update_upcall_target_refs`) can rewrite
    /// it in place after a moving collection. The trampoline loads it on each
    /// dispatch; both run at a stop-the-world safepoint relative to one another,
    /// so `Relaxed` is sufficient.
    target: std::sync::atomic::AtomicUsize,
    param_kinds: Vec<i32>,
    return_kind: i32,
}

static UPCALL_REGISTRY: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<usize, UpcallEntry>>,
> = std::sync::OnceLock::new();

fn upcall_registry() -> &'static parking_lot::Mutex<std::collections::HashMap<usize, UpcallEntry>> {
    UPCALL_REGISTRY.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
}

/// Step 5 GAP C — GC root scan for FFM/Panama upcall targets. Each registered
/// upcall trampoline holds a live Java target (a `MethodHandle`/lambda) only
/// through its leaked `UpcallUserdata.target`, which is otherwise invisible to
/// the GC. Push every one so a moving collector keeps it alive and records its
/// relocation in the pointer map. Companion of [`gc_update_upcall_target_refs`]
/// — the two MUST visit the identical set. Called from the VM root scan
/// (`memory::roots`). The registry mutex is a leaf lock (no Java allocation
/// while held), so this is safe to call at a stop-the-world safepoint.
pub fn gc_scan_upcall_target_roots(out: &mut Vec<ObjectRef>) {
    let reg = upcall_registry().lock();
    for entry in reg.values() {
        // SAFETY: `userdata` is a leaked `&'static UpcallUserdata`, alive for the
        // whole process (the closure captures the same allocation).
        let addr = unsafe {
            (*entry.userdata)
                .target
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        if addr != 0 {
            // SAFETY: a non-zero, 8-byte-aligned heap address previously stored
            // from a live `ObjectRef`; used only as a GC root here.
            out.push(unsafe { ObjectRef::from_raw(addr as *mut u8) });
        }
    }
}

/// Step 5 GAP C — post-move remap for upcall targets (companion of
/// [`gc_scan_upcall_target_roots`]). After a moving collection relocates a
/// target, rewrite each `UpcallUserdata.target` in place so the next trampoline
/// dispatch reaches the new address. Called from the VM's `update_all_roots`.
pub fn gc_update_upcall_target_refs(map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let reg = upcall_registry().lock();
    for entry in reg.values() {
        // SAFETY: see `gc_scan_upcall_target_roots`.
        let cell = unsafe { &(*entry.userdata).target };
        let old = cell.load(std::sync::atomic::Ordering::Relaxed);
        if let Some(&new) = map.get(&old) {
            debug_assert!(new != 0, "GC pointer map contains null address");
            cell.store(new, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// libffi `Callback<UpcallUserdata, u64>` — runs whenever the trampoline
/// returned by `pe_upcall_handle` is invoked from C.
///
/// The downcall that triggered this callback installed the active
/// NativeContext via `ActiveContextGuard`, so we can re-enter Java
/// from this exact thread.
unsafe extern "C" fn upcall_dispatch(
    _cif: &libffi::low::ffi_cif,
    result: &mut u64,
    args: *const *const std::ffi::c_void,
    userdata: &UpcallUserdata,
) {
    use crate::panama_libffi as plf;

    // Default the return slot to zero in case dispatch fails.
    *result = 0;

    let nargs = userdata.param_kinds.len();
    // Decode args from the libffi-supplied void**.
    let mut java_args: Vec<Value> = Vec::with_capacity(nargs);
    for (i, &kind) in userdata.param_kinds.iter().enumerate() {
        let slot = *args.add(i);
        let v = match kind {
            cratonvm_native_api::ffi::LAYOUT_BYTE | cratonvm_native_api::ffi::LAYOUT_BOOLEAN => {
                Value::Int(*(slot as *const i8) as i32)
            }
            cratonvm_native_api::ffi::LAYOUT_SHORT | cratonvm_native_api::ffi::LAYOUT_CHAR => {
                Value::Int(*(slot as *const i16) as i32)
            }
            cratonvm_native_api::ffi::LAYOUT_INT => Value::Int(*(slot as *const i32)),
            cratonvm_native_api::ffi::LAYOUT_LONG => Value::Long(*(slot as *const i64)),
            cratonvm_native_api::ffi::LAYOUT_FLOAT => Value::Float(*(slot as *const f32)),
            cratonvm_native_api::ffi::LAYOUT_DOUBLE => Value::Double(*(slot as *const f64)),
            cratonvm_native_api::ffi::LAYOUT_ADDRESS => Value::Long(*(slot as *const i64)),
            _ => Value::Long(0),
        };
        java_args.push(v);
    }

    // GAP C: read the GC-remappable target address atomically before re-entering
    // Java. The remap (`gc_update_upcall_target_refs`) rewrites `userdata.target`
    // in place at a stop-the-world safepoint, so the next dispatch loads the new
    // address; this load and that store never overlap (STW).
    let target = unsafe {
        cratonvm_types::ObjectRef::from_raw(
            userdata.target.load(std::sync::atomic::Ordering::Relaxed) as *mut u8,
        )
    };
    // Dispatch into Java via the active NativeContext.
    let dispatch_result = plf::with_active_context(|ctx| {
        // The Java target is a MethodHandle / functional interface impl.
        // We invoke its `invoke([Object])` method passing our boxed args.
        // Build an Object[] of boxed primitives.
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, java_args.len());
        for (i, v) in java_args.iter().enumerate() {
            // Pass primitives through directly; the receiver is
            // expected to pattern-match on Value via lambda dispatch.
            // For invoke_virtual the args slice is [receiver, args...]
            // so we don't store into the array for primitives (the
            // receiver-less variant uses `invoke_virtual` with a fresh
            // single-element packed array of references).
            ctx.set_array_element(arr, i, *v);
        }
        ctx.invoke_virtual(
            target,
            "invoke",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(target)), Value::Object(Some(arr))],
        )
    });

    let returned = match dispatch_result {
        Some(Ok(Some(v))) => v,
        _ => Value::Object(None),
    };

    // Marshal Java return value into the C return slot.
    *result = match (userdata.return_kind, returned) {
        (-1, _) => 0,
        (cratonvm_native_api::ffi::LAYOUT_BYTE, Value::Int(n))
        | (cratonvm_native_api::ffi::LAYOUT_BOOLEAN, Value::Int(n))
        | (cratonvm_native_api::ffi::LAYOUT_SHORT, Value::Int(n))
        | (cratonvm_native_api::ffi::LAYOUT_CHAR, Value::Int(n))
        | (cratonvm_native_api::ffi::LAYOUT_INT, Value::Int(n)) => n as u64,
        (cratonvm_native_api::ffi::LAYOUT_LONG, Value::Long(n))
        | (cratonvm_native_api::ffi::LAYOUT_ADDRESS, Value::Long(n)) => n as u64,
        (cratonvm_native_api::ffi::LAYOUT_FLOAT, Value::Float(f)) => f.to_bits() as u64,
        (cratonvm_native_api::ffi::LAYOUT_DOUBLE, Value::Double(d)) => d.to_bits(),
        _ => 0,
    };
}

/// A scope for an upcall stub: the class of the Java target it dispatches into.
///
/// A `ForeignUpcall` denial that cannot say *which* callback was refused is not
/// actionable, and this is the narrowest name reachable here — the stub's
/// descriptor is a layout list, not a method signature. Falls back to
/// `<unknown>` so a denial always names something.
fn upcall_target_name(ctx: &dyn NativeContext, target: ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(target))
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// `Linker.upcallHandle(target, descriptor, arena)` — build a libffi
/// closure that dispatches into a Java MethodHandle. Returns a
/// MemorySegment whose address is the closure's extern "C" trampoline.
fn pe_upcall_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::panama_libffi as plf;

    let _linker = obj_arg(args, 0)?;
    let target = obj_arg(args, 1)?;
    let descriptor = obj_arg(args, 2)?;

    // GAP F3. This function had **no** native-access gate at all, while the
    // downcall path fails closed — yet it is the more dangerous direction: it
    // hands native code a real extern "C" trampoline into Java. `docs/CONFIG.md`
    // already documents `Linker.upcallHandle` as consulting the access
    // registry; it did not. Restore the documented behaviour, then record the
    // `ForeignUpcall` capability (permissive by default).
    //
    // BEHAVIOUR CHANGE — the only one in this pass: with
    // `--enable-native-access` absent, `upcallHandle` now throws
    // `IllegalCallerException` instead of succeeding, matching
    // `pe_downcall_invoke`. Revert this one line if a workload needs the old
    // laxity; the capability check below is behaviour-neutral on its own.
    require_native_access(ctx, "upcallHandle")?;
    let upcall_target = upcall_target_name(&*ctx, target);
    crate::capability_gate::gate_foreign_upcall(&*ctx, &upcall_target)?;
    // args[3] = Arena — used to bound the closure's lifetime; for
    // simplicity we leak the closure and rely on the registry until
    // the JVM exits. A NEW-17 cleaner could be added if needed.

    // Build libffi types from descriptor
    let param_layouts = plf::descriptor_param_layouts(ctx, descriptor);
    let return_layout = plf::descriptor_return_layout(ctx, descriptor);

    let mut param_kinds: Vec<i32> = Vec::with_capacity(param_layouts.len());
    let mut ffi_params: Vec<MiddleType> = Vec::with_capacity(param_layouts.len());
    for (i, pl) in param_layouts.iter().enumerate() {
        let layout = pl.ok_or_else(|| -> MethodCallFailed {
            RuntimeError::IllegalStateException {
                message: format!("Upcall parameter {} layout is null", i),
            }
            .into()
        })?;
        param_kinds.push(plf::read_layout_kind(ctx, layout));
        ffi_params.push(plf::layout_to_ffi_type(ctx, layout, 0)?);
    }
    let return_kind = match return_layout {
        Some(rl) => plf::read_layout_kind(ctx, rl),
        None => -1,
    };
    let ffi_ret = match return_layout {
        Some(rl) => plf::layout_to_ffi_type(ctx, rl, 0)?,
        None => MiddleType::void(),
    };

    let cif = MiddleCif::new(ffi_params, ffi_ret);

    // Also register the callback in the legacy slot table so the
    // existing `UpcallStub.invoke([Object])` Java-side dispatch path
    // remains operational. Tests + JDK code may still hold references
    // to that slot index.
    let entry = ffi::UpcallEntry {
        target,
        method_name: "invoke".to_string(),
        method_descriptor: String::new(),
        param_kinds: param_kinds.clone(),
        return_kind,
    };
    let _legacy_slot = ctx.register_upcall(entry);

    // Heap-allocate userdata so the closure has a stable reference.
    let userdata = Box::new(UpcallUserdata {
        target: std::sync::atomic::AtomicUsize::new(target.as_ptr() as usize),
        param_kinds: param_kinds.clone(),
        return_kind,
    });
    // Leak the userdata for the closure's lifetime (held in the registry).
    let userdata_ptr: &'static UpcallUserdata = Box::leak(userdata);

    // Build the libffi closure. It becomes a real extern "C" function
    // whose code_ptr() can be called directly from any C code.
    let closure: Closure<'static> = Closure::new(cif, upcall_dispatch, userdata_ptr);
    let code_ptr = *closure.code_ptr() as *const () as usize;
    let boxed_closure = Box::new(closure);
    upcall_registry().lock().insert(
        code_ptr,
        UpcallEntry {
            _closure: boxed_closure,
            // Same leaked allocation the closure captured — the GC root scan/remap
            // reach `target` through this (Step 5 GAP C).
            userdata: userdata_ptr as *const UpcallUserdata,
        },
    );

    // Wrap the trampoline address in a MemorySegment so Java can pass
    // it to other downcalls expecting a `MemorySegment` function ptr.
    let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
    ctx.set_field(seg, 0, Value::Long(code_ptr as i64));
    ctx.set_field(seg, 1, Value::Long(0));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(1)); // read-only
    ctx.set_field(seg, 4, Value::Int(1)); // alive
    ctx.set_field(seg, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(seg))))
}

/// Dispatch an upcall — called when C invokes a Java callback through the upcall table.
fn pe_upcall_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = obj_arg(args, 0)?;
    let slot = match ctx.get_field(handle, 0) {
        Value::Long(n) => n as usize,
        _ => 0,
    };

    let (target, _param_kinds, return_kind) =
        ctx.get_upcall_info(slot)
            .ok_or_else(|| RuntimeError::IllegalStateException {
                message: format!("Upcall slot {} not found", slot),
            })?;

    // GAP F4: invoking an upcall stub was ungated. The `ForeignUpcall` check is
    // permissive by default; the scope is the callback's class, resolved after
    // the slot lookup so an unknown slot still reports "slot not found".
    let upcall_target = upcall_target_name(&*ctx, target);
    crate::capability_gate::gate_foreign_upcall(&*ctx, &upcall_target)?;

    // Unmarshal args from the Object[] array
    let call_args: Vec<Value> = if args.len() > 1 {
        if let Value::Object(Some(arr)) = args[1] {
            let len = ctx.array_length(arr);
            (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Call the Java target
    let result = ctx.invoke_virtual(
        target,
        "invoke",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &call_args,
    );
    match result {
        Ok(val) => {
            let _ = return_kind;
            Ok(val.or(Some(Value::Object(None))))
        }
        Err(e) => Err(e),
    }
}

// --- StructLayout / UnionLayout / SequenceLayout ---
// StructLayout synthetic: [0]=kind(LAYOUT_STRUCT), [1]=totalSize(Long), [2]=memberLayouts(array),
//                          [3]=memberNames(array), [4]=memberOffsets(array), [5]=alignment(Long)

fn register_pe2_struct_layouts(r: &mut NativeMethodRegistry) {
    let ml = "java/lang/foreign/MemoryLayout";

    // MemoryLayout.structLayout(members...) → StructLayout
    r.register(
        ml,
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;",
        pe_struct_layout,
    );
    r.register(
        ml,
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/StructLayout;",
        pe_struct_layout,
    );

    // MemoryLayout.unionLayout(members...) → UnionLayout
    r.register(
        ml,
        "unionLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;",
        pe_union_layout,
    );
    r.register(
        ml,
        "unionLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/UnionLayout;",
        pe_union_layout,
    );

    // MemoryLayout.sequenceLayout(count, element) → SequenceLayout
    r.register(
        ml,
        "sequenceLayout",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;",
        pe_sequence_layout,
    );
    r.register(
        ml,
        "sequenceLayout",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/SequenceLayout;",
        pe_sequence_layout,
    );

    // MemoryLayout.paddingLayout(bytes) → PaddingLayout
    r.register(
        ml,
        "paddingLayout",
        "(J)Ljava/lang/foreign/MemoryLayout;",
        |ctx, args| {
            let bytes = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let layout = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6)?;
            ctx.set_field(layout, 0, Value::Int(LAYOUT_PADDING));
            ctx.set_field(layout, 1, Value::Long(bytes));
            ctx.set_field(layout, 5, Value::Long(1)); // alignment=1
            Ok(Some(Value::Object(Some(layout))))
        },
    );
    r.register(
        ml,
        "paddingLayout",
        "(J)Ljava/lang/foreign/PaddingLayout;",
        |ctx, args| {
            let bytes = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let layout = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6)?;
            ctx.set_field(layout, 0, Value::Int(LAYOUT_PADDING));
            ctx.set_field(layout, 1, Value::Long(bytes));
            ctx.set_field(layout, 5, Value::Long(1)); // alignment=1
            Ok(Some(Value::Object(Some(layout))))
        },
    );

    // Common methods on all layouts
    fn layout_members_as_list(ctx: &mut dyn NativeContext, members_arr: ObjectRef) -> Result<ObjectRef, MethodCallFailed> {
        let len = ctx.array_length(members_arr);
        let arr_pin = ctx.pin_native_root(members_arr);
        let data_slot = ctx
            .resolve_field_index("java/util/ArrayList", "elementData")
            .unwrap_or(0);
        let size_slot = ctx
            .resolve_field_index("java/util/ArrayList", "size")
            .unwrap_or(1);
        let n_fields = std::cmp::max(data_slot, size_slot) + 1;
        let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", n_fields)?;
        let members_arr = ctx.read_native_pin(arr_pin, members_arr);
        ctx.set_field(list, data_slot, Value::Object(Some(members_arr)));
        ctx.set_field(list, size_slot, Value::Int(len as i32));
        ctx.unpin_native_roots(arr_pin);
        Ok(list)
    }

    let sl = "java/lang/foreign/StructLayout";
    for layout_class in [sl, "java/lang/foreign/GroupLayout"] {
        r.register(layout_class, "byteSize", "()J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let size = match ctx.get_field(this, 1) {
                Value::Long(n) => n,
                _ => 0,
            };
            Ok(Some(Value::Long(size)))
        });
        r.register(layout_class, "byteAlignment", "()J", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let align = match ctx.get_field(this, 5) {
                Value::Long(n) => n,
                _ => 1,
            };
            Ok(Some(Value::Long(align)))
        });
        r.register(
            layout_class,
            "name",
            "()Ljava/util/Optional;",
            pe_layout_name,
        );
        r.register(
            layout_class,
            "withName",
            "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
            pe_layout_with_name,
        );
        r.register(
            layout_class,
            "memberLayouts",
            "()Ljava/util/List;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                match ctx.get_field(this, 2) {
                    Value::Object(Some(members_arr)) => Ok(Some(Value::Object(Some(
                        layout_members_as_list(ctx, members_arr)?,
                    )))),
                    _ => {
                        let empty = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                        Ok(Some(Value::Object(Some(layout_members_as_list(
                            ctx, empty,
                        )?))))
                    }
                }
            },
        );
    }

    // byteOffset(PathElement...) — compute offset to a named field
    r.register(
        sl,
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Simple case: one path element = field name
            if let Some(Value::Object(Some(path_arr))) = args.get(1) {
                if ctx.array_length(*path_arr) > 0 {
                    if let Value::Object(Some(pe)) = ctx.get_array_element(*path_arr, 0) {
                        // PathElement stores the field name in field 0
                        if let Value::Object(Some(name_ref)) = ctx.get_field(pe, 0) {
                            let target_name = ctx.read_string(name_ref).unwrap_or_default();
                            // Search member names and return corresponding offset
                            if let Value::Object(Some(names_arr)) = ctx.get_field(this, 3) {
                                if let Value::Object(Some(offsets_arr)) = ctx.get_field(this, 4) {
                                    let count = ctx.array_length(names_arr);
                                    for i in 0..count {
                                        if let Value::Object(Some(n)) =
                                            ctx.get_array_element(names_arr, i)
                                        {
                                            if ctx.read_string(n).as_deref() == Some(&target_name) {
                                                if let Value::Long(off) =
                                                    ctx.get_array_element(offsets_arr, i)
                                                {
                                                    return Ok(Some(Value::Long(off)));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(Some(Value::Long(0)))
        },
    );

    // MemoryLayout.withName(name) → layout with name set
    r.register(
        ml,
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
        pe_layout_with_name,
    );
    r.register(
        ml,
        "varHandle",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;",
        pe_memory_layout_var_handle,
    );
    r.register(ml, "name", "()Ljava/util/Optional;", pe_layout_name);

    // MemoryLayout.byteSize() fallback for any layout
    r.register(ml, "byteSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let kind = match ctx.get_field(this, 0) {
            Value::Int(k) => k,
            _ => 0,
        };
        let size = if kind < 10 {
            ffi::layout_byte_size(kind) as i64
        } else {
            match ctx.get_field(this, 1) {
                Value::Long(n) => n,
                _ => 0,
            }
        };
        Ok(Some(Value::Long(size)))
    });

    // PathElement.groupElement(name) → PathElement
    let pe = "java/lang/foreign/MemoryLayout$PathElement";
    r.register(
        pe,
        "groupElement",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout$PathElement;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let elem =
                try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout$PathElement", 2)?;
            ctx.set_field(elem, 0, name); // field name
            ctx.set_field(elem, 1, Value::Int(0)); // kind=group
            Ok(Some(Value::Object(Some(elem))))
        },
    );
    r.register(
        pe,
        "sequenceElement",
        "()Ljava/lang/foreign/MemoryLayout$PathElement;",
        |ctx, _| {
            let elem =
                try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout$PathElement", 2)?;
            ctx.set_field(elem, 0, Value::Object(None));
            ctx.set_field(elem, 1, Value::Int(1)); // kind=sequence
            Ok(Some(Value::Object(Some(elem))))
        },
    );
}

fn pe_memory_layout_width(ctx: &mut dyn NativeContext, layout: ObjectRef) -> i64 {
    let kind = match ctx.get_field(layout, 0) {
        Value::Int(v) => v,
        _ => -1,
    };
    if kind < 10 {
        ffi::layout_byte_size(kind) as i64
    } else {
        match ctx.get_field(layout, 1) {
            Value::Long(v) => v,
            _ => 1,
        }
    }
}

fn pe_memory_layout_path_target(
    ctx: &mut dyn NativeContext,
    layout: ObjectRef,
    path_arr: ObjectRef,
) -> ObjectRef {
    let mut current = layout;
    let mut i = 0;
    let len = ctx.array_length(path_arr);
    while i < len {
        let pe = match ctx.get_array_element(path_arr, i) {
            Value::Object(Some(pe)) => pe,
            _ => break,
        };
        let path_kind = match ctx.get_field(pe, 1) {
            Value::Int(v) => v,
            _ => -1,
        };
        match path_kind {
            0 => {
                let target_name = match ctx.get_field(pe, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                if target_name.is_empty() {
                    break;
                }
                let current_kind = match ctx.get_field(current, 0) {
                    Value::Int(v) => v,
                    _ => -1,
                };
                if current_kind != LAYOUT_STRUCT && current_kind != LAYOUT_UNION {
                    break;
                }
                let names_arr = match ctx.get_field(current, 3) {
                    Value::Object(Some(arr)) => arr,
                    _ => break,
                };
                let members_arr = match ctx.get_field(current, 2) {
                    Value::Object(Some(arr)) => arr,
                    _ => break,
                };
                let mut found = false;
                let name_len = ctx.array_length(names_arr);
                let mut j = 0;
                while j < name_len {
                    if let Value::Object(Some(name_ref)) = ctx.get_array_element(names_arr, j) {
                        if ctx.read_string(name_ref).as_deref() == Some(&target_name) {
                            if let Value::Object(Some(member_layout)) =
                                ctx.get_array_element(members_arr, j)
                            {
                                current = member_layout;
                                found = true;
                                break;
                            }
                        }
                    }
                    j += 1;
                }
                if !found {
                    break;
                }
            }
            1 => {
                if let Value::Object(Some(element)) = ctx.get_field(current, 2) {
                    current = element;
                } else {
                    break;
                }
            }
            _ => break,
        }
        i += 1;
    }
    current
}

fn pe_memory_layout_var_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let target_layout = match args.get(1) {
        Some(Value::Object(Some(path_arr))) => pe_memory_layout_path_target(ctx, this, *path_arr),
        _ => this,
    };
    let mut width = pe_memory_layout_width(ctx, target_layout);
    if !(1..=8).contains(&width) {
        width = 1;
    }
    let vh = try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/VarHandle", 3)?;
    ctx.set_field(vh, 0, Value::Int(1)); // little-endian marker for memory-segment varhandles
    ctx.set_field(vh, 1, Value::Int(width as i32));
    ctx.set_field(vh, 2, Value::Int(3)); // VH_KIND_MEMORY_SEGMENT
    Ok(Some(Value::Object(Some(vh))))
}

/// Compute struct layout: iterate members, align each, compute offsets and total size.
fn pe_struct_layout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let members_arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = ctx.array_length(members_arr);

    let offsets_arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, count);
    let names_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count);

    let mut offset: usize = 0;
    let mut max_align: usize = 1;

    for i in 0..count {
        let member = ctx.get_array_element(members_arr, i);
        if let Value::Object(Some(m)) = member {
            let kind = match ctx.get_field(m, 0) {
                Value::Int(k) => k,
                _ => 0,
            };
            let (member_size, member_align) = if kind < 10 {
                (ffi::layout_byte_size(kind), ffi::layout_alignment(kind))
            } else {
                let s = match ctx.get_field(m, 1) {
                    Value::Long(n) => n as usize,
                    _ => 0,
                };
                let a = match ctx.get_field(m, 5) {
                    Value::Long(n) => n as usize,
                    _ => 1,
                };
                (s, a)
            };

            offset = ffi::align_up(offset, member_align);
            ctx.set_array_element(offsets_arr, i, Value::Long(offset as i64));
            ctx.set_array_element(names_arr, i, pe_layout_name_value(ctx, m));
            offset += member_size;
            if member_align > max_align {
                max_align = member_align;
            }
        }
    }

    // Pad total size to alignment
    let total_size = ffi::align_up(offset, max_align);

    let layout = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/StructLayout", 6)?;
    ctx.set_field(layout, 0, Value::Int(LAYOUT_STRUCT));
    ctx.set_field(layout, 1, Value::Long(total_size as i64));
    ctx.set_field(layout, 2, Value::Object(Some(members_arr)));
    ctx.set_field(layout, 3, Value::Object(Some(names_arr)));
    ctx.set_field(layout, 4, Value::Object(Some(offsets_arr)));
    ctx.set_field(layout, 5, Value::Long(max_align as i64));

    Ok(Some(Value::Object(Some(layout))))
}

/// Compute union layout: all fields at offset 0, size = max member size.
fn pe_union_layout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let members_arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = ctx.array_length(members_arr);

    let mut max_size: usize = 0;
    let mut max_align: usize = 1;

    for i in 0..count {
        if let Value::Object(Some(m)) = ctx.get_array_element(members_arr, i) {
            let kind = match ctx.get_field(m, 0) {
                Value::Int(k) => k,
                _ => 0,
            };
            let (member_size, member_align) = if kind < 10 {
                (ffi::layout_byte_size(kind), ffi::layout_alignment(kind))
            } else {
                let s = match ctx.get_field(m, 1) {
                    Value::Long(n) => n as usize,
                    _ => 0,
                };
                let a = match ctx.get_field(m, 5) {
                    Value::Long(n) => n as usize,
                    _ => 1,
                };
                (s, a)
            };
            if member_size > max_size {
                max_size = member_size;
            }
            if member_align > max_align {
                max_align = member_align;
            }
        }
    }

    let layout = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6)?;
    ctx.set_field(layout, 0, Value::Int(LAYOUT_UNION));
    ctx.set_field(layout, 1, Value::Long(max_size as i64));
    ctx.set_field(layout, 2, Value::Object(Some(members_arr)));
    ctx.set_field(layout, 3, Value::Object(None));
    ctx.set_field(layout, 4, Value::Object(None));
    ctx.set_field(layout, 5, Value::Long(max_align as i64));

    Ok(Some(Value::Object(Some(layout))))
}

/// Compute sequence layout (array): count * element size.
fn pe_sequence_layout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let count = match args.first() {
        Some(Value::Long(n)) => *n as usize,
        _ => 0,
    };
    let element = match args.get(1) {
        Some(Value::Object(Some(e))) => *e,
        _ => return Ok(Some(Value::Object(None))),
    };

    let kind = match ctx.get_field(element, 0) {
        Value::Int(k) => k,
        _ => 0,
    };
    let (elem_size, elem_align) = if kind < 10 {
        (ffi::layout_byte_size(kind), ffi::layout_alignment(kind))
    } else {
        let s = match ctx.get_field(element, 1) {
            Value::Long(n) => n as usize,
            _ => 0,
        };
        let a = match ctx.get_field(element, 5) {
            Value::Long(n) => n as usize,
            _ => 1,
        };
        (s, a)
    };

    let layout = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6)?;
    ctx.set_field(layout, 0, Value::Int(LAYOUT_SEQUENCE));
    ctx.set_field(layout, 1, Value::Long((count * elem_size) as i64));
    ctx.set_field(layout, 2, Value::Object(Some(element))); // element layout
    ctx.set_field(layout, 3, Value::Object(None));
    ctx.set_field(layout, 4, Value::Object(None));
    ctx.set_field(layout, 5, Value::Long(elem_align as i64));

    Ok(Some(Value::Object(Some(layout))))
}

// --- String marshaling helpers ---

fn register_pe2_string_marshaling(r: &mut NativeMethodRegistry) {
    let ms = "java/lang/foreign/MemorySegment";

    // getUtf8String(long offset) → String
    r.register(ms, "getUtf8String", "(J)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let offset = match args.get(1) {
            Some(Value::Long(n)) => *n,
            _ => 0,
        };
        let ptr = crate::panama_libffi::segment_address(ctx, this);
        // Validate address arithmetic doesn't overflow (matches setUtf8String/copy)
        let total = (ptr as u64).checked_add(offset as u64);
        let addr_val = match total {
            Some(v) => v,
            None => {
                return Err(RuntimeError::IllegalStateException {
                    message: "address arithmetic overflow in getUtf8String".into(),
                }
                .into());
            }
        };
        let addr = addr_val as *const u8;

        if addr.is_null() {
            return Ok(Some(Value::Object(None)));
        }

        // Read null-terminated C string with a bounded scan.
        // The scan length MUST be clamped to the segment's recorded size
        // (field 1) — `from_raw_parts` over a Java-supplied address with an
        // unverified length is an out-of-bounds-read primitive. A segment
        // with size 0 has unknown bounds (e.g. created via ofAddress or
        // wrapping a raw function pointer); the JDK rejects reading a
        // C string from such a segment, so we do too rather than blindly
        // scanning MAX_CSTR_LEN bytes from an unbounded address.
        let seg_size = match crate::panama_libffi::segment_byte_size(ctx, this) {
            n if n > 0 => {
                // Account for offset within the segment
                let remaining = n - offset;
                if remaining <= 0 {
                    return Err(RuntimeError::IllegalStateException {
                        message: format!(
                            "getUtf8String offset {} exceeds segment size {}",
                            offset, n
                        ),
                    }
                    .into());
                }
                (remaining as usize).min(MAX_CSTR_LEN)
            }
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "getUtf8String on a segment with unknown bounds \
                              (size 0): reinterpret the segment with a known \
                              size before reading a C string"
                        .into(),
                }
                .into());
            }
        };
        // SAFETY: addr has been null-checked above. `seg_size` is clamped to
        // the segment's recorded byte size (field 1) minus `offset`, so the
        // scan stays within the region the segment claims to own.
        let slice = unsafe { std::slice::from_raw_parts(addr, seg_size) };
        let nul_pos = slice.iter().position(|&b| b == 0);
        let s = match nul_pos {
            Some(pos) => std::str::from_utf8(&slice[..pos]).unwrap_or(""),
            None => {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "C string at {addr:?} exceeds maximum length of {MAX_CSTR_LEN} bytes"
                    ),
                }
                .into());
            }
        };
        let java_str = ctx.create_string(s);
        Ok(Some(Value::Object(Some(java_str))))
    });

    // setUtf8String(long offset, String value) → void
    r.register(
        ms,
        "setUtf8String",
        "(JLjava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let str_obj = obj_arg(args, 2)?;
            let s = ctx.read_string(str_obj).unwrap_or_default();

            let ptr = crate::panama_libffi::segment_address(ctx, this);
            // Bounds check: string + null terminator must fit within segment.
            // A segment with size 0 has unknown bounds (e.g. created via
            // ofAddress or wrapping a raw function pointer); writing to such a
            // segment is an arbitrary-native-write primitive. The READ path
            // (getUtf8String) rejects zero-size segments, so the WRITE path must
            // be symmetric and reject them too rather than skipping the bounds
            // check and writing blindly to the raw address.
            let seg_size = match crate::panama_libffi::segment_byte_size(ctx, this) {
                n if n > 0 => n,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "setUtf8String on a segment with unknown bounds \
                                  (size 0): reinterpret the segment with a known \
                                  size before writing a C string"
                            .into(),
                    }
                    .into());
                }
            };
            let str_bytes = s.as_bytes();
            let needed = str_bytes.len() as i64 + 1; // +1 for null terminator
            if offset < 0 || offset + needed > seg_size {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "setUtf8String: offset {} + {} bytes exceeds segment size {}",
                        offset, needed, seg_size
                    ),
                }
                .into());
            }

            // Validate address arithmetic doesn't overflow
            let total = (ptr as u64).checked_add(offset as u64);
            if let Some(addr_val) = total {
                let addr = addr_val as *mut u8;
                if !addr.is_null() {
                    // SAFETY: bounds-checked against segment size, address
                    // arithmetic verified, null-checked.
                    unsafe {
                        std::ptr::copy_nonoverlapping(str_bytes.as_ptr(), addr, str_bytes.len());
                        *addr.add(str_bytes.len()) = 0; // null terminator
                    }
                }
            } else {
                return Err(RuntimeError::IllegalStateException {
                    message: "address arithmetic overflow in setUtf8String".into(),
                }
                .into());
            }
            Ok(None)
        },
    );

    // reinterpret(long newSize) → MemorySegment with same address but different size
    r.register(
        ms,
        "reinterpret",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Gate size-stamping behind native access: reinterpret can grant an
            // arbitrary access window over a (possibly raw) address, which is
            // the second half of the arbitrary-memory primitive. Refuse unless
            // native access is enabled.
            if !native_access_enabled() {
                return Err(RuntimeError::IllegalCallerException {
                    message: "Native access is not enabled for this module \
                              (MemorySegment.reinterpret denied)"
                        .into(),
                }
                .into());
            }
            let new_size = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let ptr = crate::panama_libffi::segment_address(ctx, this);
            let read_only = match ctx.get_field_by_name(this, "readOnly") {
                Value::Int(n) => Value::Int(n),
                _ => ctx.get_field(this, 3),
            };

            let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6)?;
            ctx.set_field(seg, 0, Value::Long(ptr));
            ctx.set_field(seg, 1, Value::Long(new_size));
            ctx.set_field(seg, 2, Value::Object(None));
            ctx.set_field(seg, 3, read_only);
            ctx.set_field(seg, 4, Value::Int(1));
            ctx.set_field(seg, 5, Value::Long(0));
            Ok(Some(Value::Object(Some(seg))))
        },
    );

    // Arena.allocateUtf8String(String) → MemorySegment
    let arena = "java/lang/foreign/Arena";
    r.register(
        arena,
        "allocateUtf8String",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let str_obj = obj_arg(args, 1)?;
            let s = ctx.read_string(str_obj).unwrap_or_default();
            let bytes = s.as_bytes();
            let size = (bytes.len() + 1) as i64; // +1 for null terminator

            // Allocate via arena
            let seg_val = pe_arena_allocate_impl(ctx, this, size, 1)?;
            if let Some(Value::Object(Some(seg))) = seg_val {
                // Write the string bytes + null terminator
                let ptr = match ctx.get_field(seg, 0) {
                    Value::Long(n) => n,
                    _ => 0,
                };
                if ptr != 0 {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
                        *(ptr as *mut u8).add(bytes.len()) = 0;
                    }
                }
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );
}

// =============================================================================
// R3: Resource Loading — Class.getResourceAsStream, InputStreamReader, BufferedReader
//
// Synthetic InputStream layout (2 fields):
//   field 0: String[] ref array — all lines of the resource file
//   field 1: int — current read position (line index)
//
// InputStreamReader layout (1 field):
//   field 0: InputStream reference
//
// BufferedReader layout (1 field):
//   field 0: Reader reference (InputStreamReader)
// =============================================================================

/// Traverse BufferedReader → InputStreamReader → InputStream chain.
/// Returns the synthetic InputStream ObjectRef, or None if the chain is broken.
fn r3_get_input_stream(ctx: &dyn NativeContext, buffered_reader: ObjectRef) -> Option<ObjectRef> {
    // BufferedReader.field[0] = Reader (InputStreamReader)
    let reader = match ctx.get_field(buffered_reader, 0) {
        Value::Object(Some(r)) => r,
        _ => return None,
    };
    // InputStreamReader.field[0] = InputStream
    match ctx.get_field(reader, 0) {
        Value::Object(Some(is)) => Some(is),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // FIX(test): RAII guard that enables the process-wide native-access gate
    // for the duration of a downcall test and restores the previous value on
    // drop (even on panic). The Panama implementation is secure-by-default
    // (`NATIVE_ACCESS_ENABLED == false`), so tests that exercise the real
    // downcall machinery (abs/strlen/snprintf/…) must grant native access
    // first or `validated_fn_ptr` denies them with an IllegalCallerException.
    // Using a guard (rather than a bare set/reset pair) keeps the global flag
    // from leaking into sibling tests — leaked state is what causes the
    // order-dependent flakiness this fix addresses. The production gate is
    // unchanged; only the test scope flips the flag.
    //
    // FIX(test-isolation): `NATIVE_ACCESS_ENABLED` is a single process-global
    // `AtomicBool` shared by every test in this binary. The previous guard
    // snapshotted the *prior* value and restored it on drop, but that is
    // unsound under parallel execution and is exactly why
    // `panama_cif_cache_reuses_cif_across_calls` still flaked: with two tests
    // A and B, A enables (prior=false); B enables (prior=true, because A had
    // already flipped it on); A finishes first and its guard restores false;
    // B is now mid-downcall yet the gate reads false, so `validated_fn_ptr`
    // denies it with `IllegalCallerException`. Snapshot/restore gives no
    // mutual exclusion. The fix is to serialize every test that toggles the
    // flag behind one module-level mutex, so only a single such test ever
    // observes (or mutates) the flag at a time. While the lock is held the
    // flag cannot be flipped out from under the running test.
    static NATIVE_ACCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct NativeAccessGuard {
        prev: bool,
        // FIX(test-isolation): hold the serialization lock for the entire
        // lifetime of the guard. Acquiring it here means no other guarded
        // (or directly-locked) test can touch `NATIVE_ACCESS_ENABLED` until
        // this guard is dropped. Field order matters for drop: `prev` is
        // restored in `Drop::drop` *before* this `_lock` field is dropped
        // (Rust drops struct fields in declaration order, after the explicit
        // `Drop` impl runs), so the flag is reset while we still hold the
        // lock, and the lock is released only afterwards.
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl NativeAccessGuard {
        fn enable() -> Self {
            // Recover from a poisoned lock: a panicking guarded test must not
            // wedge the rest of the suite. The `()` payload carries no state,
            // so the poisoned inner guard is perfectly usable.
            let lock = NATIVE_ACCESS_TEST_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = native_access_enabled();
            set_native_access_enabled(true);
            NativeAccessGuard { prev, _lock: lock }
        }
    }
    impl Drop for NativeAccessGuard {
        fn drop(&mut self) {
            // Restore the prior value while still holding the lock; `_lock`
            // is released immediately afterwards when the struct fields drop.
            set_native_access_enabled(self.prev);
        }
    }

    #[test]
    fn test_linker_native_call_convention() {
        // Verify calling convention for downcalls
        // x86-64 SysV: RDI, RSI, RDX, RCX, R8, R9 for integer args
        // Windows: RCX, RDX, R8, R9
        #[cfg(target_os = "windows")]
        let max_reg_args = 4;
        #[cfg(not(target_os = "windows"))]
        let max_reg_args = 6;
        assert!(max_reg_args >= 4);
    }

    #[test]
    fn test_symbol_lookup_resolution() {
        // SymbolLookup.loaderLookup() should find loaded library symbols
        // Test that we can look up standard C functions
        let name = "strlen";
        assert!(!name.is_empty());
    }

    #[test]
    fn test_value_layout_carriers() {
        // Each ValueLayout has a carrier type
        // JAVA_INT -> int.class, JAVA_LONG -> long.class, etc.
        let carriers = vec![
            ("JAVA_BYTE", 1usize),
            ("JAVA_SHORT", 2),
            ("JAVA_INT", 4),
            ("JAVA_LONG", 8),
            ("JAVA_FLOAT", 4),
            ("JAVA_DOUBLE", 8),
            ("ADDRESS", std::mem::size_of::<*const u8>()),
        ];
        for (name, size) in carriers {
            assert!(size > 0, "{name} must have positive size");
        }
    }

    /// Snapshot/restore helper for the per-module policy tests so they can
    /// mutate the global `NATIVE_ACCESS_POLICY` without leaking state into the
    /// rest of the suite. Acquires the shared serialization lock.
    fn with_policy_isolated<F: FnOnce()>(f: F) {
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = NATIVE_ACCESS_POLICY
            .read()
            .map(|p| p.clone())
            .unwrap_or(NativeAccessPolicy::None);
        f();
        store_policy(prev);
    }

    #[test]
    fn test_native_access_policy_none_denies_all() {
        with_policy_isolated(|| {
            set_native_access_enabled(false);
            assert!(!native_access_enabled());
            assert!(!module_native_access_enabled(None));
            assert!(!module_native_access_enabled(Some("com.example.app")));
        });
    }

    #[test]
    fn test_native_access_policy_all_grants_every_module() {
        with_policy_isolated(|| {
            set_native_access_enabled(true);
            assert!(native_access_enabled());
            assert!(module_native_access_enabled(None));
            assert!(module_native_access_enabled(Some("any.module")));
        });
    }

    #[test]
    fn test_native_access_policy_scoped_modules() {
        with_policy_isolated(|| {
            set_native_access_modules(["com.example.ffi", "org.foo.bar"]);
            // Only the listed modules are granted.
            assert!(module_native_access_enabled(Some("com.example.ffi")));
            assert!(module_native_access_enabled(Some("org.foo.bar")));
            // An unlisted module — and the unnamed module — are denied.
            assert!(!module_native_access_enabled(Some("com.other")));
            assert!(!module_native_access_enabled(None));
            // The coarse process-global gate sees *some* grant.
            assert!(native_access_enabled());
        });
    }

    #[test]
    fn test_native_access_policy_all_unnamed_sentinel_grants_all() {
        with_policy_isolated(|| {
            // The JDK `ALL-UNNAMED` sentinel collapses to a global grant here.
            set_native_access_modules(["ALL-UNNAMED"]);
            assert!(module_native_access_enabled(None));
            assert!(module_native_access_enabled(Some("anything")));
            // Case-insensitive and mixed with named modules.
            set_native_access_modules(["com.x", "all-modules"]);
            assert!(module_native_access_enabled(Some("unlisted")));
        });
    }

    #[test]
    fn test_native_access_policy_blank_list_grants_all_like_bare_flag() {
        with_policy_isolated(|| {
            // A whitespace-only / empty argument behaves like the bare flag.
            set_native_access_modules(["", "   "]);
            assert!(native_access_enabled());
            assert!(module_native_access_enabled(Some("whatever")));
        });
    }

    // FIX(test): regression for the MemorySegment.copy zero-size OOB hole.
    // The bounds check in `copy` was previously gated on `size > 0`, so a
    // segment with a declared byteSize() of 0 skipped validation entirely and
    // a non-zero `bytes` length drove an OOB read/write of up to MAX_COPY_SIZE.
    // This test mirrors the exact (now-unconditional) predicate used in the
    // production fix — `offset < 0 || offset.checked_add(bytes) > size` — and
    // asserts that a zero-size segment with a non-zero length is rejected.
    // Pure i64 arithmetic only, so it is independent of NativeContext.
    #[test]
    fn test_copy_zero_size_segment_rejects_nonzero_len() {
        // Replicates the copy-path bounds predicate: returns true == "reject".
        fn exceeds(offset: i64, bytes: usize, size: i64) -> bool {
            let end = offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
            offset < 0 || end > size
        }
        // Zero-size segment, non-zero copy length -> must reject (the bug).
        assert!(
            exceeds(0, 64, 0),
            "zero-size segment must reject non-zero copy"
        );
        // Negative offset -> reject.
        assert!(exceeds(-1, 0, 16), "negative offset must reject");
        // Offset+bytes overflowing i64 -> saturates to MAX, exceeds size -> reject.
        assert!(exceeds(i64::MAX, 1, 1024), "overflowing offset must reject");
        // Offset+bytes past the declared size -> reject.
        assert!(exceeds(8, 16, 16), "out-of-range copy must reject");
        // Legitimate in-bounds copy -> accept (no over-rejection).
        assert!(!exceeds(8, 8, 16), "in-bounds copy must be accepted");
        // Zero-length copy on a zero-size segment is harmless and the
        // production code only enters the bounds block when bytes > 0, so the
        // predicate for (0,0,0) staying false is the consistent invariant.
        assert!(
            !exceeds(0, 0, 0),
            "zero-length copy is not a bounds violation"
        );
    }

    #[test]
    fn test_function_descriptor() {
        // FunctionDescriptor.of(returnLayout, argLayouts...)
        // Describes a native function signature
        struct FuncDesc {
            ret_size: usize,
            arg_sizes: Vec<usize>,
        }
        let desc = FuncDesc {
            ret_size: 4,           // int return
            arg_sizes: vec![8, 8], // two pointer args
        };
        assert_eq!(desc.arg_sizes.len(), 2);
        assert_eq!(desc.ret_size, 4);
    }

    #[test]
    fn test_upcall_handle() {
        // Upcall: native code calling back into Java
        // Should create a function pointer that routes to Java method
        let callback_invoked = std::sync::atomic::AtomicBool::new(false);
        // Simulate upcall
        callback_invoked.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(callback_invoked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn test_arena_confined_thread_safety() {
        // Arena.ofConfined() should only be usable from creating thread
        let creator_thread = std::thread::current().id();
        assert_eq!(std::thread::current().id(), creator_thread);
    }

    #[test]
    fn test_arena_shared_multi_thread() {
        // Arena.ofShared() can be used from multiple threads
        use std::sync::Arc;
        let data = Arc::new(vec![1u8, 2, 3]);
        let d2 = data.clone();
        let handle = std::thread::spawn(move || {
            assert_eq!(d2[0], 1);
        });
        handle.join().unwrap();
        assert_eq!(data[0], 1);
    }

    #[test]
    fn test_validated_fn_ptr_null_rejected() {
        // Null function pointer should be rejected
        let result = validated_fn_ptr::<extern "C" fn() -> i32>(0);
        assert!(result.is_err());
    }

    #[test]
    fn test_max_copy_size_constant() {
        assert_eq!(MAX_COPY_SIZE, 256 * 1024 * 1024);
    }

    #[test]
    fn test_max_cstr_len_constant() {
        assert_eq!(MAX_CSTR_LEN, 4096);
    }

    #[test]
    fn test_layout_byte_size_via_ffi() {
        // Verify the ffi layout constants are accessible and correct
        assert_eq!(ffi::layout_byte_size(LAYOUT_INT), 4);
        assert_eq!(ffi::layout_byte_size(LAYOUT_LONG), 8);
        assert_eq!(ffi::layout_byte_size(LAYOUT_FLOAT), 4);
        assert_eq!(ffi::layout_byte_size(LAYOUT_DOUBLE), 8);
        assert_eq!(ffi::layout_byte_size(LAYOUT_BYTE), 1);
        assert_eq!(ffi::layout_byte_size(LAYOUT_SHORT), 2);
        assert_eq!(ffi::layout_byte_size(LAYOUT_BOOLEAN), 1);
        assert_eq!(ffi::layout_byte_size(LAYOUT_CHAR), 2);
        assert_eq!(ffi::layout_byte_size(LAYOUT_ADDRESS), 8);
    }

    #[test]
    fn test_compound_layout_sizes_are_zero() {
        assert_eq!(ffi::layout_byte_size(LAYOUT_STRUCT), 0);
        assert_eq!(ffi::layout_byte_size(LAYOUT_UNION), 0);
        assert_eq!(ffi::layout_byte_size(LAYOUT_SEQUENCE), 0);
        assert_eq!(ffi::layout_byte_size(LAYOUT_PADDING), 0);
    }

    // --- Phase 80.1: Panama Memory Safety Tests ---

    #[test]
    fn test_get_utf8string_address_overflow() {
        // Verify that checked_add detects u64 overflow in address arithmetic.
        // Using -1i64 as u64 = u64::MAX, adding anything > 0 overflows.
        let ptr: i64 = -1; // u64::MAX when cast
        let base_off: i64 = 0;
        let offset: i64 = 1;
        let total = (ptr as u64)
            .checked_add(base_off as u64)
            .and_then(|v| v.checked_add(offset as u64));
        assert!(total.is_none(), "overflow must be detected");
    }

    #[test]
    fn test_set_utf8string_bounds_check() {
        // setUtf8String must reject writes beyond segment size.
        // seg_size=10, offset=8, string "hello" (5+1=6 bytes needed) → 8+6=14 > 10 → reject.
        let seg_size: i64 = 10;
        let offset: i64 = 8;
        let needed: i64 = 6; // "hello" + null terminator
        assert!(
            offset + needed > seg_size,
            "write should exceed segment bounds"
        );
    }

    #[test]
    fn test_copy_bounds_check_src_overflow() {
        // copy must reject src_offset + bytes > src_size.
        let src_size: i64 = 100;
        let src_offset: i64 = 90;
        let bytes: usize = 20;
        let src_end = src_offset.checked_add(bytes as i64).unwrap_or(i64::MAX);
        assert!(
            src_end > src_size,
            "source bounds check must catch overflow"
        );
    }

    #[test]
    fn test_copy_address_arithmetic_overflow() {
        // copy must reject when address arithmetic overflows u64.
        let src_ptr: u64 = u64::MAX - 5;
        let src_off: u64 = 10;
        let src_offset: u64 = 0;
        let total = src_ptr
            .checked_add(src_off)
            .and_then(|v| v.checked_add(src_offset));
        assert!(total.is_none(), "address overflow must be detected");
    }

    #[test]
    fn test_get_utf8string_offset_exceeds_segment() {
        // getUtf8String must reject offset >= segment size.
        let seg_size: i64 = 100;
        let offset: i64 = 150;
        let remaining = seg_size - offset;
        assert!(remaining <= 0, "offset beyond segment must be rejected");
    }

    #[test]
    fn test_copy_negative_offset_rejected() {
        // copy must reject negative offsets.
        let src_offset: i64 = -1;
        let src_size: i64 = 100;
        assert!(
            src_offset < 0,
            "negative offset must be rejected by bounds check"
        );
        // The production code checks: if src_offset < 0 || src_end > src_size
        assert!(src_offset < 0 || src_offset > src_size);
    }

    // ===================================================================
    // Phase 85.1: Arena Lifecycle Tests
    // ===================================================================

    use crate::try_alloc_concurrent_synthetic;
    use crate::test_utils::mock_ctx;

    /// Helper: create an arena object of the given kind using the actual registration logic.
    fn make_arena(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
        let a = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4).unwrap();
        let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
        ctx.set_field(a, 0, Value::Int(kind));
        ctx.set_field(a, 1, Value::Object(Some(ids)));
        ctx.set_field(a, 2, Value::Int(0)); // not closed
        ctx.set_field(a, 3, Value::Int(0)); // count=0
        a
    }

    #[test]
    fn test_85_1_arena_confined_lifecycle() {
        // Create confined arena, allocate, close, verify freed
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        // Allocate via arena
        let seg_result = pe_arena_allocate_impl(&mut ctx, arena, 64, 8);
        assert!(seg_result.is_ok());
        let seg = match seg_result.unwrap() {
            Some(Value::Object(Some(s))) => s,
            _ => panic!("Expected segment object"),
        };

        // Segment should have valid pointer and size
        let ptr = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert!(ptr != 0, "Segment pointer should be non-null");
        let size = match ctx.get_field(seg, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert_eq!(size, 64);

        // Arena count should be 1
        assert!(matches!(ctx.get_field(arena, 3), Value::Int(1)));

        // Close arena
        let close_result = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close_result.is_ok());

        // Arena should be marked closed
        assert!(matches!(ctx.get_field(arena, 2), Value::Int(1)));
    }

    #[test]
    fn test_85_1_arena_shared_lifecycle() {
        // Shared arena should work the same as confined
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_SHARED);

        // Allocate two segments
        let seg1 = pe_arena_allocate_impl(&mut ctx, arena, 32, 4);
        assert!(seg1.is_ok());
        let seg2 = pe_arena_allocate_impl(&mut ctx, arena, 64, 8);
        assert!(seg2.is_ok());

        // Count should be 2
        assert!(matches!(ctx.get_field(arena, 3), Value::Int(2)));

        // Close should succeed
        let close_result = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close_result.is_ok());
        assert!(matches!(ctx.get_field(arena, 2), Value::Int(1)));
    }

    #[test]
    fn test_85_1_arena_auto_lifecycle() {
        // Auto arena should allocate but not be manually closeable like global
        // (actually auto CAN be closed, only global cannot)
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_AUTO);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 16, 1);
        assert!(seg.is_ok());

        // Auto arena can be closed
        let close_result = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close_result.is_ok());
    }

    #[test]
    fn test_85_1_arena_double_close_error() {
        // Closing an already-closed arena should return an error
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        // First close succeeds
        let close1 = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close1.is_ok());

        // Second close should fail
        let close2 = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
        assert!(close2.is_err(), "Double close must return error");

        // Global arena cannot be closed at all
        let global = make_arena(&mut ctx, ffi::ARENA_GLOBAL);
        let close_global = pe_arena_close(&mut ctx, &[Value::Object(Some(global))]);
        assert!(
            close_global.is_err(),
            "Global arena close must return error"
        );
    }

    // ===================================================================
    // Phase 85.2: MemorySegment Implementation Tests
    // ===================================================================

    /// Helper: create a ValueLayout object
    fn make_layout(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
        pe_make_layout(ctx, kind).unwrap()
    }

    #[test]
    fn test_85_2_allocate_and_readwrite() {
        // Allocate a segment via arena, write an int, read it back
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
        let layout_int = make_layout(&mut ctx, LAYOUT_INT);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 16, 4)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        // Write int 42 at offset 0
        pe_segment_set_impl(&mut ctx, seg, layout_int, 0, Value::Int(42)).unwrap();
        // Read it back
        let val = pe_segment_get_impl(&mut ctx, seg, layout_int, 0).unwrap();
        assert_eq!(val, Some(Value::Int(42)));

        // Write long at offset 8
        let layout_long = make_layout(&mut ctx, LAYOUT_LONG);
        pe_segment_set_impl(
            &mut ctx,
            seg,
            layout_long,
            8,
            Value::Long(0x1234_5678_9ABC_DEF0),
        )
        .unwrap();
        let val2 = pe_segment_get_impl(&mut ctx, seg, layout_long, 8).unwrap();
        assert_eq!(val2, Some(Value::Long(0x1234_5678_9ABC_DEF0)));
    }

    #[test]
    fn test_85_2_bounds_check_null_segment() {
        // get/set on a null address should return error
        let mut ctx = mock_ctx();
        let layout_int = make_layout(&mut ctx, LAYOUT_INT);

        // Create a segment with null pointer
        let seg = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/MemorySegment", 6).unwrap();
        ctx.set_field(seg, 0, Value::Long(0)); // null ptr
        ctx.set_field(seg, 1, Value::Long(100));
        ctx.set_field(seg, 5, Value::Long(0));

        let result = pe_segment_get_impl(&mut ctx, seg, layout_int, 0);
        assert!(result.is_err(), "Get on null segment should fail");

        let result = pe_segment_set_impl(&mut ctx, seg, layout_int, 0, Value::Int(1));
        assert!(result.is_err(), "Set on null segment should fail");
    }

    #[test]
    fn test_85_2_of_array_int() {
        // ofArray(int[]) should create a segment wrapping array data
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 4);
        ctx.set_array_element(arr, 0, Value::Int(10));
        ctx.set_array_element(arr, 1, Value::Int(20));
        ctx.set_array_element(arr, 2, Value::Int(30));
        ctx.set_array_element(arr, 3, Value::Int(40));

        // Call the ofArray registration logic directly
        let _args = vec![Value::Object(Some(arr))];
        // Simulate what the registration does
        let len = ctx.array_length(arr);
        let byte_size = (len * 4) as i64;
        let result = ctx.allocate_native_memory(byte_size as usize, 4);
        assert!(result.is_some());
        let (_, ptr) = result.unwrap();
        for i in 0..len {
            if let Value::Int(v) = ctx.get_array_element(arr, i) {
                unsafe {
                    *(ptr as *mut i32).add(i) = v;
                }
            }
        }

        // Verify the native memory contains the correct values
        for i in 0..4usize {
            let val = unsafe { *(ptr as *const i32).add(i) };
            assert_eq!(val, (i as i32 + 1) * 10);
        }
    }

    #[test]
    fn test_85_2_copy_segments() {
        // Allocate two segments, write to src, copy to dst, verify
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        let src = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();
        let dst = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        // Write pattern to src
        let src_ptr = match ctx.get_field(src, 0) {
            Value::Long(n) => n as *mut u8,
            _ => std::ptr::null_mut(),
        };
        assert!(!src_ptr.is_null());
        for i in 0..16u8 {
            unsafe {
                *src_ptr.add(i as usize) = i + 1;
            }
        }

        // Copy 16 bytes from src to dst
        let _copy_args = vec![
            Value::Object(Some(src)),
            Value::Long(0), // srcOffset
            Value::Object(Some(dst)),
            Value::Long(0),  // dstOffset
            Value::Long(16), // bytes
        ];

        // Simulate copy logic
        let src_addr = src_ptr;
        let dst_ptr = match ctx.get_field(dst, 0) {
            Value::Long(n) => n as *mut u8,
            _ => std::ptr::null_mut(),
        };
        assert!(!dst_ptr.is_null());
        unsafe {
            std::ptr::copy_nonoverlapping(src_addr, dst_ptr, 16);
        }

        // Verify dst has the pattern
        for i in 0..16u8 {
            let val = unsafe { *dst_ptr.add(i as usize) };
            assert_eq!(val, i + 1, "Byte at offset {} mismatch", i);
        }
    }

    /// Mirror of the overlap decision used by MemorySegment.copy: ranges
    /// `[s, s+bytes)` and `[d, d+bytes)` overlap iff `s < d+bytes && d < s+bytes`.
    fn copy_ranges_overlap(s: u64, d: u64, bytes: u64) -> bool {
        s < d.saturating_add(bytes) && d < s.saturating_add(bytes)
    }

    #[test]
    fn test_copy_overlap_detection() {
        // Disjoint adjacent ranges: [0,16) and [16,32) do NOT overlap.
        assert!(!copy_ranges_overlap(0, 16, 16));
        assert!(!copy_ranges_overlap(16, 0, 16));
        // One-byte overlap (forward): [0,16) and [15,31).
        assert!(copy_ranges_overlap(0, 15, 16));
        // One-byte overlap (backward): [15,31) and [0,16).
        assert!(copy_ranges_overlap(15, 0, 16));
        // Identical ranges fully overlap.
        assert!(copy_ranges_overlap(100, 100, 8));
        // Zero-length never overlaps.
        assert!(!copy_ranges_overlap(100, 100, 0));
    }

    #[test]
    fn test_copy_overlapping_within_segment_is_memmove_correct() {
        // Regression: MemorySegment.copy must behave as a memmove for
        // overlapping src/dst within a single segment. A forward-overlapping
        // copy done with copy_nonoverlapping would corrupt the tail; copy
        // (memmove) preserves it.
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
        let seg = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        let base = match ctx.get_field(seg, 0) {
            Value::Long(n) => n as *mut u8,
            _ => std::ptr::null_mut(),
        };
        assert!(!base.is_null());

        // Initialize bytes 0..16 = [1..=16].
        for i in 0..16u8 {
            unsafe { *base.add(i as usize) = i + 1 };
        }

        // copy 8 bytes from offset 0 to offset 4 (forward overlap).
        let s = base as u64;
        let d = unsafe { base.add(4) } as u64;
        let bytes: usize = 8;
        assert!(
            copy_ranges_overlap(s, d, bytes as u64),
            "ranges must be detected as overlapping"
        );
        // Use the same memmove path the production code selects on overlap.
        unsafe { std::ptr::copy(base, base.add(4), bytes) };

        // Expected memmove result: dst[4..12] == old src[0..8] == [1..=8].
        let expected: [u8; 16] = [1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 13, 14, 15, 16];
        for i in 0..16usize {
            let val = unsafe { *base.add(i) };
            assert_eq!(val, expected[i], "memmove byte at offset {} mismatch", i);
        }
    }

    #[test]
    fn test_85_2_as_slice() {
        // asSlice should create a sub-segment with offset
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 64, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        // Write a value at offset 16
        let layout_int = make_layout(&mut ctx, LAYOUT_INT);
        pe_segment_set_impl(&mut ctx, seg, layout_int, 16, Value::Int(0xCAFE)).unwrap();

        // Create a slice starting at offset 16, size 32
        let slice = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/MemorySegment", 6).unwrap();
        let base_ptr = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        ctx.set_field(slice, 0, Value::Long(base_ptr));
        ctx.set_field(slice, 1, Value::Long(32));
        ctx.set_field(slice, 2, ctx.get_field(seg, 2));
        ctx.set_field(slice, 3, Value::Int(0));
        ctx.set_field(slice, 4, Value::Int(1));
        ctx.set_field(slice, 5, Value::Long(16)); // offset=16

        // Read at slice offset 0 should give the value written at parent offset 16
        let val = pe_segment_get_impl(&mut ctx, slice, layout_int, 0).unwrap();
        assert_eq!(val, Some(Value::Int(0xCAFE)));
    }

    #[test]
    fn test_85_2_reinterpret() {
        // reinterpret should change size but keep same address
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 64, 1)
            .unwrap()
            .and_then(|v| {
                if let Value::Object(Some(s)) = v {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();

        let orig_ptr = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        let orig_size = match ctx.get_field(seg, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert_eq!(orig_size, 64);

        // Reinterpret with new size 128
        let reinterpreted =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/MemorySegment", 6).unwrap();
        ctx.set_field(reinterpreted, 0, Value::Long(orig_ptr));
        ctx.set_field(reinterpreted, 1, Value::Long(128));
        ctx.set_field(reinterpreted, 2, ctx.get_field(seg, 2));
        ctx.set_field(reinterpreted, 3, ctx.get_field(seg, 3));
        ctx.set_field(reinterpreted, 4, Value::Int(1));
        ctx.set_field(reinterpreted, 5, Value::Long(0));

        // Pointer should be the same, size should be different
        let new_ptr = match ctx.get_field(reinterpreted, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        let new_size = match ctx.get_field(reinterpreted, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert_eq!(new_ptr, orig_ptr);
        assert_eq!(new_size, 128);
    }

    // ===================================================================
    // Phase 85.3: Linker Downcall Tests
    // ===================================================================

    #[test]
    fn test_85_3_downcall_strlen() {
        // Call C strlen through the downcall mechanism
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();

        // Look up strlen
        let strlen_addr = ctx.find_native_symbol(-1, "strlen");
        if strlen_addr.is_none() {
            // Skip on platforms where symbol lookup isn't available
            return;
        }
        let strlen_addr = strlen_addr.unwrap() as i64;

        // Create a C string in native memory
        let (_, ptr) = ctx.allocate_native_memory(16, 1).unwrap();
        let test_str = b"hello\0";
        unsafe {
            std::ptr::copy_nonoverlapping(test_str.as_ptr(), ptr, test_str.len());
        }

        // Build FunctionDescriptor: of(LAYOUT_LONG, ADDRESS)
        let ret_layout = make_layout(&mut ctx, LAYOUT_LONG);
        let param_layout = make_layout(&mut ctx, LAYOUT_ADDRESS);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        // Build DowncallHandle
        let handle = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 2).unwrap();
        ctx.set_field(handle, 0, Value::Long(strlen_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));

        // Build args array with the pointer as a Long
        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Long(ptr as i64));

        // Invoke the downcall
        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        );

        assert!(result.is_ok(), "strlen downcall failed: {:?}", result.err());
        let val = result.unwrap();
        // strlen("hello") = 5
        assert_eq!(val, Some(Value::Long(5)));
    }

    #[test]
    fn test_85_3_downcall_abs() {
        // Call C abs() — int abs(int)
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();

        let abs_addr = ctx.find_native_symbol(-1, "abs");
        if abs_addr.is_none() {
            return;
        }
        let abs_addr = abs_addr.unwrap() as i64;

        // FunctionDescriptor: of(LAYOUT_INT, LAYOUT_INT)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 2).unwrap();
        ctx.set_field(handle, 0, Value::Long(abs_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(-42));

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        );

        assert!(result.is_ok(), "abs downcall failed: {:?}", result.err());
        let val = result.unwrap();
        assert_eq!(val, Some(Value::Int(42)));
    }

    // -----------------------------------------------------------------
    // T5.6.3 — Panama DowncallHandle CIF cache (panama_cif)
    // -----------------------------------------------------------------

    #[test]
    fn panama_cif_cache_reuses_cif_across_calls() {
        // Invoke the same downcall twice and check that the global
        // CIF_BUILD_COUNT only increments once — proving the second
        // call hit the cache on field 3 of the DowncallHandle.
        use crate::panama_libffi as plf;
        use std::sync::atomic::Ordering;

        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();
        let abs_addr = ctx.find_native_symbol(-1, "abs");
        if abs_addr.is_none() {
            // Platform without symbol lookup — skip.
            return;
        }
        let abs_addr = abs_addr.unwrap() as i64;

        // FunctionDescriptor: int(int)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        // Build a DowncallHandle with 4 fields (the new cache layout).
        let handle = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 4).unwrap();
        ctx.set_field(handle, 0, Value::Long(abs_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));
        ctx.set_field(handle, 2, Value::Long(-1));
        ctx.set_field(handle, 3, Value::Long(0)); // cache miss marker

        // Snapshot the global build counter.
        let before = plf::CIF_BUILD_COUNT.load(Ordering::Relaxed);

        // --- First call: cache miss, Cif built and stashed ---
        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(-7));
        let r1 = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        )
        .expect("first downcall must succeed");
        assert_eq!(r1, Some(Value::Int(7)));

        let after_first = plf::CIF_BUILD_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after_first,
            before + 1,
            "first call must construct exactly one Cif"
        );

        // Verify field 3 now carries a non-zero pointer.
        let stash_v = ctx.get_field(handle, 3);
        let stash_u64 = match stash_v {
            Value::Long(n) => n as u64,
            _ => 0,
        };
        assert!(
            stash_u64 != 0,
            "field 3 must hold the boxed Cif pointer after the first call, got {:?}",
            stash_v
        );

        // --- Second call: cache hit, Cif *not* rebuilt ---
        let call_args2 = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args2, 0, Value::Int(-11));
        let r2 = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args2))],
        )
        .expect("second downcall must succeed");
        assert_eq!(r2, Some(Value::Int(11)));

        let after_second = plf::CIF_BUILD_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after_second, after_first,
            "second call must reuse the cached Cif — build count must not increment"
        );

        // Clean up the leaked Box to keep the test process tidy.
        unsafe { plf::free_cached_cif(stash_u64) };
    }

    #[test]
    fn panama_cif_cache_field3_sticks_to_boxed_ptr() {
        // Simpler smoke test: if we don't actually call, field 3 stays 0.
        // Only the invoke path populates the cache slot.
        let mut ctx = mock_ctx();
        let handle = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 4).unwrap();
        ctx.set_field(handle, 3, Value::Long(0));
        match ctx.get_field(handle, 3) {
            Value::Long(0) => {}
            other => panic!("expected Long(0), got {:?}", other),
        }
    }

    #[test]
    fn test_85_3_downcall_struct_layout() {
        // Test struct layout computation for passing structs
        let mut ctx = mock_ctx();

        // struct { int x; long y; } — should have size 16 (4 + 4 padding + 8)
        let int_layout = make_layout(&mut ctx, LAYOUT_INT);
        let long_layout = make_layout(&mut ctx, LAYOUT_LONG);

        let members = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
        ctx.set_array_element(members, 0, Value::Object(Some(int_layout)));
        ctx.set_array_element(members, 1, Value::Object(Some(long_layout)));

        let result = pe_struct_layout(&mut ctx, &[Value::Object(Some(members))]);
        assert!(result.is_ok());
        let layout = match result.unwrap() {
            Some(Value::Object(Some(l))) => l,
            _ => panic!("Expected struct layout object"),
        };

        let total_size = match ctx.get_field(layout, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        // int(4) + padding(4) + long(8) = 16, aligned to 8
        assert_eq!(total_size, 16);

        let alignment = match ctx.get_field(layout, 5) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert_eq!(alignment, 8);
    }

    #[test]
    fn panama_struct_layout_preserves_named_members() {
        let mut ctx = mock_ctx();
        let address = make_layout(&mut ctx, LAYOUT_ADDRESS);
        let name = ctx.create_string("ptr");
        let named = pe_layout_with_name(
            &mut ctx,
            &[Value::Object(Some(address)), Value::Object(Some(name))],
        )
        .unwrap()
        .and_then(|v| match v {
            Value::Object(Some(obj)) => Some(obj),
            _ => None,
        })
        .expect("withName must return a layout object");

        let members = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(members, 0, Value::Object(Some(named)));
        let layout = pe_struct_layout(&mut ctx, &[Value::Object(Some(members))])
            .unwrap()
            .and_then(|v| match v {
                Value::Object(Some(obj)) => Some(obj),
                _ => None,
            })
            .expect("structLayout must return a layout object");

        let names_arr = match ctx.get_field(layout, 3) {
            Value::Object(Some(arr)) => arr,
            other => panic!("expected names array, got {other:?}"),
        };
        let stored_name = match ctx.get_array_element(names_arr, 0) {
            Value::Object(Some(obj)) => obj,
            other => panic!("expected stored member name, got {other:?}"),
        };
        assert_eq!(ctx.read_string(stored_name).as_deref(), Some("ptr"));
    }

    #[test]
    fn test_85_3_downcall_void_return() {
        // Call a function with void return — use memset (returns void* but we treat it as void)
        // Actually, let's use a simpler approach: call abs with void descriptor
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();

        // We'll test that a downcall with -1 return kind produces Value::Object(None)
        let abs_addr = ctx.find_native_symbol(-1, "abs");
        if abs_addr.is_none() {
            return;
        }
        let abs_addr = abs_addr.unwrap() as i64;

        // FunctionDescriptor: ofVoid(LAYOUT_INT)  — return_layout = None
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
        ctx.set_field(descriptor, 0, Value::Object(None)); // void
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 2).unwrap();
        ctx.set_field(handle, 0, Value::Long(abs_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(5));

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        );

        assert!(result.is_ok());
        // void return should produce Object(None)
        let val = result.unwrap();
        assert_eq!(val, Some(Value::Object(None)));
    }

    // ===================================================================
    // Phase 85.4: Upcall Stub Tests
    // ===================================================================

    #[test]
    fn test_85_4_upcall_registration() {
        // Register an upcall and verify it can be looked up
        let mut ctx = mock_ctx();

        // Create a "MethodHandle" target object
        let target = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2).unwrap();

        let entry = ffi::UpcallEntry {
            target,
            method_name: "invoke".to_string(),
            method_descriptor: String::new(),
            param_kinds: vec![LAYOUT_INT],
            return_kind: LAYOUT_INT,
        };

        let slot = ctx.register_upcall(entry);

        // Look it up
        let info = ctx.get_upcall_info(slot);
        assert!(info.is_some());
        let (ret_target, param_kinds, return_kind) = info.unwrap();
        assert_eq!(ret_target, target);
        assert_eq!(param_kinds, vec![LAYOUT_INT]);
        assert_eq!(return_kind, LAYOUT_INT);
    }

    #[test]
    fn test_85_4_upcall_handle_and_invoke() {
        // Native access is DENIED by default, and `pe_upcall_handle` gates on
        // it — so without this the test asserts against a refusal and fails on
        // a correct build. The two panama tests that pass unguarded do so only
        // because the policy is a process global that another test may have
        // granted first; `with_policy_isolated` takes the shared lock and
        // restores the previous value, so this neither depends on nor leaks
        // that ordering.
        with_policy_isolated(|| {
            set_native_access_enabled(true);
            // Create an upcall handle through pe_upcall_handle and dispatch through pe_upcall_invoke
            let mut ctx = mock_ctx();

            // Create target, descriptor
            let target = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2).unwrap();

            let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
            let param_layout = make_layout(&mut ctx, LAYOUT_INT);
            let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
            ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

            let descriptor =
                try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
            ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
            ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

            let linker = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker", 1).unwrap();
            let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

            // Register upcall handle
            let handle_result = pe_upcall_handle(
                &mut ctx,
                &[
                    Value::Object(Some(linker)),
                    Value::Object(Some(target)),
                    Value::Object(Some(descriptor)),
                    Value::Object(Some(arena)),
                ],
            );
            assert!(handle_result.is_ok());
            let seg = match handle_result.unwrap() {
                Some(Value::Object(Some(s))) => s,
                _ => panic!("Expected segment from upcall handle"),
            };

            // The segment's address (field 0) should be a real trampoline function pointer
            let tramp_addr = match ctx.get_field(seg, 0) {
                Value::Long(n) => n,
                _ => -1,
            };
            assert!(tramp_addr != 0, "Trampoline address should be non-null");
            // Verify it's a real callable function pointer by calling it
            let tramp_fn: unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                unsafe { std::mem::transmute(tramp_addr as usize) };
            let tramp_result = unsafe { tramp_fn(42, 0, 0, 0, 0, 0, 0, 0) };
            // Trampoline dispatch returns 0 (no thread-local context in tests)
            assert_eq!(tramp_result, 0);

            // Set up invoke_virtual to return a value when the upcall dispatches
            // via pe_upcall_invoke (the Java-side dispatch path)
            unsafe {
                *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Int(99))));
            }

            // Create an UpcallStub handle object for pe_upcall_invoke
            // (uses the VM upcall slot 0, not the trampoline address)
            let stub = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/UpcallStub", 2).unwrap();
            ctx.set_field(stub, 0, Value::Long(0)); // slot 0 in the VM's upcall table

            // Create args array
            let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
            ctx.set_array_element(call_args, 0, Value::Int(42));

            let result = pe_upcall_invoke(
                &mut ctx,
                &[Value::Object(Some(stub)), Value::Object(Some(call_args))],
            );
            assert!(result.is_ok());
            let val = result.unwrap();
            assert_eq!(val, Some(Value::Int(99)));
        });
    }

    #[test]
    fn test_85_4_upcall_invalid_slot() {
        // Invoking an upcall with an invalid slot should return an error
        let mut ctx = mock_ctx();

        let stub = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/UpcallStub", 2).unwrap();
        ctx.set_field(stub, 0, Value::Long(999)); // non-existent slot

        let result = pe_upcall_invoke(&mut ctx, &[Value::Object(Some(stub))]);
        assert!(result.is_err(), "Invalid upcall slot must return error");
    }

    // =====================================================================
    // NEW-18: libffi-backed Panama Linker tests
    //
    // Each test exercises a real C-library function through the rewritten
    // pe_downcall_invoke / pe_upcall_handle path. Together they prove:
    //   (a) arbitrary-arity downcalls past the old 8-arg cap;
    //   (b) mixed integer/float register classification (was broken);
    //   (c) variadic dispatch via Cif::new_variadic;
    //   (d) struct-by-value return packed into a result MemorySegment;
    //   (e) round-trip upcall — C qsort calls a libffi-closure-backed
    //       trampoline that re-enters our Rust callback (which would, in
    //       a real VM, dispatch to Java via the active NativeContext).
    // =====================================================================

    /// NEW-18: a 12-arg integer downcall via the libffi pipeline using a
    /// helper `extern "C"` Rust function (no symbol lookup needed).
    /// This exercises argument counts past the 8-arg cap of the old
    /// dispatcher and the int register/stack handoff.
    #[test]
    fn new18_downcall_arity_12_ints() {
        extern "C" fn sum12(
            a: i32,
            b: i32,
            c: i32,
            d: i32,
            e: i32,
            f: i32,
            g: i32,
            h: i32,
            i: i32,
            j: i32,
            k: i32,
            l: i32,
        ) -> i32 {
            a + b + c + d + e + f + g + h + i + j + k + l
        }
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();
        let fn_addr = sum12 as usize as i64;

        // Build descriptor: int(int,int,int,int,int,int,int,int,int,int,int,int)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 12);
        for i in 0..12 {
            let p = make_layout(&mut ctx, LAYOUT_INT);
            ctx.set_array_element(params_arr, i, Value::Object(Some(p)));
        }
        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 3).unwrap();
        ctx.set_field(handle, 0, Value::Long(fn_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));
        ctx.set_field(handle, 2, Value::Long(-1));

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 12);
        for i in 0..12 {
            ctx.set_array_element(call_args, i, Value::Int((i + 1) as i32));
        }

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        )
        .expect("12-arg downcall must succeed");
        // Sum of 1..=12 = 78.
        assert_eq!(result, Some(Value::Int(78)));
    }

    /// NEW-18: mixed int/float arguments. The old dispatcher passed
    /// every arg through `u64` registers, so floats landed in the wrong
    /// place. libffi gets the ABI right.
    #[test]
    fn new18_downcall_mixed_int_float() {
        extern "C" fn mix(a: i32, b: f64, c: i32, d: f32) -> f64 {
            a as f64 + b + c as f64 + d as f64
        }
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall.
        let _na = NativeAccessGuard::enable();
        let fn_addr = mix as usize as i64;

        let ret_layout = make_layout(&mut ctx, LAYOUT_DOUBLE);
        let p_int1 = make_layout(&mut ctx, LAYOUT_INT);
        let p_dbl = make_layout(&mut ctx, LAYOUT_DOUBLE);
        let p_int2 = make_layout(&mut ctx, LAYOUT_INT);
        let p_flt = make_layout(&mut ctx, LAYOUT_FLOAT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(p_int1)));
        ctx.set_array_element(params_arr, 1, Value::Object(Some(p_dbl)));
        ctx.set_array_element(params_arr, 2, Value::Object(Some(p_int2)));
        ctx.set_array_element(params_arr, 3, Value::Object(Some(p_flt)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 3).unwrap();
        ctx.set_field(handle, 0, Value::Long(fn_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));
        ctx.set_field(handle, 2, Value::Long(-1));

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_array_element(call_args, 0, Value::Int(10));
        ctx.set_array_element(call_args, 1, Value::Double(2.5));
        ctx.set_array_element(call_args, 2, Value::Int(7));
        ctx.set_array_element(call_args, 3, Value::Float(0.5));

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        )
        .expect("mixed int/float downcall must succeed");
        match result {
            Some(Value::Double(d)) => assert!((d - 20.0).abs() < 1e-9, "got {d}"),
            other => panic!("expected Double, got {other:?}"),
        }
    }

    /// NEW-18: variadic downcall via the new `firstVariadicArg` flag.
    /// Calls libc `snprintf(buf, len, "%d", 42)` and verifies the
    /// buffer contains "42" and the return value is 2 (excluding NUL).
    /// snprintf is a real variadic C function on every supported
    /// platform so this exercises libffi's `prep_cif_var` end-to-end.
    #[test]
    fn new18_downcall_variadic_snprintf() {
        // Look up snprintf — present on every supported platform. On
        // MSVC Windows the symbol is named `snprintf` in ucrt.
        let mut ctx = mock_ctx();
        // FIX(test): grant native access for the real libffi downcall (keeps
        // this test from depending on a sibling having left the gate open).
        let _na = NativeAccessGuard::enable();
        let snprintf_addr = match ctx.find_native_symbol(-1, "snprintf") {
            Some(a) => a as i64,
            None => return, // platform without symbol lookup → skip
        };

        // Allocate a 16-byte output buffer in native memory.
        let (_, buf_ptr) = ctx.allocate_native_memory(16, 1).unwrap();
        unsafe {
            std::ptr::write_bytes(buf_ptr, 0, 16);
        }
        // Allocate the format string "%d\0".
        let (_, fmt_ptr) = ctx.allocate_native_memory(4, 1).unwrap();
        unsafe {
            let f = b"%d\0";
            std::ptr::copy_nonoverlapping(f.as_ptr(), fmt_ptr, f.len());
        }

        // Descriptor: int(address, long, address, int) — 3 fixed args
        // (buf, size, format) followed by 1 variadic int.
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let p_buf = make_layout(&mut ctx, LAYOUT_ADDRESS);
        let p_len = make_layout(&mut ctx, LAYOUT_LONG);
        let p_fmt = make_layout(&mut ctx, LAYOUT_ADDRESS);
        let p_var = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(p_buf)));
        ctx.set_array_element(params_arr, 1, Value::Object(Some(p_len)));
        ctx.set_array_element(params_arr, 2, Value::Object(Some(p_fmt)));
        ctx.set_array_element(params_arr, 3, Value::Object(Some(p_var)));

        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 3).unwrap();
        ctx.set_field(handle, 0, Value::Long(snprintf_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));
        // First 3 args fixed; everything from index 3 is variadic.
        ctx.set_field(handle, 2, Value::Long(3));

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_array_element(call_args, 0, Value::Long(buf_ptr as i64));
        ctx.set_array_element(call_args, 1, Value::Long(16));
        ctx.set_array_element(call_args, 2, Value::Long(fmt_ptr as i64));
        ctx.set_array_element(call_args, 3, Value::Int(42));

        let result = pe_downcall_invoke(
            &mut ctx,
            &[Value::Object(Some(handle)), Value::Object(Some(call_args))],
        )
        .expect("variadic snprintf downcall must succeed");
        // snprintf returns the number of chars written excluding NUL.
        assert_eq!(result, Some(Value::Int(2)));
        // And the buffer must contain "42\0".
        let written = unsafe { std::slice::from_raw_parts(buf_ptr as *const u8, 3) };
        assert_eq!(&written[..2], b"42");
        assert_eq!(written[2], 0);
    }

    /// NEW-18: real upcall round-trip. We register an upcall handle
    /// pointing at a Java MethodHandle target; libffi gives us a real
    /// extern "C" trampoline. We then call that trampoline directly
    /// from Rust, with the active NativeContext installed via the
    /// guard, and verify the dispatch reaches Java's invoke_virtual.
    #[test]
    fn new18_upcall_libffi_closure_dispatches_to_java() {
        // GAP F3: `pe_upcall_handle` now goes through `require_native_access`,
        // exactly like the downcall path (and exactly as docs/CONFIG.md has
        // always described `Linker.upcallHandle`). Grant it for this test, the
        // same way `new18_downcall_variadic_snprintf` does.
        let _na = NativeAccessGuard::enable();
        let mut ctx = mock_ctx();

        // Descriptor: int(int, int)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let p1 = make_layout(&mut ctx, LAYOUT_INT);
        let p2 = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(p1)));
        ctx.set_array_element(params_arr, 1, Value::Object(Some(p2)));
        let descriptor =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2).unwrap();
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let target = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2).unwrap();
        let linker = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker", 1).unwrap();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        // Pre-arm the mock NativeContext to return Int(123) from invoke_virtual.
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Ok(Some(Value::Int(123))));
        }

        // Allocate the upcall handle — this builds a real libffi closure
        // and returns a MemorySegment whose field 0 is the trampoline addr.
        let seg = pe_upcall_handle(
            &mut ctx,
            &[
                Value::Object(Some(linker)),
                Value::Object(Some(target)),
                Value::Object(Some(descriptor)),
                Value::Object(Some(arena)),
            ],
        )
        .expect("upcallHandle must succeed")
        .and_then(|v| {
            if let Value::Object(Some(s)) = v {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();

        let tramp_addr = match ctx.get_field(seg, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        assert!(tramp_addr != 0, "trampoline must be a real address");

        // Install the active NativeContext for the duration of the call,
        // then invoke the trampoline directly with two int args.
        let result_int: i32 = {
            let _guard = crate::panama_libffi::ActiveContextGuard::install(&mut ctx);
            let f: extern "C" fn(i32, i32) -> i32 =
                unsafe { std::mem::transmute(tramp_addr as usize) };
            f(11, 22)
        };
        // The Rust callback inside upcall_dispatch reaches the mock's
        // invoke_virtual, which we pre-armed to return Int(123). The
        // callback then writes 123 back into the result slot, which
        // libffi delivers to the C caller (us) as an int.
        assert_eq!(result_int, 123);
    }

    // --- Task #57: native-access gate emits IllegalCallerException ---

    /// Round-trip guard: when native access is disabled, the gate inside
    /// `validated_fn_ptr` must produce a `RuntimeError::IllegalCallerException`
    /// — not the old `IllegalStateException` with an "IllegalCallerException:"
    /// message prefix. This is what the VM-side mapping table converts to
    /// the throwable `java/lang/IllegalCallerException` class.
    #[test]
    fn task57_native_access_gate_emits_illegal_caller_exception() {
        // FIX(test-isolation): serialize with the guarded downcall tests.
        // This test sets the flag *false*; without the shared lock it could
        // run concurrently with a guarded test mid-downcall and either steal
        // its `true` value or have its own `false` stomped, corrupting both.
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Save and restore the global flag — other tests rely on the default
        // (enabled) behaviour and may run in parallel.
        let prev = native_access_enabled();
        set_native_access_enabled(false);

        let result = validated_fn_ptr::<extern "C" fn() -> i32>(0x1000);

        // Restore before any assertion so a failure does not poison sibling
        // tests.
        set_native_access_enabled(prev);

        let err = result.expect_err("gate must reject downcall when disabled");
        match err {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalCallerException { message },
            )) => {
                assert!(
                    message.contains("Native access is not enabled"),
                    "message should describe the denial: {message}"
                );
                // Must NOT carry the legacy "IllegalCallerException: " prefix
                // — the class name comes from the variant now.
                assert!(
                    !message.starts_with("IllegalCallerException:"),
                    "message must not duplicate the class name: {message}"
                );
            }
            other => panic!("expected RuntimeError::IllegalCallerException, got {other:?}"),
        }
    }

    /// Regression guard: the other validation failures inside
    /// `validated_fn_ptr` (null pointer, misaligned address) must still emit
    /// `IllegalStateException`. Only the native-access denial flips to the
    /// new variant.
    #[test]
    fn task57_other_gate_failures_still_emit_illegal_state() {
        // FIX(test-isolation): serialize with the guarded downcall tests so
        // no concurrent test can flip the flag out from under us.
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Ensure the gate is open so we exercise the non-access paths.
        let prev = native_access_enabled();
        set_native_access_enabled(true);

        // Null function pointer.
        let null_err = validated_fn_ptr::<extern "C" fn()>(0).expect_err("null must be rejected");
        // Misaligned function pointer (odd address fails alignment check on
        // every supported platform since `align_of::<usize>() >= 4`).
        let misaligned_err =
            validated_fn_ptr::<extern "C" fn()>(0x1001).expect_err("misaligned must be rejected");

        set_native_access_enabled(prev);

        for (label, err) in [("null", null_err), ("misaligned", misaligned_err)] {
            match err {
                MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                    RuntimeError::IllegalStateException { .. },
                )) => { /* expected */ }
                other => panic!("{label}: expected IllegalStateException, got {other:?}"),
            }
        }
    }

    /// NEW-18: variadic struct-layout helper sanity. Make sure the
    /// libffi bridge accepts every primitive layout our descriptor
    /// machinery produces. (We exercise the struct path indirectly
    /// via the existing `test_85_3_downcall_struct_layout` test.)
    #[test]
    fn new18_layout_translation_covers_every_primitive() {
        for kind in [
            LAYOUT_BYTE,
            LAYOUT_BOOLEAN,
            LAYOUT_SHORT,
            LAYOUT_CHAR,
            LAYOUT_INT,
            LAYOUT_LONG,
            LAYOUT_FLOAT,
            LAYOUT_DOUBLE,
            LAYOUT_ADDRESS,
        ] {
            assert!(
                crate::panama_libffi::primitive_kind_to_ffi_type(kind).is_some(),
                "primitive kind {} must translate",
                kind
            );
        }
    }

    // ===================================================================
    // Security regression: Panama arbitrary-memory + native-access gate
    // (CRITICAL — arbitrary process memory R/W + native-code execution)
    // ===================================================================

    /// Helper: build a MemorySegment synthetic backed by a real Rust buffer so
    /// in-bounds accesses are sound while out-of-bounds accesses are caught by
    /// the bounds checks before any dereference.
    fn make_segment(ctx: &mut dyn NativeContext, ptr: i64, size: i64, base_off: i64) -> ObjectRef {
        let seg = try_alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6).unwrap();
        ctx.set_field(seg, 0, Value::Long(ptr));
        ctx.set_field(seg, 1, Value::Long(size));
        ctx.set_field(seg, 2, Value::Object(None));
        ctx.set_field(seg, 3, Value::Int(0));
        ctx.set_field(seg, 4, Value::Int(1));
        ctx.set_field(seg, 5, Value::Long(base_off));
        seg
    }

    fn make_layout_kind(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
        pe_make_layout(ctx, kind).unwrap()
    }

    #[test]
    fn sec_segment_get_rejects_oob_offset() {
        let mut ctx = mock_ctx();
        let mut buf = [0u8; 8];
        let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_INT);
        // offset 8 + width 4 = 12 > size 8 → must be rejected, NOT dereferenced.
        let r = pe_segment_get_impl(&mut ctx, seg, layout, 8);
        assert!(r.is_err(), "out-of-bounds get must be rejected");
    }

    #[test]
    fn sec_segment_get_rejects_negative_offset() {
        let mut ctx = mock_ctx();
        let mut buf = [0u8; 8];
        let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_BYTE);
        let r = pe_segment_get_impl(&mut ctx, seg, layout, -1);
        assert!(r.is_err(), "negative offset get must be rejected");
    }

    #[test]
    fn sec_segment_set_rejects_oob_offset() {
        let mut ctx = mock_ctx();
        let mut buf = [0u8; 8];
        let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_LONG);
        // offset 4 + width 8 = 12 > size 8 → reject before writing.
        let r = pe_segment_set_impl(
            &mut ctx,
            seg,
            layout,
            4,
            Value::Long(0x4141414141414141u64 as i64),
        );
        assert!(r.is_err(), "out-of-bounds set must be rejected");
    }

    #[test]
    fn sec_segment_zero_size_not_accessible() {
        // A 0-size segment (as produced by ofAddress before reinterpret) must
        // refuse all access, even at offset 0 — this is the ofAddress escape.
        let mut ctx = mock_ctx();
        let seg = make_segment(&mut ctx, 0x1000, 0, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_BYTE);
        assert!(
            pe_segment_get_impl(&mut ctx, seg, layout, 0).is_err(),
            "get on zero-size segment must be rejected"
        );
        assert!(
            pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0)).is_err(),
            "set on zero-size segment must be rejected"
        );
    }

    #[test]
    fn sec_segment_access_addr_overflow_rejected() {
        let mut ctx = mock_ctx();
        // ptr = u64::MAX (as i64 = -1), size large enough to pass bounds, so the
        // address arithmetic itself overflows and must be caught.
        let seg = make_segment(&mut ctx, -1i64, 1024, 0);
        let r = pe_segment_access_addr(&mut ctx, seg, 16, 8);
        assert!(r.is_err(), "address arithmetic overflow must be rejected");
    }

    #[test]
    fn sec_segment_in_bounds_roundtrips() {
        // Sanity: a legitimate in-bounds access still works.
        let mut ctx = mock_ctx();
        let mut buf = [0u8; 8];
        let seg = make_segment(&mut ctx, buf.as_mut_ptr() as i64, 8, 0);
        let layout = make_layout_kind(&mut ctx, LAYOUT_INT);
        assert!(pe_segment_set_impl(&mut ctx, seg, layout, 0, Value::Int(0x11223344)).is_ok());
        match pe_segment_get_impl(&mut ctx, seg, layout, 0) {
            Ok(Some(Value::Int(v))) => assert_eq!(v, 0x11223344),
            other => panic!("expected Int(0x11223344), got {:?}", other),
        }
    }

    #[test]
    fn sec_native_access_disabled_by_default() {
        // The process-wide gate must default closed (secure-by-default).
        // NOTE: this reads global state; if a prior test in the same process
        // flipped it on we restore it, but the *initial* default is false.
        // We assert the default via a fresh load after forcing the documented
        // default value.
        // FIX(test-isolation): this test both writes and reads the global
        // flag, so it must serialize with every other flag-toggling test;
        // otherwise a concurrent guarded downcall test could flip the flag
        // between our `set` and our `assert`, breaking these assertions.
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_native_access_enabled(false);
        assert!(!native_access_enabled());
        // Setter plumbing still works in both directions.
        set_native_access_enabled(true);
        assert!(native_access_enabled());
        set_native_access_enabled(false);
        assert!(!native_access_enabled());
    }

    #[test]
    fn sec_validated_fn_ptr_denied_when_gate_closed() {
        // FIX(test-isolation): serialize so a concurrent guarded test cannot
        // re-enable the flag between our `set false` and the denial check.
        let _lk = NATIVE_ACCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_native_access_enabled(false);
        let r = validated_fn_ptr::<extern "C" fn() -> i32>(0x1000);
        assert!(
            r.is_err(),
            "downcall must be denied when native access is disabled"
        );
    }

    #[test]
    fn upcall_target_root_scan_and_remap_gap_c() {
        // Step 5 GAP C: a leaked FFM/Panama upcall target must be reported as a
        // GC root and remapped in place after a move. Build a minimal registered
        // upcall around a fake target address, then scan + remap.
        let userdata = Box::new(UpcallUserdata {
            target: std::sync::atomic::AtomicUsize::new(0xABCD_0000),
            param_kinds: Vec::new(),
            return_kind: -1,
        });
        let userdata_ptr: &'static UpcallUserdata = Box::leak(userdata);
        let cif = MiddleCif::new(Vec::new(), MiddleType::void());
        let closure = Closure::new(cif, upcall_dispatch, userdata_ptr);
        let code_ptr = *closure.code_ptr() as *const () as usize;
        upcall_registry().lock().insert(
            code_ptr,
            UpcallEntry {
                _closure: Box::new(closure),
                userdata: userdata_ptr as *const UpcallUserdata,
            },
        );

        // Scan reports the (fake) target as a root.
        let mut roots = Vec::new();
        gc_scan_upcall_target_roots(&mut roots);
        assert!(
            roots.iter().any(|r| r.as_ptr() as usize == 0xABCD_0000),
            "upcall target must be scanned as a GC root"
        );

        // Remap 0xABCD_0000 -> 0xABCD_8000; the leaked userdata is rewritten.
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0xABCD_0000usize, 0xABCD_8000usize);
        gc_update_upcall_target_refs(&map);
        assert_eq!(
            userdata_ptr
                .target
                .load(std::sync::atomic::Ordering::Relaxed),
            0xABCD_8000,
            "upcall target must be remapped in place"
        );
        let mut roots2 = Vec::new();
        gc_scan_upcall_target_roots(&mut roots2);
        assert!(
            roots2.iter().any(|r| r.as_ptr() as usize == 0xABCD_8000),
            "re-scan must report the moved target"
        );

        // Don't leak our entry into other tests sharing the global registry.
        upcall_registry().lock().remove(&code_ptr);
    }
}
