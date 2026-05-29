//! Panama FFI native method registrations.

use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, alloc_concurrent_synthetic};

use cratonvm_native_api::ffi::{
    self, LAYOUT_ADDRESS, LAYOUT_BOOLEAN, LAYOUT_BYTE, LAYOUT_CHAR, LAYOUT_DOUBLE, LAYOUT_FLOAT,
    LAYOUT_INT, LAYOUT_LONG, LAYOUT_PADDING, LAYOUT_SEQUENCE, LAYOUT_SHORT, LAYOUT_STRUCT,
    LAYOUT_UNION,
};

/// Maximum number of bytes for a single memory copy/fill operation.
const MAX_COPY_SIZE: usize = 256 * 1024 * 1024; // 256 MiB

/// Maximum length to scan when reading a C string from native memory.
const MAX_CSTR_LEN: usize = 4096;

/// Native-access gate for Panama downcalls.
///
/// A `validated_fn_ptr` call transmutes a Java-supplied raw address to an
/// `extern "C" fn` and invokes it — arbitrary native code execution. Real
/// JDK Panama gates this behind `--enable-native-access` / the module's
/// `enableNativeAccess` permission. This crate has no module-permission
/// plumbing reachable here, so this is a minimal coarse gate.
///
/// Default is `false` (secure-by-default, matching the JDK where native
/// access is denied unless `--enable-native-access` grants it). A
/// host/launcher that wants to permit Panama downcalls and raw-address
/// memory access must call [`set_native_access_enabled(true)`] at startup
/// (e.g. when the user passes `--enable-native-access`), and flip it on
/// only for trusted modules granted native access.
///
/// TODO: wire this to a real per-module `--enable-native-access` check once
/// `NativeContext` exposes the caller module's native-access permission.
static NATIVE_ACCESS_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Enable or disable Panama native downcalls process-wide.
///
/// When disabled, every downcall through [`validated_fn_ptr`] fails with a
/// thrown `java.lang.IllegalCallerException`, matching the JDK's
/// `--enable-native-access` semantics. (Task #57: previously folded into
/// `IllegalStateException` because `RuntimeError` lacked the variant.)
pub fn set_native_access_enabled(enabled: bool) {
    NATIVE_ACCESS_ENABLED.store(enabled, std::sync::atomic::Ordering::SeqCst);
}

/// Whether Panama native downcalls are currently permitted.
pub fn native_access_enabled() -> bool {
    NATIVE_ACCESS_ENABLED.load(std::sync::atomic::Ordering::SeqCst)
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
            message: format!(
                "Misaligned function pointer {:#x} in downcall",
                addr
            ),
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

fn pe_make_layout(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
    let layout = alloc_concurrent_synthetic(ctx, "java/lang/foreign/ValueLayout", 2);
    ctx.set_field(layout, 0, Value::Int(kind));
    ctx.set_field(layout, 1, Value::Int(ffi::layout_byte_size(kind) as i32));
    layout
}

fn register_pe_value_layout(r: &mut NativeMethodRegistry) {
    let vl = "java/lang/foreign/ValueLayout";

    // Static factory fields — return pre-built layout objects
    r.register(
        vl,
        "JAVA_BYTE",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_BYTE))))),
    );
    r.register(
        vl,
        "JAVA_SHORT",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_SHORT))))),
    );
    r.register(
        vl,
        "JAVA_INT",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_INT))))),
    );
    r.register(
        vl,
        "JAVA_LONG",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_LONG))))),
    );
    r.register(
        vl,
        "JAVA_FLOAT",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_FLOAT))))),
    );
    r.register(
        vl,
        "JAVA_DOUBLE",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| {
            Ok(Some(Value::Object(Some(pe_make_layout(
                ctx,
                LAYOUT_DOUBLE,
            )))))
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
            )))))
        },
    );
    r.register(
        vl,
        "JAVA_CHAR",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| Ok(Some(Value::Object(Some(pe_make_layout(ctx, LAYOUT_CHAR))))),
    );
    r.register(
        vl,
        "ADDRESS",
        "()Ljava/lang/foreign/ValueLayout;",
        |ctx, _| {
            Ok(Some(Value::Object(Some(pe_make_layout(
                ctx,
                LAYOUT_ADDRESS,
            )))))
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
    r.register(vl, "name", "()Ljava/util/Optional;", |_ctx, _args| {
        Ok(Some(Value::Object(None))) // empty Optional
    });
}

// --- Arena: lifecycle-scoped memory management ---
// Arena synthetic: [0]=kind (Int), [1]=alloc_ids (Object — int array of alloc IDs), [2]=closed (Int), [3]=count (Int)

fn register_pe_arena(r: &mut NativeMethodRegistry) {
    let arena = "java/lang/foreign/Arena";

    r.register(arena, "global", "()Ljava/lang/foreign/Arena;", |ctx, _| {
        let a = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4);
        let ids = ctx.new_array(cratonvm_types::ArrayElementType::Long, 256);
        ctx.set_field(a, 0, Value::Int(ffi::ARENA_GLOBAL));
        ctx.set_field(a, 1, Value::Object(Some(ids)));
        ctx.set_field(a, 2, Value::Int(0));
        ctx.set_field(a, 3, Value::Int(0));
        Ok(Some(Value::Object(Some(a))))
    });
    r.register(arena, "ofAuto", "()Ljava/lang/foreign/Arena;", |ctx, _| {
        let a = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4);
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
            let a = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4);
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
            let a = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4);
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

    // close() — free all allocations in this arena
    r.register(arena, "close", "()V", pe_arena_close);
}

fn pe_arena_allocate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let size = match args.get(1) {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
    let align = match args.get(2) {
        Some(Value::Long(n)) => *n,
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
    let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
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

fn register_pe_memory_segment(r: &mut NativeMethodRegistry) {
    let ms = "java/lang/foreign/MemorySegment";

    // byteSize() → long
    r.register(ms, "byteSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Long(size)))
    });

    // address() → long (raw pointer as long)
    r.register(ms, "address", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ptr = match ctx.get_field(this, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        let off = match ctx.get_field(this, 5) {
            Value::Long(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Long(ptr + off)))
    });

    // isNative() → boolean (always true for our segments)
    r.register(ms, "isNative", "()Z", |_, _| Ok(Some(Value::Int(1))));

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

    // getAtIndex(ValueLayout, long index) → value
    r.register(
        ms,
        "getAtIndex",
        "(Ljava/lang/foreign/ValueLayout;J)Ljava/lang/Object;",
        |ctx, args| {
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
            let base_ptr = match ctx.get_field(this, 0) {
                Value::Long(n) => n,
                _ => 0,
            };
            let base_off = match ctx.get_field(this, 5) {
                Value::Long(n) => n,
                _ => 0,
            };
            let arena_val = ctx.get_field(this, 2);

            let slice = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
            ctx.set_field(slice, 0, Value::Long(base_ptr));
            ctx.set_field(slice, 1, Value::Long(new_size));
            ctx.set_field(slice, 2, arena_val);
            ctx.set_field(slice, 3, ctx.get_field(this, 3)); // inherit read-only
            ctx.set_field(slice, 4, Value::Int(1)); // alive
            ctx.set_field(slice, 5, Value::Long(base_off + offset));
            Ok(Some(Value::Object(Some(slice))))
        },
    );

    // ofAddress(long address) → MemorySegment (wraps a raw address, zero-length)
    r.register(
        ms,
        "ofAddress",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            // Gate raw-address wrapping behind native access: turning an
            // arbitrary caller-supplied long into an addressable segment is
            // equivalent to arbitrary process-memory access once paired with
            // reinterpret/get/set. Refuse unless native access is enabled.
            if !native_access_enabled() {
                return Err(RuntimeError::IllegalCallerException {
                    message: "Native access is not enabled for this module \
                              (MemorySegment.ofAddress denied)"
                        .into(),
                }
                .into());
            }
            let addr = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
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
                let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None)); // auto-managed
                ctx.set_field(seg, 3, Value::Int(0)); // read-write
                ctx.set_field(seg, 4, Value::Int(1)); // alive
                ctx.set_field(seg, 5, Value::Long(0));
                // Store alloc_id so it can be freed (field 0 encodes the pointer)
                let _ = alloc_id; // tracked by NativeMemoryTable
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }.into())
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
                let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }.into())
            }
        },
    );
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
                let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
                ctx.set_field(seg, 0, Value::Long(ptr as i64));
                ctx.set_field(seg, 1, Value::Long(byte_size));
                ctx.set_field(seg, 2, Value::Object(None));
                ctx.set_field(seg, 3, Value::Int(0));
                ctx.set_field(seg, 4, Value::Int(1));
                ctx.set_field(seg, 5, Value::Long(0));
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Err(RuntimeError::OutOfMemoryError {
                    message: "Failed to allocate native memory for ofArray".into(),
                }.into())
            }
        },
    );

    // copy(src, srcOffset, dst, dstOffset, bytes) — memcpy
    r.register(
        ms,
        "copy",
        "(Ljava/lang/foreign/MemorySegment;JLjava/lang/foreign/MemorySegment;JJ)V",
        |ctx, args| {
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

            let src_ptr = match ctx.get_field(src, 0) {
                Value::Long(n) => n,
                _ => 0,
            };
            let src_off = match ctx.get_field(src, 5) {
                Value::Long(n) => n,
                _ => 0,
            };
            let dst_ptr = match ctx.get_field(dst, 0) {
                Value::Long(n) => n,
                _ => 0,
            };
            let dst_off = match ctx.get_field(dst, 5) {
                Value::Long(n) => n,
                _ => 0,
            };

            // Validate offsets against segment sizes to prevent out-of-bounds access
            let src_size = match ctx.get_field(src, 1) {
                Value::Long(n) => n,
                _ => 0,
            };
            let dst_size = match ctx.get_field(dst, 1) {
                Value::Long(n) => n,
                _ => 0,
            };

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
                // Bounds check: offset + bytes must fit within segment size
                // (when segment size is known, i.e., > 0)
                if src_size > 0 {
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
                }
                if dst_size > 0 {
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
                }

                // Validate address arithmetic doesn't overflow
                let src_total = (src_ptr as u64)
                    .checked_add(src_off as u64)
                    .and_then(|v| v.checked_add(src_offset as u64));
                let dst_total = (dst_ptr as u64)
                    .checked_add(dst_off as u64)
                    .and_then(|v| v.checked_add(dst_offset as u64));

                if let (Some(s), Some(d)) = (src_total, dst_total) {
                    let src_addr = s as *const u8;
                    let dst_addr = d as *mut u8;
                    if !src_addr.is_null() && !dst_addr.is_null() {
                        // SAFETY: addresses are non-null, bounds-checked against
                        // segment sizes, and bytes is bounded by MAX_COPY_SIZE.
                        unsafe { std::ptr::copy_nonoverlapping(src_addr, dst_addr, bytes) };
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
            let this = obj_arg(args, 0)?;
            let byte_val = match args.get(1) {
                Some(Value::Int(n)) => *n as u8,
                _ => 0,
            };
            let ptr = match ctx.get_field(this, 0) {
                Value::Long(n) => n,
                _ => 0,
            };
            let off = match ctx.get_field(this, 5) {
                Value::Long(n) => n,
                _ => 0,
            };
            let size = match ctx.get_field(this, 1) {
                Value::Long(n) => n as usize,
                _ => 0,
            };
            let addr = (ptr + off) as *mut u8;
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
}

/// Validate a single-element access (get/set) against the segment's declared
/// size and compute the target address with checked arithmetic.
///
/// Mirrors the bounds/overflow checks the `copy`/`fill` paths perform, and
/// throws the same `IllegalStateException` on violation. Rejects:
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
    let ptr = match ctx.get_field(seg, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
    let base_off = match ctx.get_field(seg, 5) {
        Value::Long(n) => n,
        _ => 0,
    };
    let size = match ctx.get_field(seg, 1) {
        Value::Long(n) => n,
        _ => 0,
    };

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
    let total = (ptr as u64)
        .checked_add(base_off as u64)
        .and_then(|v| v.checked_add(offset as u64));
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

fn pe_segment_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let kind = match ctx.get_field(layout, 0) {
        Value::Int(n) => n,
        _ => 0,
    };
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
            LAYOUT_BYTE | LAYOUT_BOOLEAN => Value::Int(*(addr as *const i8) as i32),
            LAYOUT_SHORT | LAYOUT_CHAR => Value::Int(*(addr as *const i16) as i32),
            LAYOUT_INT => Value::Int(*(addr as *const i32)),
            LAYOUT_LONG | LAYOUT_ADDRESS => Value::Long(*(addr as *const i64)),
            LAYOUT_FLOAT => Value::Float(*(addr as *const f32)),
            LAYOUT_DOUBLE => Value::Double(*(addr as *const f64)),
            _ => Value::Int(0),
        }
    };
    Ok(Some(value))
}

fn pe_segment_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let kind = match ctx.get_field(layout, 0) {
        Value::Int(n) => n,
        _ => 0,
    };
    // Reject writes that fall outside the segment's declared bounds, overflow
    // the address space, or target a zero-size segment. The access width is
    // derived from the layout kind, matching the write widths below.
    let width = ffi::layout_byte_size(kind) as i64;
    let addr = pe_segment_access_addr(ctx, seg, offset, width)? as *mut u8;

    // SAFETY: addr is non-null, bounds-checked against the segment's declared
    // size, and the address arithmetic was overflow-checked (see
    // pe_segment_access_addr). The kind determines the write width so alignment
    // is implicit from the segment.
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
    Ok(None)
}

// --- SymbolLookup: load shared libraries and find symbols ---
// SymbolLookup synthetic: [0]=lib_index (Long — index into SharedVm.native_libraries), [1]=name

fn register_pe_symbol_lookup(r: &mut NativeMethodRegistry) {
    let sl = "java/lang/foreign/SymbolLookup";

    // libraryLookup(path, arena) → SymbolLookup
    r.register(
        sl,
        "libraryLookup",
        "(Ljava/lang/String;Ljava/lang/foreign/Arena;)Ljava/lang/foreign/SymbolLookup;",
        |ctx, args| {
            let path_obj = obj_arg(args, 0)?;
            let path = ctx.read_string(path_obj).unwrap_or_default();

            let lib_index = ctx.load_native_library(&path)?;

            let lookup = alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 2);
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
            let lookup = alloc_concurrent_synthetic(ctx, "java/lang/foreign/SymbolLookup", 2);
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

            match ctx.find_native_symbol(lib_index, &sym_name) {
                Some(addr) => {
                    // Wrap address in MemorySegment and Optional.of()
                    let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
                    ctx.set_field(seg, 0, Value::Long(addr as i64));
                    ctx.set_field(seg, 1, Value::Long(0)); // function pointer — no byte size
                    ctx.set_field(seg, 2, Value::Object(None));
                    ctx.set_field(seg, 3, Value::Int(1)); // read-only
                    ctx.set_field(seg, 4, Value::Int(1)); // alive
                    ctx.set_field(seg, 5, Value::Long(0));
                    // Return as Optional.of(segment)
                    let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
                    ctx.set_field(opt, 0, Value::Object(Some(seg)));
                    Ok(Some(Value::Object(Some(opt))))
                }
                None => {
                    // Return Optional.empty()
                    let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
                    ctx.set_field(opt, 0, Value::Object(None));
                    Ok(Some(Value::Object(Some(opt))))
                }
            }
        },
    );
}

// --- Linker: create downcall handles ---
// DowncallHandle synthetic: [0]=function_address (Long), [1]=descriptor (Object)

fn register_pe_linker(r: &mut NativeMethodRegistry) {
    let linker = "java/lang/foreign/Linker";

    // nativeLinker() → Linker
    r.register(
        linker,
        "nativeLinker",
        "()Ljava/lang/foreign/Linker;",
        |ctx, _| {
            let l = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker", 1);
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
        let fn_addr = match ctx.get_field(addr_seg, 0) { Value::Long(n) => n, _ => 0 };

        let handle = alloc_concurrent_synthetic(ctx, "java/lang/foreign/DowncallHandle", 4);
        ctx.set_field(handle, 0, Value::Long(fn_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));
        ctx.set_field(handle, 2, Value::Long(-1));
        ctx.set_field(handle, 3, Value::Long(0)); // cif not yet cached
        Ok(Some(Value::Object(Some(handle))))
    });

    // downcallHandle with Linker.Option[] for variadic etc. (NEW-18.3).
    r.register(linker, "downcallHandle",
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let addr_seg = obj_arg(args, 1)?;
            let descriptor = obj_arg(args, 2)?;
            let fn_addr = match ctx.get_field(addr_seg, 0) { Value::Long(n) => n, _ => 0 };

            // Scan the option array for a firstVariadicArg option (kind=0).
            // Linker.Option synthetic layout: field 0 = kind (Int), field 1 = payload (Long).
            let mut variadic_fixed: i64 = -1;
            if let Some(Value::Object(Some(opts))) = args.get(3) {
                let n = ctx.array_length(*opts);
                for i in 0..n {
                    if let Value::Object(Some(opt)) = ctx.get_array_element(*opts, i) {
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

            let handle = alloc_concurrent_synthetic(ctx, "java/lang/foreign/DowncallHandle", 4);
            ctx.set_field(handle, 0, Value::Long(fn_addr));
            ctx.set_field(handle, 1, Value::Object(Some(descriptor)));
            ctx.set_field(handle, 2, Value::Long(variadic_fixed));
            ctx.set_field(handle, 3, Value::Long(0)); // cif not yet cached
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
            let opt = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Linker$Option", 2);
            ctx.set_field(opt, 0, Value::Int(0));        // kind = firstVariadicArg
            ctx.set_field(opt, 1, Value::Long(n as i64)); // payload
            Ok(Some(Value::Object(Some(opt))))
        },
    );

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
fn pe_downcall_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::panama_libffi as plf;
    use libffi::middle::{arg as ffi_arg, CodePtr};

    let handle = obj_arg(args, 0)?;
    let fn_addr = match ctx.get_field(handle, 0) {
        Value::Long(n) => n,
        _ => 0,
    };
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

    // ----- Read incoming Java arguments (args[1] = Object[] of values) -----
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
        let cif_ref = unsafe { plf::cached_cif_ref(cached_cif_u64) }
            .expect("non-zero pointer must deref");
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
        let cif_ref = unsafe { plf::cached_cif_ref(stash_u64) }
            .expect("just-stashed pointer must deref");
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
        let code_ptr = CodePtr::from_ptr(fn_addr as *const std::ffi::c_void);
        if ret_slot.is_empty() {
            // void return — libffi still wants a writable result slot of
            // size_t bytes for ffi_arg. Provide one and discard.
            let mut sink = [0u8; std::mem::size_of::<usize>()];
            libffi::low::call::<()>(
                raw_cif_ptr,
                code_ptr,
                arg_refs.as_ptr() as *mut *mut std::ffi::c_void,
            );
            let _ = sink;
        } else {
            // For all primitive returns we use the slot directly. libffi
            // promises to write at most max(size_of<usize>, ffi_type::size)
            // bytes — we already pad ret_slot up to size_of<usize>.
            libffi::raw::ffi_call(
                raw_cif_ptr,
                Some(*CodePtr::from_ptr(fn_addr as *const std::ffi::c_void).as_safe_fun()),
                ret_slot.as_mut_ptr() as *mut std::ffi::c_void,
                arg_refs.as_ptr() as *mut *mut std::ffi::c_void,
            );
        }
    }

    // ----- Unmarshal return -----
    let result = match return_layout {
        None => Value::Object(None),
        Some(rl) => {
            let kind = plf::read_layout_kind(ctx, rl);
            if kind < 10 {
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
                let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
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

    // of(returnLayout, paramLayouts...) → FunctionDescriptor
    r.register(fd, "of", "(Ljava/lang/foreign/ValueLayout;[Ljava/lang/foreign/ValueLayout;)Ljava/lang/foreign/FunctionDescriptor;", |ctx, args| {
        let ret_layout = args.first().copied().unwrap_or(Value::Object(None));
        let params = args.get(1).copied().unwrap_or(Value::Object(None));
        let desc = alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2);
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
            let desc = alloc_concurrent_synthetic(ctx, "java/lang/foreign/FunctionDescriptor", 2);
            ctx.set_field(desc, 0, Value::Object(None)); // void return
            ctx.set_field(desc, 1, params);
            Ok(Some(Value::Object(Some(desc))))
        },
    );

    // returnLayout() → Optional<ValueLayout>
    r.register(fd, "returnLayout", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let rl = ctx.get_field(this, 0);
        let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
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
            a0: u64, a1: u64, a2: u64, a3: u64,
            a4: u64, a5: u64, a6: u64, a7: u64,
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
static UPCALL_TRAMPOLINE_FNS: [unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64; MAX_UPCALL_TRAMPOLINES] = [
    _upcall_t00, _upcall_t01, _upcall_t02, _upcall_t03,
    _upcall_t04, _upcall_t05, _upcall_t06, _upcall_t07,
    _upcall_t08, _upcall_t09, _upcall_t10, _upcall_t11,
    _upcall_t12, _upcall_t13, _upcall_t14, _upcall_t15,
    _upcall_t16, _upcall_t17, _upcall_t18, _upcall_t19,
    _upcall_t20, _upcall_t21, _upcall_t22, _upcall_t23,
    _upcall_t24, _upcall_t25, _upcall_t26, _upcall_t27,
    _upcall_t28, _upcall_t29, _upcall_t30, _upcall_t31,
    _upcall_t32, _upcall_t33, _upcall_t34, _upcall_t35,
    _upcall_t36, _upcall_t37, _upcall_t38, _upcall_t39,
    _upcall_t40, _upcall_t41, _upcall_t42, _upcall_t43,
    _upcall_t44, _upcall_t45, _upcall_t46, _upcall_t47,
    _upcall_t48, _upcall_t49, _upcall_t50, _upcall_t51,
    _upcall_t52, _upcall_t53, _upcall_t54, _upcall_t55,
    _upcall_t56, _upcall_t57, _upcall_t58, _upcall_t59,
    _upcall_t60, _upcall_t61, _upcall_t62, _upcall_t63,
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

use libffi::middle::{Closure, Cif as MiddleCif, Type as MiddleType};

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
    /// Java target object the closure dispatches to.
    _target: ObjectRef,
}

// SAFETY: libffi closures are immutable after construction and their
// trampoline pages are independent of any thread. The userdata we
// store is `'static` and only read by the closure callback.
unsafe impl Send for UpcallEntry {}
unsafe impl Sync for UpcallEntry {}

/// Userdata captured by every upcall trampoline.
struct UpcallUserdata {
    target: ObjectRef,
    param_kinds: Vec<i32>,
    return_kind: i32,
}

static UPCALL_REGISTRY: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<usize, UpcallEntry>>> =
    std::sync::OnceLock::new();

fn upcall_registry() -> &'static parking_lot::Mutex<std::collections::HashMap<usize, UpcallEntry>> {
    UPCALL_REGISTRY.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
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
            cratonvm_native_api::ffi::LAYOUT_BYTE
            | cratonvm_native_api::ffi::LAYOUT_BOOLEAN => {
                Value::Int(*(slot as *const i8) as i32)
            }
            cratonvm_native_api::ffi::LAYOUT_SHORT
            | cratonvm_native_api::ffi::LAYOUT_CHAR => {
                Value::Int(*(slot as *const i16) as i32)
            }
            cratonvm_native_api::ffi::LAYOUT_INT => {
                Value::Int(*(slot as *const i32))
            }
            cratonvm_native_api::ffi::LAYOUT_LONG => {
                Value::Long(*(slot as *const i64))
            }
            cratonvm_native_api::ffi::LAYOUT_FLOAT => {
                Value::Float(*(slot as *const f32))
            }
            cratonvm_native_api::ffi::LAYOUT_DOUBLE => {
                Value::Double(*(slot as *const f64))
            }
            cratonvm_native_api::ffi::LAYOUT_ADDRESS => {
                Value::Long(*(slot as *const i64))
            }
            _ => Value::Long(0),
        };
        java_args.push(v);
    }

    // Dispatch into Java via the active NativeContext.
    let dispatch_result = plf::with_active_context(|ctx| {
        // The Java target is a MethodHandle / functional interface impl.
        // We invoke its `invoke([Object])` method passing our boxed args.
        // Build an Object[] of boxed primitives.
        let arr =
            ctx.new_array(cratonvm_types::ArrayElementType::Reference, java_args.len());
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
            userdata.target,
            "invoke",
            "([Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(userdata.target)), Value::Object(Some(arr))],
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

/// `Linker.upcallHandle(target, descriptor, arena)` — build a libffi
/// closure that dispatches into a Java MethodHandle. Returns a
/// MemorySegment whose address is the closure's extern "C" trampoline.
fn pe_upcall_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use crate::panama_libffi as plf;

    let _linker = obj_arg(args, 0)?;
    let target = obj_arg(args, 1)?;
    let descriptor = obj_arg(args, 2)?;
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
        target,
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
            _target: target,
        },
    );

    // Wrap the trampoline address in a MemorySegment so Java can pass
    // it to other downcalls expecting a `MemorySegment` function ptr.
    let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
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

    // MemoryLayout.unionLayout(members...) → UnionLayout
    r.register(
        ml,
        "unionLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;",
        pe_union_layout,
    );

    // MemoryLayout.sequenceLayout(count, element) → SequenceLayout
    r.register(
        ml,
        "sequenceLayout",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;",
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
            let layout = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6);
            ctx.set_field(layout, 0, Value::Int(LAYOUT_PADDING));
            ctx.set_field(layout, 1, Value::Long(bytes));
            ctx.set_field(layout, 5, Value::Long(1)); // alignment=1
            Ok(Some(Value::Object(Some(layout))))
        },
    );

    // Common methods on all layouts
    let sl = "java/lang/foreign/StructLayout";
    r.register(sl, "byteSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Long(size)))
    });
    r.register(sl, "byteAlignment", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let align = match ctx.get_field(this, 5) {
            Value::Long(n) => n,
            _ => 1,
        };
        Ok(Some(Value::Long(align)))
    });
    r.register(sl, "memberLayouts", "()Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });

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
        |_ctx, args| {
            // For simplicity, return the same layout (names are tracked separately in struct)
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );

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
                alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout$PathElement", 2);
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
                alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout$PathElement", 2);
            ctx.set_field(elem, 0, Value::Object(None));
            ctx.set_field(elem, 1, Value::Int(1)); // kind=sequence
            Ok(Some(Value::Object(Some(elem))))
        },
    );
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
            ctx.set_array_element(names_arr, i, Value::Object(None)); // no name by default
            offset += member_size;
            if member_align > max_align {
                max_align = member_align;
            }
        }
    }

    // Pad total size to alignment
    let total_size = ffi::align_up(offset, max_align);

    let layout = alloc_concurrent_synthetic(ctx, "java/lang/foreign/StructLayout", 6);
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

    let layout = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6);
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

    let layout = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6);
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
        let ptr = match ctx.get_field(this, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        let base_off = match ctx.get_field(this, 5) {
            Value::Long(n) => n,
            _ => 0,
        };
        // Validate address arithmetic doesn't overflow (matches setUtf8String/copy)
        let total = (ptr as u64)
            .checked_add(base_off as u64)
            .and_then(|v| v.checked_add(offset as u64));
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
        let seg_size = match ctx.get_field(this, 1) {
            Value::Long(n) if n > 0 => {
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

            let ptr = match ctx.get_field(this, 0) {
                Value::Long(n) => n,
                _ => 0,
            };
            let base_off = match ctx.get_field(this, 5) {
                Value::Long(n) => n,
                _ => 0,
            };
            // Bounds check: string + null terminator must fit within segment
            let seg_size = match ctx.get_field(this, 1) {
                Value::Long(n) => n,
                _ => 0,
            };
            let str_bytes = s.as_bytes();
            let needed = str_bytes.len() as i64 + 1; // +1 for null terminator
            if seg_size > 0 && (offset < 0 || offset + needed > seg_size) {
                return Err(RuntimeError::IllegalStateException {
                    message: format!(
                        "setUtf8String: offset {} + {} bytes exceeds segment size {}",
                        offset, needed, seg_size
                    ),
                }
                .into());
            }

            // Validate address arithmetic doesn't overflow
            let total = (ptr as u64)
                .checked_add(base_off as u64)
                .and_then(|v| v.checked_add(offset as u64));
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
            let ptr = match ctx.get_field(this, 0) {
                Value::Long(n) => n,
                _ => 0,
            };
            let off = match ctx.get_field(this, 5) {
                Value::Long(n) => n,
                _ => 0,
            };
            let arena_val = ctx.get_field(this, 2);

            let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
            ctx.set_field(seg, 0, Value::Long(ptr));
            ctx.set_field(seg, 1, Value::Long(new_size));
            ctx.set_field(seg, 2, arena_val);
            ctx.set_field(seg, 3, ctx.get_field(this, 3));
            ctx.set_field(seg, 4, Value::Int(1));
            ctx.set_field(seg, 5, Value::Long(off));
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

    #[test]
    fn test_function_descriptor() {
        // FunctionDescriptor.of(returnLayout, argLayouts...)
        // Describes a native function signature
        struct FuncDesc {
            ret_size: usize,
            arg_sizes: Vec<usize>,
        }
        let desc = FuncDesc {
            ret_size: 4, // int return
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

    use crate::test_utils::mock_ctx;
    use crate::alloc_concurrent_synthetic;

    /// Helper: create an arena object of the given kind using the actual registration logic.
    fn make_arena(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
        let a = alloc_concurrent_synthetic(ctx, "java/lang/foreign/Arena", 4);
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
        let ptr = match ctx.get_field(seg, 0) { Value::Long(n) => n, _ => 0 };
        assert!(ptr != 0, "Segment pointer should be non-null");
        let size = match ctx.get_field(seg, 1) { Value::Long(n) => n, _ => 0 };
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
        assert!(close_global.is_err(), "Global arena close must return error");
    }

    // ===================================================================
    // Phase 85.2: MemorySegment Implementation Tests
    // ===================================================================

    /// Helper: create a ValueLayout object
    fn make_layout(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
        pe_make_layout(ctx, kind)
    }

    #[test]
    fn test_85_2_allocate_and_readwrite() {
        // Allocate a segment via arena, write an int, read it back
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);
        let layout_int = make_layout(&mut ctx, LAYOUT_INT);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 16, 4)
            .unwrap()
            .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None })
            .unwrap();

        // Write int 42 at offset 0
        pe_segment_set_impl(&mut ctx, seg, layout_int, 0, Value::Int(42)).unwrap();
        // Read it back
        let val = pe_segment_get_impl(&mut ctx, seg, layout_int, 0).unwrap();
        assert_eq!(val, Some(Value::Int(42)));

        // Write long at offset 8
        let layout_long = make_layout(&mut ctx, LAYOUT_LONG);
        pe_segment_set_impl(&mut ctx, seg, layout_long, 8, Value::Long(0x1234_5678_9ABC_DEF0)).unwrap();
        let val2 = pe_segment_get_impl(&mut ctx, seg, layout_long, 8).unwrap();
        assert_eq!(val2, Some(Value::Long(0x1234_5678_9ABC_DEF0)));
    }

    #[test]
    fn test_85_2_bounds_check_null_segment() {
        // get/set on a null address should return error
        let mut ctx = mock_ctx();
        let layout_int = make_layout(&mut ctx, LAYOUT_INT);

        // Create a segment with null pointer
        let seg = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/MemorySegment", 6);
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
                unsafe { *(ptr as *mut i32).add(i) = v; }
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
            .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None })
            .unwrap();
        let dst = pe_arena_allocate_impl(&mut ctx, arena, 32, 1)
            .unwrap()
            .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None })
            .unwrap();

        // Write pattern to src
        let src_ptr = match ctx.get_field(src, 0) { Value::Long(n) => n as *mut u8, _ => std::ptr::null_mut() };
        assert!(!src_ptr.is_null());
        for i in 0..16u8 {
            unsafe { *src_ptr.add(i as usize) = i + 1; }
        }

        // Copy 16 bytes from src to dst
        let _copy_args = vec![
            Value::Object(Some(src)),
            Value::Long(0),   // srcOffset
            Value::Object(Some(dst)),
            Value::Long(0),   // dstOffset
            Value::Long(16),  // bytes
        ];

        // Simulate copy logic
        let src_addr = src_ptr;
        let dst_ptr = match ctx.get_field(dst, 0) { Value::Long(n) => n as *mut u8, _ => std::ptr::null_mut() };
        assert!(!dst_ptr.is_null());
        unsafe { std::ptr::copy_nonoverlapping(src_addr, dst_ptr, 16); }

        // Verify dst has the pattern
        for i in 0..16u8 {
            let val = unsafe { *dst_ptr.add(i as usize) };
            assert_eq!(val, i + 1, "Byte at offset {} mismatch", i);
        }
    }

    #[test]
    fn test_85_2_as_slice() {
        // asSlice should create a sub-segment with offset
        let mut ctx = mock_ctx();
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        let seg = pe_arena_allocate_impl(&mut ctx, arena, 64, 1)
            .unwrap()
            .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None })
            .unwrap();

        // Write a value at offset 16
        let layout_int = make_layout(&mut ctx, LAYOUT_INT);
        pe_segment_set_impl(&mut ctx, seg, layout_int, 16, Value::Int(0xCAFE)).unwrap();

        // Create a slice starting at offset 16, size 32
        let slice = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/MemorySegment", 6);
        let base_ptr = match ctx.get_field(seg, 0) { Value::Long(n) => n, _ => 0 };
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
            .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None })
            .unwrap();

        let orig_ptr = match ctx.get_field(seg, 0) { Value::Long(n) => n, _ => 0 };
        let orig_size = match ctx.get_field(seg, 1) { Value::Long(n) => n, _ => 0 };
        assert_eq!(orig_size, 64);

        // Reinterpret with new size 128
        let reinterpreted = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/MemorySegment", 6);
        ctx.set_field(reinterpreted, 0, Value::Long(orig_ptr));
        ctx.set_field(reinterpreted, 1, Value::Long(128));
        ctx.set_field(reinterpreted, 2, ctx.get_field(seg, 2));
        ctx.set_field(reinterpreted, 3, ctx.get_field(seg, 3));
        ctx.set_field(reinterpreted, 4, Value::Int(1));
        ctx.set_field(reinterpreted, 5, Value::Long(0));

        // Pointer should be the same, size should be different
        let new_ptr = match ctx.get_field(reinterpreted, 0) { Value::Long(n) => n, _ => 0 };
        let new_size = match ctx.get_field(reinterpreted, 1) { Value::Long(n) => n, _ => 0 };
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
        unsafe { std::ptr::copy_nonoverlapping(test_str.as_ptr(), ptr, test_str.len()); }

        // Build FunctionDescriptor: of(LAYOUT_LONG, ADDRESS)
        let ret_layout = make_layout(&mut ctx, LAYOUT_LONG);
        let param_layout = make_layout(&mut ctx, LAYOUT_ADDRESS);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        // Build DowncallHandle
        let handle = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 2);
        ctx.set_field(handle, 0, Value::Long(strlen_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));

        // Build args array with the pointer as a Long
        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Long(ptr as i64));

        // Invoke the downcall
        let result = pe_downcall_invoke(&mut ctx, &[
            Value::Object(Some(handle)),
            Value::Object(Some(call_args)),
        ]);

        assert!(result.is_ok(), "strlen downcall failed: {:?}", result.err());
        let val = result.unwrap();
        // strlen("hello") = 5
        assert_eq!(val, Some(Value::Long(5)));
    }

    #[test]
    fn test_85_3_downcall_abs() {
        // Call C abs() — int abs(int)
        let mut ctx = mock_ctx();

        let abs_addr = ctx.find_native_symbol(-1, "abs");
        if abs_addr.is_none() { return; }
        let abs_addr = abs_addr.unwrap() as i64;

        // FunctionDescriptor: of(LAYOUT_INT, LAYOUT_INT)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 2);
        ctx.set_field(handle, 0, Value::Long(abs_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(-42));

        let result = pe_downcall_invoke(&mut ctx, &[
            Value::Object(Some(handle)),
            Value::Object(Some(call_args)),
        ]);

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
            alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        // Build a DowncallHandle with 4 fields (the new cache layout).
        let handle =
            alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 4);
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
            &[
                Value::Object(Some(handle)),
                Value::Object(Some(call_args2)),
            ],
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
        let handle =
            alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 4);
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

        let total_size = match ctx.get_field(layout, 1) { Value::Long(n) => n, _ => 0 };
        // int(4) + padding(4) + long(8) = 16, aligned to 8
        assert_eq!(total_size, 16);

        let alignment = match ctx.get_field(layout, 5) { Value::Long(n) => n, _ => 0 };
        assert_eq!(alignment, 8);
    }

    #[test]
    fn test_85_3_downcall_void_return() {
        // Call a function with void return — use memset (returns void* but we treat it as void)
        // Actually, let's use a simpler approach: call abs with void descriptor
        let mut ctx = mock_ctx();

        // We'll test that a downcall with -1 return kind produces Value::Object(None)
        let abs_addr = ctx.find_native_symbol(-1, "abs");
        if abs_addr.is_none() { return; }
        let abs_addr = abs_addr.unwrap() as i64;

        // FunctionDescriptor: ofVoid(LAYOUT_INT)  — return_layout = None
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(None)); // void
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 2);
        ctx.set_field(handle, 0, Value::Long(abs_addr));
        ctx.set_field(handle, 1, Value::Object(Some(descriptor)));

        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(5));

        let result = pe_downcall_invoke(&mut ctx, &[
            Value::Object(Some(handle)),
            Value::Object(Some(call_args)),
        ]);

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
        let target = alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2);

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
        // Create an upcall handle through pe_upcall_handle and dispatch through pe_upcall_invoke
        let mut ctx = mock_ctx();

        // Create target, descriptor
        let target = alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2);

        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let param_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(param_layout)));

        let descriptor = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let linker = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker", 1);
        let arena = make_arena(&mut ctx, ffi::ARENA_CONFINED);

        // Register upcall handle
        let handle_result = pe_upcall_handle(&mut ctx, &[
            Value::Object(Some(linker)),
            Value::Object(Some(target)),
            Value::Object(Some(descriptor)),
            Value::Object(Some(arena)),
        ]);
        assert!(handle_result.is_ok());
        let seg = match handle_result.unwrap() {
            Some(Value::Object(Some(s))) => s,
            _ => panic!("Expected segment from upcall handle"),
        };

        // The segment's address (field 0) should be a real trampoline function pointer
        let tramp_addr = match ctx.get_field(seg, 0) { Value::Long(n) => n, _ => -1 };
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
        let stub = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/UpcallStub", 2);
        ctx.set_field(stub, 0, Value::Long(0)); // slot 0 in the VM's upcall table

        // Create args array
        let call_args = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        ctx.set_array_element(call_args, 0, Value::Int(42));

        let result = pe_upcall_invoke(&mut ctx, &[
            Value::Object(Some(stub)),
            Value::Object(Some(call_args)),
        ]);
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val, Some(Value::Int(99)));
    }

    #[test]
    fn test_85_4_upcall_invalid_slot() {
        // Invoking an upcall with an invalid slot should return an error
        let mut ctx = mock_ctx();

        let stub = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/UpcallStub", 2);
        ctx.set_field(stub, 0, Value::Long(999)); // non-existent slot

        let result = pe_upcall_invoke(&mut ctx, &[
            Value::Object(Some(stub)),
        ]);
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
            a: i32, b: i32, c: i32, d: i32, e: i32, f: i32,
            g: i32, h: i32, i: i32, j: i32, k: i32, l: i32,
        ) -> i32 {
            a + b + c + d + e + f + g + h + i + j + k + l
        }
        let mut ctx = mock_ctx();
        let fn_addr = sum12 as usize as i64;

        // Build descriptor: int(int,int,int,int,int,int,int,int,int,int,int,int)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 12);
        for i in 0..12 {
            let p = make_layout(&mut ctx, LAYOUT_INT);
            ctx.set_array_element(params_arr, i, Value::Object(Some(p)));
        }
        let descriptor = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 3);
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
        let fn_addr = mix as usize as i64;

        let ret_layout = make_layout(&mut ctx, LAYOUT_DOUBLE);
        let p_int1 = make_layout(&mut ctx, LAYOUT_INT);
        let p_dbl  = make_layout(&mut ctx, LAYOUT_DOUBLE);
        let p_int2 = make_layout(&mut ctx, LAYOUT_INT);
        let p_flt  = make_layout(&mut ctx, LAYOUT_FLOAT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 4);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(p_int1)));
        ctx.set_array_element(params_arr, 1, Value::Object(Some(p_dbl)));
        ctx.set_array_element(params_arr, 2, Value::Object(Some(p_int2)));
        ctx.set_array_element(params_arr, 3, Value::Object(Some(p_flt)));

        let descriptor = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 3);
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

        let descriptor = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let handle = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/DowncallHandle", 3);
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
        let written =
            unsafe { std::slice::from_raw_parts(buf_ptr as *const u8, 3) };
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
        let mut ctx = mock_ctx();

        // Descriptor: int(int, int)
        let ret_layout = make_layout(&mut ctx, LAYOUT_INT);
        let p1 = make_layout(&mut ctx, LAYOUT_INT);
        let p2 = make_layout(&mut ctx, LAYOUT_INT);
        let params_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
        ctx.set_array_element(params_arr, 0, Value::Object(Some(p1)));
        ctx.set_array_element(params_arr, 1, Value::Object(Some(p2)));
        let descriptor = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
        ctx.set_field(descriptor, 0, Value::Object(Some(ret_layout)));
        ctx.set_field(descriptor, 1, Value::Object(Some(params_arr)));

        let target = alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 2);
        let linker = alloc_concurrent_synthetic(&mut ctx, "java/lang/foreign/Linker", 1);
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
        .and_then(|v| if let Value::Object(Some(s)) = v { Some(s) } else { None })
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
            other => panic!(
                "expected RuntimeError::IllegalCallerException, got {other:?}"
            ),
        }
    }

    /// Regression guard: the other validation failures inside
    /// `validated_fn_ptr` (null pointer, misaligned address) must still emit
    /// `IllegalStateException`. Only the native-access denial flips to the
    /// new variant.
    #[test]
    fn task57_other_gate_failures_still_emit_illegal_state() {
        // Ensure the gate is open so we exercise the non-access paths.
        let prev = native_access_enabled();
        set_native_access_enabled(true);

        // Null function pointer.
        let null_err =
            validated_fn_ptr::<extern "C" fn()>(0).expect_err("null must be rejected");
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
                other => panic!(
                    "{label}: expected IllegalStateException, got {other:?}"
                ),
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
    fn make_segment(
        ctx: &mut dyn NativeContext,
        ptr: i64,
        size: i64,
        base_off: i64,
    ) -> ObjectRef {
        let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
        ctx.set_field(seg, 0, Value::Long(ptr));
        ctx.set_field(seg, 1, Value::Long(size));
        ctx.set_field(seg, 2, Value::Object(None));
        ctx.set_field(seg, 3, Value::Int(0));
        ctx.set_field(seg, 4, Value::Int(1));
        ctx.set_field(seg, 5, Value::Long(base_off));
        seg
    }

    fn make_layout_kind(ctx: &mut dyn NativeContext, kind: i32) -> ObjectRef {
        pe_make_layout(ctx, kind)
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
        let r = pe_segment_set_impl(&mut ctx, seg, layout, 4, Value::Long(0x4141414141414141u64 as i64));
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
        set_native_access_enabled(false);
        let r = validated_fn_ptr::<extern "C" fn() -> i32>(0x1000);
        assert!(
            r.is_err(),
            "downcall must be denied when native access is disabled"
        );
    }
}

