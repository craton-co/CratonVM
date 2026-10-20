// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! SBR5 ffmlink: the natives of `jdk.internal.foreign.abi.fallback.LibFallback`
//! (the JDK's libffi-backed `FallbackLinker`), so the JDK's own `Linker.nativeLinker()`
//! bytecode can run over this VM, and the reviewed Intrinsics of the real `AbstractLinker` (below).

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::ObjectRef;
use cratonvm_types::Value;
use std::ffi::c_void;

const LF: &str = "jdk/internal/foreign/abi/fallback/LibFallback";

fn long_of(args: &[Value], i: usize) -> i64 {
    match args.get(i) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => 0,
    }
}

fn int_of(args: &[Value], i: usize) -> i32 {
    match args.get(i) {
        Some(Value::Int(v)) => *v,
        Some(Value::Long(v)) => *v as i32,
        _ => 0,
    }
}

/// `Unsafe.allocateMemory` hands out arena HANDLES, not machine addresses, and the JDK's
/// `FallbackLinker` stores such handles inside the tables it builds for libffi. Resolve one
/// (for at least `want` readable bytes); an untagged address is returned as is.
fn real(addr: i64, want: usize) -> *mut u8 {
    let p = crate::panama_libffi::host_base_address(addr, want as i64) as usize as *mut u8;
    if !p.is_null() && crate::panama_libffi::is_arena_handle(addr) {
        HELD.with(|h| h.borrow_mut().push(addr));
    }
    p
}

thread_local! {
    /// Arena handles this thread translated and has not given back yet.
    static HELD: std::cell::RefCell<Vec<i64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Ends, on drop, every translation made since it was created: a pointer the call took is
/// given up when the call returns, so the audit does not report the block as still held when
/// the arena frees it (one warning per block per call otherwise).
struct Release(usize);

impl Release {
    fn new() -> Self {
        Release(HELD.with(|h| h.borrow().len()))
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        let drained: Vec<i64> = HELD.with(|h| {
            let mut h = h.borrow_mut();
            let at = self.0.min(h.len());
            h.split_off(at)
        });
        for addr in drained {
            crate::unsafe_natives_ext::unsafe_arena_release_translation(addr);
        }
    }
}

/// Resolve the arena handles inside a libffi type descriptor tree IN PLACE and return the
/// machine pointer of its root. `ffi_type { size, alignment: u16, type: u16, elements }`.
///
/// # Safety
/// `handle` is an `ffi_type` the JDK built (statics of libffi, or a struct descriptor in an arena).
unsafe fn fix_type(handle: i64) -> *mut libffi::raw::ffi_type {
    let t =
        real(handle, std::mem::size_of::<libffi::raw::ffi_type>()) as *mut libffi::raw::ffi_type;
    if t.is_null() {
        return t;
    }
    if (*t).type_ as u32 == libffi::raw::FFI_TYPE_STRUCT {
        let elems_handle = (*t).elements as usize as i64;
        let mut n = 0usize;
        // The element array is NULL-terminated; bound each step to the slot being read.
        let elems = real(elems_handle, 8) as *mut *mut libffi::raw::ffi_type;
        if !elems.is_null() {
            loop {
                let slot = real(elems_handle + (n as i64) * 8, 8) as *mut i64;
                if slot.is_null() || *slot == 0 {
                    break;
                }
                let fixed = fix_type(*slot);
                *slot = fixed as usize as i64;
                n += 1;
            }
            (*t).elements = real(elems_handle, 8 * (n + 1)) as *mut *mut libffi::raw::ffi_type;
        }
    }
    t
}

/// Resolve an array of `n` `ffi_type*` (handles) in place; returns the machine pointer of the array.
unsafe fn fix_type_array(handle: i64, n: usize) -> *mut *mut libffi::raw::ffi_type {
    let arr = real(handle, 8 * n.max(1)) as *mut i64;
    if arr.is_null() {
        return std::ptr::null_mut();
    }
    for i in 0..n {
        let fixed = fix_type(*arr.add(i));
        *arr.add(i) = fixed as usize as i64;
    }
    arr as *mut *mut libffi::raw::ffi_type
}

fn native_init(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn native_sizeof_cif(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(
        std::mem::size_of::<libffi::raw::ffi_cif>() as i64,
    )))
}

fn native_prep_cif(_c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    let _release = Release::new();
    // SAFETY: the JDK hands a cif it allocated of `sizeofCif()` bytes and type tables it built.
    let rc = unsafe {
        let n = int_of(a, 2) as usize;
        let atypes = fix_type_array(long_of(a, 4), n);
        let rtype = fix_type(long_of(a, 3));
        libffi::raw::ffi_prep_cif(
            real(long_of(a, 0), std::mem::size_of::<libffi::raw::ffi_cif>())
                as *mut libffi::raw::ffi_cif,
            int_of(a, 1) as libffi::raw::ffi_abi,
            n as u32,
            rtype,
            atypes,
        )
    };
    Ok(Some(Value::Int(rc as i32)))
}

fn native_prep_cif_var(_c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    let _release = Release::new();
    // SAFETY: as `native_prep_cif`.
    let rc = unsafe {
        let n = int_of(a, 3) as usize;
        let atypes = fix_type_array(long_of(a, 5), n);
        let rtype = fix_type(long_of(a, 4));
        libffi::raw::ffi_prep_cif_var(
            real(long_of(a, 0), std::mem::size_of::<libffi::raw::ffi_cif>())
                as *mut libffi::raw::ffi_cif,
            int_of(a, 1) as libffi::raw::ffi_abi,
            int_of(a, 2) as u32,
            n as u32,
            rtype,
            atypes,
        )
    };
    Ok(Some(Value::Int(rc as i32)))
}

fn native_struct_offsets(_c: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    let _release = Release::new();
    // SAFETY: the JDK hands a struct type it built and an offsets buffer of one `size_t` per member.
    let rc = unsafe {
        libffi::raw::ffi_get_struct_offsets(
            int_of(a, 0) as libffi::raw::ffi_abi,
            fix_type(long_of(a, 1)),
            real(long_of(a, 2), 8) as *mut usize,
        )
    };
    Ok(Some(Value::Int(rc as i32)))
}

fn native_default_abi(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(
        libffi::low::ffi_abi_FFI_DEFAULT_ABI as i32,
    )))
}

fn native_struct_tag(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(libffi::raw::FFI_TYPE_STRUCT as i32)))
}

macro_rules! type_getter {
    ($name:ident, $sym:ident) => {
        fn $name(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
            // SAFETY: only the address of libffi's static type descriptor is taken.
            let p = unsafe { std::ptr::addr_of_mut!(libffi::raw::$sym) } as usize;
            Ok(Some(Value::Long(p as i64)))
        }
    };
}
type_getter!(native_t_void, ffi_type_void);
type_getter!(native_t_u8, ffi_type_uint8);
type_getter!(native_t_s8, ffi_type_sint8);
type_getter!(native_t_u16, ffi_type_uint16);
type_getter!(native_t_s16, ffi_type_sint16);
type_getter!(native_t_u32, ffi_type_uint32);
type_getter!(native_t_s32, ffi_type_sint32);
type_getter!(native_t_u64, ffi_type_uint64);
type_getter!(native_t_s64, ffi_type_sint64);
type_getter!(native_t_float, ffi_type_float);
type_getter!(native_t_double, ffi_type_double);
type_getter!(native_t_pointer, ffi_type_pointer);

fn native_sizeof_short(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(
        std::mem::size_of::<std::ffi::c_short>() as i32
    )))
}
fn native_sizeof_int(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(
        std::mem::size_of::<std::ffi::c_int>() as i32
    )))
}
fn native_sizeof_long(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(
        std::mem::size_of::<std::ffi::c_long>() as i32
    )))
}
fn native_sizeof_wchar(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(if cfg!(windows) { 2 } else { 4 })))
}
fn native_align_long_long(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(
        std::mem::align_of::<std::ffi::c_longlong>() as i32,
    )))
}
fn native_align_double(_c: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(std::mem::align_of::<f64>() as i32)))
}

/// `doDowncall(cif, fn, rvalue, avalues, captureStateHeapBase, capturedState, mask, heapBases, numArgs)`.
fn native_do_downcall(ctx: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    let _release = Release::new();
    let cif = real(long_of(a, 0), std::mem::size_of::<libffi::raw::ffi_cif>())
        as *mut libffi::raw::ffi_cif;
    let target = long_of(a, 1) as usize;
    let rvalue_handle = long_of(a, 2);
    // SAFETY: the JDK's FallbackLinker built `cif` for exactly this call shape and filled one
    // value pointer per argument.
    unsafe {
        // `numArgs` (a[8]) is `args.length` of the Java call, target included: use the CIF's own count.
        let n = (*cif).nargs as usize;
        let avalues = real(long_of(a, 3), 8 * n.max(1)) as *mut i64;
        for i in 0..n {
            let slot = real(*avalues.add(i), 1);
            // A pointer ARGUMENT carries an `Unsafe.allocateMemory` handle as its VALUE
            // (`MemorySegment.address()`); C needs the machine address.
            let at = *(*cif).arg_types.add(i);
            if !slot.is_null()
                && !at.is_null()
                && (*at).type_ as u32 == libffi::raw::FFI_TYPE_POINTER
            {
                let value = slot as *mut i64;
                *value = real(*value, 1) as usize as i64;
            }
            *avalues.add(i) = slot as usize as i64;
        }
        // libffi writes a full `ffi_arg` (8 bytes) for an integral return narrower than that, and
        // the Java side allocated exactly the layout's size: return through a scratch buffer.
        let rsize = if (*cif).rtype.is_null() {
            0
        } else {
            (*(*cif).rtype).size
        };
        let mut scratch = [0u64; 64];
        let heap;
        let rbuf: *mut u8 = if rsize <= 8 * 64 {
            scratch.as_mut_ptr() as *mut u8
        } else {
            heap = vec![0u64; (rsize + 7) / 8];
            heap.as_ptr() as *mut u8
        };
        let f: unsafe extern "C" fn() = std::mem::transmute(target);
        // An upcall made by the callee re-enters Java through this thread's active context.
        let _guard = crate::panama_libffi::ActiveContextGuard::install(ctx);
        libffi::raw::ffi_call(
            cif,
            Some(f),
            rbuf as *mut std::ffi::c_void,
            avalues as *mut *mut std::ffi::c_void,
        );
        // `Linker.Option.captureCallState("errno")`: CapturableState.ERRNO is bit 2 and, on Linux,
        // the only member of the capture layout (offset 0).
        let mask = int_of(a, 6);
        let captured = long_of(a, 5);
        #[cfg(not(windows))]
        if mask & 4 != 0 && captured != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            let dst = real(captured, 4) as *mut i32;
            if !dst.is_null() {
                *dst = errno;
            }
        }
        #[cfg(windows)]
        let _ = (mask, captured);
        if rvalue_handle != 0 && rsize > 0 {
            let dst = real(rvalue_handle, rsize);
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(rbuf, dst, rsize);
            }
        }
    }
    Ok(None)
}

/// What a closure's callback needs: the VM it belongs to and the global root of the
/// `MethodHandle` the JDK's `LibFallback.createClosure` passed as user data.
struct UpData {
    root: usize,
}

unsafe extern "C" fn upcall_entry(
    _cif: &libffi::low::ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    data: &UpData,
) {
    *result = 0;
    let ret_ptr = result as *mut u64 as i64;
    let args_ptr = args as usize as i64;
    let root = data.root;
    let ran = crate::panama_libffi::with_active_context(|ctx| {
        if let Some(mh) = ctx.resolve_global_root(root) {
            // `LibFallback.doUpcall` catches everything the target throws
            // (`SharedUtils.handleUncaughtException`), so a failure here is the VM's.
            let _ = ctx.invoke_static(
                "jdk/internal/foreign/abi/fallback/LibFallback",
                "doUpcall",
                "(JJLjava/lang/invoke/MethodHandle;)V",
                &[
                    Value::Long(ret_ptr),
                    Value::Long(args_ptr),
                    Value::Object(Some(mh)),
                ],
            );
        }
    });
    if ran.is_none() {
        eprintln!("[cratonvm] FFM fallback-linker upcall on a thread with no active downcall; the VM will now exit");
        cratonvm_native_api::process_exit::exit_process(1)
    }
}

/// `createClosure(long cif, Object userData, long[] ptrs)I`: `ptrs = {closure, code, global root}`.
fn native_create_closure(ctx: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    let _release = Release::new();
    let cif = real(long_of(a, 0), std::mem::size_of::<libffi::raw::ffi_cif>())
        as *mut libffi::raw::ffi_cif;
    let (target, ptrs) = match (a.get(1), a.get(2)) {
        (Some(Value::Object(Some(t))), Some(Value::Object(Some(p)))) => (*t, *p),
        _ => return Ok(Some(Value::Int(1))),
    };
    let root = ctx.add_global_root(target);
    let data = Box::into_raw(Box::new(UpData { root }));
    let (closure, code) = libffi::low::closure_alloc();
    if closure.is_null() {
        // SAFETY: just made by `Box::into_raw`.
        unsafe { drop(Box::from_raw(data)) };
        ctx.remove_global_root(root);
        return Ok(Some(Value::Int(1)));
    }
    // SAFETY: `cif` is a prepared CIF the JDK keeps alive with the stub; `data` lives until `freeClosure`.
    let rc = unsafe {
        libffi::low::prep_closure(
            closure,
            cif as *mut libffi::low::ffi_cif,
            upcall_entry,
            data as *const UpData,
            code,
        )
    };
    if rc.is_err() {
        // SAFETY: as above.
        unsafe {
            libffi::low::closure_free(closure);
            drop(Box::from_raw(data));
        }
        ctx.remove_global_root(root);
        return Ok(Some(Value::Int(2)));
    }
    ctx.set_array_element(ptrs, 0, Value::Long(closure as usize as i64));
    ctx.set_array_element(ptrs, 1, Value::Long(code.as_ptr() as usize as i64));
    ctx.set_array_element(ptrs, 2, Value::Long(data as usize as i64));
    Ok(Some(Value::Int(0)))
}

/// `freeClosure(long closure, long globalTarget)V` (`globalTarget` is the `UpData` pointer).
fn native_free_closure(ctx: &mut dyn NativeContext, a: &[Value]) -> MethodCallResult {
    let closure = long_of(a, 0) as usize as *mut libffi::low::ffi_closure;
    let data = long_of(a, 1) as usize as *mut UpData;
    // SAFETY: both came from `native_create_closure`, and the JDK frees a stub once.
    unsafe {
        if !closure.is_null() {
            libffi::low::closure_free(closure);
        }
        if !data.is_null() {
            let d = Box::from_raw(data);
            ctx.remove_global_root(d.root);
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// The reviewed Intrinsics of the real `AbstractLinker` (SBR5 ffmlink).
//
// `Linker.nativeLinker()` is the JDK's own `SharedUtils.getSystemLinker()` bytecode over
// `CABI.FALLBACK` (`ForeignLinkerSupport.isSupported0` is false and `FallbackLinker.isSupported`
// true), so the linker object, its identity, `canonicalLayouts()` and `defaultLookup()` are the
// JDK's. The ABI-specific half of the linker, which HotSpot does with JVM-generated stubs, is the
// VM's own libffi carrier road: `AbstractLinker.downcallHandle` / `upcallStub` run the JDK's
// argument validation (the real static helpers, so the exceptions and messages are the JDK's),
// then mint the carrier. The JDK's `FallbackLinker.arrangeDowncall` would cost ~130 us per call
// here (docs/internal/jdk-only/SBR5-FFMLINK-fallback-linker-breakdown-and-gaps-20261006.md).
// ---------------------------------------------------------------------------------------------

const ABSTRACT_LINKER: &str = "jdk/internal/foreign/abi/AbstractLinker";
const SHARED_UTILS: &str = "jdk/internal/foreign/abi/SharedUtils";
const LINKER_OPTIONS: &str = "jdk/internal/foreign/abi/LinkerOptions";

fn bare_npe() -> MethodCallFailed {
    RuntimeError::NullPointerException { message: None }.into()
}

fn required(args: &[Value], i: usize) -> Result<ObjectRef, MethodCallFailed> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(bare_npe()),
    }
}

/// The checks `AbstractLinker.downcallHandle0` runs before it arranges the downcall.
fn validate_downcall(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    function: ObjectRef,
    options: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let optset = ctx.invoke_static(
        LINKER_OPTIONS,
        "forDowncall",
        "(Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljdk/internal/foreign/abi/LinkerOptions;",
        &[Value::Object(Some(function)), Value::Object(Some(options))],
    )?;
    if let Some(Value::Object(Some(optset))) = optset {
        ctx.invoke_virtual(
            this,
            "validateVariadicLayouts",
            "(Ljava/lang/foreign/FunctionDescriptor;Ljdk/internal/foreign/abi/LinkerOptions;)V",
            &[Value::Object(Some(function)), Value::Object(Some(optset))],
        )?;
    }
    Ok(())
}

fn intrinsic_downcall_with_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    crate::panama::restricted_downcall_link_check(ctx)?;
    let this = required(args, 0)?;
    let symbol = required(args, 1)?;
    ctx.invoke_static(
        SHARED_UTILS,
        "checkSymbol",
        "(Ljava/lang/foreign/MemorySegment;)V",
        &[Value::Object(Some(symbol))],
    )?;
    let function = required(args, 2)?;
    let options = required(args, 3)?;
    validate_downcall(ctx, this, function, options)?;
    crate::phases_late::foreign_ffm::p67_linker_downcall_with_address(ctx, args)
}

fn intrinsic_downcall_unbound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::panama::restricted_downcall_link_check(ctx)?;
    let this = required(args, 0)?;
    let function = required(args, 1)?;
    let options = required(args, 2)?;
    validate_downcall(ctx, this, function, options)?;
    crate::panama::pe_downcall_handle_unbound(ctx, args)
}

fn intrinsic_upcall_stub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (this, target, function, arena, options)
    crate::panama::restricted_downcall_link_check(ctx)?;
    let _arena = required(args, 3)?;
    let target = required(args, 1)?;
    let function = required(args, 2)?;
    ctx.invoke_static(
        SHARED_UTILS,
        "checkExceptions",
        "(Ljava/lang/invoke/MethodHandle;)V",
        &[Value::Object(Some(target))],
    )?;
    let options = match args.get(4) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Err(bare_npe()),
    };
    ctx.invoke_static(
        LINKER_OPTIONS,
        "forUpcall",
        "(Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljdk/internal/foreign/abi/LinkerOptions;",
        &[Value::Object(Some(function)), Value::Object(Some(options))],
    )?;
    // `type.equals(target.type())`, else `IllegalArgumentException("Wrong method handle type: " + target.type())`.
    let wanted = ctx.invoke_virtual(
        function,
        "toMethodType",
        "()Ljava/lang/invoke/MethodType;",
        &[],
    )?;
    let actual = ctx.invoke_virtual(target, "type", "()Ljava/lang/invoke/MethodType;", &[])?;
    if let (Some(Value::Object(Some(w))), Some(Value::Object(Some(t)))) = (wanted, actual) {
        let same = matches!(
            ctx.invoke_virtual(
                w,
                "equals",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(t))]
            )?,
            Some(Value::Int(1))
        );
        if !same {
            let text = match ctx.invoke_virtual(t, "toString", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Wrong method handle type: {text}"),
            }
            .into());
        }
    }
    crate::panama::pe_upcall_stub(ctx, args)
}

/// `FallbackLinker.canonicalLayouts()` lacks `int8_t` .. `int64_t`, which the platform's own
/// linker answers: this VM stands where HotSpot's platform linker stands, so the answer is that
/// linker's (a pure-Java table: the class is not otherwise used).
fn intrinsic_canonical_layouts(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _ = args;
    let class = if cfg!(windows) {
        "jdk/internal/foreign/abi/x64/windows/Windowsx64Linker"
    } else if cfg!(target_arch = "aarch64") && cfg!(target_os = "macos") {
        "jdk/internal/foreign/abi/aarch64/macos/MacOsAArch64Linker"
    } else if cfg!(target_arch = "aarch64") {
        "jdk/internal/foreign/abi/aarch64/linux/LinuxAArch64Linker"
    } else {
        "jdk/internal/foreign/abi/x64/sysv/SysVx64Linker"
    };
    let instance = ctx.invoke_static(class, "getInstance", &format!("()L{class};"), &[])?;
    match instance {
        Some(Value::Object(Some(linker))) => {
            ctx.invoke_virtual(linker, "canonicalLayouts", "()Ljava/util/Map;", &[])
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn register_ffm_fallback_linker(r: &mut NativeMethodRegistry) {
    let prev = r.current_category();
    r.set_category(NativeKind::Bridge);
    let reg =
        |r: &mut NativeMethodRegistry, n: &str, d: &str, f: cratonvm_native_api::NativeCallback| {
            r.register_with_kind(LF, n, d, f, NativeKind::Bridge);
        };
    reg(r, "init", "()Z", native_init);
    reg(r, "sizeofCif", "()J", native_sizeof_cif);
    reg(r, "ffi_prep_cif", "(JIIJJ)I", native_prep_cif);
    reg(r, "ffi_prep_cif_var", "(JIIIJJ)I", native_prep_cif_var);
    reg(r, "ffi_get_struct_offsets", "(IJJ)I", native_struct_offsets);
    reg(r, "ffi_default_abi", "()I", native_default_abi);
    reg(r, "ffi_type_struct", "()S", native_struct_tag);
    reg(r, "ffi_type_void", "()J", native_t_void);
    reg(r, "ffi_type_uint8", "()J", native_t_u8);
    reg(r, "ffi_type_sint8", "()J", native_t_s8);
    reg(r, "ffi_type_uint16", "()J", native_t_u16);
    reg(r, "ffi_type_sint16", "()J", native_t_s16);
    reg(r, "ffi_type_uint32", "()J", native_t_u32);
    reg(r, "ffi_type_sint32", "()J", native_t_s32);
    reg(r, "ffi_type_uint64", "()J", native_t_u64);
    reg(r, "ffi_type_sint64", "()J", native_t_s64);
    reg(r, "ffi_type_float", "()J", native_t_float);
    reg(r, "ffi_type_double", "()J", native_t_double);
    reg(r, "ffi_type_pointer", "()J", native_t_pointer);
    reg(r, "ffi_sizeof_short", "()I", native_sizeof_short);
    reg(r, "ffi_sizeof_int", "()I", native_sizeof_int);
    reg(r, "ffi_sizeof_long", "()I", native_sizeof_long);
    reg(r, "ffi_sizeof_wchar", "()I", native_sizeof_wchar);
    reg(r, "alignof_long_long", "()I", native_align_long_long);
    reg(r, "alignof_double", "()I", native_align_double);
    reg(
        r,
        "createClosure",
        "(JLjava/lang/Object;[J)I",
        native_create_closure,
    );
    reg(r, "freeClosure", "(JJ)V", native_free_closure);
    reg(
        r,
        "doDowncall",
        "(JJJJLjava/lang/Object;JI[Ljava/lang/Object;I)V",
        native_do_downcall,
    );
    r.register_with_kind(
        ABSTRACT_LINKER,
        "downcallHandle",
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;",
        intrinsic_downcall_with_address,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        ABSTRACT_LINKER,
        "downcallHandle",
        "(Ljava/lang/foreign/FunctionDescriptor;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/invoke/MethodHandle;",
        intrinsic_downcall_unbound,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        ABSTRACT_LINKER,
        "upcallStub",
        "(Ljava/lang/invoke/MethodHandle;Ljava/lang/foreign/FunctionDescriptor;Ljava/lang/foreign/Arena;[Ljava/lang/foreign/Linker$Option;)Ljava/lang/foreign/MemorySegment;",
        intrinsic_upcall_stub,
        NativeKind::Intrinsic,
    );
    // `CABI.computeCurrent`: `ForeignLinkerSupport.isSupported()` is false (no JVM stubs), so the
    // JDK asks whether its libffi fallback is available (it loads `libfallbackLinker`, which this
    // JDK image does not ship). The natives of `LibFallback` above are the VM's.
    r.register_with_kind(
        "jdk/internal/foreign/abi/fallback/FallbackLinker",
        "isSupported",
        "()Z",
        |_c, _a| Ok(Some(Value::Int(1))),
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        "jdk/internal/foreign/abi/fallback/FallbackLinker",
        "canonicalLayouts",
        "()Ljava/util/Map;",
        intrinsic_canonical_layouts,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        "jdk/internal/vm/ForeignLinkerSupport",
        "isSupported0",
        "()Z",
        |_c, _a| Ok(Some(Value::Int(0))),
        NativeKind::Bridge,
    );
    r.set_category(prev);
}
